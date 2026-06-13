use serde::{Deserialize, Serialize};

use crate::error::RemuxError;
use crate::session::{SessionId, SessionStatus, TermSize};
use crate::terminal::TerminalSnapshot;

/// Wire protocol version. Bumped on any breaking change to the serialized
/// request/response/event/snapshot types. Defined here so future breaks can be
/// detected via a handshake instead of silently corrupting state.
pub const PROTOCOL_VERSION: u32 = 1;

/// Unique identifier for a connected client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ClientId(pub uuid::Uuid);

impl ClientId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

/// Selector for addressing a session by name or ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionSelector {
    Name(String),
    Id(SessionId),
}

/// Attachment mode for a client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AttachMode {
    Control,
    Observer,
}

/// Request to create a new session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateSessionRequest {
    pub name: Option<String>,
    pub command: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub size: TermSize,
}

/// IPC request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Optional, lenient protocol handshake. If sent, it must be the FIRST
    /// message on the connection; the daemon validates `version` against
    /// `PROTOCOL_VERSION` and rejects a mismatch. Clients are not required to
    /// send it.
    Hello {
        version: u32,
    },
    Ping,
    ListSessions,
    CreateSession(CreateSessionRequest),
    InspectSession {
        session: SessionSelector,
    },
    AttachSession {
        session: SessionSelector,
        size: TermSize,
        mode: AttachMode,
        client_id: ClientId,
    },
    DetachSession {
        session: SessionSelector,
        client_id: ClientId,
    },
    ResizeSession {
        session: SessionSelector,
        size: TermSize,
        client_id: ClientId,
    },
    SendInput {
        session: SessionSelector,
        data: Vec<u8>,
    },
    ReadScrollback {
        session: SessionSelector,
        lines: usize,
    },
    RenameSession {
        session: SessionSelector,
        new_name: String,
    },
    KillSession {
        session: SessionSelector,
        signal: Option<i32>,
    },
    CaptureScreen {
        session: SessionSelector,
    },
}

/// Summary of a session (used in listings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: String,
    pub status: SessionStatus,
    pub command: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub pid: Option<u32>,
    pub attached_clients: usize,
}

/// Full details of a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionDetails {
    pub id: SessionId,
    pub name: String,
    pub status: SessionStatus,
    pub command: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub last_exit_code: Option<i32>,
    pub controlling_client: Option<ClientId>,
    pub attached_clients: Vec<ClientId>,
    pub last_size: TermSize,
    pub pid: Option<u32>,
}

/// Bootstrap data sent to a client on attach.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachBootstrap {
    pub session: SessionDetails,
    pub scrollback: Vec<u8>,
    pub vt_snapshot: Option<TerminalSnapshot>,
}

/// A chunk of scrollback output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScrollbackChunk {
    pub data: Vec<u8>,
    pub lines: usize,
}

/// IPC response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Reply to a `Hello` handshake, echoing the daemon's protocol version.
    Hello {
        version: u32,
    },
    Pong,
    Ok,
    Error(RemuxError),
    SessionList(Vec<SessionSummary>),
    SessionDetails(SessionDetails),
    Created(SessionDetails),
    Attached(AttachBootstrap),
    Scrollback(ScrollbackChunk),
    Screen(TerminalSnapshot),
}

/// Server-pushed event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Output {
        session: SessionId,
        data: Vec<u8>,
    },
    StateSnapshot {
        session: SessionId,
        snapshot: TerminalSnapshot,
    },
    SessionUpdated(SessionSummary),
    SessionExited {
        session: SessionId,
        exit_code: Option<i32>,
    },
    /// Sent when a session has been asked to terminate (e.g. via `KillSession`)
    /// but the process has not yet exited. This is purely informational; the
    /// authoritative `SessionExited` (with the real exit code) follows once the
    /// PTY pump observes the process death.
    SessionTerminating {
        session: SessionId,
    },
    ControlLost {
        session: SessionId,
    },
    Error(RemuxError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{CellColor, CellData};

    fn sample_term_size() -> TermSize {
        TermSize { cols: 80, rows: 24 }
    }

    fn sample_session_id() -> SessionId {
        SessionId::new()
    }

    fn sample_client_id() -> ClientId {
        ClientId::new()
    }

    fn sample_snapshot() -> TerminalSnapshot {
        TerminalSnapshot {
            cols: 80,
            rows: 24,
            cells: vec![CellData {
                ch: 'A',
                fg: CellColor::Rgb(255, 0, 0),
                bg: CellColor::Default,
                bold: true,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strikethrough: false,
            }],
            cursor_row: 1,
            cursor_col: 2,
            alternate_screen: false,
        }
    }

    // --- JSON roundtrip tests ---

    #[test]
    fn request_hello_json_roundtrip() {
        let req = Request::Hello {
            version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, Request::Hello { version } if version == PROTOCOL_VERSION));
    }

    #[test]
    fn request_hello_bincode_roundtrip() {
        let req = Request::Hello { version: 7 };
        let bytes = bincode::serialize(&req).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back, Request::Hello { version: 7 }));
    }

    #[test]
    fn response_hello_json_roundtrip() {
        let resp = Response::Hello {
            version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, Response::Hello { version } if version == PROTOCOL_VERSION));
    }

    #[test]
    fn response_hello_bincode_roundtrip() {
        let resp = Response::Hello { version: 42 };
        let bytes = bincode::serialize(&resp).expect("serialize");
        let back: Response = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back, Response::Hello { version: 42 }));
    }

    #[test]
    fn event_session_terminating_json_roundtrip() {
        let ev = Event::SessionTerminating {
            session: sample_session_id(),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, Event::SessionTerminating { .. }));
    }

    #[test]
    fn event_session_terminating_bincode_roundtrip() {
        let ev = Event::SessionTerminating {
            session: sample_session_id(),
        };
        let bytes = bincode::serialize(&ev).expect("serialize");
        let back: Event = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back, Event::SessionTerminating { .. }));
    }

    #[test]
    fn request_ping_json_roundtrip() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, Request::Ping));
    }

    #[test]
    fn request_create_session_json_roundtrip() {
        let req = Request::CreateSession(CreateSessionRequest {
            name: Some("test-session".to_string()),
            command: vec!["bash".to_string()],
            cwd: Some(std::path::PathBuf::from("/home/user")),
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            size: sample_term_size(),
        });
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn request_attach_session_json_roundtrip() {
        let req = Request::AttachSession {
            session: SessionSelector::Name("my-session".to_string()),
            size: sample_term_size(),
            mode: AttachMode::Control,
            client_id: sample_client_id(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn request_send_input_json_roundtrip() {
        let req = Request::SendInput {
            session: SessionSelector::Id(sample_session_id()),
            data: b"ls -la\n".to_vec(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn request_kill_session_json_roundtrip() {
        let req = Request::KillSession {
            session: SessionSelector::Name("test".to_string()),
            signal: Some(9),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn request_capture_screen_json_roundtrip() {
        let req = Request::CaptureScreen {
            session: SessionSelector::Name("my-session".to_string()),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn response_screen_json_roundtrip() {
        let resp = Response::Screen(sample_snapshot());
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn response_session_list_json_roundtrip() {
        let now = chrono::Utc::now();
        let resp = Response::SessionList(vec![SessionSummary {
            id: sample_session_id(),
            name: "session-1".to_string(),
            status: SessionStatus::Running,
            command: vec!["bash".to_string()],
            cwd: std::path::PathBuf::from("/tmp"),
            created_at: now,
            pid: Some(12345),
            attached_clients: 2,
        }]);
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn response_attached_json_roundtrip() {
        let now = chrono::Utc::now();
        let sid = sample_session_id();
        let cid = sample_client_id();
        let details = SessionDetails {
            id: sid.clone(),
            name: "s".to_string(),
            status: SessionStatus::Running,
            command: vec!["vim".to_string()],
            cwd: std::path::PathBuf::from("/home"),
            created_at: now,
            updated_at: now,
            last_exit_code: None,
            controlling_client: Some(cid.clone()),
            attached_clients: vec![cid.clone()],
            last_size: sample_term_size(),
            pid: Some(999),
        };
        let snapshot = TerminalSnapshot {
            cols: 80,
            rows: 24,
            cells: vec![CellData {
                ch: 'A',
                fg: CellColor::Indexed(1),
                bg: CellColor::Default,
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strikethrough: false,
            }],
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: false,
        };
        let bootstrap = AttachBootstrap {
            session: details,
            scrollback: b"hello".to_vec(),
            vt_snapshot: Some(snapshot),
        };
        let resp = Response::Attached(bootstrap);
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn response_error_json_roundtrip() {
        let resp = Response::Error(RemuxError::SessionNotFound("nope".to_string()));
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: Response = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn event_output_json_roundtrip() {
        let sid = sample_session_id();
        let ev = Event::Output {
            session: sid,
            data: b"output text".to_vec(),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn event_session_exited_json_roundtrip() {
        let sid = sample_session_id();
        let ev = Event::SessionExited {
            session: sid,
            exit_code: Some(0),
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    // --- Bincode roundtrip tests ---

    #[test]
    fn request_ping_bincode_roundtrip() {
        let req = Request::Ping;
        let bytes = bincode::serialize(&req).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back, Request::Ping));
    }

    #[test]
    fn request_create_session_bincode_roundtrip() {
        let req = Request::CreateSession(CreateSessionRequest {
            name: Some("test-session".to_string()),
            command: vec!["bash".to_string()],
            cwd: Some(std::path::PathBuf::from("/home/user")),
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            size: sample_term_size(),
        });
        let bytes = bincode::serialize(&req).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&req).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn request_attach_session_bincode_roundtrip() {
        let req = Request::AttachSession {
            session: SessionSelector::Name("my-session".to_string()),
            size: sample_term_size(),
            mode: AttachMode::Observer,
            client_id: sample_client_id(),
        };
        let bytes = bincode::serialize(&req).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&req).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn request_capture_screen_bincode_roundtrip() {
        let req = Request::CaptureScreen {
            session: SessionSelector::Id(sample_session_id()),
        };
        let bytes = bincode::serialize(&req).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&req).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn response_screen_bincode_roundtrip() {
        let resp = Response::Screen(sample_snapshot());
        let bytes = bincode::serialize(&resp).expect("serialize");
        let back: Response = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&resp).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn response_session_list_bincode_roundtrip() {
        let now = chrono::Utc::now();
        let resp = Response::SessionList(vec![SessionSummary {
            id: sample_session_id(),
            name: "session-1".to_string(),
            status: SessionStatus::Running,
            command: vec!["bash".to_string()],
            cwd: std::path::PathBuf::from("/tmp"),
            created_at: now,
            pid: Some(12345),
            attached_clients: 2,
        }]);
        let bytes = bincode::serialize(&resp).expect("serialize");
        let back: Response = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&resp).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn response_error_bincode_roundtrip() {
        let resp = Response::Error(RemuxError::ConnectionFailed("refused".to_string()));
        let bytes = bincode::serialize(&resp).expect("serialize");
        let back: Response = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&resp).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn event_output_bincode_roundtrip() {
        let sid = sample_session_id();
        let ev = Event::Output {
            session: sid,
            data: b"hello world".to_vec(),
        };
        let bytes = bincode::serialize(&ev).expect("serialize");
        let back: Event = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&ev).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }

    #[test]
    fn event_state_snapshot_bincode_roundtrip() {
        let sid = sample_session_id();
        let snapshot = TerminalSnapshot {
            cols: 120,
            rows: 40,
            cells: vec![
                CellData {
                    ch: 'X',
                    fg: CellColor::Indexed(2),
                    bg: CellColor::Indexed(0),
                    bold: true,
                    dim: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                    strikethrough: false,
                },
                CellData {
                    ch: 'Y',
                    fg: CellColor::Default,
                    bg: CellColor::Default,
                    bold: false,
                    dim: false,
                    italic: true,
                    underline: true,
                    reverse: false,
                    strikethrough: false,
                },
            ],
            cursor_row: 5,
            cursor_col: 10,
            alternate_screen: true,
        };
        let ev = Event::StateSnapshot {
            session: sid,
            snapshot,
        };
        let bytes = bincode::serialize(&ev).expect("serialize");
        let back: Event = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            bincode::serialize(&ev).unwrap(),
            bincode::serialize(&back).unwrap()
        );
    }
}
