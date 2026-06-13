//! `remux-authz` — the shared **principal + RBAC** authorization model for
//! Remux's network layer (the `remux-gateway` `/v1` API and the
//! `remux-control-plane` `/cp/v1` fleet API).
//!
//! This crate is pure: no network, no async, no `tracing`. It defines the model
//! both services enforce, so the gateway and control plane agree on permission
//! names, roles, and the credential→principal resolution rules.
//!
//! The pieces (Phase A of auth hardening):
//! - [`Permission`] — a fine-grained, stably-named capability spanning both
//!   surfaces (`"session.read"`, `"fleet.resolve"`, `"host.register"`, …).
//! - [`Role`] / [`Policy`] — a named permission set and the named collection of
//!   roles. Built-in roles live in [`policy::builtin_roles`] /
//!   [`Policy::builtin`].
//! - [`Principal`] — an authenticated subject holding role names, and
//!   [`permits`] — the deny-by-default authorization decision (union of the
//!   principal's known roles' permissions; unknown roles grant nothing).
//! - [`TokenStore`] — a constant-time bearer-token → [`Principal`] resolver (the
//!   Phase A credential resolver).
//! - [`load_auth_config`] — parse the shared TOML auth-config into a merged
//!   [`Policy`] and the token→principal pairs.
//!
//! Phase B (OIDC/JWT) — [`JwtValidator`] in [`jwt`] — adds a second, additive way
//! to produce a [`Principal`]: validate a JWT (HS256 / static RS256-ES256 PEM /
//! parsed JWKS) and map its claims (subject + array-or-`scope` roles). It is pure
//! and offline (`parse_jwks` consumes an already-fetched JWKS; no HTTP client
//! here). The services try the [`TokenStore`] first, then the validator, and feed
//! the resulting [`Principal`] through the **same** [`Policy`]/[`permits`]
//! decision and audit shape.
//!
//! **Designed for the later phase:** Phase C (mTLS + cert pinning) plugs in the
//! same way — another way to produce a [`Principal`], decision/audit unchanged.

mod config;
mod jwt;
mod permission;
mod policy;
mod principal;
mod token_store;

pub use config::{load_auth_config, AuthConfigError, AuthConfigFile, RoleEntry, TokenEntry};
pub use jwt::{parse_jwks, Jwks, JwtConfig, JwtError, JwtKey, JwtValidator};
pub use permission::{ParsePermissionError, Permission};
pub use policy::{builtin_roles, Policy, Role};
pub use principal::{permits, Authorizer, Principal};
pub use token_store::{constant_time_eq, TokenStore};

/// Extract a bearer token from an `Authorization` header value, if it is a
/// well-formed `Bearer <token>` (case-insensitive scheme). Shared by both
/// services so header parsing is identical.
pub fn bearer_from_header(value: &str) -> Option<&str> {
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// A short, non-reversible hex id for an arbitrary token string, for audit
/// logging. Never the token itself; stable for a given token (FNV-1a, 64-bit).
pub fn audit_id_for(token: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in token.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_from_header("Bearer abc123"), Some("abc123"));
        assert_eq!(bearer_from_header("bearer abc123"), Some("abc123"));
        assert_eq!(bearer_from_header("Bearer  spaced  "), Some("spaced"));
        assert_eq!(bearer_from_header("Basic xyz"), None);
        assert_eq!(bearer_from_header("Bearer "), None);
        assert_eq!(bearer_from_header("abc"), None);
    }

    #[test]
    fn audit_id_is_stable_and_not_the_token() {
        let id = audit_id_for("my-token");
        assert_eq!(id.len(), 16);
        assert_ne!(id, "my-token");
        assert_eq!(id, audit_id_for("my-token"));
        assert_ne!(audit_id_for("a"), audit_id_for("b"));
    }
}
