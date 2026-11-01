//! Validated, opt-in OIDC configuration kept separate from solver resource limits.

use jsonwebtoken::Algorithm;
use reqwest::Url;

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub issuer: String,
    pub audience: String,
    pub algorithms: Vec<Algorithm>,
    pub allow_insecure_http: bool,
}

impl AuthConfig {
    /// Disabled by default. Enabling authentication requires both issuer and audience.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        Self::from_lookup(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => anyhow::bail!("invalid {name}"),
        })
    }

    fn from_lookup(
        mut get: impl FnMut(&str) -> anyhow::Result<Option<String>>,
    ) -> anyhow::Result<Option<Self>> {
        let enabled = get("OPT_AUTH_ENABLED")?.unwrap_or_else(|| "false".into());
        if !enabled
            .parse::<bool>()
            .map_err(|_| anyhow::anyhow!("invalid OPT_AUTH_ENABLED"))?
        {
            return Ok(None);
        }
        let config = Self {
            issuer: get("OPT_AUTH_ISSUER")?.unwrap_or_default(),
            audience: get("OPT_AUTH_AUDIENCE")?.unwrap_or_default(),
            algorithms: get("OPT_AUTH_ALGORITHMS")?
                .unwrap_or_else(|| "RS256".into())
                .split(',')
                .map(|value| {
                    value
                        .trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid OPT_AUTH_ALGORITHMS"))
                })
                .collect::<anyhow::Result<_>>()?,
            allow_insecure_http: get("OPT_AUTH_ALLOW_INSECURE_HTTP")?
                .unwrap_or_else(|| "false".into())
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid OPT_AUTH_ALLOW_INSECURE_HTTP"))?,
        };
        config.validate()?;
        Ok(Some(config))
    }

    /// Also validate configurations constructed directly by application code.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_url(&self.issuer, self.allow_insecure_http)?;
        anyhow::ensure!(
            !self.audience.trim().is_empty() && self.audience.trim() == self.audience,
            "OPT_AUTH_AUDIENCE must be a nonempty API audience"
        );
        anyhow::ensure!(
            !self.algorithms.is_empty()
                && self.algorithms.iter().all(|algorithm| !matches!(
                    algorithm,
                    Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
                )),
            "OPT_AUTH_ALGORITHMS must contain only asymmetric signing algorithms"
        );
        Ok(())
    }
}

/// Only administrator-configured discovery and JWKS URLs may initiate HTTP requests.
pub(crate) fn validate_url(value: &str, allow_http: bool) -> anyhow::Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("invalid OIDC URL"))?;
    anyhow::ensure!(
        (url.scheme() == "https" || (allow_http && url.scheme() == "http"))
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "OIDC URLs require HTTPS and must not contain credentials, query, or fragment"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[(&str, &str)]) -> anyhow::Result<Option<AuthConfig>> {
        AuthConfig::from_lookup(|name| {
            Ok(values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned()))
        })
    }

    #[test]
    fn opt_in_requires_complete_valid_configuration() {
        assert!(parse(&[]).unwrap().is_none());
        let base = [
            ("OPT_AUTH_ENABLED", "true"),
            ("OPT_AUTH_ISSUER", "https://iam.example/realms/optimizer"),
            ("OPT_AUTH_AUDIENCE", "optimizer"),
        ];
        assert!(parse(&base).unwrap().is_some());
        assert!(parse(&base[..2]).is_err());
        assert!(parse(&[("OPT_AUTH_ENABLED", "yes")]).is_err());
        for (name, value) in [
            ("OPT_AUTH_ALGORITHMS", "HS256"),
            ("OPT_AUTH_ALGORITHMS", "none"),
            ("OPT_AUTH_ALGORITHMS", ""),
            ("OPT_AUTH_AUDIENCE", " "),
            ("OPT_AUTH_ISSUER", "http://localhost:8080"),
            ("OPT_AUTH_ALLOW_INSECURE_HTTP", "yes"),
        ] {
            let mut values = base.to_vec();
            values.retain(|(key, _)| *key != name);
            values.push((name, value));
            assert!(parse(&values).is_err(), "{name}");
        }
        for url in [
            "https://user:password@iam.example",
            "https://iam.example?issuer=evil",
            "https://iam.example/#fragment",
            "file:///tmp/keys",
        ] {
            assert!(validate_url(url, true).is_err());
        }
        assert!(validate_url("http://localhost:8080/realms/dev", true).is_ok());
    }
}
