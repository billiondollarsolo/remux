//! `DaemonConn` — the gateway's adapter onto the daemon's **local Unix socket**.
//!
//! It is a thin, typed client that:
//! - opens `tokio::net::UnixStream` to the daemon socket,
//! - performs the `Hello { version: PROTOCOL_VERSION }` handshake and **refuses**
//!   (clear error) if the daemon's `Response::Hello { version }` mismatches — the
//!   gateway must never proxy a mismatched wire format,
//! - exposes typed `request()` helpers over `remux_core::framing`,
//! - exposes a `subscribe()` observer stream and a `wait()` verb *composed* from
//!   that stream (mirroring `crates/remux-cli/src/cmd/wait.rs`) — the load-bearing
//!   demonstration that a public verb is a *composition* of internal primitives,
//!   not a 1:1 request.
//!
//! The daemon stays Unix-socket-only; this connects as an ordinary client.

use std::path::Path;
use std::time::Duration;

use regex::Regex;
use remux_core::framing::{read_message, write_message};
use remux_core::{
    AttachBootstrap, AttachMode, ClientId, CreateSessionRequest, Event, RemuxError, Request,
    Response, ScrollbackChunk, SessionDetails, SessionSelector, SessionSummary, TermSize,
    TerminalSnapshot, PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixStream;
use tokio::time::{timeout_at, Instant};

use crate::error::ApiError;

/// Validate a daemon-advertised protocol version against the one this gateway
/// was built against.
///
/// Extracted as a free function so the refusal logic is unit-testable without a
/// socket: a daemon advertising `PROTOCOL_VERSION + 1` must be rejected so the
/// gateway never proxies a wire format it cannot speak.
pub fn check_protocol_version(daemon_version: u32) -> Result<(), ApiError> {
    if daemon_version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ApiError::Protocol(format!(
            "daemon protocol version {daemon_version} does not match gateway version \
             {PROTOCOL_VERSION}; refusing to proxy a mismatched wire format"
        )))
    }
}

/// The predicate a [`DaemonConn::wait`] blocks on. Mirrors the CLI's
/// `--idle` / `--for-regex` / `--exit` modes (`cmd/wait.rs`).
#[derive(Debug, Clone)]
pub enum WaitPredicate {
    /// Succeed when no output arrives for the given duration.
    Idle(Duration),
    /// Succeed when the rolling decoded output buffer matches this regex.
    Regex(String),
    /// Succeed when the session exits; propagate the child's exit code.
    Exit,
}

/// The typed outcome of a wait. The string form is the public `WaitResult.result`
/// token; the optional code is the session's exit code (for `Exit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Matched,
    Idle,
    Exited(Option<i32>),
    Timeout,
}

impl WaitOutcome {
    /// The public result string (`"matched" | "idle" | "exited" | "timeout"`).
    pub fn result_str(&self) -> &'static str {
        match self {
            WaitOutcome::Matched => "matched",
            WaitOutcome::Idle => "idle",
            WaitOutcome::Exited(_) => "exited",
            WaitOutcome::Timeout => "timeout",
        }
    }

    /// The exit code to report in the public `WaitResult` (only meaningful for
    /// an `Exited` outcome).
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            WaitOutcome::Exited(code) => *code,
            _ => None,
        }
    }
}

/// A connection to the daemon over its local Unix socket, post-handshake.
pub struct DaemonConn {
    stream: UnixStream,
    line_buf: Vec<u8>,
}

impl DaemonConn {
    /// Connect to the daemon's Unix socket and perform the version handshake.
    ///
    /// Refuses (returns [`ApiError::Protocol`]) if the daemon advertises a
    /// different `PROTOCOL_VERSION`.
    pub async fn connect(socket_path: &Path) -> Result<Self, ApiError> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            ApiError::DaemonUnavailable(format!(
                "failed to connect to daemon at {}: {e}",
                socket_path.display()
            ))
        })?;
        let mut conn = Self {
            stream,
            line_buf: Vec::new(),
        };
        conn.handshake().await?;
        Ok(conn)
    }

    /// Send `Hello`, read the daemon's `Response::Hello`, and validate the
    /// version. A `Response::Error` is surfaced; any non-`Hello` reply is a
    /// protocol error. A version mismatch refuses the connection.
    async fn handshake(&mut self) -> Result<(), ApiError> {
        write_message(
            &mut self.stream,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .await
        .map_err(ApiError::from)?;

        match read_message::<Response>(&mut self.stream, &mut self.line_buf)
            .await
            .map_err(ApiError::from)?
        {
            Some(Response::Hello { version }) => check_protocol_version(version),
            Some(Response::Error(e)) => Err(ApiError::from(e)),
            Some(other) => Err(ApiError::Protocol(format!(
                "unexpected handshake reply: {other:?}"
            ))),
            None => Err(ApiError::DaemonUnavailable(
                "daemon closed connection during handshake".to_string(),
            )),
        }
    }

    /// Send a request and read a single response. The raw primitive the typed
    /// helpers below are built on.
    pub async fn request(&mut self, request: Request) -> Result<Response, ApiError> {
        write_message(&mut self.stream, &request)
            .await
            .map_err(ApiError::from)?;
        match read_message::<Response>(&mut self.stream, &mut self.line_buf)
            .await
            .map_err(ApiError::from)?
        {
            Some(Response::Error(e)) => Err(ApiError::from(e)),
            Some(resp) => Ok(resp),
            None => Err(ApiError::DaemonUnavailable(
                "daemon closed connection".to_string(),
            )),
        }
    }

    /// List sessions.
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionSummary>, ApiError> {
        match self.request(Request::ListSessions).await? {
            Response::SessionList(list) => Ok(list),
            other => Err(unexpected("ListSessions", &other)),
        }
    }

    /// Create a session.
    pub async fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<SessionDetails, ApiError> {
        match self.request(Request::CreateSession(req)).await? {
            Response::Created(details) => Ok(details),
            other => Err(unexpected("CreateSession", &other)),
        }
    }

    /// Inspect a session.
    pub async fn inspect_session(
        &mut self,
        session: SessionSelector,
    ) -> Result<SessionDetails, ApiError> {
        match self.request(Request::InspectSession { session }).await? {
            Response::SessionDetails(details) => Ok(details),
            other => Err(unexpected("InspectSession", &other)),
        }
    }

    /// Kill a session (optionally with an explicit signal).
    pub async fn kill_session(
        &mut self,
        session: SessionSelector,
        signal: Option<i32>,
    ) -> Result<(), ApiError> {
        match self
            .request(Request::KillSession { session, signal })
            .await?
        {
            Response::Ok => Ok(()),
            other => Err(unexpected("KillSession", &other)),
        }
    }

    /// Rename a session.
    pub async fn rename_session(
        &mut self,
        session: SessionSelector,
        new_name: String,
    ) -> Result<(), ApiError> {
        match self
            .request(Request::RenameSession { session, new_name })
            .await?
        {
            Response::Ok => Ok(()),
            other => Err(unexpected("RenameSession", &other)),
        }
    }

    /// Resize a session's PTY. Requires a `client_id` (the daemon treats the
    /// resizing client as a participant).
    pub async fn resize_session(
        &mut self,
        session: SessionSelector,
        size: TermSize,
        client_id: ClientId,
    ) -> Result<(), ApiError> {
        match self
            .request(Request::ResizeSession {
                session,
                size,
                client_id,
            })
            .await?
        {
            Response::Ok => Ok(()),
            other => Err(unexpected("ResizeSession", &other)),
        }
    }

    /// Capture the current rendered screen as a structured snapshot.
    pub async fn capture_screen(
        &mut self,
        session: SessionSelector,
    ) -> Result<TerminalSnapshot, ApiError> {
        match self.request(Request::CaptureScreen { session }).await? {
            Response::Screen(snapshot) => Ok(snapshot),
            other => Err(unexpected("CaptureScreen", &other)),
        }
    }

    /// Read scrollback from a session.
    pub async fn read_scrollback(
        &mut self,
        session: SessionSelector,
        lines: usize,
    ) -> Result<ScrollbackChunk, ApiError> {
        match self
            .request(Request::ReadScrollback { session, lines })
            .await?
        {
            Response::Scrollback(chunk) => Ok(chunk),
            other => Err(unexpected("ReadScrollback", &other)),
        }
    }

    /// Send input bytes to a session's PTY. Fire-and-forget: the daemon does not
    /// reply on success (mirrors `cmd/send.rs`), so this only writes the frame.
    pub async fn send_input(
        &mut self,
        session: SessionSelector,
        data: Vec<u8>,
    ) -> Result<(), ApiError> {
        write_message(&mut self.stream, &Request::SendInput { session, data })
            .await
            .map_err(ApiError::from)
    }

    /// Attach as an **Observer** and consume the resulting event stream.
    ///
    /// Returns an [`ObserverStream`] (the read side) plus a [`SubscribeHandle`]
    /// the caller can use to detach. The connection is consumed: a subscription
    /// owns the socket for the lifetime of the stream (exactly as the CLI's
    /// `wait`/`attach` split the client).
    pub async fn subscribe(
        self,
        session: SessionSelector,
    ) -> Result<(ObserverStream, SubscribeHandle), ApiError> {
        let size = TermSize { cols: 80, rows: 24 };
        let (stream, handle, _bootstrap) = self.attach(session, size, AttachMode::Observer).await?;
        Ok((stream, handle))
    }

    /// Attach as a **Control** client and consume the resulting event stream.
    ///
    /// This is the load-bearing primitive for the WebSocket `/stream` endpoint:
    /// unlike [`Self::subscribe`] (read-only Observer), a Control attach makes
    /// this connection the controlling client, so input forwarded via the
    /// returned [`SubscribeHandle::send_input`] and resizes via
    /// [`SubscribeHandle::resize`] are accepted by the daemon (they carry the
    /// same `client_id` that holds control).
    ///
    /// Returns the [`ObserverStream`], the [`SubscribeHandle`] (write/control
    /// side), and the [`AttachBootstrap`] so the caller can repaint the screen
    /// faithfully on connect.
    pub async fn subscribe_control(
        self,
        session: SessionSelector,
        size: TermSize,
    ) -> Result<(ObserverStream, SubscribeHandle, AttachBootstrap), ApiError> {
        self.attach(session, size, AttachMode::Control).await
    }

    /// Shared attach implementation for Observer/Control subscriptions.
    async fn attach(
        mut self,
        session: SessionSelector,
        size: TermSize,
        mode: AttachMode,
    ) -> Result<(ObserverStream, SubscribeHandle, AttachBootstrap), ApiError> {
        let client_id = ClientId::new();
        let resp = self
            .request(Request::AttachSession {
                session: session.clone(),
                size,
                mode: mode.clone(),
                client_id: client_id.clone(),
            })
            .await?;
        let bootstrap = match resp {
            Response::Attached(b) => b,
            other => {
                let what = match mode {
                    AttachMode::Control => "AttachSession{Control}",
                    AttachMode::Observer => "AttachSession{Observer}",
                };
                return Err(unexpected(what, &other));
            }
        };

        let line_buf = self.line_buf;
        let (read_half, write_half) = tokio::io::split(self.stream);
        let stream = ObserverStream {
            reader: BufReader::new(Box::new(read_half)),
            event_line: line_buf,
        };
        let handle = SubscribeHandle {
            writer: Box::new(write_half),
            session,
            client_id,
        };
        Ok((stream, handle, bootstrap))
    }

    /// Wait on semantic state, composed from the observer stream + a predicate.
    ///
    /// This is a faithful re-implementation of `cmd/wait.rs`: it attaches as an
    /// Observer (never stealing control), runs the predicate over the event
    /// stream honoring an optional overall timeout, and best-effort detaches.
    /// It is the clearest example that a public verb is **not** a single internal
    /// request — `wait` has no `Request::Wait`.
    pub async fn wait(
        self,
        session: SessionSelector,
        predicate: WaitPredicate,
        timeout: Option<Duration>,
    ) -> Result<WaitOutcome, ApiError> {
        // Compile the regex up front so a bad pattern is a clear bad-request
        // error, not a hang.
        let regex = match &predicate {
            WaitPredicate::Regex(re) => Some(
                Regex::new(re).map_err(|e| ApiError::BadRequest(format!("invalid regex: {e}")))?,
            ),
            _ => None,
        };

        let (mut stream, mut handle) = self.subscribe(session).await?;

        let deadline = timeout.map(|d| Instant::now() + d);
        let outcome = wait_loop(&mut stream, &predicate, regex.as_ref(), deadline).await?;

        // Best-effort detach so the daemon doesn't keep us as a phantom observer.
        let _ = handle.detach().await;

        Ok(outcome)
    }
}

/// Build a clear protocol error for an unexpected response to a typed helper.
fn unexpected(op: &str, resp: &Response) -> ApiError {
    ApiError::Protocol(format!("unexpected response to {op}: {resp:?}"))
}

/// The read side of an observer subscription: a typed stream of [`Event`]s.
pub struct ObserverStream {
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    event_line: Vec<u8>,
}

impl ObserverStream {
    /// Read the next event, or `Ok(None)` if the daemon closed the connection.
    ///
    /// On a **Control** attach the same connection also carries the `Response::Ok`
    /// reply to a `ResizeSession` request issued over the control side. That frame
    /// is not an [`Event`]; it is silently skipped here (bounded) so the resize
    /// control path does not corrupt the event stream.
    pub async fn next_event(&mut self) -> Result<Option<Event>, ApiError> {
        // Bound the skip so a genuinely broken stream cannot spin forever.
        for _ in 0..64 {
            match read_message::<Event>(&mut self.reader, &mut self.event_line).await {
                Ok(Some(ev)) => return Ok(Some(ev)),
                Ok(None) => return Ok(None),
                // A non-Event frame (e.g. a `Response::Ok` resize ack) — skip it.
                Err(RemuxError::ProtocolError(_)) => continue,
                Err(e) => return Err(ApiError::from(e)),
            }
        }
        Err(ApiError::Protocol(
            "too many non-event frames on the observer stream".to_string(),
        ))
    }
}

/// The write/control side of an observer subscription, used to detach.
pub struct SubscribeHandle {
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    session: SessionSelector,
    client_id: ClientId,
}

impl SubscribeHandle {
    /// Detach this observer from the session (best-effort cleanup).
    pub async fn detach(&mut self) -> Result<(), ApiError> {
        write_message(
            &mut self.writer,
            &Request::DetachSession {
                session: self.session.clone(),
                client_id: self.client_id.clone(),
            },
        )
        .await
        .map_err(ApiError::from)
    }

    /// Forward raw input bytes to the session over this (control) connection.
    ///
    /// Fire-and-forget, matching the daemon's `SendInput` (no reply). Sent on the
    /// same connection that holds control, so a Control attach's input is accepted.
    pub async fn send_input(&mut self, data: Vec<u8>) -> Result<(), ApiError> {
        write_message(
            &mut self.writer,
            &Request::SendInput {
                session: self.session.clone(),
                data,
            },
        )
        .await
        .map_err(ApiError::from)
    }

    /// Resize the session's PTY over this (control) connection.
    ///
    /// Fire-and-forget over the control connection; the daemon would reply `Ok`,
    /// but on a control-attached connection the reply interleaves with the event
    /// stream, so the read side ([`ObserverStream`]) is where any reply lands.
    /// For the `/stream` use case the resize is advisory and we do not block on a
    /// reply.
    pub async fn resize(&mut self, size: TermSize) -> Result<(), ApiError> {
        write_message(
            &mut self.writer,
            &Request::ResizeSession {
                session: self.session.clone(),
                size,
                client_id: self.client_id.clone(),
            },
        )
        .await
        .map_err(ApiError::from)
    }
}

/// The core event-consuming loop, factored to take any [`ObserverStream`].
/// A self-contained copy of `cmd/wait.rs`'s `wait_loop`, adapted to the
/// gateway's `WaitPredicate`/`WaitOutcome` and event stream.
async fn wait_loop(
    stream: &mut ObserverStream,
    predicate: &WaitPredicate,
    regex: Option<&Regex>,
    deadline: Option<Instant>,
) -> Result<WaitOutcome, ApiError> {
    // Rolling decoded buffer for regex matching, capped to recent output.
    const MAX_BUF: usize = 64 * 1024;
    let mut text_buf = String::new();

    let idle_dur = match predicate {
        WaitPredicate::Idle(d) => Some(*d),
        _ => None,
    };
    let mut idle_at = idle_dur.map(|d| Instant::now() + d);

    loop {
        // The next wake-up is the earlier of the overall deadline and the idle
        // timer; if neither is set we block on the next event indefinitely.
        let next_tick = match (deadline, idle_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let event_result = match next_tick {
            Some(when) => match timeout_at(when, stream.next_event()).await {
                Ok(res) => res,
                Err(_elapsed) => {
                    // A timer fired. Decide which one.
                    if let Some(d) = deadline {
                        if Instant::now() >= d {
                            return Ok(WaitOutcome::Timeout);
                        }
                    }
                    if idle_at.is_some() {
                        return Ok(WaitOutcome::Idle);
                    }
                    continue;
                }
            },
            None => stream.next_event().await,
        };

        match event_result {
            Ok(Some(event)) => match event {
                Event::Output { data, .. } => {
                    if let Some(d) = idle_dur {
                        idle_at = Some(Instant::now() + d);
                    }
                    if let Some(re) = regex {
                        text_buf.push_str(&String::from_utf8_lossy(&data));
                        if re.is_match(&text_buf) {
                            return Ok(WaitOutcome::Matched);
                        }
                        if text_buf.len() > MAX_BUF {
                            let cut = text_buf.len() - MAX_BUF;
                            let mut idx = cut;
                            while idx < text_buf.len() && !text_buf.is_char_boundary(idx) {
                                idx += 1;
                            }
                            text_buf.drain(..idx);
                        }
                    }
                }
                Event::SessionExited { exit_code, .. } => {
                    // For any predicate, the session ending surfaces the exit:
                    // for `Exit` it's the success outcome; for idle/regex the
                    // predicate can never be satisfied once the session is gone.
                    return Ok(WaitOutcome::Exited(exit_code));
                }
                _ => {}
            },
            Ok(None) => {
                // Daemon disconnected. Treat as exit with unknown code.
                return Ok(WaitOutcome::Exited(None));
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_protocol_version_accepts_matching() {
        assert!(check_protocol_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn check_protocol_version_refuses_mismatch() {
        let err = check_protocol_version(PROTOCOL_VERSION + 1)
            .expect_err("a higher version must be refused");
        // It must be a clear protocol error mapping to 502, not silently proxied.
        assert!(matches!(err, ApiError::Protocol(_)));
        assert_eq!(err.http_status(), 502);
        let msg = err.to_string();
        assert!(msg.contains("does not match"), "message was: {msg}");
    }

    #[test]
    fn check_protocol_version_refuses_arbitrary_other() {
        // Any version other than the gateway's own is refused.
        assert!(check_protocol_version(PROTOCOL_VERSION.wrapping_add(7)).is_err());
    }

    #[test]
    fn wait_outcome_result_strings() {
        assert_eq!(WaitOutcome::Matched.result_str(), "matched");
        assert_eq!(WaitOutcome::Idle.result_str(), "idle");
        assert_eq!(WaitOutcome::Exited(Some(7)).result_str(), "exited");
        assert_eq!(WaitOutcome::Timeout.result_str(), "timeout");
    }

    #[test]
    fn wait_outcome_exit_codes() {
        assert_eq!(WaitOutcome::Exited(Some(7)).exit_code(), Some(7));
        assert_eq!(WaitOutcome::Exited(None).exit_code(), None);
        assert_eq!(WaitOutcome::Matched.exit_code(), None);
        assert_eq!(WaitOutcome::Timeout.exit_code(), None);
    }
}
