use remux_core::{RemuxError, Request, Response, SessionSelector};

use crate::client::RemuxClient;
use crate::render::render_session_details;

/// Handle the `inspect` command.
pub async fn run(client: &mut RemuxClient, name: String, json: bool) -> Result<(), RemuxError> {
    let session = parse_selector(&name);

    let response = client
        .send_request(Request::InspectSession { session })
        .await?;

    match response {
        Response::SessionDetails(details) => {
            if json {
                let json_str = serde_json::to_string_pretty(&details)
                    .map_err(|e| RemuxError::Internal(format!("json serialization error: {e}")))?;
                println!("{json_str}");
            } else {
                render_session_details(&details);
            }
            Ok(())
        }
        Response::Error(e) => Err(e),
        other => Err(RemuxError::ProtocolError(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Parse a session name or ID into a SessionSelector.
fn parse_selector(name: &str) -> SessionSelector {
    if let Ok(uuid) = uuid::Uuid::parse_str(name) {
        SessionSelector::Id(remux_core::SessionId(uuid))
    } else {
        SessionSelector::Name(name.to_string())
    }
}
