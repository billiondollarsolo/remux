//! The axum application: router, shared state, the error wrapper that maps
//! [`ApiError`] to a JSON HTTP response, the bearer-auth middleware, and the
//! REST handlers (AW2). WebSocket handlers (AW3) live in [`crate::ws`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use remux_core::TermSize;

use crate::auth::{bearer_from_header, AuthConfig};
use crate::convert::wait_result;
use crate::daemon_conn::WaitPredicate;
use crate::dto::{
    CreateSessionBody, ResizeBody, ScreenView, ScrollbackView, SessionView, WaitBody,
};
use crate::error::ApiError;
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
}

impl AppState {
    pub fn new(socket_path: PathBuf, auth: AuthConfig) -> Self {
        Self {
            socket_path: Arc::new(socket_path),
            auth,
        }
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

/// Build the full router with auth + the `/v1` surface.
pub fn router(state: AppState) -> Router {
    // The `/v1` surface that REQUIRES auth (everything except health).
    let authed = Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route(
            "/sessions/:id",
            get(get_session).delete(delete_session).patch(patch_session),
        )
        .route("/sessions/:id/input", post(send_input))
        .route("/sessions/:id/screen", get(get_screen))
        .route("/sessions/:id/scrollback", get(get_scrollback))
        .route("/sessions/:id/resize", post(resize_session))
        .route("/sessions/:id/wait", post(wait_session))
        .route("/sessions/:id/stream", get(crate::ws::stream_ws))
        .route("/sessions/:id/events", get(crate::ws::events_ws))
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

    // Health is public (no auth).
    let public = Router::new().route("/health", get(health));

    Router::new()
        .nest("/v1", public.merge(authed))
        .with_state(state)
}

/// Bearer-auth middleware. Deny-by-default: rejects with `401` + a small JSON
/// body unless a valid token is presented.
///
/// Accepts the token via the `Authorization: Bearer <token>` header (REST + WS)
/// and, for WebSocket upgrade routes, additionally via `?token=<token>` (browsers
/// cannot set `Authorization` on a WS handshake). The query fallback is accepted
/// on every authed route for simplicity; it is only meaningful for WS.
async fn auth_layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let presented = extract_token(request.headers(), request.uri().query());
    let ok = presented.map(|t| state.auth.verify(&t)).unwrap_or(false);
    if !ok {
        let body = json!({ "error": "missing or invalid bearer token", "kind": "unauthorized" });
        return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    }
    next.run(request).await
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
async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// `GET /v1/sessions` — list sessions.
async fn list_sessions(State(state): State<AppState>) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    let summaries = conn.list_sessions().await?;
    let views: Vec<SessionView> = summaries.into_iter().map(SessionView::from).collect();
    Ok(Json(views).into_response())
}

/// `POST /v1/sessions` — create a session.
async fn create_session(
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
async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    let details = conn.inspect_session(parse_selector(&id)).await?;
    let view: SessionView = details.into();
    Ok(Json(view).into_response())
}

#[derive(Debug, Deserialize)]
struct DeleteQuery {
    signal: Option<i32>,
}

/// `DELETE /v1/sessions/{id}` — kill a session.
async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    conn.kill_session(parse_selector(&id), q.signal).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct RenameBody {
    name: String,
}

/// `PATCH /v1/sessions/{id}` — rename a session.
async fn patch_session(
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
async fn send_input(
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
        parsed.into_bytes()?
    } else {
        body.to_vec()
    };

    let mut conn = state.connect().await?;
    conn.send_input(parse_selector(&id), data).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// JSON body for `POST .../input`: exactly one of `text` / `bytes_hex`.
#[derive(Debug, Deserialize)]
struct InputBody {
    text: Option<String>,
    bytes_hex: Option<String>,
}

impl InputBody {
    fn into_bytes(self) -> Result<Vec<u8>, ApiError> {
        match (self.text, self.bytes_hex) {
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
async fn get_screen(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiErrorResponse> {
    let mut conn = state.connect().await?;
    let snapshot = conn.capture_screen(parse_selector(&id)).await?;
    let view: ScreenView = snapshot.into();
    Ok(Json(view).into_response())
}

#[derive(Debug, Deserialize)]
struct ScrollbackQuery {
    lines: Option<usize>,
}

/// `GET /v1/sessions/{id}/scrollback?lines=N` — read scrollback as decoded text.
async fn get_scrollback(
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
async fn resize_session(
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
struct WaitQuery {
    timeout_ms: Option<u64>,
}

/// `POST /v1/sessions/{id}/wait` — wait on semantic state.
async fn wait_session(
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
        assert!(none.into_bytes().is_err());
        let both = InputBody {
            text: Some("a".into()),
            bytes_hex: Some("61".into()),
        };
        assert!(both.into_bytes().is_err());
        let text = InputBody {
            text: Some("hi\\n".into()),
            bytes_hex: None,
        };
        assert_eq!(text.into_bytes().unwrap(), b"hi\n");
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
