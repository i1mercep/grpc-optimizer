//! Local OIDC provider shared by verifier, transport, and interoperability tests.
#![allow(dead_code)]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::get,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

pub struct ProviderState {
    pub issuer: String,
    pub discovery: Mutex<Option<Value>>,
    pub keys: Mutex<Value>,
    pub raw_keys: Mutex<Option<String>>,
    pub status: AtomicU16,
    pub delay_millis: AtomicU64,
    pub requests: AtomicUsize,
    pub chunked: AtomicBool,
}

pub struct Provider {
    pub state: Arc<ProviderState>,
    task: JoinHandle<()>,
}

impl Provider {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}/realms/optimizer", listener.local_addr().unwrap());
        let state = Arc::new(ProviderState {
            issuer,
            discovery: Mutex::new(None),
            keys: Mutex::new(json!({"keys": [jwk(1)]})),
            raw_keys: Mutex::new(None),
            status: AtomicU16::new(200),
            delay_millis: AtomicU64::new(0),
            requests: AtomicUsize::new(0),
            chunked: AtomicBool::new(false),
        });
        let app = Router::new()
            .route(
                "/realms/optimizer/.well-known/openid-configuration",
                get(discovery),
            )
            .route("/realms/optimizer/keys", get(keys))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, task }
    }

    pub fn issuer(&self) -> String {
        self.state.issuer.clone()
    }
    pub fn token(&self, key: usize) -> String {
        token(key, &claims(&self.state.issuer))
    }
    pub fn requests(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn discovery(State(state): State<Arc<ProviderState>>) -> axum::Json<Value> {
    axum::Json(state.discovery.lock().unwrap().clone().unwrap_or_else(|| {
        json!({
            "issuer": state.issuer,
            "jwks_uri": format!("{}/keys", state.issuer),
        })
    }))
}

async fn keys(State(state): State<Arc<ProviderState>>) -> axum::response::Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(
        state.delay_millis.load(Ordering::SeqCst),
    ))
    .await;
    let status = StatusCode::from_u16(state.status.load(Ordering::SeqCst)).unwrap();
    let body = state
        .raw_keys
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| state.keys.lock().unwrap().to_string());
    if status.is_redirection() {
        return (
            status,
            [("location", format!("{}/keys", state.issuer))],
            body,
        )
            .into_response();
    }
    let body = if state.chunked.load(Ordering::SeqCst) {
        Body::from_stream(futures::stream::once(async {
            Ok::<_, std::convert::Infallible>(Bytes::from(body))
        }))
    } else {
        Body::from(body)
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

pub fn jwk(index: usize) -> Value {
    serde_json::from_str(if index == 1 {
        include_str!("../fixtures/auth/key.json")
    } else {
        include_str!("../fixtures/auth/key-2.json")
    })
    .unwrap()
}

pub fn claims(issuer: &str) -> Value {
    json!({"iss": issuer, "aud": "grpc-optimizer", "sub": "service-user", "exp": get_current_timestamp() + 3600})
}

pub fn token(index: usize, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(format!("key-{index}"));
    sign(&header, claims, index)
}

pub fn sign(header: &Header, claims: &Value, index: usize) -> String {
    let bytes = if index == 1 {
        include_bytes!("../fixtures/auth/key.der").as_slice()
    } else {
        include_bytes!("../fixtures/auth/key-2.der").as_slice()
    };
    encode(header, claims, &EncodingKey::from_rsa_der(bytes)).unwrap()
}
