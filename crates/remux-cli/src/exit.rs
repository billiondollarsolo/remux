//! Exit-code taxonomy (ROBUSTNESS_PLAN §5.3).
//!
//! Maps `RemuxError` variants to meaningful process exit codes so scripts and
//! agents can branch on the outcome of a command.
//!
//! | Code | Meaning |
//! | --- | --- |
//! | 0 | Success |
//! | 1 | Generic/usage error |
//! | 3 | Session not found |
//! | 5 | Permission denied |
//! | 6 | Daemon unreachable |

use remux_core::RemuxError;

/// Map a `RemuxError` to its process exit code.
pub fn exit_code_for(err: &RemuxError) -> i32 {
    match err {
        RemuxError::SessionNotFound(_) => 3,
        RemuxError::PermissionDenied => 5,
        RemuxError::DaemonNotRunning | RemuxError::ConnectionFailed(_) => 6,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_not_found_is_3() {
        assert_eq!(exit_code_for(&RemuxError::SessionNotFound("x".into())), 3);
    }

    #[test]
    fn permission_denied_is_5() {
        assert_eq!(exit_code_for(&RemuxError::PermissionDenied), 5);
    }

    #[test]
    fn daemon_unreachable_is_6() {
        assert_eq!(exit_code_for(&RemuxError::DaemonNotRunning), 6);
        assert_eq!(exit_code_for(&RemuxError::ConnectionFailed("x".into())), 6);
    }

    #[test]
    fn other_errors_are_1() {
        assert_eq!(exit_code_for(&RemuxError::SessionExited(Some(2))), 1);
        assert_eq!(exit_code_for(&RemuxError::InvalidRequest("x".into())), 1);
        assert_eq!(exit_code_for(&RemuxError::NotAttached), 1);
        assert_eq!(exit_code_for(&RemuxError::Internal("x".into())), 1);
    }
}
