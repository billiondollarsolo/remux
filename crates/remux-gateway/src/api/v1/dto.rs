//! Public `/v1` DTOs — **stable, JSON-shaped, and independent of**
//! `remux_core::protocol`.
//!
//! Conventions (the public JSON contract):
//! - `id` is a uuid rendered as a **string** (not a tagged enum).
//! - timestamps are **RFC3339 strings** (`created_at`).
//! - `status` is a **lowercase string** (`"running" | "exited" | "starting" |
//!   "failed"`), never the internal `SessionStatus` enum's PascalCase.
//! - tagged enums (`WaitBody`) use a `kind` discriminator with `snake_case`
//!   variant names.
//!
//! Every `serde(rename)` / string-mapping decision lives in this layer (and its
//! sibling [`super::convert`]). `protocol.rs` knows nothing about these shapes.

use remux_core::TerminalSnapshot;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Default terminal size used when a `CreateSessionBody` omits `size`.
pub fn default_size() -> SizeBody {
    SizeBody { cols: 80, rows: 24 }
}

/// Public view of a session (the shape returned by list/create/inspect).
///
/// Independent of `protocol::SessionSummary`/`SessionDetails`: uuid and
/// timestamp are strings, and `status` is a lowercase token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SessionView {
    /// uuid as string (stable JSON).
    #[schema(example = "5f3c0000-0000-0000-0000-000000000000")]
    pub id: String,
    #[schema(example = "build")]
    pub name: String,
    /// `"running" | "exited" | "starting" | "failed"`.
    #[schema(example = "running")]
    pub status: String,
    pub command: Vec<String>,
    pub cwd: String,
    /// RFC3339 timestamp.
    #[schema(example = "2026-06-13T18:02:11+00:00")]
    pub created_at: String,
    pub pid: Option<u32>,
    pub attached_clients: usize,
    pub last_exit_code: Option<i32>,
}

/// Terminal dimensions in the public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SizeBody {
    #[schema(example = 80)]
    pub cols: u16,
    #[schema(example = 24)]
    pub rows: u16,
}

impl Default for SizeBody {
    fn default() -> Self {
        default_size()
    }
}

/// Request body for `POST /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionBody {
    #[serde(default)]
    pub name: Option<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment as `[name, value]` pairs (JSON-friendly; avoids a map so
    /// duplicate keys / ordering are explicit).
    #[serde(default)]
    pub env: Vec<[String; 2]>,
    #[serde(default = "default_size")]
    pub size: SizeBody,
}

/// Request body for `POST /v1/sessions/{id}/resize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResizeBody {
    pub cols: u16,
    pub rows: u16,
}

/// Predicate for `POST /v1/sessions/{id}/wait`.
///
/// Tagged on `kind` with `snake_case` variant names, mirroring the CLI's
/// `--idle` / `--for-regex` / `--exit` modes (`cmd/wait.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitBody {
    /// Succeed when no output arrives for `ms` milliseconds.
    Idle { ms: u64 },
    /// Succeed when the rolling decoded output buffer matches `pattern`.
    Regex { pattern: String },
    /// Succeed when the session exits (propagating its exit code).
    Exit,
}

/// Result of a wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct WaitResult {
    /// `"matched" | "idle" | "exited" | "timeout"`.
    #[schema(example = "matched")]
    pub result: String,
    pub exit_code: Option<i32>,
}

/// The structured-screen contract. This is the existing `TerminalSnapshot`
/// shape (already `Serialize`) re-exposed under a gateway-owned public newtype.
///
/// The coupling to `TerminalSnapshot` is *deliberate and documented* (see the
/// plan §12, "snapshot-as-`ScreenView`"): the per-cell color / cursor /
/// alt-screen data **is** the differentiator's payload. It is pinned under
/// `/v1`; if `terminal.rs` changes shape, `/v1` version-bumps. `#[serde(transparent)]`
/// keeps the JSON identical to the inner snapshot (cols/rows/cells/cursor/…).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScreenView(pub TerminalSnapshot);

/// Manual schema for [`ScreenView`]: the inner `TerminalSnapshot` lives in
/// `remux-core` (which has no `utoipa` dependency), so we describe the public
/// structured-screen contract here by hand. It documents the grid metadata and
/// the per-cell shape (the differentiator's payload) without coupling
/// `remux-core` to `utoipa`.
impl utoipa::ToSchema for ScreenView {}

impl utoipa::PartialSchema for ScreenView {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};
        use utoipa::openapi::{KnownFormat, SchemaFormat};

        let u16_prop = || {
            ObjectBuilder::new()
                .schema_type(Type::Integer)
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32)))
                .build()
        };
        let cell = ObjectBuilder::new()
            .schema_type(Type::Object)
            .description(Some(
                "A single rendered cell: its character plus fg/bg color and SGR \
                 attributes (the structured per-cell payload).",
            ))
            .property("ch", ObjectBuilder::new().schema_type(Type::String).build())
            .property("fg", ObjectBuilder::new().build())
            .property("bg", ObjectBuilder::new().build())
            .property(
                "bold",
                ObjectBuilder::new().schema_type(Type::Boolean).build(),
            )
            .build();

        let obj = ObjectBuilder::new()
            .schema_type(Type::Object)
            .description(Some(
                "The structured screen snapshot: grid dimensions, cursor \
                 position, alt-screen flag, and the flat per-cell grid. Pinned \
                 under /v1 (mirrors remux-core's TerminalSnapshot).",
            ))
            .property("cols", u16_prop())
            .property("rows", u16_prop())
            .property("cursor_row", u16_prop())
            .property("cursor_col", u16_prop())
            .property(
                "alternate_screen",
                ObjectBuilder::new().schema_type(Type::Boolean).build(),
            )
            .property(
                "cells",
                utoipa::openapi::schema::ArrayBuilder::new()
                    .items(cell)
                    .build(),
            )
            .build();
        utoipa::openapi::RefOr::T(Schema::Object(obj))
    }
}

/// A chunk of scrollback, as the public contract exposes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScrollbackView {
    /// Decoded text (lossy UTF-8). Raw-byte access is an AW2 content-type
    /// concern; the DTO carries decoded text for the JSON path.
    pub text: String,
    pub lines: usize,
}

/// JSON body for `POST /v1/sessions/{id}/input` (the `application/json` form).
///
/// Exactly one of `text` (with `\n \t \r \\` interpreted) or `bytes_hex`
/// (e.g. `"1b5b41"`). Raw binary may also be POSTed with a non-JSON content type
/// and a raw body. This DTO exists so the input contract is part of the OpenAPI
/// spec; the handler parses the same shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InputBody {
    /// Text input; only `\n \t \r \\` escapes are interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "echo hello\\n")]
    pub text: Option<String>,
    /// Hex-encoded raw bytes (e.g. `"1b5b41"` for an up-arrow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_hex: Option<String>,
}

/// Request body for `PATCH /v1/sessions/{id}` (rename).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RenameBody {
    #[schema(example = "new-name")]
    pub name: String,
}

/// The consistent JSON error body the gateway returns on any error
/// (`{ "error": "...", "kind": "..." }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApiErrorBody {
    /// Human-readable message.
    #[schema(example = "not found: no such session")]
    pub error: String,
    /// Stable machine-readable category (e.g. `not_found`, `forbidden`,
    /// `bad_request`, `unauthorized`, `daemon_unavailable`).
    #[schema(example = "not_found")]
    pub kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use remux_core::{CellColor, CellData, TerminalSnapshot};

    fn sample_snapshot() -> TerminalSnapshot {
        TerminalSnapshot {
            cols: 80,
            rows: 24,
            cells: vec![CellData {
                ch: 'A',
                fg: CellColor::Rgb(0, 255, 0),
                bg: CellColor::Default,
                bold: true,
                dim: false,
                italic: false,
                underline: false,
                reverse: false,
                strikethrough: false,
            }],
            cursor_row: 7,
            cursor_col: 12,
            alternate_screen: false,
        }
    }

    #[test]
    fn session_view_json_roundtrip() {
        let v = SessionView {
            id: "5f3c0000-0000-0000-0000-000000000000".to_string(),
            name: "build".to_string(),
            status: "running".to_string(),
            command: vec!["cargo".to_string(), "build".to_string()],
            cwd: "/home/mj/api".to_string(),
            created_at: "2026-06-13T18:02:11+00:00".to_string(),
            pid: Some(48213),
            attached_clients: 0,
            last_exit_code: None,
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: SessionView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, back);
    }

    #[test]
    fn session_view_status_is_lowercase_string() {
        let v = SessionView {
            id: "x".into(),
            name: "n".into(),
            status: "exited".into(),
            command: vec![],
            cwd: "/".into(),
            created_at: "2026-06-13T18:02:11+00:00".into(),
            pid: None,
            attached_clients: 0,
            last_exit_code: Some(0),
        };
        let value: serde_json::Value = serde_json::to_value(&v).unwrap();
        assert_eq!(value["status"], serde_json::json!("exited"));
        assert!(value["id"].is_string());
        assert!(value["created_at"].is_string());
    }

    #[test]
    fn create_session_body_defaults() {
        // Only `command` is required; size/env/cwd/name default.
        let json = r#"{ "command": ["bash"] }"#;
        let body: CreateSessionBody = serde_json::from_str(json).expect("deserialize");
        assert_eq!(body.command, vec!["bash".to_string()]);
        assert_eq!(body.name, None);
        assert_eq!(body.cwd, None);
        assert!(body.env.is_empty());
        assert_eq!(body.size, default_size());
    }

    #[test]
    fn create_session_body_full_roundtrip() {
        let body = CreateSessionBody {
            name: Some("build".into()),
            command: vec!["cargo".into(), "build".into()],
            cwd: Some("/home/mj/api".into()),
            env: vec![["TERM".into(), "xterm-256color".into()]],
            size: SizeBody {
                cols: 120,
                rows: 40,
            },
        };
        let json = serde_json::to_string(&body).unwrap();
        let back: CreateSessionBody = serde_json::from_str(&json).unwrap();
        assert_eq!(body, back);
    }

    #[test]
    fn size_body_default() {
        assert_eq!(SizeBody::default(), SizeBody { cols: 80, rows: 24 });
    }

    #[test]
    fn wait_body_tagged_roundtrip() {
        let cases = [
            (WaitBody::Idle { ms: 500 }, r#"{"kind":"idle","ms":500}"#),
            (
                WaitBody::Regex {
                    pattern: "ok|FAIL".into(),
                },
                r#"{"kind":"regex","pattern":"ok|FAIL"}"#,
            ),
            (WaitBody::Exit, r#"{"kind":"exit"}"#),
        ];
        for (body, expected_json) in cases {
            let json = serde_json::to_string(&body).unwrap();
            assert_eq!(json, expected_json);
            let back: WaitBody = serde_json::from_str(&json).unwrap();
            assert_eq!(body, back);
        }
    }

    #[test]
    fn wait_result_roundtrip() {
        let r = WaitResult {
            result: "matched".into(),
            exit_code: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"result":"matched","exit_code":null}"#);
        let back: WaitResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn screen_view_is_transparent_over_snapshot() {
        let snap = sample_snapshot();
        let view = ScreenView(snap.clone());
        // The wrapper serializes identically to the raw snapshot.
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::to_value(&snap).unwrap()
        );
        let back: ScreenView =
            serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
        assert_eq!(view, back);
        // Spot-check the structured-cell contract is preserved.
        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(value["cursor_row"], serde_json::json!(7));
        assert_eq!(value["cursor_col"], serde_json::json!(12));
        assert_eq!(
            value["cells"][0]["fg"]["Rgb"],
            serde_json::json!([0, 255, 0])
        );
    }

    #[test]
    fn scrollback_view_roundtrip() {
        let v = ScrollbackView {
            text: "line1\nline2\n".into(),
            lines: 2,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: ScrollbackView = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}
