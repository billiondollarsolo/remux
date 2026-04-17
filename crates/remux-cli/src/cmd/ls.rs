use remux_core::{RemuxError, Request, Response, SessionSelector};

use crate::client::RemuxClient;
use crate::render::render_session_list;

/// Handle the `ls` command.
pub async fn run(
    client: &mut RemuxClient,
    json: bool,
    preview: bool,
) -> Result<(), RemuxError> {
    let response = client.send_request(Request::ListSessions).await?;

    match response {
        Response::SessionList(sessions) => {
            if json {
                let json_str = serde_json::to_string_pretty(&sessions)
                    .map_err(|e| RemuxError::Internal(format!("json serialization error: {e}")))?;
                println!("{json_str}");
            } else {
                render_session_list(&sessions);
            }

            // If preview mode is requested, show scrollback for each session.
            if preview && !sessions.is_empty() {
                println!();
                for session in &sessions {
                    let selector = SessionSelector::Name(session.name.clone());
                    let scrollback_response = client
                        .send_request(Request::ReadScrollback {
                            session: selector,
                            lines: 5,
                        })
                        .await?;
                    match scrollback_response {
                        Response::Scrollback(chunk) => {
                            println!("--- {} (last {} lines) ---", session.name, chunk.lines);
                            let text = String::from_utf8_lossy(&chunk.data);
                            print!("{text}");
                        }
                        Response::Error(e) => {
                            eprintln!("  Error reading scrollback for {}: {e}", session.name);
                        }
                        _ => {}
                    }
                }
            }

            Ok(())
        }
        Response::Error(e) => Err(e),
        other => Err(RemuxError::ProtocolError(format!(
            "unexpected response: {other:?}"
        ))),
    }
}
