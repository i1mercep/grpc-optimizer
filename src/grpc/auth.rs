//! Async gRPC authentication before decoding, with explicit health exemptions.

use std::{
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use futures::future::BoxFuture;
use http::{Request, Response};
use tonic::{Status, body::Body};
use tower::{Layer, Service};

use crate::auth::{AuthenticationError, MAX_TOKEN_BYTES, TokenVerifier};

/// Carries the original deadline through discovery/key-refresh waits into streaming handlers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RpcDeadline(pub Option<Instant>);

#[derive(Clone, Default)]
pub struct AuthLayer {
    verifier: Option<Arc<dyn TokenVerifier>>,
}

impl AuthLayer {
    pub fn new(verifier: Option<Arc<dyn TokenVerifier>>) -> Self {
        Self { verifier }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            verifier: self.verifier.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    verifier: Option<Arc<dyn TokenVerifier>>,
}

impl<S> Service<Request<Body>> for AuthService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        // Use the instance whose readiness was polled; clones need not be ready.
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let verifier = self.verifier.clone();
        Box::pin(async move {
            if let Some(verifier) = verifier
                && !matches!(
                    request.uri().path(),
                    "/grpc.health.v1.Health/Check" | "/grpc.health.v1.Health/Watch"
                )
            {
                let result = authenticate(&mut request, verifier.as_ref()).await;
                if let Err(status) = result {
                    return Ok(status.into_http());
                }
            }
            inner.call(request).await
        })
    }
}

async fn authenticate(
    request: &mut Request<Body>,
    verifier: &dyn TokenVerifier,
) -> Result<(), Status> {
    let mut values = request
        .headers()
        .get_all(http::header::AUTHORIZATION)
        .iter();
    let raw = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| Status::unauthenticated("bearer access token required"))?;
    if values.next().is_some() {
        return Err(Status::unauthenticated("invalid bearer credentials"));
    }
    let (scheme, token) = raw
        .split_once(' ')
        .ok_or_else(|| Status::unauthenticated("invalid bearer credentials"))?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || token
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b',')
    {
        return Err(Status::unauthenticated("invalid bearer credentials"));
    }
    let metadata = tonic::metadata::MetadataMap::from_headers(request.headers().clone());
    let deadline = super::deadline(&tonic::Request::from_parts(
        metadata,
        http::Extensions::new(),
        (),
    ))?;
    let verified = async {
        verifier.verify(token).await.map_err(|error| match error {
            AuthenticationError::InvalidToken => Status::unauthenticated(error.to_string()),
            AuthenticationError::Unavailable => Status::unavailable(error.to_string()),
        })
    };
    let principal = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), verified)
            .await
            .map_err(|_| Status::deadline_exceeded("RPC deadline exceeded"))??,
        None => verified.await?,
    };
    // Downstream tracing and handlers need the identity, never the credential.
    request.headers_mut().remove(http::header::AUTHORIZATION);
    request.extensions_mut().insert(principal);
    request.extensions_mut().insert(RpcDeadline(deadline));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Principal;
    use std::{sync::Mutex, time::Duration};
    use tower::ServiceExt;

    struct SlowVerifier(Arc<Mutex<Option<Instant>>>);
    impl TokenVerifier for SlowVerifier {
        fn verify<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Principal, AuthenticationError>> {
            Box::pin(async {
                *self.0.lock().unwrap() = Some(Instant::now());
                tokio::time::sleep(Duration::from_millis(30)).await;
                Ok(Principal {
                    issuer: "test".into(),
                    subject: "user".into(),
                    audiences: vec!["api".into()],
                    expires_at: u64::MAX,
                })
            })
        }
    }

    #[tokio::test]
    async fn carries_identity_original_deadline_and_removes_credentials() {
        let started = Arc::new(Mutex::new(None));
        let verifier = SlowVerifier(started.clone());
        let inner = tower::service_fn(move |request: Request<Body>| {
            let started = started.clone();
            async move {
                assert_eq!(
                    request.extensions().get::<Principal>().unwrap().subject,
                    "user"
                );
                assert!(!request.headers().contains_key("authorization"));
                let request = tonic::Request::from_http(request);
                let deadline = crate::grpc::deadline(&request).unwrap().unwrap();
                assert!(deadline <= started.lock().unwrap().unwrap() + Duration::from_millis(200));
                Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
            }
        });
        let service = AuthLayer::new(Some(Arc::new(verifier))).layer(inner);
        let request = Request::builder()
            .uri("/optimization.v1.Optimizer/Solve")
            .header("authorization", "bearer test-token")
            .header("grpc-timeout", "200m")
            .body(Body::empty())
            .unwrap();
        service.oneshot(request).await.unwrap();
    }

    #[tokio::test]
    async fn authentication_wait_obeys_the_rpc_deadline() {
        let inner = tower::service_fn(|_: Request<Body>| async {
            Err::<Response<Body>, _>(std::io::Error::other(
                "expired authentication reached decoding",
            ))
        });
        let service =
            AuthLayer::new(Some(Arc::new(SlowVerifier(Arc::new(Mutex::new(None)))))).layer(inner);
        let request = Request::builder()
            .uri("/optimization.v1.Optimizer/Solve")
            .header("authorization", "Bearer token")
            .header("grpc-timeout", "1m")
            .body(Body::empty())
            .unwrap();
        let response = service.oneshot(request).await.unwrap();
        assert_eq!(response.headers()["grpc-status"], "4");
    }

    #[tokio::test]
    async fn rejected_credentials_never_reach_the_inner_service() {
        let started = Arc::new(Mutex::new(None));
        let inner = tower::service_fn(|_: Request<Body>| async {
            Err::<Response<Body>, _>(std::io::Error::other(
                "unauthenticated request reached decoding",
            ))
        });
        let service = AuthLayer::new(Some(Arc::new(SlowVerifier(started.clone())))).layer(inner);
        for path in [
            "/optimization.v1.Optimizer/Solve",
            "/grpc.health.v1.Health/Check-extra",
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
        ] {
            let request = Request::builder().uri(path).body(Body::empty()).unwrap();
            let response = service.clone().oneshot(request).await.unwrap();
            assert_eq!(response.headers()["grpc-status"], "16");
        }
        assert!(started.lock().unwrap().is_none());
    }
}
