//! Shared bearer metadata and verified HTTPS transport for Rust command-line clients.

use std::time::Duration;

use tonic::{
    Request, Status,
    metadata::{Ascii, MetadataValue},
    service::{Interceptor, interceptor::InterceptedService},
    transport::{Channel, ClientTlsConfig, Endpoint},
};

use crate::{auth::MAX_TOKEN_BYTES, pb::optimizer_client::OptimizerClient};

pub type Client = OptimizerClient<InterceptedService<Channel, BearerToken>>;

/// Clones share only validated metadata; Debug deliberately excludes credentials.
#[derive(Clone, Default)]
pub struct BearerToken(Option<MetadataValue<Ascii>>);

impl BearerToken {
    pub fn new(token: Option<&str>) -> anyhow::Result<Self> {
        let value = token
            .map(|token| {
                anyhow::ensure!(
                    !token.is_empty()
                        && token.len() <= MAX_TOKEN_BYTES
                        && token
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() && byte != b','),
                    "invalid access token"
                );
                let mut value: MetadataValue<Ascii> = format!("Bearer {token}")
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid access token"))?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        Ok(Self(value))
    }
}

impl Interceptor for BearerToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(value) = &self.0 {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }
        Ok(request)
    }
}

/// HTTPS validates the endpoint hostname and uses the operating system's CA roots.
pub fn endpoint(uri: String, timeout: Duration) -> anyhow::Result<Endpoint> {
    let secure = uri.starts_with("https://");
    let mut endpoint = Endpoint::from_shared(uri)?.connect_timeout(timeout);
    if secure {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_credentials_are_validated_and_marked_sensitive() {
        for value in ["", "Bearer token", "line\nbreak", "one,two", "é"] {
            assert!(BearerToken::new(Some(value)).is_err());
        }
        assert!(BearerToken::new(Some(&"x".repeat(MAX_TOKEN_BYTES + 1))).is_err());
        let mut bearer = BearerToken::new(Some("test-token")).unwrap();
        let request = bearer.call(Request::new(())).unwrap();
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            "Bearer test-token"
        );
        assert!(
            request
                .metadata()
                .get("authorization")
                .unwrap()
                .is_sensitive()
        );
        assert!(!format!("{:?}", request.metadata()).contains("test-token"));
        assert!(
            BearerToken::default()
                .call(Request::new(()))
                .unwrap()
                .metadata()
                .is_empty()
        );
    }
}
