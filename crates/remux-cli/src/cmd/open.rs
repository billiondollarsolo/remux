//! `remux open` — intent routing (AW6, client front-end).
//!
//! This is the human/agent front-end for the control plane's **intent routing**
//! (`POST /cp/v1/resolve`). The split is the elegant part:
//!
//! - the **control plane** decides WHICH host + session satisfies an intent
//!   (labels → host, reuse-or-create a session): pure intent.
//! - the **local fleet registry** (`[[fleet.hosts]]`) knows HOW to reach that
//!   host directly (an SSH target), so once the CP has resolved a target we can
//!   attach over the existing SSH transport.
//!
//! Flow:
//!   1. `POST <cp>/cp/v1/resolve { labels, command?, reuse_name? }` →
//!      `{ host, gateway_url, session_id, name, created }`.
//!   2. If the resolved `host` is present in the local `[[fleet.hosts]]` registry
//!      with an `ssh` target, attach over SSH to the resolved session (reusing
//!      the remote-attach path: `connect_remote(ssh)` + `cmd::attach::run`).
//!   3. Otherwise we don't know how to reach the host directly, so we DON'T fail:
//!      print the resolved target (human or `--json`) plus a hint that adding the
//!      host to `[[fleet.hosts]]` enables auto-attach, or that the gateway's
//!      browser UI can be used. Exit 0.
//!
//! The resolve + target-decision + formatting logic is kept in **pure**,
//! unit-testable functions ([`resolve`], [`decide_target`], [`format_target`]).
//!
//! **TLS-trust posture (v1).** The control plane defaults to a self-signed cert,
//! so the resolve client accepts self-signed certs (cert pinning is the deferred
//! follow-up).

use std::collections::BTreeMap;

use remux_core::{Config, FleetHost, RemuxError};
use serde::{Deserialize, Serialize};

use crate::client::RemuxClient;

/// The control plane's `POST /cp/v1/resolve` response shape (mirrors
/// `remux-control-plane`'s `ResolveResult`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveTarget {
    pub host: String,
    pub gateway_url: String,
    pub session_id: String,
    pub name: String,
    pub created: bool,
}

/// What `remux open` should do with a resolved target, given the local fleet
/// registry. Pure decision so it is unit-testable; the actual attach / printing
/// is performed by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    /// The resolved host is in the local fleet registry: attach to `session`
    /// over SSH at `ssh_target` (the existing remote-attach path).
    AttachViaSsh {
        ssh_target: String,
        /// The session to attach to — the resolved `name` (falling back to the
        /// id), which `cmd::attach::run` resolves on the remote daemon.
        session: String,
    },
    /// The host is not in the local registry; we can't reach it directly, so
    /// print the target and a hint instead of failing.
    PrintTarget,
}

/// How `remux open` resolves the control-plane URL + token: explicit flag, then
/// `[control_plane]` config, then the `REMUX_CP_URL` / `REMUX_CP_TOKEN` envs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpEndpoint {
    pub url: String,
    pub token: Option<String>,
}

/// Resolve the control-plane endpoint from (in precedence order) the flags, the
/// `[control_plane]` config, then the environment. Returns an error string if no
/// URL can be determined.
pub fn resolve_endpoint(
    flag_url: Option<String>,
    flag_token: Option<String>,
    config: &Config,
    env_url: Option<String>,
    env_token: Option<String>,
) -> Result<CpEndpoint, String> {
    let url = flag_url
        .filter(|s| !s.is_empty())
        .or_else(|| config.control_plane.url.clone().filter(|s| !s.is_empty()))
        .or_else(|| env_url.filter(|s| !s.is_empty()))
        .ok_or_else(|| {
            "no control plane URL: pass --control-plane <URL>, set [control_plane].url, \
             or REMUX_CP_URL"
                .to_string()
        })?;
    let token = flag_token
        .filter(|s| !s.is_empty())
        .or_else(|| config.control_plane.token.clone().filter(|s| !s.is_empty()))
        .or_else(|| env_token.filter(|s| !s.is_empty()));
    Ok(CpEndpoint { url, token })
}

/// Decide what to do with a resolved target given the local fleet registry. If
/// the resolved host name matches a `[[fleet.hosts]]` entry, attach over its SSH
/// target; otherwise print the target. Pure and unit-tested.
pub fn decide_target(target: &ResolveTarget, fleet_hosts: &[FleetHost]) -> OpenAction {
    match fleet_hosts.iter().find(|h| h.name == target.host) {
        Some(host) => OpenAction::AttachViaSsh {
            ssh_target: host.ssh.clone(),
            // Prefer the session NAME (human-friendly, stable); fall back to the
            // id if the resolve response had no name.
            session: if target.name.is_empty() {
                target.session_id.clone()
            } else {
                target.name.clone()
            },
        },
        None => OpenAction::PrintTarget,
    }
}

/// Format the resolved target for the user when we cannot auto-attach (the host
/// is not in the local fleet registry). Returns the string to print: a JSON
/// object when `json`, else a human line plus a hint.
pub fn format_target(target: &ResolveTarget, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(target).unwrap_or_else(|_| "{}".to_string());
    }
    let verb = if target.created { "created" } else { "reusing" };
    format!(
        "Resolved intent -> host {host} ({gateway})\n  session: {name} ({id}) [{verb}]\n\
         Host {host} is not in your local [[fleet.hosts]] registry, so remux can't \
         attach over SSH automatically.\n  - Add it to [[fleet.hosts]] (name = \"{host}\", \
         ssh = \"user@{host}\") to enable `remux open` auto-attach.\n  - Or open the gateway's \
         browser UI at {gateway} and attach there.",
        host = target.host,
        gateway = target.gateway_url,
        name = target.name,
        id = target.session_id,
        verb = verb,
    )
}

/// The JSON body for `POST /cp/v1/resolve`.
fn resolve_body(
    labels: &BTreeMap<String, String>,
    command: &[String],
    reuse_name: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert(
        "labels".to_string(),
        serde_json::to_value(labels).unwrap_or(serde_json::Value::Null),
    );
    if !command.is_empty() {
        body.insert(
            "command".to_string(),
            serde_json::to_value(command).unwrap_or(serde_json::Value::Null),
        );
    }
    if let Some(name) = reuse_name {
        body.insert(
            "reuse_name".to_string(),
            serde_json::Value::String(name.to_string()),
        );
    }
    serde_json::Value::Object(body)
}

/// Call `POST <cp>/cp/v1/resolve`, returning the resolved target. Accepts
/// self-signed control-plane certs (v1). The HTTP/JSON plumbing is isolated here
/// so the decision/formatting logic stays pure.
pub async fn resolve(
    endpoint: &CpEndpoint,
    labels: &BTreeMap<String, String>,
    command: &[String],
    reuse_name: Option<&str>,
) -> Result<ResolveTarget, String> {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/cp/v1/resolve", endpoint.url.trim_end_matches('/'));
    let mut req = http
        .post(&url)
        .json(&resolve_body(labels, command, reuse_name));
    if let Some(token) = endpoint.token.as_deref() {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("control plane returned {status}: {body}"));
    }
    resp.json::<ResolveTarget>()
        .await
        .map_err(|e| format!("invalid resolve response: {e}"))
}

/// `remux open` entry point. Resolves the intent against the control plane, then
/// either attaches over SSH (host in the local fleet) or prints the target.
///
/// Returns `Ok(())` on success (including the print-only branch). Attach errors
/// are surfaced as `RemuxError`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: &Config,
    endpoint: CpEndpoint,
    labels: BTreeMap<String, String>,
    command: Vec<String>,
    reuse_name: Option<String>,
    json: bool,
    read_only: bool,
) -> Result<(), RemuxError> {
    let target = resolve(&endpoint, &labels, &command, reuse_name.as_deref())
        .await
        .map_err(RemuxError::ConnectionFailed)?;

    match decide_target(&target, &config.fleet.hosts) {
        OpenAction::AttachViaSsh {
            ssh_target,
            session,
        } => {
            let client = RemuxClient::connect_remote(&ssh_target).await?;
            let status_line = config.client.status_line;
            crate::cmd::attach::run(
                client,
                session,
                &config.client.detach_key,
                read_only,
                status_line,
            )
            .await
        }
        OpenAction::PrintTarget => {
            println!("{}", format_target(&target, json));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, ssh: &str) -> FleetHost {
        FleetHost {
            name: name.to_string(),
            ssh: ssh.to_string(),
            labels: BTreeMap::new(),
        }
    }

    fn target(host: &str, created: bool) -> ResolveTarget {
        ResolveTarget {
            host: host.to_string(),
            gateway_url: format!("https://{host}:8443"),
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            name: "api-shell".to_string(),
            created,
        }
    }

    #[test]
    fn decide_attaches_when_host_in_fleet() {
        let hosts = vec![host("devbox", "user@devbox"), host("prod", "ops@prod")];
        let action = decide_target(&target("devbox", true), &hosts);
        assert_eq!(
            action,
            OpenAction::AttachViaSsh {
                ssh_target: "user@devbox".to_string(),
                session: "api-shell".to_string(),
            }
        );
    }

    #[test]
    fn decide_prints_when_host_not_in_fleet() {
        let hosts = vec![host("devbox", "user@devbox")];
        let action = decide_target(&target("staging", false), &hosts);
        assert_eq!(action, OpenAction::PrintTarget);
    }

    #[test]
    fn decide_falls_back_to_id_when_name_empty() {
        let hosts = vec![host("devbox", "user@devbox")];
        let mut t = target("devbox", true);
        t.name = String::new();
        let action = decide_target(&t, &hosts);
        assert_eq!(
            action,
            OpenAction::AttachViaSsh {
                ssh_target: "user@devbox".to_string(),
                session: t.session_id.clone(),
            }
        );
    }

    #[test]
    fn format_human_created_branch() {
        let s = format_target(&target("staging", true), false);
        assert!(s.contains("host staging"));
        assert!(s.contains("https://staging:8443"));
        assert!(s.contains("api-shell"));
        assert!(s.contains("[created]"));
        assert!(s.contains("[[fleet.hosts]]"));
    }

    #[test]
    fn format_human_reuse_branch() {
        let s = format_target(&target("staging", false), false);
        assert!(s.contains("[reusing]"));
    }

    #[test]
    fn format_json_branch_roundtrips() {
        let t = target("staging", true);
        let s = format_target(&t, true);
        let back: ResolveTarget = serde_json::from_str(&s).expect("valid json");
        assert_eq!(back, t);
        // created:false also serializes the flag.
        let s2 = format_target(&target("staging", false), true);
        assert!(s2.contains("\"created\": false"));
    }

    #[test]
    fn endpoint_precedence_flag_over_config_over_env() {
        let mut config = Config::default();
        config.control_plane.url = Some("https://config-cp:9443".to_string());
        config.control_plane.token = Some("config-token".to_string());

        // Flag wins.
        let ep = resolve_endpoint(
            Some("https://flag-cp:9443".to_string()),
            Some("flag-token".to_string()),
            &config,
            Some("https://env-cp:9443".to_string()),
            Some("env-token".to_string()),
        )
        .unwrap();
        assert_eq!(ep.url, "https://flag-cp:9443");
        assert_eq!(ep.token.as_deref(), Some("flag-token"));

        // No flag -> config.
        let ep = resolve_endpoint(None, None, &config, None, None).unwrap();
        assert_eq!(ep.url, "https://config-cp:9443");
        assert_eq!(ep.token.as_deref(), Some("config-token"));

        // No flag/config -> env.
        let empty = Config::default();
        let ep = resolve_endpoint(
            None,
            None,
            &empty,
            Some("https://env-cp:9443".to_string()),
            Some("env-token".to_string()),
        )
        .unwrap();
        assert_eq!(ep.url, "https://env-cp:9443");
        assert_eq!(ep.token.as_deref(), Some("env-token"));
    }

    #[test]
    fn endpoint_missing_url_errors() {
        let empty = Config::default();
        let err = resolve_endpoint(None, None, &empty, None, None).unwrap_err();
        assert!(err.contains("no control plane URL"));
    }

    #[test]
    fn resolve_body_omits_optional_fields_when_absent() {
        let mut labels = BTreeMap::new();
        labels.insert("env".to_string(), "dev".to_string());
        let body = resolve_body(&labels, &[], None);
        assert_eq!(body["labels"]["env"], "dev");
        assert!(body.get("command").is_none());
        assert!(body.get("reuse_name").is_none());

        let body = resolve_body(&labels, &["/bin/sh".to_string()], Some("api"));
        assert_eq!(body["command"][0], "/bin/sh");
        assert_eq!(body["reuse_name"], "api");
    }
}
