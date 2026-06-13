//! `remux-control-plane` — AW6 federation **core** (control plane).
//!
//! This crate federates over a fleet of `remux-gateway` instances. It is the
//! first real slice of the control plane described in
//! [`docs/AGENT_API_PLAN.md`] §8 and `spec.md` §10 (Fleet/Agent model).
//!
//! What ships here:
//! - [`registry`] — an in-memory, **outbound** host registry. Gateways register
//!   *themselves* (outbound), preserving the invariant that the daemon never
//!   grows an inbound network listener.
//! - [`client`] — [`GatewayClient`], a reqwest wrapper over one gateway's public
//!   `/v1` API, reusing the gateway's shared DTOs (`SessionView`,
//!   `CreateSessionBody`). v1 trusts self-signed gateway certs (`--gateway-tls-insecure`).
//! - [`app`] — the `/cp/v1` axum router: the registry endpoints (register/
//!   heartbeat/deregister/list), the federated fleet API (concurrent fan-out of
//!   `GET /v1/sessions`), and intent-based session [`resolve`]ing.
//! - [`auth`] — bearer auth with a constant-time token→[`Principal`] resolve and
//!   per-route [`Permission`] checks against an RBAC [`Policy`] (the shared
//!   [`remux_authz`] model), deny-by-default, with per-request audit logging.
//! - [`tls`] — rustls material (operator PEM or self-signed for loopback).
//! - [`server`] — serving over TLS via `axum-server`.
//!
//! **Deferred (NEXT steps, not in this crate):** RBAC/OIDC/mTLS, gateway-cert
//! pinning / CA trust, cross-host session migration, and the `remux open` CLI +
//! gateway `--register` auto-registration.

pub mod app;
pub mod auth;
pub mod client;
pub mod registry;
pub mod server;
pub mod tls;

pub use app::{router, AppState};
pub use auth::AuthConfig;
pub use client::GatewayClient;
pub use registry::{HostEntry, HostView, Registry};
pub use remux_authz::{Permission, Principal};
pub use server::{bind_listener, serve};
pub use tls::TlsMaterial;
