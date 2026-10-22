use std::time::Duration;

use grpc_optimizer::{
    config::Config,
    engine::Engine,
    grpc::Service,
    pb::{
        BoundOverride, JobOutcome, SessionRequest, SolveInput, SolveRequest, SolveStatus,
        job_outcome, optimizer_client::OptimizerClient, session_request::Kind,
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
async fn unary_and_bidi_solve() {
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
async fn per_job_errors_do_not_terminate_bidi() {
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
    let mut bidi = server
        .client
        .solve_stream(tokio_stream::iter(input))
        .await
        .unwrap()
        .into_inner();
    let mut outcomes = Vec::new();
    while let Some(item) = bidi.message().await.unwrap() {
        outcomes.push(item);
    }
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().any(|o| matches!(&o.outcome, Some(job_outcome::Outcome::Error(e)) if e.code == Code::InvalidArgument as i32)));
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(&o.outcome, Some(job_outcome::Outcome::Result(_))))
    );
}

fn model_message() -> SessionRequest {
    SessionRequest {
        kind: samples::fixture("production")
            .unwrap()
            .request
            .model
            .map(Kind::Model),
    }
}

fn input_message(input: SolveInput) -> SessionRequest {
    SessionRequest {
        kind: Some(Kind::Input(input)),
    }
}

fn input(id: &str) -> SolveInput {
    SolveInput {
        request_id: id.into(),
        ..SolveInput::default()
    }
}

#[tokio::test]
async fn session_sends_model_once_and_applies_per_job_inputs() {
    let mut server = RunningServer::start(Config::default()).await;
    let capped = SolveInput {
        constraint_bounds: vec![BoundOverride {
            index: 0,
            lower: None,
            upper: Some(50.0),
        }],
        ..input("capped")
    };
    let invalid = SolveInput {
        objective_indices: vec![9],
        objective_values: vec![1.0],
        ..input("invalid")
    };
    let messages = [
        model_message(),
        input_message(input("base")),
        input_message(capped),
        input_message(invalid),
        input_message(input("after")),
    ];
    let mut output = server
        .client
        .solve_session(tokio_stream::iter(messages))
        .await
        .unwrap()
        .into_inner();
    let mut outcomes = std::collections::HashMap::new();
    while let Some(outcome) = output.message().await.unwrap() {
        outcomes.insert(outcome.request_id.clone(), outcome);
    }
    assert_eq!(outcomes.len(), 4);
    success(&outcomes["base"]);
    success(&outcomes["after"]);
    let Some(job_outcome::Outcome::Result(result)) = &outcomes["capped"].outcome else {
        panic!("{:?}", outcomes["capped"]);
    };
    assert!((result.solution.as_ref().unwrap().objective - 2000.0).abs() < 1e-6);
    assert!(matches!(
        &outcomes["invalid"].outcome,
        Some(job_outcome::Outcome::Error(e)) if e.code == Code::InvalidArgument as i32
    ));
}

#[tokio::test]
async fn session_protocol_violations_terminate_the_call() {
    let mut server = RunningServer::start(Config::default()).await;
    let mut bad_model = model_message();
    if let Some(Kind::Model(model)) = &mut bad_model.kind {
        model.sense = 0;
    }
    let cases = [
        vec![input_message(input("early"))],
        vec![model_message(), model_message()],
        vec![bad_model, input_message(input("a"))],
        vec![SessionRequest { kind: None }],
    ];
    for messages in cases {
        let error = match server
            .client
            .solve_session(tokio_stream::iter(messages))
            .await
        {
            Err(error) => error,
            Ok(response) => {
                let mut output = response.into_inner();
                loop {
                    match output.message().await {
                        Ok(Some(_)) => continue,
                        Ok(None) => panic!("session ended without an error"),
                        Err(error) => break error,
                    }
                }
            }
        };
        assert_eq!(error.code(), Code::InvalidArgument);
    }
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
    let error = match server.client.solve_stream(request).await {
        Ok(response) => response.into_inner().message().await.unwrap_err(),
        Err(error) => error,
    };
    assert!(matches!(
        error.code(),
        Code::DeadlineExceeded | Code::Cancelled
    ));
    drop(tx);
}
