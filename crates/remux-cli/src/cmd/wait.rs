use std::time::Duration;

use regex::Regex;
use remux_core::framing::{read_message, write_message};
use remux_core::{
    AttachMode, ClientId, Event, RemuxError, Request, Response, SessionSelector, TermSize,
};
use tokio::io::BufReader;
use tokio::time::{timeout_at, Instant};

use crate::client::RemuxClient;
use crate::raw_mode::get_terminal_size;

/// The predicate `remux wait` blocks on. Exactly one is selected.
pub enum WaitPredicate {
    /// Succeed when no output arrives for the given duration.
    Idle(Duration),
    /// Succeed when the rolling decoded output buffer matches this regex.
    ForRegex(String),
    /// Succeed when the session exits; propagate the child's exit code.
    Exit,
}

/// Outcome of a wait, used both for the `--json` result string and the process
/// exit code.
enum WaitOutcome {
    Matched,
    Idle,
    Exited(Option<i32>),
    Timeout,
}

impl WaitOutcome {
    fn result_str(&self) -> &'static str {
        match self {
            WaitOutcome::Matched => "matched",
            WaitOutcome::Idle => "idle",
            WaitOutcome::Exited(_) => "exited",
            WaitOutcome::Timeout => "timeout",
        }
    }

    /// The process exit code this outcome should produce.
    fn exit_code(&self) -> i32 {
        match self {
            WaitOutcome::Matched | WaitOutcome::Idle => 0,
            WaitOutcome::Exited(code) => code.unwrap_or(0),
            WaitOutcome::Timeout => 4,
        }
    }
}

/// Handle the `wait` command. Attaches as an Observer (so it never steals
/// control) and consumes the event stream, applying the selected predicate.
///
/// On success the process exit code is returned via `Ok(code)`; the caller is
/// responsible for `process::exit`. Daemon/protocol errors are returned as
/// `Err` so `main` can map them through `exit_code_for`.
pub async fn run(
    mut client: RemuxClient,
    name: String,
    predicate: WaitPredicate,
    timeout: Option<Duration>,
    json: bool,
) -> Result<i32, RemuxError> {
    let session = parse_selector(&name);
    let size: TermSize = get_terminal_size();
    let client_id = ClientId::new();

    // Attach as an Observer so we receive the event stream without taking
    // control of the session.
    let response = client
        .send_request(Request::AttachSession {
            session: session.clone(),
            size,
            mode: AttachMode::Observer,
            client_id: client_id.clone(),
        })
        .await?;

    match response {
        Response::Attached(_) => {}
        Response::Error(e) => return Err(e),
        other => {
            return Err(RemuxError::ProtocolError(format!(
                "unexpected response: {other:?}"
            )));
        }
    }

    // Compile the regex up front so a bad pattern is a usage error, not a hang.
    let regex = match &predicate {
        WaitPredicate::ForRegex(re) => Some(
            Regex::new(re)
                .map_err(|e| RemuxError::InvalidRequest(format!("invalid regex: {e}")))?,
        ),
        _ => None,
    };

    let (read_half, write_half) = client.split();
    let mut daemon_reader = BufReader::new(read_half);
    let mut daemon_writer = write_half;

    // Overall timeout deadline (if any).
    let deadline = timeout.map(|d| Instant::now() + d);

    let outcome = wait_loop(&mut daemon_reader, &predicate, regex.as_ref(), deadline).await?;

    // Best-effort detach so the daemon doesn't keep us as a phantom observer.
    let _ = write_message(
        &mut daemon_writer,
        &Request::DetachSession { session, client_id },
    )
    .await;

    if json {
        println!(
            "{{\"result\":\"{}\",\"exit_code\":{}}}",
            outcome.result_str(),
            outcome.exit_code()
        );
    }

    Ok(outcome.exit_code())
}

/// The core event-consuming loop. Reads the daemon event stream and applies the
/// predicate, honoring the overall deadline and (for `--idle`) the idle timer.
async fn wait_loop<R>(
    daemon_reader: &mut R,
    predicate: &WaitPredicate,
    regex: Option<&Regex>,
    deadline: Option<Instant>,
) -> Result<WaitOutcome, RemuxError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Rolling decoded buffer for regex matching. Capped so a long-running
    // session doesn't grow it without bound; we only need recent output.
    const MAX_BUF: usize = 64 * 1024;
    let mut text_buf = String::new();
    let mut event_line = Vec::new();

    // For `--idle`, the next instant at which we declare idle. Reset on output.
    let idle_dur = match predicate {
        WaitPredicate::Idle(d) => Some(*d),
        _ => None,
    };
    let mut idle_at = idle_dur.map(|d| Instant::now() + d);

    loop {
        // The next wake-up is the earlier of the overall deadline and the idle
        // timer. If neither is set we block on the next event indefinitely.
        let next_tick = match (deadline, idle_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let event_result = match next_tick {
            Some(when) => {
                match timeout_at(when, read_message::<Event>(daemon_reader, &mut event_line)).await
                {
                    Ok(res) => res,
                    Err(_elapsed) => {
                        // A timer fired. Decide which one.
                        if let Some(d) = deadline {
                            if Instant::now() >= d {
                                return Ok(WaitOutcome::Timeout);
                            }
                        }
                        // Otherwise it was the idle timer.
                        if idle_at.is_some() {
                            return Ok(WaitOutcome::Idle);
                        }
                        continue;
                    }
                }
            }
            None => read_message::<Event>(daemon_reader, &mut event_line).await,
        };

        match event_result {
            Ok(Some(event)) => match event {
                Event::Output { data, .. } => {
                    // Reset the idle timer on each output chunk.
                    if let Some(d) = idle_dur {
                        idle_at = Some(Instant::now() + d);
                    }
                    if let Some(re) = regex {
                        text_buf.push_str(&String::from_utf8_lossy(&data));
                        if re.is_match(&text_buf) {
                            return Ok(WaitOutcome::Matched);
                        }
                        // Cap the rolling buffer to recent output.
                        if text_buf.len() > MAX_BUF {
                            let cut = text_buf.len() - MAX_BUF;
                            // Keep on a char boundary.
                            let mut idx = cut;
                            while idx < text_buf.len() && !text_buf.is_char_boundary(idx) {
                                idx += 1;
                            }
                            text_buf.drain(..idx);
                        }
                    }
                }
                Event::SessionExited { exit_code, .. } => {
                    if matches!(predicate, WaitPredicate::Exit) {
                        return Ok(WaitOutcome::Exited(exit_code));
                    }
                    // For idle/regex predicates, the session ending means the
                    // predicate can never be satisfied; surface the exit.
                    return Ok(WaitOutcome::Exited(exit_code));
                }
                // Other events don't affect the predicate.
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

/// Parse a session name or ID into a SessionSelector.
fn parse_selector(name: &str) -> SessionSelector {
    if let Ok(uuid) = uuid::Uuid::parse_str(name) {
        SessionSelector::Id(remux_core::SessionId(uuid))
    } else {
        SessionSelector::Name(name.to_string())
    }
}

/// Parse a small duration string. Supports `Nms` (milliseconds), `Ns`
/// (seconds), `Nm` (minutes), and a bare `N` (interpreted as seconds). Returns
/// `None` on any malformed input.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Order matters: check the two-char "ms" suffix before the single "m"/"s".
    if let Some(num) = s.strip_suffix("ms") {
        let n: u64 = num.trim().parse().ok()?;
        return Some(Duration::from_millis(n));
    }
    if let Some(num) = s.strip_suffix('s') {
        let n: u64 = num.trim().parse().ok()?;
        return Some(Duration::from_secs(n));
    }
    if let Some(num) = s.strip_suffix('m') {
        let n: u64 = num.trim().parse().ok()?;
        return Some(Duration::from_secs(n * 60));
    }
    // Bare number = seconds.
    let n: u64 = s.parse().ok()?;
    Some(Duration::from_secs(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_milliseconds() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("0ms"), Some(Duration::from_millis(0)));
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2s"), Some(Duration::from_secs(2)));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1m"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn parse_duration_bare_is_seconds() {
        assert_eq!(parse_duration("10"), Some(Duration::from_secs(10)));
    }

    #[test]
    fn parse_duration_trimmed() {
        assert_eq!(
            parse_duration("  500ms  "),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("ms"), None);
        assert_eq!(parse_duration("12x"), None);
        assert_eq!(parse_duration("1.5s"), None);
    }

    #[test]
    fn outcome_exit_codes() {
        assert_eq!(WaitOutcome::Matched.exit_code(), 0);
        assert_eq!(WaitOutcome::Idle.exit_code(), 0);
        assert_eq!(WaitOutcome::Exited(Some(7)).exit_code(), 7);
        assert_eq!(WaitOutcome::Exited(None).exit_code(), 0);
        assert_eq!(WaitOutcome::Timeout.exit_code(), 4);
    }

    #[test]
    fn outcome_result_strings() {
        assert_eq!(WaitOutcome::Matched.result_str(), "matched");
        assert_eq!(WaitOutcome::Idle.result_str(), "idle");
        assert_eq!(WaitOutcome::Exited(None).result_str(), "exited");
        assert_eq!(WaitOutcome::Timeout.result_str(), "timeout");
    }
}
