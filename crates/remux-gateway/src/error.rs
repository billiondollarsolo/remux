//! Public `ApiError` for the gateway, derived from `RemuxError` and the
//! exit-code taxonomy, with an HTTP-status mapping.
//!
//! This pre-defines the status taxonomy AW2's axum server will reuse (plan
//! §5.4). It deliberately does **not** depend on `http`/axum: [`ApiError::http_status`]
//! returns a bare `u16` so this crate stays free of any web dependency.

use remux_core::RemuxError;
use thiserror::Error;

/// Error surfaced by the gateway API layer / `DaemonConn`.
///
/// Variants are coarse, public-facing categories. Internal `RemuxError`
/// variants are folded into them via [`From`] so the HTTP mapping is a simple
/// match. Keeping the taxonomy here (not in `remux-core`) preserves the
/// decoupling: the public status mapping is a gateway concern.
#[derive(Debug, Clone, Error)]
pub enum ApiError {
    /// The requested session does not exist (internal `SessionNotFound`, exit 3).
    #[error("not found: {0}")]
    NotFound(String),

    /// The caller is not permitted (internal `PermissionDenied`, exit 5).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A wait (or other bounded operation) timed out (exit 4).
    #[error("timeout: {0}")]
    Timeout(String),

    /// The daemon is unreachable / not running (internal `ConnectionFailed` /
    /// `DaemonNotRunning`, exit 6).
    #[error("daemon unavailable: {0}")]
    DaemonUnavailable(String),

    /// The request was malformed (bad regex, bad body, invalid request).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// A wire-format / protocol error talking to the daemon — including the
    /// `PROTOCOL_VERSION` handshake mismatch the gateway refuses to proxy.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Anything else (maps to 500).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// The HTTP status code this error maps to.
    ///
    /// Mirrors plan §5.4:
    /// - not found -> 404
    /// - forbidden -> 403
    /// - timeout -> 504 (gateway timed out waiting on the daemon/session)
    /// - daemon unavailable -> 503
    /// - bad request -> 400
    /// - protocol error -> 502 (bad upstream response)
    /// - internal -> 500
    pub fn http_status(&self) -> u16 {
        match self {
            ApiError::NotFound(_) => 404,
            ApiError::Forbidden(_) => 403,
            ApiError::Timeout(_) => 504,
            ApiError::DaemonUnavailable(_) => 503,
            ApiError::BadRequest(_) => 400,
            ApiError::Protocol(_) => 502,
            ApiError::Internal(_) => 500,
        }
    }
}

impl From<RemuxError> for ApiError {
    fn from(e: RemuxError) -> Self {
        match e {
            RemuxError::SessionNotFound(s) => ApiError::NotFound(s),
            RemuxError::SessionExists(s) => {
                ApiError::BadRequest(format!("session already exists: {s}"))
            }
            RemuxError::SessionExited(code) => {
                ApiError::BadRequest(format!("session exited with code: {code:?}"))
            }
            RemuxError::PermissionDenied => ApiError::Forbidden("permission denied".to_string()),
            RemuxError::NotAttached => ApiError::BadRequest("not attached".to_string()),
            RemuxError::AlreadyAttached(s) => {
                ApiError::BadRequest(format!("already attached: {s}"))
            }
            RemuxError::DaemonNotRunning => {
                ApiError::DaemonUnavailable("daemon not running".to_string())
            }
            RemuxError::ConnectionFailed(s) => ApiError::DaemonUnavailable(s),
            RemuxError::InvalidRequest(s) => ApiError::BadRequest(s),
            RemuxError::ProtocolError(s) => ApiError::Protocol(s),
            RemuxError::PtyError(s) => ApiError::Internal(format!("pty error: {s}")),
            RemuxError::IoError(s) => ApiError::Internal(format!("io error: {s}")),
            RemuxError::ConfigError(s) => ApiError::Internal(format!("config error: {s}")),
            RemuxError::Internal(s) => ApiError::Internal(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_to_404() {
        let e: ApiError = RemuxError::SessionNotFound("x".into()).into();
        assert!(matches!(e, ApiError::NotFound(_)));
        assert_eq!(e.http_status(), 404);
    }

    #[test]
    fn permission_denied_maps_to_403() {
        let e: ApiError = RemuxError::PermissionDenied.into();
        assert!(matches!(e, ApiError::Forbidden(_)));
        assert_eq!(e.http_status(), 403);
    }

    #[test]
    fn daemon_unreachable_maps_to_503() {
        let e: ApiError = RemuxError::ConnectionFailed("refused".into()).into();
        assert_eq!(e.http_status(), 503);
        let e: ApiError = RemuxError::DaemonNotRunning.into();
        assert_eq!(e.http_status(), 503);
    }

    #[test]
    fn invalid_request_maps_to_400() {
        let e: ApiError = RemuxError::InvalidRequest("bad regex".into()).into();
        assert!(matches!(e, ApiError::BadRequest(_)));
        assert_eq!(e.http_status(), 400);
    }

    #[test]
    fn protocol_error_maps_to_502() {
        let e: ApiError = RemuxError::ProtocolError("version mismatch".into()).into();
        assert!(matches!(e, ApiError::Protocol(_)));
        assert_eq!(e.http_status(), 502);
    }

    #[test]
    fn timeout_maps_to_504() {
        let e = ApiError::Timeout("wait expired".into());
        assert_eq!(e.http_status(), 504);
    }

    #[test]
    fn internal_maps_to_500() {
        let e: ApiError = RemuxError::Internal("boom".into()).into();
        assert_eq!(e.http_status(), 500);
        let e: ApiError = RemuxError::PtyError("x".into()).into();
        assert_eq!(e.http_status(), 500);
    }
}
