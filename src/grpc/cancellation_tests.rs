use std::{sync::Arc, time::Duration};

use highs::HighsModelStatus;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::{
    Code, Request, Status,
    transport::{Channel, Server},
};

use crate::{
    config::Config,
    engine::Engine,
    pb::{BatchRequest, SolveRequest, SolveStatus, optimizer_client::OptimizerClient},
    samples::{self, Family},
    solver::TestControl,
};

use super::Service;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Unary,
    Batch,
    Collect,
    Bidi,
}

impl Mode {
    const ALL: [Self; 4] = [Self::Unary, Self::Batch, Self::Collect, Self::Bidi];
}

struct RunningServer {
    client: OptimizerClient<Channel>,
    engine: Engine,
    stop: CancellationToken,
}

impl RunningServer {
    async fn start(control: Arc<TestControl>) -> Self {
        Self::with_config(
            control,
            Config {
                cpu_budget: 1,
                max_concurrent_solves: 1,
                ..Config::default()
            },
        )
        .await
    }

    async fn with_config(control: Arc<TestControl>, config: Config) -> Self {
        let mut engine = Engine::new(config).unwrap();
        engine.test_control = Some(control);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let signal = stop.clone();
        let service = Service::new(engine.clone()).into_server();
        tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    signal.cancelled_owned(),
                )
                .await
                .unwrap();
        });
        let client = OptimizerClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Self {
            client,
            engine,
            stop,
        }
    }

    async fn assert_occupied(&self) {
        let error = self.client.clone().solve(healthy_job()).await.unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
    }

    async fn assert_capacity_recovers(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.client.clone().solve(healthy_job()).await {
                    Ok(response) => {
                        assert_eq!(response.into_inner().status, SolveStatus::Optimal as i32);
                        break;
                    }
                    Err(error) if error.code() == Code::ResourceExhausted => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => panic!("unexpected recovery error: {error}"),
                }
            }
        })
        .await
        .expect("native worker must release capacity");
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.engine.shutdown();
        self.stop.cancel();
    }
}

fn healthy_job() -> SolveRequest {
    samples::fixture("production").unwrap().request
}

fn timed<T>(value: T, deadline: bool) -> Request<T> {
    let mut request = Request::new(value);
    if deadline {
        request.set_timeout(Duration::from_secs(1));
    }
    request
}

async fn call(
    mut client: OptimizerClient<Channel>,
    mode: Mode,
    job: SolveRequest,
    deadline: bool,
) -> Result<(), Status> {
    match mode {
        Mode::Unary => {
            client.solve(timed(job, deadline)).await?;
        }
        Mode::Batch => {
            let mut output = client
                .solve_batch(timed(BatchRequest { jobs: vec![job] }, deadline))
                .await?
                .into_inner();
            while output.message().await?.is_some() {}
        }
        Mode::Collect => {
            client
                .collect(timed(tokio_stream::iter([job]), deadline))
                .await?;
        }
        Mode::Bidi => {
            let mut output = client
                .solve_stream(timed(tokio_stream::iter([job]), deadline))
                .await?
                .into_inner();
            while output.message().await?.is_some() {}
        }
    }
    Ok(())
}

#[tokio::test]
async fn disconnects_interrupt_native_work_and_restore_capacity_in_every_rpc_pattern() {
    for mode in Mode::ALL {
        let job = samples::generate(Family::Production, 100, 42).unwrap();
        let (control, entered, release) = TestControl::new(&job.request_id, "simplex");
        let server = RunningServer::start(control.clone()).await;
        let task = tokio::spawn(call(server.client.clone(), mode, job, false));
        let token = tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .expect("native callback must start")
            .unwrap();
        server.assert_occupied().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .unwrap_or_else(|_| panic!("{mode:?} disconnect did not cancel native work"));
        // Cancellation alone cannot release a permit while the callback is paused.
        server.assert_occupied().await;
        release.send(()).unwrap();
        server.assert_capacity_recovers().await;
        assert_eq!(
            *control.status.lock().unwrap(),
            Some(HighsModelStatus::ReachedInterrupt),
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn deadlines_interrupt_native_work_and_restore_capacity_in_every_rpc_pattern() {
    for mode in Mode::ALL {
        let job = samples::generate(Family::Production, 100, 42).unwrap();
        let (control, entered, release) = TestControl::new(&job.request_id, "simplex");
        let server = RunningServer::start(control.clone()).await;
        let task = tokio::spawn(call(server.client.clone(), mode, job, true));
        let token = tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .expect("native callback must start before deadline")
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .expect_err("expired RPC must not succeed");
        assert!(
            matches!(error.code(), Code::DeadlineExceeded | Code::Cancelled),
            "{mode:?}: {error}"
        );
        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .unwrap();
        server.assert_occupied().await;
        release.send(()).unwrap();
        server.assert_capacity_recovers().await;
        assert_eq!(
            *control.status.lock().unwrap(),
            Some(HighsModelStatus::ReachedInterrupt),
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn collected_response_overflow_interrupts_other_running_jobs() {
    let job = samples::generate(Family::Production, 100, 42).unwrap();
    let (control, entered, release) = TestControl::new(&job.request_id, "simplex");
    let server = RunningServer::with_config(
        control.clone(),
        Config {
            cpu_budget: 2,
            max_concurrent_solves: 2,
            max_collect_bytes: 1,
            ..Config::default()
        },
    )
    .await;
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tx.send(job).await.unwrap();
    let mut client = server.client.clone();
    let task = tokio::spawn(async move {
        client
            .collect(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
    });
    let token = tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    // Only submit the overflowing result after the other job has entered HiGHS.
    tx.send(healthy_job()).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), Code::ResourceExhausted);
    tokio::time::timeout(Duration::from_secs(5), token.cancelled())
        .await
        .unwrap();
    release.send(()).unwrap();
    server.engine.shutdown();
    tokio::time::timeout(Duration::from_secs(5), server.engine.drain())
        .await
        .unwrap();
    assert_eq!(
        *control.status.lock().unwrap(),
        Some(HighsModelStatus::ReachedInterrupt)
    );
}
