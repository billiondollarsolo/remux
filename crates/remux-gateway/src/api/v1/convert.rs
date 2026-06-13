//! The **only** module that knows both worlds: `remux_core` protocol/session
//! types on one side and the public `/v1` DTOs on the other.
//!
//! Every string-mapping and rename decision lives here (and in the DTO layer).
//! `protocol.rs` stays oblivious to the public JSON shape. If the internal
//! protocol changes, this layer absorbs the change so the `/v1` contract holds.

use std::path::{Path, PathBuf};

use remux_core::{
    CreateSessionRequest, ScrollbackChunk, SessionDetails, SessionStatus, SessionSummary, TermSize,
    TerminalSnapshot,
};

use super::dto::{
    CreateSessionBody, ScreenView, ScrollbackView, SessionView, SizeBody, WaitResult,
};

/// Map the internal `SessionStatus` enum to the public lowercase status string.
///
/// The lowercase tokens are part of the `/v1` contract; `SessionStatus`'s
/// PascalCase serde representation never reaches a client.
pub fn status_to_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Exited => "exited",
        SessionStatus::Failed => "failed",
    }
}

/// Render a `PathBuf` as the public `cwd` string (lossy UTF-8 — paths are not
/// guaranteed UTF-8, but the JSON contract is a string).
fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl From<SessionSummary> for SessionView {
    fn from(s: SessionSummary) -> Self {
        SessionView {
            id: s.id.0.to_string(),
            name: s.name,
            status: status_to_str(s.status).to_string(),
            command: s.command,
            cwd: path_to_string(&s.cwd),
            created_at: s.created_at.to_rfc3339(),
            pid: s.pid,
            attached_clients: s.attached_clients,
            // A summary has no exit code; the field is part of the public shape
            // and is filled from `SessionDetails` when available.
            last_exit_code: None,
        }
    }
}

impl From<SessionDetails> for SessionView {
    fn from(d: SessionDetails) -> Self {
        SessionView {
            id: d.id.0.to_string(),
            name: d.name,
            status: status_to_str(d.status).to_string(),
            command: d.command,
            cwd: path_to_string(&d.cwd),
            created_at: d.created_at.to_rfc3339(),
            pid: d.pid,
            attached_clients: d.attached_clients.len(),
            last_exit_code: d.last_exit_code,
        }
    }
}

impl From<SizeBody> for TermSize {
    fn from(s: SizeBody) -> Self {
        TermSize {
            cols: s.cols,
            rows: s.rows,
        }
    }
}

impl From<TermSize> for SizeBody {
    fn from(s: TermSize) -> Self {
        SizeBody {
            cols: s.cols,
            rows: s.rows,
        }
    }
}

impl From<CreateSessionBody> for CreateSessionRequest {
    fn from(b: CreateSessionBody) -> Self {
        CreateSessionRequest {
            name: b.name,
            command: b.command,
            cwd: b.cwd.map(PathBuf::from),
            // `[name, value]` arrays -> `(name, value)` tuples.
            env: b
                .env
                .into_iter()
                .map(|[k, v]| (k, v))
                .collect::<Vec<(String, String)>>(),
            size: b.size.into(),
        }
    }
}

impl From<TerminalSnapshot> for ScreenView {
    fn from(s: TerminalSnapshot) -> Self {
        ScreenView(s)
    }
}

impl From<ScreenView> for TerminalSnapshot {
    fn from(s: ScreenView) -> Self {
        s.0
    }
}

impl From<ScrollbackChunk> for ScrollbackView {
    fn from(c: ScrollbackChunk) -> Self {
        ScrollbackView {
            text: String::from_utf8_lossy(&c.data).into_owned(),
            lines: c.lines,
        }
    }
}

/// Build the public `WaitResult` from a `(result, exit_code)` pair produced by
/// the `DaemonConn::wait` loop. Centralizes the result-string tokens so the
/// public contract (`"matched"|"idle"|"exited"|"timeout"`) is defined here.
pub fn wait_result(result: &str, exit_code: Option<i32>) -> WaitResult {
    WaitResult {
        result: result.to_string(),
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remux_core::{
        CellColor, CellData, ClientId, SessionDetails, SessionId, SessionStatus, SessionSummary,
        TermSize, TerminalSnapshot,
    };

    fn ts() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc
            .with_ymd_and_hms(2026, 6, 13, 18, 2, 11)
            .unwrap()
    }

    #[test]
    fn status_strings_are_lowercase_contract() {
        assert_eq!(status_to_str(SessionStatus::Starting), "starting");
        assert_eq!(status_to_str(SessionStatus::Running), "running");
        assert_eq!(status_to_str(SessionStatus::Exited), "exited");
        assert_eq!(status_to_str(SessionStatus::Failed), "failed");
    }

    #[test]
    fn summary_to_view_maps_promised_fields() {
        let id = SessionId::new();
        let summary = SessionSummary {
            id: id.clone(),
            name: "build".into(),
            status: SessionStatus::Running,
            command: vec!["cargo".into(), "build".into()],
            cwd: PathBuf::from("/home/mj/api"),
            created_at: ts(),
            pid: Some(48213),
            attached_clients: 2,
        };
        let view: SessionView = summary.into();
        assert_eq!(view.id, id.0.to_string());
        assert_eq!(view.name, "build");
        assert_eq!(view.status, "running");
        assert_eq!(view.command, vec!["cargo".to_string(), "build".to_string()]);
        assert_eq!(view.cwd, "/home/mj/api");
        assert_eq!(view.created_at, ts().to_rfc3339());
        assert_eq!(view.pid, Some(48213));
        assert_eq!(view.attached_clients, 2);
        assert_eq!(view.last_exit_code, None);
    }

    #[test]
    fn details_to_view_maps_exit_code_and_client_count() {
        let id = SessionId::new();
        let c1 = ClientId::new();
        let c2 = ClientId::new();
        let details = SessionDetails {
            id: id.clone(),
            name: "build".into(),
            status: SessionStatus::Exited,
            command: vec!["bash".into()],
            cwd: PathBuf::from("/tmp"),
            created_at: ts(),
            updated_at: ts(),
            last_exit_code: Some(3),
            controlling_client: Some(c1.clone()),
            attached_clients: vec![c1, c2],
            last_size: TermSize { cols: 80, rows: 24 },
            pid: Some(999),
        };
        let view: SessionView = details.into();
        assert_eq!(view.id, id.0.to_string());
        assert_eq!(view.status, "exited");
        assert_eq!(view.last_exit_code, Some(3));
        assert_eq!(view.attached_clients, 2);
        assert_eq!(view.pid, Some(999));
    }

    #[test]
    fn create_body_to_request_maps_env_and_size() {
        let body = CreateSessionBody {
            name: Some("build".into()),
            command: vec!["cargo".into(), "build".into()],
            cwd: Some("/home/mj/api".into()),
            env: vec![
                ["TERM".into(), "xterm-256color".into()],
                ["FOO".into(), "bar".into()],
            ],
            size: SizeBody {
                cols: 120,
                rows: 40,
            },
        };
        let req: CreateSessionRequest = body.into();
        assert_eq!(req.name, Some("build".to_string()));
        assert_eq!(req.command, vec!["cargo".to_string(), "build".to_string()]);
        assert_eq!(req.cwd, Some(PathBuf::from("/home/mj/api")));
        assert_eq!(
            req.env,
            vec![
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("FOO".to_string(), "bar".to_string()),
            ]
        );
        assert_eq!(
            req.size,
            TermSize {
                cols: 120,
                rows: 40
            }
        );
    }

    #[test]
    fn size_body_termsize_roundtrips_both_ways() {
        let body = SizeBody {
            cols: 100,
            rows: 30,
        };
        let ts: TermSize = body.into();
        assert_eq!(
            ts,
            TermSize {
                cols: 100,
                rows: 30
            }
        );
        let back: SizeBody = ts.into();
        assert_eq!(back, body);
    }

    #[test]
    fn snapshot_screenview_roundtrips_both_ways() {
        let snap = TerminalSnapshot {
            cols: 80,
            rows: 24,
            cells: vec![CellData {
                ch: 'A',
                fg: CellColor::Indexed(2),
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
        };
        let view: ScreenView = snap.clone().into();
        let back: TerminalSnapshot = view.into();
        assert_eq!(snap, back);
    }

    #[test]
    fn scrollback_chunk_to_view_decodes_text() {
        let chunk = ScrollbackChunk {
            data: b"hello\nworld\n".to_vec(),
            lines: 2,
        };
        let view: ScrollbackView = chunk.into();
        assert_eq!(view.text, "hello\nworld\n");
        assert_eq!(view.lines, 2);
    }
}
