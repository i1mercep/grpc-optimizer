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
        solver::check_budget(deadline, &cancel)?;
        if request.encoded_len() > self.config.max_message_bytes {
            return Err(Error::Exhausted("request exceeds message limit".into()));
        }
        let config = self.config.clone();
        self.run_blocking(move || solver::solve(request, &config, deadline, &cancel))
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
