use remux_core::{RemuxError, Request, Response, SessionSelector};

use crate::client::RemuxClient;
use crate::render_snapshot::{snapshot_to_ansi, snapshot_to_text};

/// Output format for `remux peek`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekFormat {
    /// Plain text: rows of `ch`, trailing blanks trimmed.
    Text,
    /// Like text, but with SGR color/attribute runs per line.
    Ansi,
    /// Pretty-printed JSON of the `TerminalSnapshot`.
    Json,
}

/// Handle the `peek` command. Captures the current screen of a session and
/// renders it as plain text (default), ANSI-colored text, or JSON.
pub async fn run(
    client: &mut RemuxClient,
    name: String,
    format: PeekFormat,
) -> Result<(), RemuxError> {
    let session = parse_selector(&name);

    let response = client
        .send_request(Request::CaptureScreen { session })
        .await?;

    let snapshot = match response {
        Response::Screen(snapshot) => snapshot,
        Response::Error(e) => return Err(e),
        other => {
            return Err(RemuxError::ProtocolError(format!(
                "unexpected response: {other:?}"
            )));
        }
    };

    match format {
        PeekFormat::Text => {
            println!("{}", snapshot_to_text(&snapshot));
        }
        PeekFormat::Ansi => {
            println!("{}", snapshot_to_ansi(&snapshot));
        }
        PeekFormat::Json => {
            let json_str = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| RemuxError::Internal(format!("json serialization error: {e}")))?;
            println!("{json_str}");
        }
    }

    Ok(())
}

/// Parse a session name or ID into a SessionSelector.
fn parse_selector(name: &str) -> SessionSelector {
    if let Ok(uuid) = uuid::Uuid::parse_str(name) {
        SessionSelector::Id(remux_core::SessionId(uuid))
    } else {
        SessionSelector::Name(name.to_string())
    }
}
