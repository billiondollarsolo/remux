use remux_core::{RemuxError, Request, Response, SessionSelector};

use crate::client::RemuxClient;

/// Handle the `logs` command.
pub async fn run(client: &mut RemuxClient, name: String, lines: usize) -> Result<(), RemuxError> {
    let session = parse_selector(&name);

    let response = client
        .send_request(Request::ReadScrollback { session, lines })
        .await?;

    match response {
        Response::Scrollback(chunk) => {
            let text = String::from_utf8_lossy(&chunk.data);
            print!("{text}");
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
