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
//! What is deliberately **not** here: the HTTP/WS server (axum) and OpenAPI
//! generation. Those are AW2 — see the plan. This crate adds no web dependency.

pub mod api;
pub mod daemon_conn;
pub mod error;

pub use api::v1::convert;
pub use api::v1::dto;
pub use daemon_conn::{DaemonConn, WaitPredicate};
pub use error::ApiError;
