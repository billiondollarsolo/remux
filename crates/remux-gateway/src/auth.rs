//! Bearer-token authentication and authorization for the gateway (AW4 v1).
//!
//! Deny-by-default: every `/v1/*` route except `GET /v1/health` and
//! `GET /v1/openapi.json` requires a valid bearer token. For REST and WebSocket
//! routes the token may arrive in the `Authorization: Bearer <token>` header;
//! WebSocket routes ADDITIONALLY accept `?token=<token>` (browsers cannot set
//! `Authorization` on a WS handshake).
//!
//! v1 supports a **coarse scope split** (the plan §6.2): a token resolves to a
//! [`Scope`] of either [`Scope::ReadWrite`] (the full-access token) or
//! [`Scope::ReadOnly`] (an observe-only token). Read scope may call the safe
//! endpoints (list/inspect/screen/scrollback/wait/`/events`); write scope is
//! required for anything that mutates or injects (create/delete/rename/input/
//! resize/`/stream`). A read-only token hitting a write route is `403`
//! (distinct from the `401` for an unrecognized token).
//!
//! Each token comparison is **constant-time** to avoid leaking the secret
//! through timing. Tokens are never logged in the clear; the audit line logs a
//! short non-reversible hash (`token_audit_id`).

use std::sync::Arc;

/// The coarse authorization scope a token grants (plan §6.2).
///
/// `ReadOnly` ⊂ `ReadWrite`: anything a read token may do, a read-write token
/// may also do. Enforcement is per-route via [`Scope::satisfies`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Observe-only: list/inspect/screen/scrollback/wait and the `/events` WS.
    ReadOnly,
    /// Full access: everything `ReadOnly` allows, plus all mutating/injecting
    /// endpoints (create/delete/rename/input/resize and the `/stream` WS).
    ReadWrite,
}

impl Scope {
    /// Whether a token of `self` scope is allowed to call a route that
    /// `required` scope. `ReadWrite` satisfies any requirement; `ReadOnly`
    /// satisfies only a `ReadOnly` requirement.
    pub fn satisfies(self, required: Scope) -> bool {
        matches!(
            (self, required),
            (Scope::ReadWrite, _) | (Scope::ReadOnly, Scope::ReadOnly)
        )
    }
}

/// Shared auth configuration: the accepted bearer token(s) and their scopes.
///
/// v1 carries at most two static tokens: a required **read-write** token and an
/// optional **read-only** token (the plan's coarse model). It is cheap to clone
/// (the secrets sit behind an `Arc`) and is shared into the axum state.
#[derive(Clone)]
pub struct AuthConfig {
    /// The read-write token (always present).
    read_write: Arc<String>,
    /// The optional read-only token. `None` if no `--read-token` was configured.
    read_only: Option<Arc<String>>,
}

impl AuthConfig {
    /// Build an auth config with a read-write token and no read-only token.
    pub fn new(read_write: String) -> Self {
        Self {
            read_write: Arc::new(read_write),
            read_only: None,
        }
    }

    /// Build an auth config with both a read-write token and an optional
    /// read-only token. A read-only token equal to the read-write token is
    /// ignored (the read-write token wins, granting the broader scope).
    pub fn with_scopes(read_write: String, read_only: Option<String>) -> Self {
        let read_only = read_only
            .filter(|t| !t.is_empty() && *t != read_write)
            .map(Arc::new);
        Self {
            read_write: Arc::new(read_write),
            read_only,
        }
    }

    /// Resolve a presented bearer token to its [`Scope`], or `None` if it
    /// matches no configured token (the caller turns that into `401`).
    ///
    /// The read-write token is checked first so that a token configured as both
    /// resolves to the broader scope. Every configured token is compared in
    /// **constant time**; an unknown token still pays a constant-time compare
    /// against each configured token (no early bail on the first byte).
    pub fn resolve_scope(&self, presented: &str) -> Option<Scope> {
        // Evaluate both compares without short-circuiting so the timing does not
        // reveal which (if any) token matched.
        let rw = constant_time_eq(self.read_write.as_bytes(), presented.as_bytes());
        let ro = self
            .read_only
            .as_ref()
            .map(|t| constant_time_eq(t.as_bytes(), presented.as_bytes()))
            .unwrap_or(false);
        if rw {
            Some(Scope::ReadWrite)
        } else if ro {
            Some(Scope::ReadOnly)
        } else {
            None
        }
    }

    /// Constant-time check that a presented token matches *some* configured
    /// token (any scope). Retained for callers that only need an accept/reject
    /// decision.
    pub fn verify(&self, presented: &str) -> bool {
        self.resolve_scope(presented).is_some()
    }

    /// A short, non-reversible id for a presented token, for audit logging.
    /// Never the token itself. Stable for a given token string.
    pub fn token_audit_id(&self) -> String {
        short_hash(self.read_write.as_bytes())
    }

    /// Whether a read-only token is configured.
    pub fn has_read_only(&self) -> bool {
        self.read_only.is_some()
    }
}

/// A short, non-reversible id for an arbitrary token string, for audit logging.
/// Never the token itself.
pub fn audit_id_for(token: &str) -> String {
    short_hash(token.as_bytes())
}

/// Extract a bearer token from an `Authorization` header value, if it is a
/// well-formed `Bearer <token>`.
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

/// Constant-time byte-slice equality. Compares over the max length so the timing
/// does not reveal the secret's length or the position of the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Fold the length difference into the accumulator so unequal lengths always
    // fail, but we still iterate a fixed function of the inputs.
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

/// A short hex hash of a byte string (FNV-1a, 64-bit) for non-secret audit ids.
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
    fn verify_accepts_exact_token() {
        let cfg = AuthConfig::new("s3cr3t-token".to_string());
        assert!(cfg.verify("s3cr3t-token"));
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let cfg = AuthConfig::new("s3cr3t-token".to_string());
        assert!(!cfg.verify("s3cr3t-toker"));
        assert!(!cfg.verify("s3cr3t-token-extra"));
        assert!(!cfg.verify("short"));
        assert!(!cfg.verify(""));
    }

    #[test]
    fn resolve_scope_single_token_is_read_write() {
        let cfg = AuthConfig::new("rw".to_string());
        assert_eq!(cfg.resolve_scope("rw"), Some(Scope::ReadWrite));
        assert_eq!(cfg.resolve_scope("nope"), None);
        assert!(!cfg.has_read_only());
    }

    #[test]
    fn resolve_scope_with_read_only_token() {
        let cfg = AuthConfig::with_scopes("rw-token".to_string(), Some("ro-token".to_string()));
        assert_eq!(cfg.resolve_scope("rw-token"), Some(Scope::ReadWrite));
        assert_eq!(cfg.resolve_scope("ro-token"), Some(Scope::ReadOnly));
        assert_eq!(cfg.resolve_scope("bogus"), None);
        assert!(cfg.has_read_only());
    }

    #[test]
    fn read_only_equal_to_read_write_is_ignored() {
        // If the same string is given for both, it resolves to the broader scope
        // and no separate read-only token is registered.
        let cfg = AuthConfig::with_scopes("same".to_string(), Some("same".to_string()));
        assert_eq!(cfg.resolve_scope("same"), Some(Scope::ReadWrite));
        assert!(!cfg.has_read_only());
    }

    #[test]
    fn empty_read_only_is_ignored() {
        let cfg = AuthConfig::with_scopes("rw".to_string(), Some(String::new()));
        assert!(!cfg.has_read_only());
        assert_eq!(cfg.resolve_scope("rw"), Some(Scope::ReadWrite));
    }

    #[test]
    fn scope_satisfies_matrix() {
        assert!(Scope::ReadWrite.satisfies(Scope::ReadWrite));
        assert!(Scope::ReadWrite.satisfies(Scope::ReadOnly));
        assert!(Scope::ReadOnly.satisfies(Scope::ReadOnly));
        assert!(!Scope::ReadOnly.satisfies(Scope::ReadWrite));
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

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
        let cfg = AuthConfig::new("my-token".to_string());
        let id = cfg.token_audit_id();
        assert_eq!(id.len(), 16);
        assert_ne!(id, "my-token");
        // Stable across calls.
        assert_eq!(id, cfg.token_audit_id());
        // The free function agrees with the config method for the same token.
        assert_eq!(id, audit_id_for("my-token"));
    }
}
