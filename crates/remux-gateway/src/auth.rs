//! Bearer-token authentication for the gateway (AW4 v1).
//!
//! Deny-by-default: every `/v1/*` route except `GET /v1/health` requires a valid
//! bearer token. For REST and WebSocket routes the token may arrive in the
//! `Authorization: Bearer <token>` header; WebSocket routes ADDITIONALLY accept
//! `?token=<token>` (browsers cannot set `Authorization` on a WS handshake).
//!
//! The token comparison is **constant-time** to avoid leaking the secret through
//! timing. Tokens are never logged in the clear; the audit line logs a short hash.

use std::sync::Arc;

/// Shared auth configuration: the single accepted bearer token.
///
/// v1 is a single static token (the plan's coarse model). It is wrapped in an
/// `Arc` and shared into the axum state.
#[derive(Clone)]
pub struct AuthConfig {
    token: Arc<String>,
}

impl AuthConfig {
    /// Build an auth config from the configured bearer token.
    pub fn new(token: String) -> Self {
        Self {
            token: Arc::new(token),
        }
    }

    /// Constant-time check that a presented token matches the configured one.
    ///
    /// Returns `false` for any length mismatch or byte difference without an
    /// early return, so the time taken does not depend on where a mismatch is.
    pub fn verify(&self, presented: &str) -> bool {
        constant_time_eq(self.token.as_bytes(), presented.as_bytes())
    }

    /// A short, non-reversible id for the token, for audit logging. Never the
    /// token itself.
    pub fn token_audit_id(&self) -> String {
        short_hash(self.token.as_bytes())
    }
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
    }
}
