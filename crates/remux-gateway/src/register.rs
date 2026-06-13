//! Outbound auto-registration with a control plane (AW6, gateway side).
//!
//! When the gateway is started with `--register <cp-url>`, it **registers
//! itself** with the control plane on startup (`POST /cp/v1/register`), then
//! keeps the registration fresh with a background heartbeat task
//! (`POST /cp/v1/heartbeat` every `ttl/2`), and best-effort deregisters
//! (`DELETE /cp/v1/hosts/{name}`) on graceful shutdown.
//!
//! This preserves the non-negotiable invariant that the **daemon** never grows
//! an inbound network listener: it is the gateway that dials *out* to the
//! control plane, and the control plane only ever calls a gateway it was first
//! told about by that gateway.
//!
//! **TLS-trust posture (Phase C — secure by default).** The registration client
//! verifies the control plane's certificate against system roots by default. An
//! operator pins a self-signed control plane with `--register-pin <SHA256>` (no CA
//! needed) or trusts a CA bundle with `--register-ca <PEM>`. `--register-tls-insecure`
//! (default **false**) remains an explicit, loudly-logged dev-only opt-out. See
//! [`crate::peer_tls`].
//!
//! Registration failures are **never fatal**: the gateway logs and retries with
//! bounded backoff and keeps serving its `/v1` API regardless. A wrong pin / CA
//! mismatch simply surfaces as a TLS error on every attempt (logged) — the
//! gateway never crashes.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use tokio::sync::watch;

use crate::peer_tls::{self, PeerVerification};

/// Everything needed to register this gateway with a control plane.
#[derive(Debug, Clone)]
pub struct RegisterConfig {
    /// The control plane's base URL (e.g. `https://cp.internal:9443`).
    pub cp_url: String,
    /// The register-token the control plane authenticates the gateway with.
    pub register_token: String,
    /// The gateway's externally-reachable base URL (what the CP will dial back).
    pub advertise_url: String,
    /// The logical host name to register under.
    pub name: String,
    /// Selector labels (`env=dev`, `region=…`) for fan-out / routing.
    pub labels: BTreeMap<String, String>,
    /// The gateway's OWN read-write bearer token, handed to the CP so it can call
    /// the gateway's `/v1` API.
    pub gateway_token: String,
    /// Registration TTL in seconds; the heartbeat runs every `ttl/2`.
    pub ttl_secs: u64,
    /// How to verify the control plane's TLS certificate (secure by default:
    /// system roots; CA bundle, SHA-256 pin, or dev-only insecure).
    pub verification: PeerVerification,
}

/// The bounded backoff schedule for a failed registration attempt (seconds).
/// Capped so a long CP outage doesn't grow an unbounded delay; the gateway keeps
/// retrying at the cap.
const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 15, 30];

/// The maximum number of register attempts in the startup burst before falling
/// back to the heartbeat loop (which keeps trying via re-register on 404).
const STARTUP_ATTEMPTS: usize = 6;

/// Build a reqwest client for talking to the control plane, honoring the
/// configured TLS-verification posture (system roots / CA bundle / pin / insecure)
/// and a bounded request timeout.
fn build_client(verification: &PeerVerification) -> Result<reqwest::Client, String> {
    peer_tls::build_client(verification, Duration::from_secs(10), "--register-ca")
        .map_err(|e| e.to_string())
}

/// The JSON body for `POST /cp/v1/register`.
fn register_body(cfg: &RegisterConfig) -> serde_json::Value {
    json!({
        "name": cfg.name,
        "url": cfg.advertise_url,
        "labels": cfg.labels,
        "token": cfg.gateway_token,
        "ttl_secs": cfg.ttl_secs,
    })
}

/// Perform a single `POST /cp/v1/register`. Returns `Ok(())` on a 2xx, else a
/// human error string.
async fn register_once(http: &reqwest::Client, cfg: &RegisterConfig) -> Result<(), String> {
    let url = format!("{}/cp/v1/register", cfg.cp_url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .bearer_auth(&cfg.register_token)
        .json(&register_body(cfg))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("control plane returned {status}: {body}"))
    }
}

/// Perform a single `POST /cp/v1/heartbeat`. Returns `Ok(true)` on success,
/// `Ok(false)` if the host is unknown (404 → the gateway should re-register),
/// else an error string.
async fn heartbeat_once(http: &reqwest::Client, cfg: &RegisterConfig) -> Result<bool, String> {
    let url = format!("{}/cp/v1/heartbeat", cfg.cp_url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .bearer_auth(&cfg.register_token)
        .json(&json!({ "name": cfg.name }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(true)
    } else if status.as_u16() == 404 {
        Ok(false)
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("control plane returned {status}: {body}"))
    }
}

/// Best-effort `DELETE /cp/v1/hosts/{name}` on graceful shutdown.
async fn deregister_once(http: &reqwest::Client, cfg: &RegisterConfig) {
    let url = format!(
        "{}/cp/v1/hosts/{}",
        cfg.cp_url.trim_end_matches('/'),
        cfg.name
    );
    match http
        .delete(&url)
        .bearer_auth(&cfg.register_token)
        .send()
        .await
    {
        Ok(resp) => {
            tracing::info!(status = resp.status().as_u16(), name = %cfg.name, "deregistered from control plane");
        }
        Err(e) => {
            tracing::warn!(error = %e, name = %cfg.name, "deregister from control plane failed (best-effort)");
        }
    }
}

/// Spawn the auto-registration lifecycle: register (with bounded backoff), then
/// heartbeat every `ttl/2`, then best-effort deregister when `shutdown` fires.
///
/// Never panics and never crashes the gateway: every failure is logged and the
/// loop keeps trying. Returns immediately after spawning the background task.
pub fn spawn(cfg: RegisterConfig, mut shutdown: watch::Receiver<bool>) {
    if cfg.gateway_token.is_empty() {
        tracing::warn!(
            "registering with the control plane without a gateway bearer token; \
             the control plane will not be able to call this gateway's /v1 API"
        );
    }
    if matches!(cfg.verification, PeerVerification::Insecure) {
        tracing::warn!(
            cp_url = %cfg.cp_url,
            "registering with the control plane in INSECURE TLS mode \
             (--register-tls-insecure): accepting ANY control-plane cert. \
             Dev only — pin the control plane with --register-pin or trust a CA \
             with --register-ca for production."
        );
    }

    tokio::spawn(async move {
        let http = match build_client(&cfg.verification) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to build control-plane HTTP client; auto-registration disabled");
                return;
            }
        };

        // Startup registration burst with bounded backoff. A failure here is not
        // fatal — fall through to the heartbeat loop, which re-registers on 404.
        let mut registered = false;
        for attempt in 0..STARTUP_ATTEMPTS {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return;
                    }
                }
                result = register_once(&http, &cfg) => {
                    match result {
                        Ok(()) => {
                            tracing::info!(cp_url = %cfg.cp_url, name = %cfg.name, "registered with the control plane");
                            registered = true;
                            break;
                        }
                        Err(e) => {
                            let delay = backoff_for(attempt);
                            tracing::warn!(error = %e, attempt = attempt + 1, retry_in_secs = delay, "control-plane registration failed; retrying");
                            if sleep_or_shutdown(delay, &mut shutdown).await {
                                return;
                            }
                        }
                    }
                }
            }
        }
        if !registered {
            tracing::warn!("initial control-plane registration did not succeed; the heartbeat loop will keep retrying");
        }

        // Heartbeat loop: refresh `last_seen` every ttl/2; on a 404 (host expired
        // / never registered) re-register. Runs until shutdown is signalled.
        let interval = Duration::from_secs((cfg.ttl_secs / 2).max(1));
        loop {
            if sleep_or_shutdown(interval.as_secs(), &mut shutdown).await {
                break;
            }
            match heartbeat_once(&http, &cfg).await {
                Ok(true) => {
                    tracing::debug!(name = %cfg.name, "heartbeat ok");
                }
                Ok(false) => {
                    tracing::info!(name = %cfg.name, "control plane does not know this host; re-registering");
                    if let Err(e) = register_once(&http, &cfg).await {
                        tracing::warn!(error = %e, "re-registration failed; will retry on next heartbeat");
                    } else {
                        tracing::info!(name = %cfg.name, "re-registered with the control plane");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, name = %cfg.name, "heartbeat failed; will retry");
                }
            }
        }

        // Graceful shutdown: best-effort deregister so the CP drops us promptly.
        deregister_once(&http, &cfg).await;
    });
}

/// The backoff delay (seconds) for a given attempt index, clamped to the tail of
/// [`BACKOFF_SECS`].
fn backoff_for(attempt: usize) -> u64 {
    let idx = attempt.min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[idx]
}

/// Sleep `secs` seconds, or return early if shutdown is signalled. Returns
/// `true` if shutdown fired (the caller should stop), `false` if the sleep
/// elapsed normally.
async fn sleep_or_shutdown(secs: u64, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => false,
        _ = shutdown.changed() => *shutdown.borrow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RegisterConfig {
        let mut labels = BTreeMap::new();
        labels.insert("env".to_string(), "dev".to_string());
        RegisterConfig {
            cp_url: "https://cp:9443/".to_string(),
            register_token: "reg".to_string(),
            advertise_url: "https://gw:8443".to_string(),
            name: "host-a".to_string(),
            labels,
            gateway_token: "gw-tok".to_string(),
            ttl_secs: 30,
            verification: PeerVerification::Insecure,
        }
    }

    #[test]
    fn register_body_has_expected_shape() {
        let body = register_body(&cfg());
        assert_eq!(body["name"], "host-a");
        assert_eq!(body["url"], "https://gw:8443");
        assert_eq!(body["token"], "gw-tok");
        assert_eq!(body["ttl_secs"], 30);
        assert_eq!(body["labels"]["env"], "dev");
    }

    #[test]
    fn backoff_is_bounded_and_monotone_to_cap() {
        // Early attempts ramp up; beyond the table they clamp to the last value.
        assert_eq!(backoff_for(0), 1);
        assert_eq!(backoff_for(1), 2);
        let last = *BACKOFF_SECS.last().unwrap();
        assert_eq!(backoff_for(BACKOFF_SECS.len()), last);
        assert_eq!(backoff_for(1000), last);
    }
}
