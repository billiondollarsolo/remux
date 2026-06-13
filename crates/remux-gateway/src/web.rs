//! AW5 — the built-in browser client served by the gateway.
//!
//! A minimal, self-contained xterm.js terminal (HTML/JS/CSS) is embedded into
//! the binary with [`include_str!`] (zero new dependencies) and served from
//! routes **outside** the `/v1` bearer-auth group: the static assets carry no
//! secrets, and the user supplies their token in-page (or via `?token=`). The
//! WebSocket/REST calls the page makes are still authenticated by the existing
//! `/v1` auth.
//!
//! The UI is served by default; `--no-web-ui` (which clears
//! [`AppState::web_ui_enabled`]) makes `GET /` (and the asset routes) return
//! `404`.
//!
//! NOTE: the static client loads xterm.js from a CDN; vendoring it for offline /
//! air-gapped use is a deliberate follow-up (see `web/index.html`).

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use crate::app::AppState;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

/// Serve a static asset with the given content type, or `404` when the web UI is
/// disabled.
fn serve(state: &AppState, content_type: &'static str, body: &'static str) -> Response {
    if !state.web_ui_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// `GET /` — the built-in browser client (HTML shell).
pub async fn index(State(state): State<AppState>) -> Response {
    serve(&state, "text/html; charset=utf-8", INDEX_HTML)
}

/// `GET /app.js` — the client logic.
pub async fn app_js(State(state): State<AppState>) -> Response {
    serve(&state, "application/javascript; charset=utf-8", APP_JS)
}

/// `GET /style.css` — the client styling.
pub async fn style_css(State(state): State<AppState>) -> Response {
    serve(&state, "text/css; charset=utf-8", STYLE_CSS)
}
