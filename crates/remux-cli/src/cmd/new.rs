use remux_core::{CreateSessionRequest, RemuxError, Request, Response, TermSize};

use crate::client::RemuxClient;
use crate::raw_mode::get_terminal_size;

/// Handle the `new` command.
pub async fn run(
    client: &mut RemuxClient,
    name: Option<String>,
    command: Vec<String>,
    json: bool,
) -> Result<(), RemuxError> {
    let session_name = name.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "session".to_string())
    });

    let cmd = if command.is_empty() {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())]
    } else {
        command
    };

    let cwd = std::env::current_dir().ok();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let size: TermSize = get_terminal_size();

    let request = Request::CreateSession(CreateSessionRequest {
        name: Some(session_name),
        command: cmd,
        cwd,
        env,
        size,
    });

    let response = client.send_request(request).await?;

    match response {
        Response::Created(details) => {
            if json {
                let json_str = serde_json::to_string_pretty(&details)
                    .map_err(|e| RemuxError::Internal(format!("json serialization error: {e}")))?;
                println!("{json_str}");
            } else {
                println!("Created session: {}", details.name);
                println!("  ID:     {}", details.id.0);
                println!("  Status: {:?}", details.status);
                println!(
                    "  Size:   {}x{}",
                    details.last_size.cols, details.last_size.rows
                );
            }
            Ok(())
        }
        Response::Error(e) => Err(e),
        other => Err(RemuxError::ProtocolError(format!(
            "unexpected response: {:?}",
            other.variant_name()
        ))),
    }
}

/// Helper to get the variant name of a Response (for error messages).
trait VariantName {
    fn variant_name(&self) -> &'static str;
}

impl VariantName for Response {
    fn variant_name(&self) -> &'static str {
        match self {
            Response::Pong => "Pong",
            Response::Ok => "Ok",
            Response::Error(_) => "Error",
            Response::SessionList(_) => "SessionList",
            Response::SessionDetails(_) => "SessionDetails",
            Response::Created(_) => "Created",
            Response::Attached(_) => "Attached",
            Response::Scrollback(_) => "Scrollback",
        }
    }
}
