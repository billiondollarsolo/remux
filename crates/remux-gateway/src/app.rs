//! The axum application: router, shared state, the error wrapper that maps
//! [`ApiError`] to a JSON HTTP response, the bearer-auth middleware, and the
//! REST handlers (AW2). WebSocket handlers (AW3) live in [`crate::ws`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use remux_core::TermSize;

use crate::auth::{audit_id_for, bearer_from_header, AuthConfig, Permission, Principal};
use crate::convert::wait_result;
use crate::daemon_conn::WaitPredicate;
use crate::dto::{
    ApiErrorBody, CreateSessionBody, InputBody, RenameBody, ResizeBody, ScreenView, ScrollbackView,
    SessionView, WaitBody, WaitResult,
};
use crate::error::ApiError;
use crate::mtls::MtlsPrincipal;
use crate::selector::parse_selector;
use crate::DaemonConn;

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// Path to the daemon's Unix socket. Handlers open a fresh [`DaemonConn`]
    /// per request (per-request connections are fine per the plan §4.1).
    pub socket_path: Arc<PathBuf>,
    /// Bearer-token auth configuration.
    pub auth: AuthConfig,
    /// Whether the built-in browser client (AW5) is served. When `false`,
    /// `GET /`, `/app.js`, and `/style.css` return `404`. Defaults to `true`;
    /// disabled via `--no-web-ui`.
    pub web_ui_enabled: bool,
}

impl AppState {
    pub fn new(socket_path: PathBuf, auth: AuthConfig) -> Self {
        Self {
            socket_path: Arc::new(socket_path),
            auth,
            web_ui_enabled: true,
        }
    }

    /// Set whether the built-in browser client is served (default `true`).
    pub fn with_web_ui(mut self, enabled: bool) -> Self {
        self.web_ui_enabled = enabled;
        self
    }

    /// Open a fresh daemon connection (with handshake).
    pub async fn connect(&self) -> Result<DaemonConn, ApiError> {
        DaemonConn::connect(self.socket_path.as_path()).await
    }
}

/// A thin wrapper so [`ApiError`] can implement `IntoResponse` here (orphan-rule
/// safe: the wrapper is local to the gateway crate's server module).
pub struct ApiErrorResponse(pub ApiError);

impl From<ApiError> for ApiErrorResponse {
    fn from(e: ApiError) -> Self {
        ApiErrorResponse(e)
    }
}

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> Response {
        let err = self.0;
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = json!({
            "error": err.to_string(),
            "kind": err_kind(&err),
        });
        (status, Json(body)).into_response()
    }
}

/// A stable machine-readable `kind` token for the JSON error body.
fn err_kind(err: &ApiError) -> &'static str {
    match err {
        ApiError::NotFound(_) => "not_found",
        ApiError::Forbidden(_) => "forbidden",
        ApiError::Timeout(_) => "timeout",
        ApiError::DaemonUnavailable(_) => "daemon_unavailable",
        ApiError::BadRequest(_) => "bad_request",
        ApiError::Protocol(_) => "protocol",
        ApiError::Internal(_) => "internal",
    }
}

/// Identity resolved by the auth middleware and stashed in request extensions so
/// the audit middleware can log it without re-resolving the token.
#[derive(Clone)]
struct AuthContext {
    subject: String,
    roles: String,
    token_id: String,
    /// The auth method that produced the principal (`"static"` / `"jwt"`).
    method: &'static str,
}

/// Build the full router with auth + the `/v1` surface.
///
/// The authed surface uses **per-route permission** enforcement (the shared
/// [`remux_authz`] RBAC model; plan §6.2): each route group declares the
/// [`Permission`] it requires, and a middleware resolves the bearer to a
/// [`Principal`] (else `401`) and rejects a principal lacking the permission with
/// `403`. A per-request audit middleware wraps the whole `/v1` surface.
///
/// Routes are grouped by required permission. Single-permission groups keep the
/// router declarative while preserving the exact endpoint set.
pub fn router(state: AppState) -> Router {
    // A one-route-group guard: enforce `perm` before the inner routes run. The
    // closure captures the required permission and resolves the principal via the
    // shared state, so each route declares exactly the permission it needs.
    let guard = |perm: Permission| {
        let state = state.clone();
        middleware::from_fn_with_state(
            state,
            move |State(st): State<AppState>, req: Request, next: Next| {
                enforce_permission(st, perm, req, next)
            },
        )
    };

    // GET-style read routes (each declares its precise permission).
    let list = Router::new()
        .route("/sessions", get(list_sessions))
        .layer(guard(Permission::SessionList));
    let read = Router::new()
        .route("/sessions/:id", get(get_session))
        .route("/sessions/:id/screen", get(get_screen))
        .route("/sessions/:id/scrollback", get(get_scrollback))
        .layer(guard(Permission::SessionRead));
    let wait = Router::new()
        .route("/sessions/:id/wait", post(wait_session))
        .layer(guard(Permission::SessionWait));
    let events = Router::new()
        .route("/sessions/:id/events", get(crate::ws::events_ws))
        .layer(guard(Permission::EventsRead));

    // Mutating / injecting routes.
    let create = Router::new()
        .route("/sessions", post(create_session))
        .layer(guard(Permission::SessionCreate));
    let kill = Router::new()
        .route("/sessions/:id", axum::routing::delete(delete_session))
        .layer(guard(Permission::SessionKill));
    let rename = Router::new()
        .route("/sessions/:id", axum::routing::patch(patch_session))
        .layer(guard(Permission::SessionRename));
    let input = Router::new()
        .route("/sessions/:id/input", post(send_input))
        .layer(guard(Permission::SessionInput));
    let resize = Router::new()
        .route("/sessions/:id/resize", post(resize_session))
        .layer(guard(Permission::SessionResize));
    let stream = Router::new()
        .route("/sessions/:id/stream", get(crate::ws::stream_ws))
        .layer(guard(Permission::SessionStream));

    // Public (no auth): health + the OpenAPI document (discoverability).
    let public = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_json));

    // AW5: the built-in browser client (xterm.js), served OUTSIDE the `/v1`
    // auth group — the static HTML/JS/CSS carry no secrets; the user supplies a
    // token in-page. Returns 404 when `--no-web-ui` disabled it.
    let web_routes = Router::new()
        .route("/", get(crate::web::index))
        .route("/app.js", get(crate::web::app_js))
        .route("/style.css", get(crate::web::style_css));

    let v1 = public
        .merge(list)
        .merge(read)
        .merge(wait)
        .merge(events)
        .merge(create)
        .merge(kill)
        .merge(rename)
        .merge(input)
        .merge(resize)
        .merge(stream);

    Router::new()
        .nest("/v1", v1)
        // Audit every /v1 request (after routing, so the matched path is known).
        .layer(middleware::from_fn(audit_layer))
        .merge(web_routes)
        .with_state(state)
}

/// Resolve the presented bearer to a [`Principal`] and enforce `required`.
///
/// Deny-by-default:
/// - no/unknown token → `401` (`unauthorized`).
/// - a recognized principal lacking `required` → `403` (`forbidden`), a distinct
///   outcome from the `401`.
///
/// On success the resolved [`AuthContext`] (subject + roles + token id) is
/// inserted into request extensions so the audit layer can log it.
async fn enforce_permission(
    state: AppState,
    required: Permission,
    mut request: Request,
    next: Next,
) -> Response {
    // Precedence: a verified client certificate (mTLS) is the authenticated
    // principal — cert identity WINS over any bearer presented in the same
    // request. The custom acceptor injects `Option<MtlsPrincipal>` per
    // connection (present only when a valid client cert was verified).
    let mtls_principal = request
        .extensions()
        .get::<Option<MtlsPrincipal>>()
        .and_then(|o| o.clone());

    let presented = extract_token(request.headers(), request.uri().query());
    let (principal, method): (Principal, &'static str) = if let Some(mp) = mtls_principal {
        (mp.0, "mtls")
    } else {
        match presented
            .as_deref()
            .and_then(|t| state.auth.authenticate(t))
        {
            Some((p, m)) => (p, m.as_str()),
            None => {
                let body =
                    json!({ "error": "missing or invalid bearer token", "kind": "unauthorized" });
                return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
            }
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
    // Stash identity for the audit layer (token never logged in the clear). An
    // mTLS principal has no bearer token; its id is the cert subject.
    let token_id = if method == "mtls" {
        format!("cert:{}", principal.subject)
    } else {
        presented.as_deref().map(audit_id_for).unwrap_or_default()
    };
    let ctx = AuthContext {
        subject: principal.subject.clone(),
        roles: principal.roles_display(),
        token_id,
        method,
    };
    request.extensions_mut().insert(ctx.clone());
    let mut response = next.run(request).await;
    // Propagate the identity onto the response so the (outer) audit layer can
    // log it — request-extension mutations made here are not visible to the
    // outer layer, but response extensions are.
    response.extensions_mut().insert(ctx);
    response
}

/// Per-request audit middleware (plan §6.2 "Audit log").
///
/// Emits one structured `tracing` line per `/v1` request with: method, the
/// matched route path (with `{id}` placeholders, not the concrete id), HTTP
/// status, the resolved principal's subject + roles + non-reversible `token_id`
/// (or `anonymous` for the public routes / rejected requests), the client remote
/// address, and the latency in ms. **No secret material is logged** — only the
/// hashed token id.
async fn audit_layer(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    // Prefer the matched route template (`/v1/sessions/:id`) over the concrete
    // URI so ids are not spread across cardinality-exploding log lines.
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

    // The auth context (if any) was inserted by the permission middleware; for
    // public or rejected requests it is absent → anonymous.
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
        target: "remux_gateway::audit",
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

/// Pull a bearer token from the `Authorization` header or the `token` query param.
pub fn extract_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            if let Some(tok) = bearer_from_header(s) {
                return Some(tok.to_string());
            }
        }
    }
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some(val) = pair.strip_prefix("token=") {
                if !val.is_empty() {
                    return Some(urldecode(val));
                }
            }
        }
    }
    None
}

/// Minimal percent-decoding for the `token` query value (handles `%XX` + `+`).
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

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /v1/health` — liveness, no auth.
#[utoipa::path(
    get,
    path = "/v1/health",
    tag = "health",
    responses((status = 200, description = "Gateway is live")),
)]
pub(crate) async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// `GET /v1/openapi.json` — the generated OpenAPI 3.1 document (no auth, for
/// discoverability).
#[utoipa::path(
    get,
    path = "/v1/openapi.json",
    tag = "meta",
    responses((status = 200, description = "OpenAPI 3.1 document for the /v1 surface")),
)]
pub(crate) async fn openapi_json() -> impl IntoResponse {
    Json(crate::api::v1::openapi::api_doc())
}

/// `GET /v1/sessions` — list sessions.
#[utoipa::path(
    get,
    path = "/v1/sessions",
    tag = "sessions",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "All sessions", body = [SessionView]),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
    ),
)]
pub(crate) async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    let summaries = conn.list_sessions().await?;
    let views: Vec<SessionView> = summaries.into_iter().map(SessionView::from).collect();
    Ok(Json(views).into_response())
}

/// `POST /v1/sessions` — create a session.
#[utoipa::path(
    post,
    path = "/v1/sessions",
    tag = "sessions",
    security(("bearer" = [])),
    request_body = CreateSessionBody,
    responses(
        (status = 201, description = "Session created", body = SessionView),
        (status = 400, description = "Invalid request", body = ApiErrorBody),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 403, description = "Read-only token", body = ApiErrorBody),
    ),
)]
pub(crate) async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Response, ApiErrorResponse> {
    if body.command.is_empty() {
        return Err(ApiError::BadRequest("command must not be empty".to_string()).into());
    }
    let mut conn = state.connect().await?;
    let details = conn.create_session(body.into()).await?;
    let view: SessionView = details.into();
    let location = format!("/v1/sessions/{}", view.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(view),
    )
        .into_response())
}

/// `GET /v1/sessions/{id}` — inspect a session.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Session uuid or name")),
    responses(
        (status = 200, description = "Session details", body = SessionView),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    let details = conn.inspect_session(parse_selector(&id)).await?;
    let view: SessionView = details.into();
    Ok(Json(view).into_response())
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteQuery {
    signal: Option<i32>,
}

/// `DELETE /v1/sessions/{id}` — kill a session.
#[utoipa::path(
    delete,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "Session uuid or name"),
        ("signal" = Option<i32>, Query, description = "Signal to send (default SIGTERM)"),
    ),
    responses(
        (status = 204, description = "Session killed"),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 403, description = "Read-only token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    conn.kill_session(parse_selector(&id), q.signal).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `PATCH /v1/sessions/{id}` — rename a session.
#[utoipa::path(
    patch,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Session uuid or name")),
    request_body = RenameBody,
    responses(
        (status = 200, description = "Updated session", body = SessionView),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 403, description = "Read-only token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn patch_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Result<Response, ApiErrorResponse> {
    let selector = parse_selector(&id);
    let mut conn = state.connect().await?;
    conn.rename_session(selector.clone(), body.name).await?;
    // Return the updated view.
    let details = conn.inspect_session(selector).await?;
    let view: SessionView = details.into();
    Ok(Json(view).into_response())
}

/// `POST /v1/sessions/{id}/input` — send input (fire-and-forget).
///
/// Body variants (mirroring `cmd/send.rs`'s `InputSource`):
/// - `application/json` with `{ "text": "..." }` (only `\n \t \r \\` interpreted)
///   or `{ "bytes_hex": "1b5b41" }`.
/// - any other content type (e.g. `application/octet-stream`) → the raw body
///   bytes are sent verbatim.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/input",
    tag = "sessions",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Session uuid or name")),
    request_body = InputBody,
    responses(
        (status = 202, description = "Input accepted (fire-and-forget)"),
        (status = 400, description = "Invalid input body", body = ApiErrorBody),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 403, description = "Read-only token", body = ApiErrorBody),
    ),
)]
pub(crate) async fn send_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiErrorResponse> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("application/json"))
        .unwrap_or(false);

    let data: Vec<u8> = if is_json {
        let parsed: InputBody = serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("invalid input body: {e}")))?;
        input_body_to_bytes(parsed)?
    } else {
        body.to_vec()
    };

    let mut conn = state.connect().await?;
    conn.send_input(parse_selector(&id), data).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Convert an [`InputBody`] (the JSON form) into the raw bytes to send: exactly
/// one of `text` / `bytes_hex`.
fn input_body_to_bytes(body: InputBody) -> Result<Vec<u8>, ApiError> {
    match (body.text, body.bytes_hex) {
        (Some(t), None) => Ok(interpret_text_escapes(&t)),
        (None, Some(h)) => decode_hex(&h),
        (Some(_), Some(_)) => Err(ApiError::BadRequest(
            "provide exactly one of `text` or `bytes_hex`".to_string(),
        )),
        (None, None) => Err(ApiError::BadRequest(
            "input body must contain `text` or `bytes_hex`".to_string(),
        )),
    }
}

/// Interpret the limited escape set `cmd/send.rs` honors for `text`:
/// `\n \t \r \\` (everything else is passed through literally, backslash kept).
fn interpret_text_escapes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push(b'\n');
                    chars.next();
                }
                Some('t') => {
                    out.push(b'\t');
                    chars.next();
                }
                Some('r') => {
                    out.push(b'\r');
                    chars.next();
                }
                Some('\\') => {
                    out.push(b'\\');
                    chars.next();
                }
                _ => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

/// Decode a hex string (e.g. `"1b5b41"`) into bytes.
fn decode_hex(h: &str) -> Result<Vec<u8>, ApiError> {
    let trimmed: String = h.chars().filter(|c| !c.is_whitespace()).collect();
    if !trimmed.len().is_multiple_of(2) {
        return Err(ApiError::BadRequest(
            "bytes_hex must have an even number of hex digits".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char)
            .to_digit(16)
            .ok_or_else(|| ApiError::BadRequest("bytes_hex has a non-hex digit".to_string()))?;
        let lo = (bytes[i + 1] as char)
            .to_digit(16)
            .ok_or_else(|| ApiError::BadRequest("bytes_hex has a non-hex digit".to_string()))?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Ok(out)
}

/// `GET /v1/sessions/{id}/screen` — capture the screen as structured JSON cells.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/screen",
    tag = "sessions",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Session uuid or name")),
    responses(
        (status = 200, description = "Structured screen snapshot", body = ScreenView),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn get_screen(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    let snapshot = conn.capture_screen(parse_selector(&id)).await?;
    let view: ScreenView = snapshot.into();
    Ok(Json(view).into_response())
}

#[derive(Debug, Deserialize)]
pub(crate) struct ScrollbackQuery {
    lines: Option<usize>,
}

/// `GET /v1/sessions/{id}/scrollback?lines=N` — read scrollback as decoded text.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/scrollback",
    tag = "sessions",
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "Session uuid or name"),
        ("lines" = Option<usize>, Query, description = "Max lines (default 1000)"),
    ),
    responses(
        (status = 200, description = "Scrollback chunk", body = ScrollbackView),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn get_scrollback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ScrollbackQuery>,
) -> Result<Response, ApiErrorResponse> {
    let lines = q.lines.unwrap_or(1000);
    let mut conn = state.connect().await?;
    let chunk = conn.read_scrollback(parse_selector(&id), lines).await?;
    let view: ScrollbackView = chunk.into();
    Ok(Json(view).into_response())
}

/// `POST /v1/sessions/{id}/resize` — resize the PTY.
///
/// Resize requires being the controlling client (the daemon enforces it), so the
/// gateway briefly attaches as Control over a throwaway connection, issues the
/// resize, and detaches. This may transiently steal control from a live
/// `/stream` attachment (the daemon emits `ControlLost`), which is the intended
/// semantics of an explicit resize request.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/resize",
    tag = "sessions",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Session uuid or name")),
    request_body = ResizeBody,
    responses(
        (status = 200, description = "Resized"),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 403, description = "Read-only token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn resize_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ResizeBody>,
) -> Result<Response, ApiErrorResponse> {
    let selector = parse_selector(&id);
    let size = TermSize {
        cols: body.cols,
        rows: body.rows,
    };
    let conn = state.connect().await?;
    let (_stream, mut handle, _bootstrap) = conn.subscribe_control(selector, size).await?;
    // The Control attach itself applies `size` on attach; send an explicit resize
    // too so a no-op-size attach still resizes, then detach.
    handle.resize(size).await?;
    let _ = handle.detach().await;
    Ok(StatusCode::OK.into_response())
}

#[derive(Debug, Deserialize)]
pub(crate) struct WaitQuery {
    timeout_ms: Option<u64>,
}

/// `POST /v1/sessions/{id}/wait` — wait on semantic state.
#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/wait",
    tag = "sessions",
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "Session uuid or name"),
        ("timeout_ms" = Option<u64>, Query, description = "Overall wait timeout in ms"),
    ),
    request_body = WaitBody,
    responses(
        (status = 200, description = "Wait outcome", body = WaitResult),
        (status = 400, description = "Invalid predicate (e.g. bad regex)", body = ApiErrorBody),
        (status = 401, description = "Missing or invalid token", body = ApiErrorBody),
        (status = 404, description = "No such session", body = ApiErrorBody),
    ),
)]
pub(crate) async fn wait_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
    Json(body): Json<WaitBody>,
) -> Result<Response, ApiErrorResponse> {
    let selector = parse_selector(&id);
    let predicate = match body {
        WaitBody::Idle { ms } => WaitPredicate::Idle(Duration::from_millis(ms)),
        WaitBody::Regex { pattern } => WaitPredicate::Regex(pattern),
        WaitBody::Exit => WaitPredicate::Exit,
    };
    let timeout = q.timeout_ms.map(Duration::from_millis);
    let conn = state.connect().await?;
    let outcome = conn.wait(selector, predicate, timeout).await?;
    let result = wait_result(outcome.result_str(), outcome.exit_code());
    Ok(Json(result).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    #[test]
    fn extract_token_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer abc123"),
        );
        assert_eq!(extract_token(&headers, None), Some("abc123".to_string()));
    }

    #[test]
    fn extract_token_from_query() {
        let headers = HeaderMap::new();
        assert_eq!(
            extract_token(&headers, Some("foo=1&token=xyz789&bar=2")),
            Some("xyz789".to_string())
        );
    }

    #[test]
    fn extract_token_header_precedence_over_query() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer fromheader"),
        );
        assert_eq!(
            extract_token(&headers, Some("token=fromquery")),
            Some("fromheader".to_string())
        );
    }

    #[test]
    fn extract_token_none_when_absent() {
        let headers = HeaderMap::new();
        assert_eq!(extract_token(&headers, Some("foo=1")), None);
        assert_eq!(extract_token(&headers, None), None);
    }

    #[test]
    fn urldecode_handles_percent_and_plus() {
        assert_eq!(urldecode("a%2Bb"), "a+b");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("plain"), "plain");
    }

    #[test]
    fn text_escapes_interpreted() {
        assert_eq!(interpret_text_escapes("echo hi\\n"), b"echo hi\n");
        assert_eq!(interpret_text_escapes("a\\tb"), b"a\tb");
        assert_eq!(interpret_text_escapes("c\\\\d"), b"c\\d");
        // Unknown escape keeps the backslash + char.
        assert_eq!(interpret_text_escapes("x\\zy"), b"x\\zy");
    }

    #[test]
    fn hex_decode_roundtrip() {
        assert_eq!(decode_hex("1b5b41").unwrap(), vec![0x1b, 0x5b, 0x41]);
        assert_eq!(decode_hex("1b 5b 41").unwrap(), vec![0x1b, 0x5b, 0x41]);
        assert!(decode_hex("1b5").is_err());
        assert!(decode_hex("zz").is_err());
    }

    #[test]
    fn input_body_requires_exactly_one() {
        let none = InputBody {
            text: None,
            bytes_hex: None,
        };
        assert!(input_body_to_bytes(none).is_err());
        let both = InputBody {
            text: Some("a".into()),
            bytes_hex: Some("61".into()),
        };
        assert!(input_body_to_bytes(both).is_err());
        let text = InputBody {
            text: Some("hi\\n".into()),
            bytes_hex: None,
        };
        assert_eq!(input_body_to_bytes(text).unwrap(), b"hi\n");
    }

    #[test]
    fn err_kind_tokens() {
        assert_eq!(err_kind(&ApiError::NotFound("x".into())), "not_found");
        assert_eq!(err_kind(&ApiError::Forbidden("x".into())), "forbidden");
        assert_eq!(
            err_kind(&ApiError::DaemonUnavailable("x".into())),
            "daemon_unavailable"
        );
        assert_eq!(err_kind(&ApiError::BadRequest("x".into())), "bad_request");
    }
}
