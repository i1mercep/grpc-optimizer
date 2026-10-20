use std::{sync::Arc, time::Instant};

use prost::Message;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    config::Config,
    model::{Error, Result},
    pb::{SolveRequest, SolveResult},
    solver,
};

#[derive(Clone)]
pub struct Engine {
    pub config: Arc<Config>,
    slots: Arc<Semaphore>,
    rpcs: Arc<Semaphore>,
    workers: TaskTracker,
    shutdown: CancellationToken,
    #[cfg(test)]
    pub(crate) test_control: Option<Arc<solver::TestControl>>,
}

impl Engine {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(config.max_concurrent_solves)),
            rpcs: Arc::new(Semaphore::new(config.max_active_rpcs)),
            config: Arc::new(config),
            workers: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            #[cfg(test)]
            test_control: None,
        })
    }

    pub fn admit_rpc(&self) -> Result<OwnedSemaphorePermit> {
        if self.shutdown.is_cancelled() {
            return Err(Error::Unavailable);
        }
        self.rpcs
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Exhausted("active RPC limit reached".into()))
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        self.slots.close();
        self.rpcs.close();
        self.workers.close();
    }

    pub async fn drain(&self) {
        self.workers.wait().await;
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub async fn solve(
        &self,
        request: SolveRequest,
        deadline: Option<Instant>,
        cancel: CancellationToken,
    ) -> Result<SolveResult> {
        // Dropping one solve must interrupt its worker without cancelling sibling
        // jobs that share the RPC's parent token.
        let cancel = cancel.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        solver::check_budget(deadline, &cancel)?;
        if request.encoded_len() > self.config.max_message_bytes {
            return Err(Error::Exhausted("request exceeds message limit".into()));
        }
        let config = self.config.clone();
        #[cfg(test)]
        let test_control = self
            .test_control
            .clone()
            .filter(|c| c.request_id == request.request_id);
        self.run_blocking(move || {
            solver::solve_inner(
                request,
                &config,
                deadline,
                &cancel,
                #[cfg(test)]
                test_control,
            )
        })
        .await
    }

    async fn run_blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        // Register before checking shutdown so drain cannot miss admitted work.
        let tracked = self.workers.token();
        if self.shutdown.is_cancelled() {
            return Err(Error::Unavailable);
        }
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            Error::Exhausted("solver capacity reached; retry with lower concurrency".into())
        })?;
        tokio::task::spawn_blocking(move || {
            // The native work owns this permit, including after its async caller is dropped.
            let _permit = permit;
            let _tracked = tracked;
            work()
        })
        .await
        .map_err(|e| Error::Internal(format!("solver worker failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn dropping_a_solve_cancels_its_worker_without_cancelling_siblings() {
        let request =
            crate::samples::generate(crate::samples::Family::Production, 100, 42).unwrap();
        let (control, entered, release) = solver::TestControl::new(&request.request_id, "simplex");
        let mut engine = Engine::new(Config {
            cpu_budget: 2,
            max_concurrent_solves: 2,
            ..Config::default()
        })
        .unwrap();
        engine.test_control = Some(control.clone());
        let parent = CancellationToken::new();
        let cloned = engine.clone();
        let token = parent.clone();
        let task = tokio::spawn(async move { cloned.solve(request, None, token).await });
        let worker_token = tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(worker_token.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "one job must not cancel the shared parent"
        );
        let healthy = crate::samples::fixture("production").unwrap().request;
        let result = engine.solve(healthy, None, parent.clone()).await.unwrap();
        assert_eq!(result.status, crate::pb::SolveStatus::Optimal as i32);
        assert!(
            !parent.is_cancelled(),
            "normal completion must not cancel the parent either"
        );
        release.send(()).unwrap();
        engine.shutdown();
        tokio::time::timeout(Duration::from_secs(5), engine.drain())
            .await
            .unwrap();
        assert_eq!(
            *control.status.lock().unwrap(),
            Some(highs::HighsModelStatus::ReachedInterrupt)
        );
    }

    #[tokio::test]
    async fn shutdown_drains_an_active_native_solve_without_cancelling_it() {
        let request =
            crate::samples::generate(crate::samples::Family::Production, 100, 42).unwrap();
        let (control, entered, release) = solver::TestControl::new(&request.request_id, "simplex");
        let mut engine = Engine::new(Config::default()).unwrap();
        engine.test_control = Some(control.clone());
        let cloned = engine.clone();
        let task =
            tokio::spawn(
                async move { cloned.solve(request, None, CancellationToken::new()).await },
            );
        let worker_token = tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap();
        engine.shutdown();
        assert!(!worker_token.is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), engine.drain())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.status, crate::pb::SolveStatus::Optimal as i32);
        tokio::time::timeout(Duration::from_secs(5), engine.drain())
            .await
            .unwrap();
        assert_eq!(
            *control.status.lock().unwrap(),
            Some(highs::HighsModelStatus::Optimal)
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_retains_native_permit_and_shutdown_drains() {
        let engine = Engine::new(Config {
            max_concurrent_solves: 1,
            ..Config::default()
        })
        .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let cloned = engine.clone();
        let task = tokio::spawn(async move {
            cloned
                .run_blocking(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(matches!(
            engine.run_blocking(|| Ok(())).await,
            Err(Error::Exhausted(_))
        ));
        engine.shutdown();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), engine.drain())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), engine.drain())
            .await
            .unwrap();
        assert!(matches!(engine.admit_rpc(), Err(Error::Unavailable)));
    }
}
