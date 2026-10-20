use std::time::Duration;

use grpc_optimizer::{
    config::Config,
    engine::Engine,
    grpc::Service,
    pb::{
        BatchRequest, JobOutcome, SolveRequest, SolveStatus, job_outcome,
        optimizer_client::OptimizerClient,
    },
    samples,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{
    Code, Request,
    transport::{Channel, Server},
};

struct RunningServer {
    client: OptimizerClient<Channel>,
    engine: Engine,
    shutdown: CancellationToken,
}

impl RunningServer {
    async fn start(mut config: Config) -> Self {
        config.cpu_budget = 4;
        config.max_concurrent_solves = 4;
        let engine = Engine::new(config).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let signal = shutdown.clone();
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
            shutdown,
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.engine.shutdown();
        self.shutdown.cancel();
    }
}

fn jobs(count: usize) -> Vec<SolveRequest> {
    (0..count)
        .map(|i| {
            let mut request = samples::fixture("production").unwrap().request;
            request.request_id = format!("job-{i}");
            request
        })
        .collect()
}

fn success(outcome: &JobOutcome) {
    let Some(job_outcome::Outcome::Result(result)) = &outcome.outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(result.request_id, outcome.request_id);
    assert_eq!(result.status, SolveStatus::Optimal as i32);
    assert!((result.solution.as_ref().unwrap().objective - 3600.0).abs() < 1e-6);
}

#[tokio::test]
async fn all_patterns_solve_and_collect_preserves_submission_order() {
    let mut server = RunningServer::start(Config {
        stream_window: 2,
        ..Config::default()
    })
    .await;
    let result = server
        .client
        .solve(jobs(1).remove(0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.status, SolveStatus::Optimal as i32);
    let mut output = server
        .client
        .solve_batch(BatchRequest { jobs: jobs(4) })
        .await
        .unwrap()
        .into_inner();
    let mut count = 0;
    while let Some(outcome) = output.message().await.unwrap() {
        success(&outcome);
        count += 1;
    }
    assert_eq!(count, 4);
    let response = server
        .client
        .collect(tokio_stream::iter(jobs(4)))
        .await
        .unwrap()
        .into_inner();
    for (i, outcome) in response.outcomes.iter().enumerate() {
        assert_eq!(outcome.request_id, format!("job-{i}"));
        success(outcome);
    }
    assert_eq!(response.outcomes.len(), 4);
    let mut output = server
        .client
        .solve_stream(tokio_stream::iter(jobs(4)))
        .await
        .unwrap()
        .into_inner();
    let mut count = 0;
    while let Some(outcome) = output.message().await.unwrap() {
        success(&outcome);
        count += 1;
    }
    assert_eq!(count, 4);
}

#[tokio::test]
async fn bidi_returns_before_input_closes_and_drains_after_half_close() {
    let mut server = RunningServer::start(Config::default()).await;
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    let mut output = server
        .client
        .solve_stream(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    tx.send(jobs(1).remove(0)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(3), output.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    success(&first);
    for job in jobs(3) {
        tx.send(job).await.unwrap();
    }
    drop(tx);
    let mut remaining = 0;
    while let Some(outcome) = output.message().await.unwrap() {
        success(&outcome);
        remaining += 1;
    }
    assert_eq!(remaining, 3);
}

#[tokio::test]
async fn per_job_errors_do_not_terminate_batch_collect_or_bidi() {
    let mut server = RunningServer::start(Config::default()).await;
    let mut input = jobs(2);
    input[0].model.as_mut().unwrap().sense = 0;
    assert_eq!(
        server
            .client
            .solve(input[0].clone())
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let collected = server
        .client
        .collect(tokio_stream::iter(input.clone()))
        .await
        .unwrap()
        .into_inner()
        .outcomes;
    let mut batch = server
        .client
        .solve_batch(BatchRequest {
            jobs: input.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut batched = Vec::new();
    while let Some(item) = batch.message().await.unwrap() {
        batched.push(item);
    }
    let mut bidi = server
        .client
        .solve_stream(tokio_stream::iter(input))
        .await
        .unwrap()
        .into_inner();
    let mut streamed = Vec::new();
    while let Some(item) = bidi.message().await.unwrap() {
        streamed.push(item);
    }
    for outcomes in [collected, batched, streamed] {
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().any(|o| matches!(&o.outcome, Some(job_outcome::Outcome::Error(e)) if e.code == Code::InvalidArgument as i32)));
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(&o.outcome, Some(job_outcome::Outcome::Result(_))))
        );
    }
}

#[tokio::test]
async fn finite_job_and_collected_byte_limits_are_enforced() {
    let mut server = RunningServer::start(Config {
        max_batch_jobs: 2,
        stream_window: 2,
        ..Config::default()
    })
    .await;
    assert_eq!(
        server
            .client
            .solve_batch(BatchRequest { jobs: jobs(3) })
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    assert_eq!(
        server
            .client
            .collect(tokio_stream::iter(jobs(3)))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    let mut server = RunningServer::start(Config {
        max_collect_bytes: 1,
        ..Config::default()
    })
    .await;
    assert_eq!(
        server
            .client
            .collect(tokio_stream::iter(jobs(1)))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    let response = server
        .client
        .collect(tokio_stream::empty())
        .await
        .unwrap()
        .into_inner();
    assert!(response.outcomes.is_empty());
}

#[tokio::test]
async fn active_rpc_admission_and_transport_size_are_bounded() {
    let mut server = RunningServer::start(Config {
        max_active_rpcs: 1,
        ..Config::default()
    })
    .await;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let output = server
        .client
        .solve_stream(ReceiverStream::new(rx))
        .await
        .unwrap();
    assert_eq!(
        server
            .client
            .solve(jobs(1).remove(0))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    drop(output);
    drop(tx);
    let mut server = RunningServer::start(Config {
        max_message_bytes: 64,
        ..Config::default()
    })
    .await;
    let error = server.client.solve(jobs(1).remove(0)).await.unwrap_err();
    assert!(matches!(
        error.code(),
        Code::OutOfRange | Code::ResourceExhausted
    ));
}

#[tokio::test]
async fn slow_bidi_consumer_does_not_block_other_calls() {
    let mut server = RunningServer::start(Config {
        stream_window: 2,
        ..Config::default()
    })
    .await;
    let mut output = server
        .client
        .solve_stream(tokio_stream::iter(jobs(12)))
        .await
        .unwrap()
        .into_inner();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let result = server
        .client
        .solve(jobs(1).remove(0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.status, SolveStatus::Optimal as i32);
    let mut count = 0;
    while let Some(outcome) = output.message().await.unwrap() {
        success(&outcome);
        count += 1;
    }
    assert_eq!(count, 12);
}

#[tokio::test]
async fn deadlines_are_enforced_and_invalid_metadata_is_rejected() {
    let mut server = RunningServer::start(Config::default()).await;
    let mut request = Request::new(jobs(1).remove(0));
    request.set_timeout(Duration::ZERO);
    // Tonic's transport maps its own TimeoutExpired to CANCELLED; our handler
    // maps an expired application budget to DEADLINE_EXCEEDED. Either race must
    // terminate the RPC instead of returning a successful optimization result.
    assert!(matches!(
        server.client.solve(request).await.unwrap_err().code(),
        Code::DeadlineExceeded | Code::Cancelled
    ));
    let mut request = Request::new(jobs(1).remove(0));
    request
        .metadata_mut()
        .insert("grpc-timeout", "invalid".parse().unwrap());
    assert!(grpc_optimizer::grpc::deadline(&request).is_err());
    for unit in ["H", "M", "S", "m", "u", "n"] {
        request
            .metadata_mut()
            .insert("grpc-timeout", format!("1{unit}").parse().unwrap());
        assert!(grpc_optimizer::grpc::deadline(&request).unwrap().is_some());
    }
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut request = Request::new(ReceiverStream::new(rx));
    request.set_timeout(Duration::from_millis(30));
    assert!(matches!(
        server.client.collect(request).await.unwrap_err().code(),
        Code::DeadlineExceeded | Code::Cancelled
    ));
    drop(tx);
}
