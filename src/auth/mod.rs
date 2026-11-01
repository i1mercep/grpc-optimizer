//! Transport-independent authentication contracts and the OIDC verification adapter.

use futures::future::BoxFuture;

pub mod config;
pub mod oidc;

/// Maximum accepted token length, independent of transport header limits.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// Identity established by a verified access token; never contains the token itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub issuer: String,
    pub subject: String,
    pub audiences: Vec<String>,
    pub expires_at: u64,
}

/// Public failures deliberately exclude credentials and provider response bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthenticationError {
    #[error("invalid access token")]
    InvalidToken,
    #[error("authentication provider unavailable")]
    Unavailable,
}

/// Verifies access tokens without depending on gRPC or optimization types.
pub trait TokenVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<Principal, AuthenticationError>>;
}
