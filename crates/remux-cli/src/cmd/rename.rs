use remux_core::{RemuxError, Request, Response, SessionSelector};

use crate::client::RemuxClient;

/// Handle the `rename` command.
pub async fn run(
    client: &mut RemuxClient,
    old_name: String,
    new_name: String,
) -> Result<(), RemuxError> {
    let session = parse_selector(&old_name);

    let response = client
        .send_request(Request::RenameSession {
            session,
            new_name: new_name.clone(),
        })
        .await?;

    match response {
        Response::Ok => {
            println!("Session renamed: {old_name} -> {new_name}");
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
