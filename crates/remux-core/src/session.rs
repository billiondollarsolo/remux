use serde::{Deserialize, Serialize};

/// Unique identifier for a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(pub uuid::Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Current status of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Starting,
    Running,
    Exited,
    Failed,
}

/// Terminal dimensions (columns x rows).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_new_generates_unique_ids() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b, "each SessionId::new() should produce a unique ID");
    }

    #[test]
    fn session_id_serializes_roundtrip_json() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn session_status_variants() {
        let statuses = [SessionStatus::Starting, SessionStatus::Running, SessionStatus::Exited, SessionStatus::Failed];
        for status in statuses {
            let json = serde_json::to_string(&status).expect("serialize");
            let back: SessionStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(status, back);
        }
    }

    #[test]
    fn term_size_roundtrip() {
        let size = TermSize { cols: 120, rows: 40 };
        let json = serde_json::to_string(&size).expect("serialize");
        let back: TermSize = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(size, back);
    }
}
