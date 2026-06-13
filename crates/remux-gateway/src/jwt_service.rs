//! Service-side JWT/OIDC wiring (Phase B), shared by the gateway and the
//! control plane (which depends on this crate).
//!
//! [`remux_authz`] owns the **pure** [`JwtValidator`] (signature/claim
//! verification, claim→[`Principal`] mapping). This module owns the *service*
//! concerns the pure crate deliberately avoids:
//! - building a validator from CLI flags / env (an HS256 secret, a static
//!   RS256/ES256 public-key PEM file, or a JWKS URL),
//! - **fetching** a JWKS over HTTPS with the existing `reqwest` (system roots),
//!   caching it in memory, and refreshing it on a TTL — keeping the last good
//!   set on a fetch failure (logged, never fatal).
//!
//! A presented bearer token is tried against the static [`TokenStore`] FIRST;
//! only if that misses **and** JWT is configured does [`JwtAuth::validate`] run.
//! Whichever yields a [`Principal`] flows through the identical RBAC `permits`
//! decision — JWT principals are not special.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use remux_authz::{parse_jwks, JwtConfig, JwtError, JwtKey, JwtValidator, Principal};

/// The auth method that produced a [`Principal`], for the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// A static bearer token resolved via the [`remux_authz::TokenStore`].
    Static,
    /// A validated JWT/OIDC bearer.
    Jwt,
}

impl AuthMethod {
    /// The stable audit string (`"static"` / `"jwt"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Static => "static",
            AuthMethod::Jwt => "jwt",
        }
    }
}

/// The CLI/env-derived JWT settings. Exactly one key source must be present for
/// JWT to be enabled; if none is set, JWT auth is simply off (the caller passes
/// `None`).
#[derive(Debug, Clone, Default)]
pub struct JwtSettings {
    /// HS256 shared secret (symmetric).
    pub hs256_secret: Option<String>,
    /// Path to a static RS256/ES256 public-key PEM file.
    pub public_key_pem: Option<std::path::PathBuf>,
    /// A JWKS URL to fetch + cache (RS256/ES256).
    pub jwks_url: Option<String>,
    /// Required issuer (`iss`), if any.
    pub issuer: Option<String>,
    /// Required audience (`aud`), if any.
    pub audience: Option<String>,
    /// The roles claim name (default `"roles"`).
    pub roles_claim: Option<String>,
    /// JWKS refresh TTL in seconds (default 300).
    pub jwks_ttl_secs: Option<u64>,
    /// Accept a self-signed/invalid TLS cert when fetching the JWKS URL.
    pub jwks_tls_insecure: bool,
}

impl JwtSettings {
    /// Whether any JWT key source is configured.
    pub fn is_enabled(&self) -> bool {
        self.hs256_secret.as_deref().is_some_and(|s| !s.is_empty())
            || self.public_key_pem.is_some()
            || self.jwks_url.as_deref().is_some_and(|s| !s.is_empty())
    }

    fn roles_claim(&self) -> String {
        self.roles_claim
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "roles".to_string())
    }
}

/// An error building [`JwtAuth`] from settings.
#[derive(Debug, thiserror::Error)]
pub enum JwtSetupError {
    /// More than one mutually-exclusive key source was given.
    #[error("provide at most one of --jwt-hs256-secret / --jwt-public-key / --jwt-jwks-url")]
    MultipleKeys,
    /// The public-key PEM file could not be read.
    #[error("failed to read --jwt-public-key {path:?}: {source}")]
    PemIo {
        path: String,
        source: std::io::Error,
    },
    /// The key material was invalid.
    #[error("invalid JWT key: {0}")]
    Key(#[from] JwtError),
    /// The initial JWKS fetch failed (a JWKS URL must be reachable at startup so
    /// a misconfiguration fails closed rather than silently rejecting tokens).
    #[error("initial JWKS fetch from {url} failed: {reason}")]
    JwksFetch { url: String, reason: String },
}

/// Build a [`JwtValidator`] from a [`JwtSettings`] and a concrete [`JwtKey`],
/// applying issuer/audience/roles-claim.
fn validator_from(settings: &JwtSettings, key: JwtKey) -> JwtValidator {
    let mut config = JwtConfig::new(key).with_roles_claim(settings.roles_claim());
    if let Some(iss) = settings.issuer.clone().filter(|s| !s.is_empty()) {
        config = config.with_issuer(iss);
    }
    if let Some(aud) = settings.audience.clone().filter(|s| !s.is_empty()) {
        config = config.with_audience(aud);
    }
    JwtValidator::new(config)
}

/// Read the PEM and decide RS256 vs ES256 from its header.
fn key_from_pem(path: &Path) -> Result<JwtKey, JwtSetupError> {
    let pem = std::fs::read(path).map_err(|source| JwtSetupError::PemIo {
        path: path.display().to_string(),
        source,
    })?;
    let header = String::from_utf8_lossy(&pem);
    // EC public keys carry `BEGIN PUBLIC KEY` too, so distinguish by trying EC
    // first only when the body looks like an EC key; otherwise default to RSA and
    // fall back to EC. Simplest robust approach: try RSA, then EC.
    if header.contains("EC PUBLIC KEY") || header.contains("BEGIN EC") {
        return JwtKey::es256_pem(&pem).map_err(JwtSetupError::Key);
    }
    match JwtKey::rs256_pem(&pem) {
        Ok(k) => Ok(k),
        Err(_) => JwtKey::es256_pem(&pem).map_err(JwtSetupError::Key),
    }
}

/// The validator behind [`JwtAuth`]. Static keys hold a fixed validator; a JWKS
/// URL holds a validator swapped by the background refresher.
enum Backing {
    Static(JwtValidator),
    Jwks(Arc<RwLock<JwtValidator>>),
}

/// Service-side JWT authentication: an optionally-refreshing [`JwtValidator`]
/// that maps a JWT bearer to a [`Principal`].
#[derive(Clone)]
pub struct JwtAuth {
    inner: Arc<Backing>,
}

impl JwtAuth {
    /// Build [`JwtAuth`] from settings. Returns `Ok(None)` when no JWT key source
    /// is configured (JWT auth disabled — behavior is exactly as before).
    ///
    /// For a JWKS URL this performs an initial blocking-async fetch (so a bad URL
    /// fails closed at startup) and spawns a background refresher on the TTL.
    pub async fn from_settings(settings: &JwtSettings) -> Result<Option<Self>, JwtSetupError> {
        if !settings.is_enabled() {
            return Ok(None);
        }
        let sources = [
            settings
                .hs256_secret
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            settings.public_key_pem.is_some(),
            settings.jwks_url.as_deref().is_some_and(|s| !s.is_empty()),
        ];
        if sources.iter().filter(|b| **b).count() > 1 {
            return Err(JwtSetupError::MultipleKeys);
        }

        if let Some(secret) = settings.hs256_secret.as_deref().filter(|s| !s.is_empty()) {
            let v = validator_from(settings, JwtKey::hs256(secret.as_bytes().to_vec()));
            return Ok(Some(Self {
                inner: Arc::new(Backing::Static(v)),
            }));
        }
        if let Some(path) = &settings.public_key_pem {
            let key = key_from_pem(path)?;
            let v = validator_from(settings, key);
            return Ok(Some(Self {
                inner: Arc::new(Backing::Static(v)),
            }));
        }

        // JWKS URL path: fetch once, cache, refresh on a TTL.
        let url = settings
            .jwks_url
            .clone()
            .filter(|s| !s.is_empty())
            .expect("jwks_url present");
        let ttl = Duration::from_secs(settings.jwks_ttl_secs.unwrap_or(300).max(1));
        let client = build_http_client(settings.jwks_tls_insecure).map_err(|reason| {
            JwtSetupError::JwksFetch {
                url: url.clone(),
                reason,
            }
        })?;
        // Initial fetch: must succeed so a misconfigured URL fails closed.
        let jwks = fetch_jwks(&client, &url)
            .await
            .map_err(|reason| JwtSetupError::JwksFetch {
                url: url.clone(),
                reason,
            })?;
        let validator = validator_from(settings, JwtKey::Jwks(jwks));
        let cell = Arc::new(RwLock::new(validator));

        // Background refresher: re-fetch every TTL; on failure keep the last good
        // validator (logged, never fatal).
        let refresher_cell = cell.clone();
        let refresher_settings = settings.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ttl).await;
                match fetch_jwks(&client, &url).await {
                    Ok(jwks) => {
                        let v = validator_from(&refresher_settings, JwtKey::Jwks(jwks));
                        if let Ok(mut guard) = refresher_cell.write() {
                            *guard = v;
                        }
                        tracing::debug!(url = %url, "refreshed JWKS");
                    }
                    Err(e) => {
                        tracing::warn!(
                            url = %url, error = %e,
                            "JWKS refresh failed; keeping the last good key set"
                        );
                    }
                }
            }
        });

        Ok(Some(Self {
            inner: Arc::new(Backing::Jwks(cell)),
        }))
    }

    /// Validate a JWT bearer, mapping it to a [`Principal`]. Returns a
    /// [`JwtError`] on any verification/mapping failure (caller → `401`).
    pub fn validate(&self, token: &str) -> Result<Principal, JwtError> {
        match self.inner.as_ref() {
            Backing::Static(v) => v.validate(token),
            Backing::Jwks(cell) => {
                let guard = cell
                    .read()
                    .map_err(|_| JwtError::Verification("JWKS lock poisoned".to_string()))?;
                guard.validate(token)
            }
        }
    }
}

/// Build a reqwest client for JWKS fetching (system roots; optionally accepting
/// invalid certs for self-signed dev IdPs).
fn build_http_client(tls_insecure: bool) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(tls_insecure)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Fetch and parse a JWKS document.
async fn fetch_jwks(client: &reqwest::Client, url: &str) -> Result<remux_authz::Jwks, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;
    parse_jwks(&body).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_when_no_source() {
        let s = JwtSettings::default();
        assert!(!s.is_enabled());
        assert!(JwtAuth::from_settings(&s).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn hs256_settings_build_and_validate() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        use serde_json::json;
        let secret = "a-shared-secret-for-the-test-123";
        let settings = JwtSettings {
            hs256_secret: Some(secret.to_string()),
            ..Default::default()
        };
        let auth = JwtAuth::from_settings(&settings)
            .await
            .unwrap()
            .expect("enabled");
        let exp = jsonwebtoken::get_current_timestamp() as i64 + 3600;
        let token = encode(
            &Header::new(Algorithm::HS256),
            &json!({ "sub": "svc-user", "roles": ["operator"], "exp": exp }),
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        let p = auth.validate(&token).expect("valid");
        assert_eq!(p.subject, "svc-user");
        assert_eq!(p.roles, vec!["operator".to_string()]);
    }

    #[tokio::test]
    async fn multiple_key_sources_rejected() {
        let settings = JwtSettings {
            hs256_secret: Some("x".to_string()),
            public_key_pem: Some("/nonexistent.pem".into()),
            ..Default::default()
        };
        assert!(matches!(
            JwtAuth::from_settings(&settings).await,
            Err(JwtSetupError::MultipleKeys)
        ));
    }

    #[test]
    fn auth_method_strings() {
        assert_eq!(AuthMethod::Static.as_str(), "static");
        assert_eq!(AuthMethod::Jwt.as_str(), "jwt");
    }
}
