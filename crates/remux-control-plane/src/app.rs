//! The control-plane axum application: shared state, the two-token auth +
//! audit middleware, and the `/cp/v1` handlers — the outbound host registry, the
//! federated fleet API (concurrent fan-out), and intent-based session routing.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use remux_gateway::dto::{CreateSessionBody, SessionView, SizeBody};

use crate::auth::{audit_id_for, bearer_from_header, AuthConfig, Permission, Principal};
use crate::client::GatewayClient;
use crate::registry::{HostEntry, HostView, Registry, DEFAULT_TTL};

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// The in-memory host registry.
    pub registry: Registry,
    /// Admin + register bearer tokens.
    pub auth: AuthConfig,
    /// Whether outbound gateway calls accept self-signed certs (v1 default true).
    pub gateway_tls_insecure: bool,
    /// Per-gateway request timeout for fan-out / resolve calls.
    pub gateway_timeout: Duration,
}

impl AppState {
    pub fn new(auth: AuthConfig) -> Self {
        Self {
            registry: Registry::new(),
            auth,
            gateway_tls_insecure: true,
            gateway_timeout: crate::client::DEFAULT_GATEWAY_TIMEOUT,
        }
    }

    /// Set the outbound gateway TLS-insecure flag.
    pub fn with_gateway_tls_insecure(mut self, insecure: bool) -> Self {
        self.gateway_tls_insecure = insecure;
        self
    }

    /// Set the per-gateway request timeout.
    pub fn with_gateway_timeout(mut self, timeout: Duration) -> Self {
        self.gateway_timeout = timeout;
        self
    }

    /// Build a [`GatewayClient`] for a registered host using its stored token and
    /// the control plane's TLS-trust/timeout posture.
    fn client_for(&self, host: &HostEntry) -> Result<GatewayClient, crate::client::GatewayError> {
        GatewayClient::new(
            host.gateway_url.clone(),
            host.gateway_token.clone(),
            self.gateway_tls_insecure,
            self.gateway_timeout,
        )
    }
}

/// Identity resolved by the auth middleware, stashed for the audit layer.
#[derive(Clone)]
struct AuthContext {
    subject: String,
    roles: String,
    token_id: String,
    /// The auth method that produced the principal (`"static"` / `"jwt"`).
    method: &'static str,
}

/// Build the full `/cp/v1` router with **per-route permission** enforcement (the
/// shared [`remux_authz`] RBAC model) + audit layer.
///
/// Each route group declares the [`Permission`] it requires; a middleware
/// resolves the bearer to a [`Principal`] (else `401`) and rejects a principal
/// lacking the permission with `403`.
pub fn router(state: AppState) -> Router {
    // A one-route-group guard: enforce `perm` before the inner routes run.
    let guard = |perm: Permission| {
        let state = state.clone();
        middleware::from_fn_with_state(
            state,
            move |State(st): State<AppState>, req: Request, next: Next| {
                enforce_permission(st, perm, req, next)
            },
        )
    };

    // Registration surface: register / heartbeat / deregister.
    let register_routes = Router::new()
        .route("/register", post(register))
        .route("/heartbeat", post(heartbeat))
        .route("/hosts/:name", delete(deregister))
        .layer(guard(Permission::HostRegister));

    // Fleet API: read hosts, read federated sessions, resolve.
    let hosts = Router::new()
        .route("/hosts", get(list_hosts))
        .layer(guard(Permission::FleetHostsRead));
    let sessions = Router::new()
        .route("/sessions", get(federated_sessions))
        .layer(guard(Permission::FleetSessionsRead));
    let resolve_route = Router::new()
        .route("/resolve", post(resolve))
        .layer(guard(Permission::FleetResolve));

    // Public (no auth): health.
    let public = Router::new().route("/health", get(health));

    Router::new()
        .nest(
            "/cp/v1",
            public
                .merge(register_routes)
                .merge(hosts)
                .merge(sessions)
                .merge(resolve_route),
        )
        .layer(middleware::from_fn(audit_layer))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Auth + audit middleware
// ---------------------------------------------------------------------------

/// Deny-by-default: resolve the presented bearer to a [`Principal`] and enforce
/// `required`. A missing/unknown token is `401`; a known principal lacking the
/// permission is `403`.
async fn enforce_permission(
    state: AppState,
    required: Permission,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = extract_token(request.headers());
    let (principal, method): (Principal, &'static str) = match presented
        .as_deref()
        .and_then(|t| state.auth.authenticate(t))
    {
        Some((p, m)) => (p, m.as_str()),
        None => {
            let body =
                json!({ "error": "missing or invalid bearer token", "kind": "unauthorized" });
            return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
        }
    };
    if !state.auth.permits(&principal, required) {
        let body = json!({
            "error": format!(
                "principal {:?} lacks the {} permission required for this endpoint",
                principal.subject, required
            ),
            "kind": "forbidden",
        });
        return (StatusCode::FORBIDDEN, Json(body)).into_response();
    }
    let token_id = presented.as_deref().map(audit_id_for).unwrap_or_default();
    let ctx = AuthContext {
        subject: principal.subject.clone(),
        roles: principal.roles_display(),
        token_id,
        method,
    };
    request.extensions_mut().insert(ctx.clone());
    let mut response = next.run(request).await;
    response.extensions_mut().insert(ctx);
    response
}

/// Per-request audit middleware: one structured `tracing` line per `/cp/v1`
/// request (method, matched path, status, principal subject + roles, hashed
/// token id, peer, latency — never the raw token).
async fn audit_layer(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let start = Instant::now();
    let response = next.run(request).await;
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    let (subject, roles, token_id, auth_method) = match response.extensions().get::<AuthContext>() {
        Some(ctx) => (
            ctx.subject.clone(),
            ctx.roles.clone(),
            ctx.token_id.clone(),
            ctx.method,
        ),
        None => (
            "anonymous".to_string(),
            String::new(),
            "anonymous".to_string(),
            "none",
        ),
    };

    tracing::info!(
        target: "remux_control_plane::audit",
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        subject = %subject,
        roles = %roles,
        auth_method = %auth_method,
        token_id = %token_id,
        remote = %remote,
        latency_ms = format!("{latency_ms:.2}"),
        "request"
    );

    response
}

/// Pull a bearer token from the `Authorization` header.
fn extract_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    bearer_from_header(s).map(|t| t.to_string())
}

// ---------------------------------------------------------------------------
// Registration handlers (register token)
// ---------------------------------------------------------------------------

/// `GET /cp/v1/health` — liveness, no auth.
async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Body for `POST /cp/v1/register`.
#[derive(Debug, Deserialize)]
struct RegisterBody {
    name: String,
    url: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    /// The gateway's bearer token the control plane will use to call it.
    token: String,
    /// Optional TTL in seconds (defaults to [`DEFAULT_TTL`]).
    #[serde(default)]
    ttl_secs: Option<u64>,
}

/// `POST /cp/v1/register` — idempotent upsert of a host by name; sets
/// `last_seen = now`. The gateway registers itself (outbound), preserving the
/// daemon's no-inbound-listener invariant.
async fn register(State(state): State<AppState>, Json(body): Json<RegisterBody>) -> Response {
    if body.name.trim().is_empty() {
        return bad_request("name must not be empty");
    }
    if body.url.trim().is_empty() {
        return bad_request("url must not be empty");
    }
    let ttl = body
        .ttl_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TTL);
    let created = state
        .registry
        .upsert(body.name.clone(), body.url, body.labels, body.token, ttl)
        .await;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (
        status,
        Json(json!({ "name": body.name, "created": created })),
    )
        .into_response()
}

/// Body for `POST /cp/v1/heartbeat`.
#[derive(Debug, Deserialize)]
struct HeartbeatBody {
    name: String,
}

/// `POST /cp/v1/heartbeat` — refresh `last_seen` for a registered host. A host
/// that isn't registered gets `404` (it should re-register).
async fn heartbeat(State(state): State<AppState>, Json(body): Json<HeartbeatBody>) -> Response {
    if state.registry.heartbeat(&body.name).await {
        (
            StatusCode::OK,
            Json(json!({ "name": body.name, "ok": true })),
        )
            .into_response()
    } else {
        not_found(&format!("no such host: {}", body.name))
    }
}

/// `DELETE /cp/v1/hosts/{name}` — deregister a host.
async fn deregister(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if state.registry.remove(&name).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found(&format!("no such host: {name}"))
    }
}

// ---------------------------------------------------------------------------
// Fleet API handlers (admin token)
// ---------------------------------------------------------------------------

/// `GET /cp/v1/hosts` — list every registered host with its computed health.
async fn list_hosts(State(state): State<AppState>) -> Response {
    let hosts: Vec<HostView> = state.registry.views().await;
    Json(hosts).into_response()
}

/// Parse repeated `label=k=v` query params from a raw query string into a
/// selector map. serde_urlencoded does not support repeated keys / sequences, so
/// we scan the raw query ourselves. A `label` entry whose value is not `k=v` (no
/// inner `=`) is rejected so the caller learns it was malformed.
fn parse_label_selectors(query: Option<&str>) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let Some(query) = query else {
        return Ok(out);
    };
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "label" {
            continue;
        }
        let value = urldecode(value);
        match value.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                out.insert(k.to_string(), v.to_string());
            }
            _ => return Err(format!("invalid label selector '{value}' (expected k=v)")),
        }
    }
    Ok(out)
}

/// Minimal percent-decoding for query values (handles `%XX` + `+`).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The per-host fan-out result for `GET /cp/v1/sessions`.
#[derive(Debug, Serialize)]
struct HostSessions {
    host: String,
    url: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    sessions: Vec<SessionView>,
}

/// `GET /cp/v1/sessions[?label=k=v]…` — concurrent fan-out to every healthy host
/// matching ALL given labels. Each host's `GET /v1/sessions` is called via a
/// [`GatewayClient`]; results are aggregated and tagged by host. An unreachable
/// or erroring host is reported `ok: false` with its error and **never** makes
/// the whole request fail.
async fn federated_sessions(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let selectors = match parse_label_selectors(query.as_deref()) {
        Ok(s) => s,
        Err(e) => return bad_request(&e),
    };
    let hosts = state.registry.healthy_matching(&selectors).await;

    let mut set: JoinSet<HostSessions> = JoinSet::new();
    for host in hosts {
        let state = state.clone();
        set.spawn(async move {
            let url = host.gateway_url.clone();
            let name = host.name.clone();
            match state.client_for(&host) {
                Ok(client) => match client.list_sessions().await {
                    Ok(sessions) => HostSessions {
                        host: name,
                        url,
                        ok: true,
                        error: None,
                        sessions,
                    },
                    Err(e) => HostSessions {
                        host: name,
                        url,
                        ok: false,
                        error: Some(e.to_string()),
                        sessions: vec![],
                    },
                },
                Err(e) => HostSessions {
                    host: name,
                    url,
                    ok: false,
                    error: Some(e.to_string()),
                    sessions: vec![],
                },
            }
        });
    }

    let mut results: Vec<HostSessions> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(hs) => results.push(hs),
            Err(e) => tracing::warn!(error = %e, "fan-out task panicked"),
        }
    }
    // Deterministic ordering by host name.
    results.sort_by(|a, b| a.host.cmp(&b.host));
    Json(results).into_response()
}

/// Body for `POST /cp/v1/resolve` (intent routing v1).
#[derive(Debug, Deserialize)]
struct ResolveBody {
    #[serde(default)]
    labels: BTreeMap<String, String>,
    /// Command for a new session if one must be created.
    #[serde(default)]
    command: Option<Vec<String>>,
    /// If a session with this name already exists on the chosen host, reuse it.
    #[serde(default)]
    reuse_name: Option<String>,
}

/// The response from `POST /cp/v1/resolve`.
#[derive(Debug, Serialize)]
struct ResolveResult {
    host: String,
    gateway_url: String,
    session_id: String,
    name: String,
    created: bool,
}

/// `POST /cp/v1/resolve` — intent routing v1. Pick the first HEALTHY host
/// matching all labels (deterministic, by name). If `reuse_name` names an
/// existing session on that host, return it; otherwise create one via the
/// gateway's `POST /v1/sessions`.
async fn resolve(State(state): State<AppState>, Json(body): Json<ResolveBody>) -> Response {
    let hosts = state.registry.healthy_matching(&body.labels).await;
    let host = match hosts.into_iter().next() {
        Some(h) => h,
        None => return not_found("no healthy host matches the requested labels"),
    };

    let client = match state.client_for(&host) {
        Ok(c) => c,
        Err(e) => return upstream_error(&host.name, &e.to_string()),
    };

    // Reuse: if a session of `reuse_name` already exists on the host, return it.
    if let Some(reuse_name) = body.reuse_name.as_deref() {
        match client.list_sessions().await {
            Ok(sessions) => {
                if let Some(existing) = sessions.into_iter().find(|s| s.name == reuse_name) {
                    return Json(ResolveResult {
                        host: host.name.clone(),
                        gateway_url: host.gateway_url.clone(),
                        session_id: existing.id,
                        name: existing.name,
                        created: false,
                    })
                    .into_response();
                }
            }
            Err(e) => return upstream_error(&host.name, &e.to_string()),
        }
    }

    // Otherwise create a new session.
    let command = match body.command {
        Some(c) if !c.is_empty() => c,
        _ => {
            return bad_request("no existing session to reuse; `command` is required to create one")
        }
    };
    let create = CreateSessionBody {
        name: body.reuse_name.clone(),
        command,
        cwd: None,
        env: vec![],
        size: SizeBody::default(),
    };
    match client.create_session(&create).await {
        Ok(view) => (
            StatusCode::CREATED,
            Json(ResolveResult {
                host: host.name.clone(),
                gateway_url: host.gateway_url.clone(),
                session_id: view.id,
                name: view.name,
                created: true,
            }),
        )
            .into_response(),
        Err(e) => upstream_error(&host.name, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Small response helpers
// ---------------------------------------------------------------------------

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg, "kind": "bad_request" })),
    )
        .into_response()
}

fn not_found(msg: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": msg, "kind": "not_found" })),
    )
        .into_response()
}

/// A gateway-call failure during resolve maps to `502 Bad Gateway`.
fn upstream_error(host: &str, msg: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": format!("host {host}: {msg}"), "kind": "upstream_error" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_labels_ok_and_err() {
        let ok = parse_label_selectors(Some("label=env=dev&label=region=us")).unwrap();
        assert_eq!(ok.get("env"), Some(&"dev".to_string()));
        assert_eq!(ok.get("region"), Some(&"us".to_string()));
        // Value may contain '=' (split on first only).
        let eq = parse_label_selectors(Some("label=k=a=b")).unwrap();
        assert_eq!(eq.get("k"), Some(&"a=b".to_string()));
        // No query -> empty map (match all).
        assert!(parse_label_selectors(None).unwrap().is_empty());
        // Non-label params are ignored.
        assert!(parse_label_selectors(Some("foo=bar")).unwrap().is_empty());
        // Malformed label value (no inner '=') -> error.
        assert!(parse_label_selectors(Some("label=nope")).is_err());
        assert!(parse_label_selectors(Some("label==v")).is_err());
        // Percent-encoded selector decodes.
        let dec = parse_label_selectors(Some("label=env%3Ddev")).unwrap();
        assert_eq!(dec.get("env"), Some(&"dev".to_string()));
    }

    #[test]
    fn extract_token_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(extract_token(&headers), Some("abc".to_string()));
        assert_eq!(extract_token(&HeaderMap::new()), None);
    }
}
