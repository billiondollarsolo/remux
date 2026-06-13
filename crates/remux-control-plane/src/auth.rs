//! Bearer-token auth for the control plane (deny-by-default, constant-time
//! compare), mirroring the gateway's AW4 posture.
//!
//! The control plane resolves **two** distinct tokens per route group:
//! - the **admin** token guards the fleet API (`GET /cp/v1/hosts`,
//!   `GET /cp/v1/sessions`, `POST /cp/v1/resolve`);
//! - the **register** token guards the registration surface a gateway uses to
//!   join (`POST /cp/v1/register`, `POST /cp/v1/heartbeat`,
//!   `DELETE /cp/v1/hosts/{name}`).
//!
//! Separating them lets an operator hand each gateway only the lower-privilege
//! register token while keeping the admin (read-the-whole-fleet) token closely
//! held. Tokens are never logged in the clear; the audit line logs a short,
//! non-reversible id.

use std::sync::Arc;

/// Which token group a route requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The fleet/admin token (list hosts, federated sessions, resolve).
    Admin,
    /// The registration token (register, heartbeat, deregister).
    Register,
}

impl TokenKind {
    /// The audit-log string for this token kind.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Admin => "admin",
            TokenKind::Register => "register",
        }
    }
}

/// The shared auth config: the admin and register tokens.
#[derive(Clone)]
pub struct AuthConfig {
    admin: Arc<String>,
    register: Arc<String>,
}

impl AuthConfig {
    /// Build an auth config from the admin and register tokens.
    pub fn new(admin: String, register: String) -> Self {
        Self {
            admin: Arc::new(admin),
            register: Arc::new(register),
        }
    }

    /// Constant-time check that `presented` matches the token for `kind`.
    pub fn verify(&self, kind: TokenKind, presented: &str) -> bool {
        let expected = match kind {
            TokenKind::Admin => self.admin.as_bytes(),
            TokenKind::Register => self.register.as_bytes(),
        };
        constant_time_eq(expected, presented.as_bytes())
    }

    /// A short, non-reversible audit id for the admin token (for the startup log).
    pub fn admin_audit_id(&self) -> String {
        short_hash(self.admin.as_bytes())
    }

    /// A short, non-reversible audit id for the register token.
    pub fn register_audit_id(&self) -> String {
        short_hash(self.register.as_bytes())
    }
}

/// A short, non-reversible id for an arbitrary token string, for audit logging.
/// Never the token itself.
pub fn audit_id_for(token: &str) -> String {
    short_hash(token.as_bytes())
}

/// Extract a bearer token from an `Authorization: Bearer <token>` header value.
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

/// Constant-time byte-slice equality (folds the length difference in so unequal
/// lengths always fail without revealing the secret's length or first diff).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff: u8 =
        (a.len() as u64 ^ b.len() as u64) as u8 | ((a.len() as u64 ^ b.len() as u64) >> 8) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// A short hex hash (FNV-1a, 64-bit) for non-secret audit ids.
fn short_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_and_register_are_distinct() {
        let cfg = AuthConfig::new("admin-tok".into(), "reg-tok".into());
        assert!(cfg.verify(TokenKind::Admin, "admin-tok"));
        assert!(!cfg.verify(TokenKind::Admin, "reg-tok"));
        assert!(cfg.verify(TokenKind::Register, "reg-tok"));
        assert!(!cfg.verify(TokenKind::Register, "admin-tok"));
        assert!(!cfg.verify(TokenKind::Admin, ""));
        assert!(!cfg.verify(TokenKind::Register, "nope"));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_from_header("Bearer abc123"), Some("abc123"));
        assert_eq!(bearer_from_header("bearer abc123"), Some("abc123"));
        assert_eq!(bearer_from_header("Basic xyz"), None);
        assert_eq!(bearer_from_header("Bearer "), None);
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn audit_id_is_stable_and_not_the_token() {
        let cfg = AuthConfig::new("my-admin".into(), "my-reg".into());
        let id = cfg.admin_audit_id();
        assert_eq!(id.len(), 16);
        assert_ne!(id, "my-admin");
        assert_eq!(id, audit_id_for("my-admin"));
        assert_ne!(cfg.admin_audit_id(), cfg.register_audit_id());
    }
}
