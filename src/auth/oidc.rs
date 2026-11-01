//! OIDC discovery, bounded JWKS caching, and local cryptographic verification.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::future::BoxFuture;
use jsonwebtoken::{
    Algorithm, AlgorithmFamily, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, EllipticCurve, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use reqwest::{Client, Url};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
    sync::{Mutex, RwLock},
    time::Instant,
};

use super::{
    AuthenticationError, MAX_TOKEN_BYTES, Principal, TokenVerifier,
    config::{AuthConfig, validate_url},
};

const CACHE_TTL: Duration = Duration::from_secs(300);
const REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
}

struct CachedKey {
    key: DecodingKey,
    algorithms: Vec<Algorithm>,
}

struct KeyCache {
    keys: HashMap<String, Arc<CachedKey>>,
    fetched_at: Instant,
}

struct RefreshState {
    last_attempt: Instant,
    failed: bool,
}

/// Shared by all RPCs; refreshes are serialized and never hold solver capacity.
pub struct OidcVerifier {
    config: AuthConfig,
    client: Client,
    jwks_uri: Url,
    cache: Arc<RwLock<KeyCache>>,
    refresh: Arc<Mutex<RefreshState>>,
}

impl OidcVerifier {
    /// Fail startup unless discovery identifies the configured issuer and supplies usable keys.
    pub async fn discover(config: AuthConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let discovery_uri = Url::parse(&format!(
            "{}/.well-known/openid-configuration",
            config.issuer.trim_end_matches('/')
        ))?;
        let discovery: Discovery = get_document(&client, discovery_uri)
            .await
            .map_err(|_| anyhow::anyhow!("OIDC discovery failed"))?;
        anyhow::ensure!(
            discovery.issuer == config.issuer,
            "OIDC discovery issuer mismatch"
        );
        let jwks_uri = validate_url(&discovery.jwks_uri, config.allow_insecure_http)?;
        let keys = load_keys(&client, &jwks_uri, &config.algorithms)
            .await
            .map_err(|_| anyhow::anyhow!("initial OIDC signing key retrieval failed"))?;
        let now = Instant::now();
        Ok(Self {
            config,
            client,
            jwks_uri,
            cache: Arc::new(RwLock::new(KeyCache {
                keys,
                fetched_at: now,
            })),
            refresh: Arc::new(Mutex::new(RefreshState {
                last_attempt: now,
                failed: false,
            })),
        })
    }

    /// Recheck after acquiring the refresh lock so concurrent misses share one fetch.
    async fn key(&self, kid: &str) -> Result<Arc<CachedKey>, AuthenticationError> {
        {
            let cache = self.cache.read().await;
            if cache.fetched_at.elapsed() < CACHE_TTL
                && let Some(key) = cache.keys.get(kid)
            {
                return Ok(key.clone());
            }
        }
        let mut refresh = self.refresh.clone().lock_owned().await;
        {
            let cache = self.cache.read().await;
            if cache.fetched_at.elapsed() < CACHE_TTL
                && let Some(key) = cache.keys.get(kid)
            {
                return Ok(key.clone());
            }
        }
        if refresh.last_attempt.elapsed() >= REFRESH_COOLDOWN {
            refresh.last_attempt = Instant::now();
            refresh.failed = true;
            let client = self.client.clone();
            let jwks_uri = self.jwks_uri.clone();
            let algorithms = self.config.algorithms.clone();
            let cache = self.cache.clone();
            // Keep the bounded fetch and its lock alive if the initiating RPC ends.
            // Other callers still share one refresh and observe the same cooldown.
            refresh = tokio::spawn(async move {
                match load_keys(&client, &jwks_uri, &algorithms).await {
                    Ok(keys) => {
                        *cache.write().await = KeyCache {
                            keys,
                            fetched_at: Instant::now(),
                        };
                        refresh.failed = false;
                    }
                    Err(_) => tracing::warn!("OIDC signing key refresh failed"),
                }
                refresh
            })
            .await
            .map_err(|_| AuthenticationError::Unavailable)?;
        }
        let cache = self.cache.read().await;
        if cache.fetched_at.elapsed() >= CACHE_TTL || refresh.failed {
            return Err(AuthenticationError::Unavailable);
        }
        cache
            .keys
            .get(kid)
            .cloned()
            .ok_or(AuthenticationError::InvalidToken)
    }

    async fn verify_token(&self, token: &str) -> Result<Principal, AuthenticationError> {
        let invalid = AuthenticationError::InvalidToken;
        if token.len() > MAX_TOKEN_BYTES || token.is_empty() {
            return Err(invalid);
        }
        let header = decode_header(token).map_err(|_| invalid)?;
        if !self.config.algorithms.contains(&header.alg)
            || header
                .crit
                .as_ref()
                .is_some_and(|fields| !fields.is_empty())
        {
            return Err(invalid);
        }
        let kid = header
            .kid
            .filter(|value| !value.is_empty())
            .ok_or(invalid)?;
        let key = self.key(&kid).await?;
        if !key.algorithms.contains(&header.alg) {
            return Err(invalid);
        }
        let mut validation = Validation::new(header.alg);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        let claims = decode::<Claims>(token, &key.key, &validation)
            .map_err(|_| invalid)?
            .claims;
        if claims.sub.trim().is_empty() {
            return Err(invalid);
        }
        Ok(Principal {
            issuer: claims.iss,
            subject: claims.sub,
            expires_at: claims.exp,
            audiences: match claims.aud {
                Audience::One(value) => vec![value],
                Audience::Many(values) => values,
            },
        })
    }
}

impl TokenVerifier for OidcVerifier {
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<Principal, AuthenticationError>> {
        Box::pin(self.verify_token(token))
    }
}

/// Bound streamed bodies as well as Content-Length; never include response bodies in errors.
async fn get_document<T: DeserializeOwned>(
    client: &Client,
    url: Url,
) -> Result<T, AuthenticationError> {
    let unavailable = AuthenticationError::Unavailable;
    let mut response = client.get(url).send().await.map_err(|_| unavailable)?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > MAX_DOCUMENT_BYTES as u64)
    {
        return Err(unavailable);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable)? {
        if body.len().saturating_add(chunk.len()) > MAX_DOCUMENT_BYTES {
            return Err(unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| unavailable)
}

async fn load_keys(
    client: &Client,
    url: &Url,
    algorithms: &[Algorithm],
) -> Result<HashMap<String, Arc<CachedKey>>, AuthenticationError> {
    let document: JwkSet = get_document(client, url.clone()).await?;
    let mut keys = HashMap::new();
    for jwk in document.keys {
        if jwk
            .common
            .public_key_use
            .as_ref()
            .is_some_and(|usage| *usage != PublicKeyUse::Signature)
            || jwk
                .common
                .key_operations
                .as_ref()
                .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
        {
            continue;
        }
        let Some(kid) = jwk.common.key_id.as_ref().filter(|kid| !kid.is_empty()) else {
            continue;
        };
        // Pin each key to compatible algorithms, including its curve and optional JWK alg.
        let algorithms: Vec<_> = algorithms
            .iter()
            .copied()
            .filter(|algorithm| {
                if jwk
                    .common
                    .key_algorithm
                    .is_some_and(|declared| declared != KeyAlgorithm::from(*algorithm))
                {
                    return false;
                }
                match &jwk.algorithm {
                    AlgorithmParameters::RSA(_) => algorithm.family() == AlgorithmFamily::Rsa,
                    AlgorithmParameters::EllipticCurve(key) => matches!(
                        (&key.curve, algorithm),
                        (EllipticCurve::P256, Algorithm::ES256)
                            | (EllipticCurve::P384, Algorithm::ES384)
                    ),
                    AlgorithmParameters::OctetKeyPair(key) => {
                        key.curve == EllipticCurve::Ed25519 && *algorithm == Algorithm::EdDSA
                    }
                    _ => false,
                }
            })
            .collect();
        if algorithms.is_empty() {
            continue;
        }
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthenticationError::Unavailable)?;
        if keys
            .insert(kid.clone(), Arc::new(CachedKey { key, algorithms }))
            .is_some()
        {
            return Err(AuthenticationError::Unavailable);
        }
    }
    if keys.is_empty() {
        return Err(AuthenticationError::Unavailable);
    }
    Ok(keys)
}

#[cfg(test)]
#[path = "../../tests/support/oidc.rs"]
mod test_provider;

#[cfg(test)]
mod tests {
    use super::test_provider::{Provider, claims, jwk, sign, token};
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode, get_current_timestamp};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn config(provider: &Provider) -> AuthConfig {
        AuthConfig {
            issuer: provider.issuer(),
            audience: "grpc-optimizer".into(),
            algorithms: vec![Algorithm::RS256],
            allow_insecure_http: true,
        }
    }

    async fn expire(verifier: &OidcVerifier) {
        verifier.cache.write().await.fetched_at = Instant::now() - CACHE_TTL;
        allow_refresh(verifier).await;
    }

    async fn allow_refresh(verifier: &OidcVerifier) {
        verifier.refresh.lock().await.last_attempt = Instant::now() - REFRESH_COOLDOWN;
    }

    #[tokio::test]
    async fn verifies_claims_and_rejects_invalid_tokens() {
        let provider = Provider::start().await;
        let verifier = OidcVerifier::discover(config(&provider)).await.unwrap();
        let principal = verifier.verify(&provider.token(1)).await.unwrap();
        assert_eq!(principal.subject, "service-user");
        let mut array = claims(&provider.issuer());
        array["aud"] = json!(["other", "grpc-optimizer"]);
        assert_eq!(
            verifier
                .verify(&token(1, &array))
                .await
                .unwrap()
                .audiences
                .len(),
            2
        );
        for (field, value) in [
            ("iss", json!("https://evil.example")),
            ("aud", json!("login-client")),
            ("sub", json!("")),
            ("sub", json!(" ")),
            ("exp", json!(get_current_timestamp() - 60)),
            ("nbf", json!(get_current_timestamp() + 60)),
            ("exp", json!("tomorrow")),
        ] {
            let mut invalid = claims(&provider.issuer());
            invalid[field] = value;
            assert_eq!(
                verifier.verify(&token(1, &invalid)).await,
                Err(AuthenticationError::InvalidToken),
                "{field}"
            );
        }
        for field in ["iss", "aud", "sub", "exp"] {
            let mut invalid = claims(&provider.issuer());
            invalid.as_object_mut().unwrap().remove(field);
            assert_eq!(
                verifier.verify(&token(1, &invalid)).await,
                Err(AuthenticationError::InvalidToken)
            );
        }
        let mut skew = claims(&provider.issuer());
        skew["exp"] = json!(get_current_timestamp() - 10);
        skew["nbf"] = json!(get_current_timestamp() + 10);
        assert!(verifier.verify(&token(1, &skew)).await.is_ok());
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("key-1".into());
        header.jku = Some("http://untrusted.invalid/keys".into());
        header.x5u = Some("http://untrusted.invalid/cert".into());
        header.jwk = Some(serde_json::from_value(jwk(2)).unwrap());
        assert!(
            verifier
                .verify(&sign(&header, &claims(&provider.issuer()), 1))
                .await
                .is_ok()
        );
        header.crit = Some(vec!["unrecognized-extension".into()]);
        assert_eq!(
            verifier
                .verify(&sign(&header, &claims(&provider.issuer()), 1))
                .await,
            Err(AuthenticationError::InvalidToken)
        );
        header.crit = None;
        assert_eq!(
            verifier
                .verify(&sign(&header, &claims(&provider.issuer()), 2))
                .await,
            Err(AuthenticationError::InvalidToken)
        );
        header.kid = None;
        assert_eq!(
            verifier
                .verify(&sign(&header, &claims(&provider.issuer()), 1))
                .await,
            Err(AuthenticationError::InvalidToken)
        );
        header.alg = Algorithm::RS512;
        header.kid = Some("key-1".into());
        assert_eq!(
            verifier
                .verify(&sign(&header, &claims(&provider.issuer()), 1))
                .await,
            Err(AuthenticationError::InvalidToken)
        );
        let hmac = encode(
            &Header::new(Algorithm::HS256),
            &claims(&provider.issuer()),
            &EncodingKey::from_secret(b"untrusted"),
        )
        .unwrap();
        for invalid in [
            hmac,
            "opaque-access-token".into(),
            "e30.e30.".into(),
            "x".repeat(MAX_TOKEN_BYTES + 1),
        ] {
            assert_eq!(
                verifier.verify(&invalid).await,
                Err(AuthenticationError::InvalidToken)
            );
        }
        assert_eq!(provider.requests(), 1);
    }

    #[tokio::test]
    async fn rotation_coalesces_refreshes_and_replaces_removed_keys() {
        let provider = Provider::start().await;
        let verifier = OidcVerifier::discover(config(&provider)).await.unwrap();
        *provider.state.keys.lock().unwrap() = json!({"keys": [jwk(2)]});
        // Initial retrieval also counts toward the cooldown.
        assert_eq!(
            verifier.verify(&provider.token(2)).await,
            Err(AuthenticationError::InvalidToken)
        );
        assert_eq!(provider.requests(), 1);
        allow_refresh(&verifier).await;
        let new_token = provider.token(2);
        let outcomes =
            futures::future::join_all((0..12).map(|_| verifier.verify(&new_token))).await;
        assert!(outcomes.into_iter().all(|result| result.is_ok()));
        assert_eq!(provider.requests(), 2);
        assert_eq!(
            verifier.verify(&provider.token(1)).await,
            Err(AuthenticationError::InvalidToken)
        );
        assert_eq!(provider.requests(), 2);
    }

    #[tokio::test]
    async fn algorithms_are_pinned_to_configuration_and_key_type() {
        let provider = Provider::start().await;
        let mut settings = config(&provider);
        settings.algorithms = vec![Algorithm::ES256];
        let mut public_key = jwk(1);
        public_key.as_object_mut().unwrap().remove("alg");
        *provider.state.keys.lock().unwrap() = json!({"keys": [public_key]});
        assert!(OidcVerifier::discover(settings.clone()).await.is_err());
        settings.algorithms = vec![Algorithm::RS512];
        let verifier = OidcVerifier::discover(settings).await.unwrap();
        let mut header = Header::new(Algorithm::RS512);
        header.kid = Some("key-1".into());
        assert!(
            verifier
                .verify(&sign(&header, &claims(&provider.issuer()), 1))
                .await
                .is_ok()
        );
        assert_eq!(
            verifier.verify(&provider.token(1)).await,
            Err(AuthenticationError::InvalidToken)
        );
    }

    #[tokio::test]
    async fn outages_use_fresh_keys_but_never_expired_cache() {
        let provider = Provider::start().await;
        let verifier = OidcVerifier::discover(config(&provider)).await.unwrap();
        provider.state.status.store(503, Ordering::SeqCst);
        assert!(verifier.verify(&provider.token(1)).await.is_ok());
        allow_refresh(&verifier).await;
        assert_eq!(
            verifier.verify(&provider.token(2)).await,
            Err(AuthenticationError::Unavailable)
        );
        assert!(verifier.verify(&provider.token(1)).await.is_ok());
        expire(&verifier).await;
        assert_eq!(
            verifier.verify(&provider.token(1)).await,
            Err(AuthenticationError::Unavailable)
        );
        let attempts = provider.requests();
        assert_eq!(
            verifier.verify(&provider.token(1)).await,
            Err(AuthenticationError::Unavailable)
        );
        assert_eq!(provider.requests(), attempts);
        provider.state.status.store(200, Ordering::SeqCst);
        allow_refresh(&verifier).await;
        assert!(verifier.verify(&provider.token(1)).await.is_ok());
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_cancel_shared_refresh() {
        let provider = Provider::start().await;
        let verifier = Arc::new(OidcVerifier::discover(config(&provider)).await.unwrap());
        expire(&verifier).await;
        provider.state.delay_millis.store(200, Ordering::SeqCst);
        let caller = {
            let verifier = verifier.clone();
            let token = provider.token(1);
            tokio::spawn(async move { verifier.verify(&token).await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.requests() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());

        // Other callers share the ongoing fetch instead of failing for the cooldown.
        assert!(verifier.verify(&provider.token(1)).await.is_ok());
        assert_eq!(provider.requests(), 2);
    }

    #[tokio::test]
    async fn discovery_and_key_documents_are_validated_and_bounded() {
        let provider = Provider::start().await;
        for document in [
            json!({"issuer": "https://evil.example", "jwks_uri": format!("{}/keys", provider.issuer())}),
            json!({"issuer": provider.issuer(), "jwks_uri": "file:///tmp/key"}),
            json!({"issuer": provider.issuer()}),
        ] {
            *provider.state.discovery.lock().unwrap() = Some(document);
            assert!(OidcVerifier::discover(config(&provider)).await.is_err());
        }
        *provider.state.discovery.lock().unwrap() = None;
        for keys in [
            json!({"keys": []}),
            json!({"keys": [jwk(1), jwk(1)]}),
            json!({"keys": [{"kid":"shared", "kty":"oct", "k":"c2VjcmV0"}]}),
        ] {
            *provider.state.keys.lock().unwrap() = keys;
            assert!(OidcVerifier::discover(config(&provider)).await.is_err());
        }
        for field in ["use", "key_ops"] {
            let mut key = jwk(1);
            key[field] = if field == "use" {
                json!("enc")
            } else {
                json!(["sign"])
            };
            *provider.state.keys.lock().unwrap() = json!({"keys": [key]});
            assert!(OidcVerifier::discover(config(&provider)).await.is_err());
        }
        *provider.state.keys.lock().unwrap() = json!({"keys": [jwk(1)]});
        provider.state.status.store(302, Ordering::SeqCst);
        let requests = provider.requests();
        assert!(OidcVerifier::discover(config(&provider)).await.is_err());
        assert_eq!(provider.requests(), requests + 1);
        provider.state.status.store(200, Ordering::SeqCst);
        *provider.state.raw_keys.lock().unwrap() = Some("x".repeat(MAX_DOCUMENT_BYTES + 1));
        for chunked in [false, true] {
            provider.state.chunked.store(chunked, Ordering::SeqCst);
            assert!(OidcVerifier::discover(config(&provider)).await.is_err());
        }
        *provider.state.raw_keys.lock().unwrap() = Some("{invalid".into());
        assert!(OidcVerifier::discover(config(&provider)).await.is_err());
    }

    #[tokio::test]
    async fn slow_providers_have_a_bounded_timeout() {
        let provider = Provider::start().await;
        provider.state.delay_millis.store(6000, Ordering::SeqCst);
        let start = Instant::now();
        assert!(OidcVerifier::discover(config(&provider)).await.is_err());
        assert!(start.elapsed() < Duration::from_secs(6));
    }
}
