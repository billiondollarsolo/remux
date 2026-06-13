//! WebSocket endpoints (AW3).
//!
//! - `GET /v1/sessions/{id}/stream` — a fully attachable terminal over **binary**
//!   frames. Server->client: terminal output bytes verbatim. Client->server:
//!   input bytes forwarded to the PTY. A **text** frame carrying
//!   `{"type":"resize","cols":N,"rows":N}` is a control message (text-vs-binary
//!   distinguishes control from raw input). On session exit the server sends a
//!   close frame. On socket close the gateway detaches cleanly.
//!
//! - `GET /v1/sessions/{id}/events` — a **structured JSON** channel for non-raw
//!   consumers. It emits typed JSON messages for lifecycle events (e.g.
//!   `{"type":"exited","exit_code":N}`, `{"type":"updated", ...}`) from an
//!   Observer subscription. Raw output stays on `/stream`; this is the
//!   differentiating structured channel.
//!
//! Auth is enforced by the `/v1` middleware (which accepts the token via the
//! `Authorization` header OR the `?token=` query param) before the upgrade.

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;

use remux_core::{Event, TermSize};

use crate::app::AppState;
use crate::convert::status_to_str;
use crate::selector::parse_selector;

/// `GET /v1/sessions/{id}/stream` — upgrade to the interactive binary terminal.
pub async fn stream_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_stream(socket, state, id).await {
            tracing::debug!(error = %e, "stream websocket ended with error");
        }
    })
}

/// `GET /v1/sessions/{id}/events` — upgrade to the structured JSON event channel.
pub async fn events_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_events(socket, state, id).await {
            tracing::debug!(error = %e, "events websocket ended with error");
        }
    })
}

/// A client->server control message on `/stream`, sent as a WS **text** frame.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlMsg {
    Resize { cols: u16, rows: u16 },
}

/// Drive the interactive `/stream` session: Control-attach to the daemon, then
/// pump daemon `Event::Output` -> binary WS frames and WS frames -> input/resize.
async fn run_stream(socket: WebSocket, state: AppState, id: String) -> Result<(), String> {
    let selector = parse_selector(&id);
    let size = TermSize { cols: 80, rows: 24 };

    let conn = state
        .connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    let (mut events, mut handle, bootstrap) = conn
        .subscribe_control(selector, size)
        .await
        .map_err(|e| format!("control attach failed: {e}"))?;

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Faithful repaint: send the VT snapshot's scrollback (the bytes the daemon
    // captured) so the client paints the current screen on connect. We send the
    // raw scrollback bytes verbatim as a binary OUTPUT frame.
    if !bootstrap.scrollback.is_empty()
        && ws_tx
            .send(Message::Binary(bootstrap.scrollback.clone()))
            .await
            .is_err()
    {
        let _ = handle.detach().await;
        return Ok(());
    }

    loop {
        tokio::select! {
            // Daemon -> client.
            ev = events.next_event() => {
                match ev {
                    Ok(Some(Event::Output { data, .. })) => {
                        if ws_tx.send(Message::Binary(data)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Some(Event::SessionExited { exit_code, .. })) => {
                        // Inform the client and close cleanly.
                        let _ = ws_tx
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: 1000,
                                reason: format!("session exited: {exit_code:?}").into(),
                            })))
                            .await;
                        break;
                    }
                    Ok(Some(_)) => { /* other events ignored on the raw stream */ }
                    Ok(None) => break, // daemon closed
                    Err(e) => {
                        tracing::debug!(error = %e, "stream: daemon event error");
                        break;
                    }
                }
            }
            // Client -> daemon.
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        if handle.send_input(bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // A text frame is a control message (e.g. resize).
                        match serde_json::from_str::<ControlMsg>(&text) {
                            Ok(ControlMsg::Resize { cols, rows }) => {
                                let _ = handle.resize(TermSize { cols, rows }).await;
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "stream: bad control frame");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => { /* ping/pong handled by axum */ }
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "stream: websocket recv error");
                        break;
                    }
                }
            }
        }
    }

    // Clean teardown: detach from the daemon.
    let _ = handle.detach().await;
    Ok(())
}

/// Drive the structured `/events` channel: Observer-attach and emit typed JSON
/// for lifecycle events. Raw output is NOT forwarded here (that's `/stream`).
async fn run_events(socket: WebSocket, state: AppState, id: String) -> Result<(), String> {
    let selector = parse_selector(&id);

    let conn = state
        .connect()
        .await
        .map_err(|e| format!("daemon connect failed: {e}"))?;
    let (mut events, mut handle) = conn
        .subscribe(selector)
        .await
        .map_err(|e| format!("observer attach failed: {e}"))?;

    let (mut ws_tx, mut ws_rx) = socket.split();

    loop {
        tokio::select! {
            ev = events.next_event() => {
                match ev {
                    Ok(Some(event)) => {
                        if let Some(json) = event_to_json(&event) {
                            let text = serde_json::to_string(&json)
                                .unwrap_or_else(|_| "{}".to_string());
                            if ws_tx.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                            // After an exit, close the channel.
                            if matches!(event, Event::SessionExited { .. }) {
                                let _ = ws_tx
                                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                        code: 1000,
                                        reason: "session exited".into(),
                                    })))
                                    .await;
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, "events: daemon event error");
                        break;
                    }
                }
            }
            // Drain client frames so a client close terminates the stream.
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    let _ = handle.detach().await;
    Ok(())
}

/// Map a daemon [`Event`] to the structured JSON the `/events` channel emits.
/// Returns `None` for events that carry no structured meaning on this channel
/// (e.g. raw `Output`, which belongs on `/stream`).
fn event_to_json(event: &Event) -> Option<serde_json::Value> {
    match event {
        Event::SessionExited { exit_code, .. } => {
            Some(json!({ "type": "exited", "exit_code": exit_code }))
        }
        Event::SessionUpdated(summary) => Some(json!({
            "type": "updated",
            "id": summary.id.0.to_string(),
            "name": summary.name,
            "status": status_to_str(summary.status),
            "attached_clients": summary.attached_clients,
        })),
        Event::SessionTerminating { .. } => Some(json!({ "type": "terminating" })),
        Event::ControlLost { .. } => Some(json!({ "type": "control_lost" })),
        Event::StateSnapshot { snapshot, .. } => {
            // The structured screen, as the public ScreenView JSON.
            Some(json!({ "type": "snapshot", "screen": snapshot }))
        }
        Event::Error(e) => Some(json!({ "type": "error", "message": e.to_string() })),
        Event::Output { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remux_core::{RemuxError, SessionId};

    #[test]
    fn control_msg_resize_parses() {
        let msg: ControlMsg =
            serde_json::from_str(r#"{"type":"resize","cols":120,"rows":40}"#).unwrap();
        match msg {
            ControlMsg::Resize { cols, rows } => {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
        }
    }

    #[test]
    fn output_event_has_no_structured_json() {
        let ev = Event::Output {
            session: SessionId::new(),
            data: b"raw".to_vec(),
        };
        assert!(event_to_json(&ev).is_none());
    }

    #[test]
    fn exited_event_json() {
        let ev = Event::SessionExited {
            session: SessionId::new(),
            exit_code: Some(3),
        };
        let v = event_to_json(&ev).unwrap();
        assert_eq!(v["type"], "exited");
        assert_eq!(v["exit_code"], 3);
    }

    #[test]
    fn error_event_json() {
        let ev = Event::Error(RemuxError::SessionNotFound("x".into()));
        let v = event_to_json(&ev).unwrap();
        assert_eq!(v["type"], "error");
        assert!(v["message"].is_string());
    }
}
