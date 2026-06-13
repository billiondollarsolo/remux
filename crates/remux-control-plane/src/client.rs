//! [`GatewayClient`] — a thin reqwest wrapper over one gateway's `/v1` API.
//!
//! Each client is bound to a gateway base URL + the bearer token the gateway
//! handed the control plane at registration time. It reuses the gateway's public
//! DTOs (`SessionView`, `CreateSessionBody`) so the federation layer speaks the
//! exact same contract as the gateway itself.
//!
//! **TLS-trust posture (Phase C — secure by default).** The control plane
//! verifies each gateway's certificate against system roots by default. To call a
//! self-signed gateway, an operator pins its leaf with `--gateway-pin <SHA256>`
//! (no CA needed) or trusts a CA bundle with `--gateway-ca <PEM>`.
//! `--gateway-tls-insecure` (default **false**) remains an explicit, loudly-logged
//! dev-only opt-out. A wrong pin / CA mismatch surfaces as a TLS error on that
//! gateway's fan-out row (`ok:false`), never a panic. Per-gateway request timeouts
//! are bounded so one slow/hung gateway cannot stall fan-out. See
//! [`remux_gateway::peer_tls`].

use std::time::Duration;

use remux_gateway::dto::{CreateSessionBody, SessionView};
use remux_gateway::peer_tls::{self, PeerVerification};

/// The default per-request timeout for an outbound gateway call. Bounded so a
/// hung gateway is reported as an error instead of stalling the fan-out.
pub const DEFAULT_GATEWAY_TIMEOUT: Duration = Duration::from_secs(5);

/// An error talking to a gateway, rendered to a human string for the per-host
/// fan-out report (`ok: false`, `error: "…"`).
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("request failed: {0}")]
    Request(String),
    #[error("gateway returned status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("invalid response body: {0}")]
    Decode(String),
}

/// A reqwest-backed client for a single gateway's `/v1` API.
#[derive(Clone)]
pub struct GatewayClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl GatewayClient {
    /// Build a client for `base_url` (e.g. `https://host:8443`) authenticating
    /// with `token`. `verification` selects the gateway TLS-trust posture
    /// (system roots / CA bundle / SHA-256 leaf pin / dev-insecure);
    /// `timeout` bounds every request.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        verification: &PeerVerification,
        timeout: Duration,
    ) -> Result<Self, GatewayError> {
        let http = peer_tls::build_client(verification, timeout, "--gateway-ca")
            .map_err(|e| GatewayError::Request(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    /// `GET /v1/sessions` on this gateway.
    pub async fn list_sessions(&self) -> Result<Vec<SessionView>, GatewayError> {
        let url = format!("{}/v1/sessions", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| GatewayError::Request(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Status {
                status: status.as_u16(),
                body: truncate(&body),
            });
        }
        resp.json::<Vec<SessionView>>()
            .await
            .map_err(|e| GatewayError::Decode(e.to_string()))
    }

    /// `POST /v1/sessions` on this gateway, returning the created [`SessionView`].
    pub async fn create_session(
        &self,
        body: &CreateSessionBody,
    ) -> Result<SessionView, GatewayError> {
        let url = format!("{}/v1/sessions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| GatewayError::Request(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Status {
                status: status.as_u16(),
                body: truncate(&body),
            });
        }
        resp.json::<SessionView>()
            .await
            .map_err(|e| GatewayError::Decode(e.to_string()))
    }
}

/// Clamp an error body so a misbehaving gateway can't bloat our log/JSON.
fn truncate(s: &str) -> String {
    const MAX: usize = 256;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…", &s[..MAX])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_trailing_slash_trimmed() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let c = GatewayClient::new(
            "https://h:8443/",
            "tok",
            &PeerVerification::Insecure,
            DEFAULT_GATEWAY_TIMEOUT,
        )
        .unwrap();
        assert_eq!(c.base_url, "https://h:8443");
    }

    #[test]
    fn truncate_clamps_long_bodies() {
        let long = "x".repeat(1000);
        let out = truncate(&long);
        assert!(out.len() < 1000);
        assert!(out.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }
}
