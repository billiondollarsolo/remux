//! `remux-gateway` — AW0: the public, versioned `/v1` API contract and the
//! `DaemonConn` adapter that bridges it onto the internal daemon protocol.
//!
//! This crate is the **hinge** described in [`docs/AGENT_API_PLAN.md`] §2: it
//! establishes a public API that is *independent of* `remux_core::protocol`,
//! so the internal wire format can keep evolving under `PROTOCOL_VERSION` while
//! the published `/v1` contract stays stable.
//!
//! What lives here (AW0):
//! - [`api::v1::dto`] — public, JSON-shaped DTOs (uuid/timestamps as strings,
//!   `status` as a lowercase string). Independent of the protocol types.
//! - [`api::v1::convert`] — the *only* place that knows both worlds: `From`/`Into`
//!   mappings between `remux_core` protocol/session types and the DTOs. All
//!   `serde(rename)`/string-mapping decisions live here and in the DTO layer.
//! - [`DaemonConn`] — an adapter that connects to the daemon over its local Unix
//!   socket, performs the `Hello` handshake (refusing a version mismatch), and
//!   exposes typed `request()` / `subscribe()` helpers plus a composed `wait()`.
//! - [`ApiError`] — a public error type with an HTTP-status mapping (`u16`),
//!   pre-defining the taxonomy AW2's axum server will reuse.
//!
//! AW2/AW3/AW4 build on this foundation, all in this crate:
//! - [`app`] — the axum router, REST `/v1` handlers, the JSON error wrapper, the
//!   scope-enforcing bearer-auth middleware, and the per-request audit middleware.
//! - [`ws`] — the WebSocket `/stream` (binary, attachable) and `/events`
//!   (structured JSON) endpoints.
//! - [`auth`] — bearer-token auth with a constant-time token→[`Principal`]
//!   resolve and per-route [`Permission`] checks against an RBAC [`Policy`]
//!   (the shared [`remux_authz`] model; plan §6.2).
//! - [`tls`] — rustls material (operator PEM or self-signed for loopback).
//! - [`server`] — serving over TLS via `axum-server`.
//! - [`api::v1::openapi`] — the generated OpenAPI 3.1 document (T0.5), served at
//!   `GET /v1/openapi.json` and committed to `docs/openapi.yaml`.
//!
//! The `remux-gateway` binary (`src/main.rs`) ties these together: a
//! TLS-terminating, bearer-authed HTTPS/WSS server bound to `127.0.0.1` by
//! default, translating the public `/v1` contract onto the local daemon socket.

pub mod api;
pub mod app;
pub mod auth;
pub mod daemon_conn;
pub mod error;
pub mod register;
pub mod selector;
pub mod server;
pub mod tls;
pub mod web;
pub mod ws;

pub use api::v1::convert;
pub use api::v1::dto;
pub use api::v1::openapi;
pub use app::{router, AppState};
pub use auth::AuthConfig;
pub use daemon_conn::{DaemonConn, WaitOutcome, WaitPredicate};
pub use error::ApiError;
pub use remux_authz::{Permission, Principal};
pub use selector::parse_selector;
pub use server::{bind_listener, serve};
pub use tls::TlsMaterial;
