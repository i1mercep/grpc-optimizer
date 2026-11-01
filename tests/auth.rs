use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::future::BoxFuture;
use grpc_optimizer::{
    auth::{AuthenticationError, Principal, TokenVerifier, config::AuthConfig, oidc::OidcVerifier},
    config::Config,
    engine::Engine,
    grpc::{Service, auth::AuthLayer},
    pb::{
        DESCRIPTOR, SessionRequest, SolveInput, optimizer_client::OptimizerClient,
        optimizer_server::OptimizerServer, session_request,
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

#[path = "support/oidc.rs"]
mod oidc;

struct RunningServer {
    address: String,
    client: OptimizerClient<Channel>,
    engine: Engine,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    async fn start(verifier: Option<Arc<dyn TokenVerifier>>) -> Self {
        let engine = Engine::new(Config {
            cpu_budget: 4,
            max_concurrent_solves: 4,
            max_active_rpcs: 4,
            ..Config::default()
        })
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let service = Service::new(engine.clone()).into_server();
        let reflection = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(DESCRIPTOR)
            .build_v1()
            .unwrap();
        let (reporter, health) = tonic_health::server::health_reporter();
        reporter.set_serving::<OptimizerServer<Service>>().await;
        let shutdown = CancellationToken::new();
        let signal = shutdown.clone();
        let task = tokio::spawn(async move {
            Server::builder()
                .layer(AuthLayer::new(verifier))
                .add_service(service)
                .add_service(health)
                .add_service(reflection)
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    signal.cancelled_owned(),
                )
                .await
                .unwrap();
        });
        let client = OptimizerClient::connect(address.clone()).await.unwrap();
        Self {
            address,
            client,
            engine,
            shutdown,
            task,
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.engine.shutdown();
        self.shutdown.cancel();
        self.task.abort();
    }
}

fn request<T>(body: T, token: &str) -> Request<T> {
    let mut request = Request::new(body);
    request.set_timeout(Duration::from_secs(5));
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn job() -> grpc_optimizer::pb::SolveRequest {
    samples::fixture("production").unwrap().request
}

fn session() -> Vec<SessionRequest> {
    vec![
        SessionRequest {
            kind: Some(session_request::Kind::Model(job().model.unwrap())),
        },
        SessionRequest {
            kind: Some(session_request::Kind::Input(SolveInput {
                request_id: "session".into(),
                ..Default::default()
            })),
        },
    ]
}

async fn verifier(provider: &oidc::Provider) -> Arc<OidcVerifier> {
    Arc::new(
        OidcVerifier::discover(AuthConfig {
            issuer: provider.issuer(),
            audience: "grpc-optimizer".into(),
            algorithms: vec![jsonwebtoken::Algorithm::RS256],
            allow_insecure_http: true,
        })
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn authenticates_all_optimizer_rpcs_and_rejects_before_admission() {
    let provider = oidc::Provider::start().await;
    let mut server = RunningServer::start(Some(verifier(&provider).await)).await;
    let token = provider.token(1);
    assert_eq!(
        server
            .client
            .solve(request(job(), &token))
            .await
            .unwrap()
            .into_inner()
            .status,
        1
    );
    let mut output = server
        .client
        .solve_stream(request(tokio_stream::iter(vec![job()]), &token))
        .await
        .unwrap()
        .into_inner();
    assert!(output.message().await.unwrap().unwrap().outcome.is_some());
    assert!(output.message().await.unwrap().is_none());
    let mut output = server
        .client
        .solve_session(request(tokio_stream::iter(session()), &token))
        .await
        .unwrap()
        .into_inner();
    assert!(output.message().await.unwrap().unwrap().outcome.is_some());
    assert!(output.message().await.unwrap().is_none());
    for invalid in [
        None,
        Some("Basic user"),
        Some("Bearer opaque"),
        Some("Bearer "),
        Some("Bearer token extra"),
        Some("Bearer one,two"),
    ] {
        let mut call = Request::new(job());
        if let Some(value) = invalid {
            call.metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        let error = server.client.solve(call).await.unwrap_err();
        assert_eq!(error.code(), Code::Unauthenticated);
        assert!(!error.message().contains("opaque"));
    }
    let mut duplicated = request(job(), &token);
    duplicated
        .metadata_mut()
        .append("authorization", format!("Bearer {token}").parse().unwrap());
    assert_eq!(
        server.client.solve(duplicated).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    assert_eq!(
        server
            .client
            .solve_stream(tokio_stream::iter(vec![job()]))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert_eq!(
        server
            .client
            .solve_session(tokio_stream::iter(session()))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    let _permits: Vec<_> = (0..4).map(|_| server.engine.admit_rpc().unwrap()).collect();
    assert_eq!(
        server.client.solve(job()).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    assert_eq!(
        server
            .client
            .solve(request(job(), &token))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
}

#[tokio::test]
async fn health_is_public_and_reflection_requires_authentication() {
    use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};
    use tonic_reflection::pb::v1::{
        ServerReflectionRequest, server_reflection_client::ServerReflectionClient,
        server_reflection_request::MessageRequest,
    };
    let provider = oidc::Provider::start().await;
    let server = RunningServer::start(Some(verifier(&provider).await)).await;
    let channel = Channel::from_shared(server.address.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut health = HealthClient::new(channel.clone());
    assert_eq!(
        health
            .check(HealthCheckRequest {
                service: "optimization.v1.Optimizer".into()
            })
            .await
            .unwrap()
            .into_inner()
            .status,
        1
    );
    let mut watch = health
        .watch(HealthCheckRequest {
            service: "optimization.v1.Optimizer".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(watch.message().await.unwrap().unwrap().status, 1);
    drop(watch);
    let mut reflection = ServerReflectionClient::new(channel);
    let message = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    assert_eq!(
        reflection
            .server_reflection_info(tokio_stream::iter(vec![message.clone()]))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    let mut output = reflection
        .server_reflection_info(request(
            tokio_stream::iter(vec![message]),
            &provider.token(1),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(output.message().await.unwrap().is_some());
}

struct SwitchVerifier {
    allowed: AtomicBool,
    calls: AtomicUsize,
}
impl TokenVerifier for SwitchVerifier {
    fn verify<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Principal, AuthenticationError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            if !self.allowed.load(Ordering::SeqCst) {
                return Err(AuthenticationError::InvalidToken);
            }
            Ok(Principal {
                issuer: "issuer".into(),
                subject: "user".into(),
                audiences: vec!["api".into()],
                expires_at: 1,
            })
        })
    }
}

#[tokio::test]
async fn streams_authenticate_only_at_start_and_disabled_mode_remains_compatible() {
    let verifier = Arc::new(SwitchVerifier {
        allowed: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
    });
    let mut server = RunningServer::start(Some(verifier.clone())).await;
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    let mut output = server
        .client
        .solve_stream(request(ReceiverStream::new(rx), "token"))
        .await
        .unwrap()
        .into_inner();
    verifier.allowed.store(false, Ordering::SeqCst);
    tx.send(job()).await.unwrap();
    drop(tx);
    assert!(output.message().await.unwrap().is_some());
    assert!(output.message().await.unwrap().is_none());
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        server
            .client
            .solve(request(job(), "token"))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    let mut anonymous = RunningServer::start(None).await;
    assert_eq!(
        anonymous
            .client
            .solve(job())
            .await
            .unwrap()
            .into_inner()
            .status,
        1
    );
}

/// Runs separately after Python dependencies are installed; no external IAM is required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run through just auth-smoke after just python-setup"]
async fn authenticated_python_interoperability() {
    let provider = oidc::Provider::start().await;
    let server = RunningServer::start(Some(verifier(&provider).await)).await;
    let target = server.address.strip_prefix("http://").unwrap().to_owned();
    let token = provider.token(1);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let python = root.join(".venv/bin/python");
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(python)
            .arg("clients/python/smoke_test.py")
            .current_dir(root)
            .env("OPT_GRPC_TARGET", target)
            .env("OPT_ACCESS_TOKEN", token)
            .status()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(status.success());
}
