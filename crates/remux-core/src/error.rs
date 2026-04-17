use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Comprehensive error type for Remux.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum RemuxError {
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session already exists: {0}")]
    SessionExists(String),
    #[error("session exited with code: {0:?}")]
    SessionExited(Option<i32>),
    #[error("not attached to any session")]
    NotAttached,
    #[error("already attached to session: {0}")]
    AlreadyAttached(String),
    #[error("daemon not running")]
    DaemonNotRunning,
    #[error("daemon connection failed: {0}")]
    ConnectionFailed(String),
    #[error("permission denied")]
    PermissionDenied,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("pty error: {0}")]
    PtyError(String),
    #[error("io error: {0}")]
    IoError(String),
    #[error("protocol error: {0}")]
    ProtocolError(String),
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("internal error: {0}")]
    Internal(String),
}
