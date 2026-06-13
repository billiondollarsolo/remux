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
//!   `CreateSessionBody`). Gateway TLS is verified **secure by default** (system
//!   roots; or `--gateway-ca` / `--gateway-pin`; `--gateway-tls-insecure` is a
//!   dev-only opt-out) via [`remux_gateway::peer_tls`].
//! - [`app`] — the `/cp/v1` axum router: the registry endpoints (register/
//!   heartbeat/deregister/list), the federated fleet API (concurrent fan-out of
//!   `GET /v1/sessions`), and intent-based session [`resolve`]ing.
//! - [`auth`] — bearer auth with a constant-time token→[`Principal`] resolve and
//!   per-route [`Permission`] checks against an RBAC [`Policy`] (the shared
//!   [`remux_authz`] model), deny-by-default, with per-request audit logging.
//! - [`tls`] — rustls material (operator PEM or self-signed for loopback).
//! - [`server`] — serving over TLS via `axum-server`.
//!
//! Auth hardening is complete: RBAC + JWT/OIDC + **mTLS** (`--client-ca`,
//! `--mtls-mode`, `--mtls-identities`) + **gateway-cert pinning / CA trust**
//! (secure by default). **Deferred (future work):** cross-host session migration,
//! client-cert revocation (CRL/OCSP), and multi-tenant policy.

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
pub use server::{bind_listener, serve, serve_mtls};
pub use tls::TlsMaterial;
