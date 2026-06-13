use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use remux_core::{
    AttachBootstrap, AttachMode, ClientId, Config, CreateSessionRequest, Event, RemuxError,
    ScrollbackChunk, SessionDetails, SessionId, SessionSelector, SessionStatus, SessionSummary,
    TermSize, TerminalSnapshot,
};

use crate::persistence;
use crate::pty;
use crate::scrollback::ScrollbackBuffer;
use crate::vt::VtState;

/// Handle to a live session, owned by the daemon.
pub struct SessionHandle {
    pub id: SessionId,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub status: SessionStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub last_exit_code: Option<i32>,
    pub last_size: TermSize,
    pub controlling_client: Option<ClientId>,
    pub attached_clients: Vec<ClientId>,

    /// Child process PID.
    pub pid: Option<u32>,

    /// PTY master reader. Taken by the PTY pump task on session creation.
    pub pty_reader: Option<Box<dyn std::io::Read + Send + 'static>>,
    /// PTY master writer. Kept in the session for sending input.
    pub pty_writer: Option<Box<dyn std::io::Write + Send + 'static>>,
    /// PTY child handle, for checking exit status.
    pub pty_child: Option<Box<dyn portable_pty::Child + Send + 'static>>,

    /// Portable master PTY handle for resize.
    pub master_pty: Option<Box<dyn portable_pty::MasterPty + Send + 'static>>,
    /// Virtual terminal state (alacritty_terminal).
    pub vt: Option<VtState>,

    /// Scrollback ring buffer.
    pub scrollback: ScrollbackBuffer,
    /// Partial line accumulator for scrollback line-splitting.
    pub partial_line: Vec<u8>,

    /// Event subscriber channels (one per attached client).
    pub subscribers: Vec<mpsc::Sender<Event>>,

    /// Background PTY pump task handle.
    #[allow(dead_code)]
    pub pump_handle: Option<JoinHandle<()>>,
}

/// A per-session handle behind its own lock.
///
/// The PTY output hot path clones this `Arc` once (out of the registry) and
/// thereafter locks ONLY this handle per output chunk — never the registry — so
/// independent sessions never contend on a single global mutex.
pub type SharedSessionHandle = Arc<Mutex<SessionHandle>>;

impl SessionHandle {
    /// Build a SessionSummary from this handle.
    pub fn to_summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            status: self.status,
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
            pid: self.pid,
            attached_clients: self.attached_clients.len(),
        }
    }

    /// Build SessionDetails from this handle.
    pub fn to_details(&self) -> SessionDetails {
        SessionDetails {
            id: self.id.clone(),
            name: self.name.clone(),
            status: self.status,
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_exit_code: self.last_exit_code,
            controlling_client: self.controlling_client.clone(),
            attached_clients: self.attached_clients.clone(),
            last_size: self.last_size,
            pid: self.pid,
        }
    }

    /// Take the PTY master reader out of this handle (for the PTY pump task).
    pub fn take_pty_reader(&mut self) -> Option<Box<dyn std::io::Read + Send + 'static>> {
        self.pty_reader.take()
    }

    /// Attach a client to this session. Returns bootstrap data and an event
    /// receiver. Operates purely on `self`; the caller holds only this handle's
    /// lock (never the registry lock).
    pub fn attach(
        &mut self,
        size: TermSize,
        mode: AttachMode,
        client_id: ClientId,
    ) -> Result<(AttachBootstrap, mpsc::Receiver<Event>), RemuxError> {
        if self.status == SessionStatus::Exited {
            return Err(RemuxError::SessionExited(self.last_exit_code));
        }

        if mode == AttachMode::Control {
            if let Some(ref ctrl) = self.controlling_client {
                if *ctrl == client_id {
                    return Err(RemuxError::AlreadyAttached(self.name.clone()));
                }
                // Notify the old controlling client that control was taken
                let old_client_id = ctrl.clone();
                let control_lost_event = Event::ControlLost {
                    session: self.id.clone(),
                };
                self.subscribers
                    .retain(|tx| match tx.try_send(control_lost_event.clone()) {
                        Ok(()) => true,
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    });
                tracing::info!(
                    session_id = %self.id.0,
                    old_client = ?old_client_id,
                    new_client = ?client_id,
                    "stealing control from old client"
                );
            }
            self.controlling_client = Some(client_id.clone());
        }

        if !self.attached_clients.contains(&client_id) {
            self.attached_clients.push(client_id.clone());
        }

        let (tx, rx) = mpsc::channel(256);
        self.subscribers.push(tx);

        // Resize PTY if this is the controlling client
        if mode == AttachMode::Control {
            self.last_size = size;
            if let Some(ref master) = self.master_pty {
                if let Err(e) = pty::resize_pty_master(master.as_ref(), size) {
                    tracing::warn!(
                        session_id = %self.id.0,
                        error = %e,
                        "failed to resize pty on attach"
                    );
                }
            }
            // Also resize the VT so the snapshot grid matches the client's
            // terminal dimensions (otherwise reattach paints a stale size).
            if let Some(ref mut vt) = self.vt {
                vt.resize(size);
            }
        }

        self.updated_at = chrono::Utc::now();

        let scrollback_bytes = self.scrollback.read_all_bytes();
        let details = self.to_details();
        let vt_snapshot = self.vt.as_ref().map(|vt| vt.snapshot());

        let bootstrap = AttachBootstrap {
            session: details,
            scrollback: scrollback_bytes,
            vt_snapshot,
        };

        tracing::info!(
            session_id = %self.id.0,
            client_id = ?client_id,
            mode = ?mode,
            "client attached to session"
        );

        Ok((bootstrap, rx))
    }

    /// Detach a client from this session.
    pub fn detach(&mut self, client_id: &ClientId) -> Result<(), RemuxError> {
        let was_attached = self.attached_clients.iter().any(|c| c == client_id);
        if !was_attached {
            return Err(RemuxError::NotAttached);
        }

        self.attached_clients.retain(|c| c != client_id);

        if self.controlling_client.as_ref() == Some(client_id) {
            self.controlling_client = None;
        }

        self.subscribers.retain(|tx| !tx.is_closed());
        self.updated_at = chrono::Utc::now();

        tracing::info!(
            session_id = %self.id.0,
            client_id = ?client_id,
            "client detached from session"
        );

        Ok(())
    }

    /// Resize this session's PTY (controlling client only).
    pub fn resize(&mut self, size: TermSize, client_id: &ClientId) -> Result<(), RemuxError> {
        if self.controlling_client.as_ref() != Some(client_id) {
            return Err(RemuxError::PermissionDenied);
        }

        if let Some(ref master) = self.master_pty {
            pty::resize_pty_master(master.as_ref(), size)?;
        }

        // Also resize the VT state if present
        if let Some(ref mut vt) = self.vt {
            vt.resize(size);
        }

        self.last_size = size;
        self.updated_at = chrono::Utc::now();

        tracing::debug!(
            session_id = %self.id.0,
            cols = size.cols,
            rows = size.rows,
            "resized session"
        );
        Ok(())
    }

    /// Send input data to this session's PTY.
    ///
    /// Permission rule: input is allowed when the client is the controlling
    /// client, OR when the client is not attached at all (a pure headless
    /// injector, e.g. `remux send`). Input is denied only when the client IS
    /// attached but is not the controller — i.e. an observer. This preserves
    /// the observer read-only guarantee while enabling headless injection.
    pub fn send_input(&mut self, data: &[u8], client_id: &ClientId) -> Result<(), RemuxError> {
        let is_controller = self.controlling_client.as_ref() == Some(client_id);
        let is_attached = self.attached_clients.iter().any(|c| c == client_id);
        // Deny only attached observers (attached but not controlling).
        if !is_controller && is_attached {
            return Err(RemuxError::PermissionDenied);
        }

        if self.status == SessionStatus::Exited {
            return Err(RemuxError::SessionExited(self.last_exit_code));
        }

        if let Some(ref mut writer) = self.pty_writer {
            use std::io::Write;
            writer
                .write_all(data)
                .map_err(|e| RemuxError::PtyError(format!("failed to write to pty: {e}")))?;
            writer
                .flush()
                .map_err(|e| RemuxError::PtyError(format!("failed to flush pty: {e}")))?;
        } else {
            return Err(RemuxError::SessionExited(None));
        }

        Ok(())
    }

    /// Read scrollback from this session.
    pub fn read_scrollback(&self, lines: usize) -> ScrollbackChunk {
        let line_data = self.scrollback.read_last(lines);
        let mut data = Vec::new();
        for line in &line_data {
            data.extend_from_slice(line);
            data.push(b'\n');
        }

        ScrollbackChunk {
            data,
            lines: line_data.len(),
        }
    }

    /// Capture the current screen of this session as a `TerminalSnapshot`.
    pub fn capture_screen(&self) -> Result<TerminalSnapshot, RemuxError> {
        self.vt.as_ref().map(|vt| vt.snapshot()).ok_or_else(|| {
            RemuxError::InvalidRequest(format!(
                "session {} has no terminal state to capture",
                self.name
            ))
        })
    }

    /// Send a signal to this session's process group.
    pub fn kill(&self, signal: Option<i32>) -> Result<(), RemuxError> {
        let sig = signal.unwrap_or(nix::sys::signal::Signal::SIGTERM as i32);
        let raw_pid = match self.pid {
            Some(pid) => pid,
            None => return Ok(()), // session already exited, nothing to kill
        };
        let pid = nix::unistd::Pid::from_raw(raw_pid as i32);
        let pgid = nix::unistd::getpgid(Some(pid))
            .map_err(|e| RemuxError::PtyError(format!("failed to get pgid: {e}")))?;
        let nix_signal = nix::sys::signal::Signal::try_from(sig)
            .map_err(|e| RemuxError::InvalidRequest(format!("invalid signal: {e}")))?;
        nix::sys::signal::kill(pgid, nix_signal)
            .map_err(|e| RemuxError::PtyError(format!("failed to kill: {e}")))?;

        tracing::info!(session_id = %self.id.0, signal = ?signal, "killed session");
        Ok(())
    }

    /// Append raw bytes to this session's scrollback buffer and update VT state.
    ///
    /// After feeding the bytes through the VT, drain any responses the terminal
    /// generated (replies to Device Attributes / cursor-position / device-status
    /// queries). Detached terminal-query answering: if NO client is currently
    /// attached, the daemon writes those replies back to the PTY itself so a
    /// backgrounded TUI that queried the terminal doesn't hang waiting for an
    /// answer. If a client IS attached, the responses are discarded — the
    /// client's real terminal answers the queries, and writing them here too
    /// would double-answer.
    ///
    /// This is on the PTY output hot path and operates purely on `self`, so the
    /// pump holds only this session's lock — never the registry lock.
    pub fn append_to_scrollback(&mut self, data: &[u8]) {
        self.scrollback.append_bytes(data, &mut self.partial_line);

        let responses = match self.vt {
            Some(ref mut vt) => {
                vt.process(data);
                vt.take_responses()
            }
            None => Vec::new(),
        };

        if responses.is_empty() {
            return;
        }

        // Only answer queries ourselves when no real terminal is attached.
        if self.attached_clients.is_empty() {
            if let Some(ref mut writer) = self.pty_writer {
                use std::io::Write;
                if let Err(e) = writer.write_all(&responses).and_then(|()| writer.flush()) {
                    // Robustness: never panic on a write error here. The PTY
                    // may have just exited; log at debug and move on.
                    tracing::debug!(
                        session_id = %self.id.0,
                        error = %e,
                        "failed to write detached terminal-query response to pty"
                    );
                }
            }
        }
    }

    /// Check the exit code of this session's child process.
    pub fn check_exit_code(&mut self) -> Option<i32> {
        if let Some(ref mut child) = self.pty_child {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status.exit_code() as i32),
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.id.0,
                        error = %e,
                        "failed to wait on child process"
                    );
                    return None;
                }
            }
        }
        None
    }

    /// Broadcast an event to all subscribers of this session.
    ///
    /// Backpressure / resync policy: when a subscriber's channel is `Full`, the
    /// client is lagging and would otherwise miss this event, silently
    /// corrupting its screen. Instead of dropping the event, we send a fresh
    /// `Event::StateSnapshot` built from this session's current `VtState` so the
    /// client can repaint and self-heal once it drains its backlog. The
    /// snapshot is only built on the `Full` path — never on the normal hot path.
    /// If the channel is still full even for the snapshot, we keep the
    /// subscriber (best-effort) and record a warning. `Closed` -> drop.
    ///
    /// Operates purely on `self`, so the pump broadcasts while holding only this
    /// session's lock.
    pub fn broadcast_event(&mut self, event: Event) {
        // Split the borrow so the `retain` closure can mutate the subscriber
        // list while still reading the VT for a resync snapshot.
        let session_id = &self.id;
        let subscribers = &mut self.subscribers;
        let vt = &self.vt;
        let last_size = self.last_size;

        // Lazily built only when at least one subscriber is full, so the
        // (potentially expensive) snapshot is never produced on the normal hot
        // path. Built from the live VT in this same locked session struct.
        let mut resync: Option<Event> = None;
        subscribers.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let snapshot_event = resync.get_or_insert_with(|| Event::StateSnapshot {
                    session: session_id.clone(),
                    snapshot: vt.as_ref().map(|vt| vt.snapshot()).unwrap_or_else(|| {
                        TerminalSnapshot {
                            cols: last_size.cols,
                            rows: last_size.rows,
                            cells: Vec::new(),
                            cursor_row: 0,
                            cursor_col: 0,
                            alternate_screen: false,
                        }
                    }),
                });
                match tx.try_send(snapshot_event.clone()) {
                    Ok(()) => {
                        tracing::warn!(
                            session_id = %session_id.0,
                            "subscriber lagging, sent resync snapshot"
                        );
                        true
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            session_id = %session_id.0,
                            "subscriber channel full even for resync snapshot, keeping subscriber"
                        );
                        true
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }

    /// Mark this session as exited, tearing down its live PTY/VT handles.
    ///
    /// Operates purely on `self`. Scrollback persistence (which needs `Config`)
    /// is handled separately by the caller before this is invoked.
    pub fn mark_exited(&mut self, exit_code: Option<i32>) {
        self.status = SessionStatus::Exited;
        self.last_exit_code = exit_code;
        self.updated_at = chrono::Utc::now();
        self.pty_reader = None;
        self.pty_writer = None;
        self.pty_child = None;
        self.master_pty = None;

        tracing::info!(
            session_id = %self.id.0,
            exit_code = ?exit_code,
            "session exited"
        );
    }
}

/// The session registry.
///
/// The registry (this struct) is itself protected by an outer `Mutex` (see
/// [`SharedSessionManager`]) guarding the `sessions` map and the `name_index`.
/// Each session lives behind its OWN `Mutex` ([`SharedSessionHandle`]), so the
/// PTY output hot path can lock a single session without contending on the
/// registry.
///
/// **Lock ordering (no inversion):** the only nesting allowed is
/// `registry -> handle`. Registry methods may lock a handle while the registry
/// lock is held, but the PTY pump locks ONLY a handle (after cloning its
/// `Arc<Mutex<..>>` from the registry and releasing the registry lock), so it
/// never acquires the registry lock while holding a handle lock. No two handle
/// locks are ever held simultaneously — registry iteration (e.g.
/// `list_sessions`) locks one handle, finishes with it, then moves on.
pub struct SessionManager {
    config: Config,
    sessions: HashMap<SessionId, SharedSessionHandle>,
    name_index: HashMap<String, SessionId>,
}

impl SessionManager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
            name_index: HashMap::new(),
        }
    }

    /// Resolve a SessionSelector to a SessionId.
    pub fn resolve_selector(&self, selector: &SessionSelector) -> Result<SessionId, RemuxError> {
        match selector {
            SessionSelector::Id(id) => Ok(id.clone()),
            SessionSelector::Name(name) => self
                .name_index
                .get(name)
                .cloned()
                .ok_or_else(|| RemuxError::SessionNotFound(name.clone())),
        }
    }

    /// Clone the shared handle for a resolved id. Acquires nothing on the handle
    /// itself — only clones the `Arc` — so the caller can release the registry
    /// lock before locking the handle.
    pub fn get_handle(&self, session_id: &SessionId) -> Option<SharedSessionHandle> {
        self.sessions.get(session_id).cloned()
    }

    /// Resolve a selector and clone its shared handle in one step. The registry
    /// lock is held only for the duration of this call; the caller then locks
    /// the returned handle, guaranteeing the `registry -> handle` order.
    pub fn resolve_handle(
        &self,
        selector: &SessionSelector,
    ) -> Result<SharedSessionHandle, RemuxError> {
        let id = self.resolve_selector(selector)?;
        self.sessions
            .get(&id)
            .cloned()
            .ok_or_else(|| RemuxError::SessionNotFound(format!("{id:?}")))
    }

    /// Create a new session from a create request. Returns the new id, the
    /// details, and the shared handle (so the caller can start the PTY pump
    /// from a handle clone without re-locking the registry).
    pub fn create_session(
        &mut self,
        req: CreateSessionRequest,
    ) -> Result<(SessionId, SessionDetails, SharedSessionHandle), RemuxError> {
        let name = req.name.unwrap_or_else(|| {
            req.cwd
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| format!("session-{}", self.sessions.len() + 1))
                })
        });

        if self.name_index.contains_key(&name) {
            return Err(RemuxError::SessionExists(name));
        }

        let id = SessionId::new();
        let now = chrono::Utc::now();
        let size = req.size;
        let cwd = req
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let pty_process = pty::spawn_pty(req.command.clone(), cwd.clone(), req.env.clone(), size)?;

        let pid = Some(pty_process.pid);
        let scrollback = ScrollbackBuffer::new(self.config.daemon.max_scrollback_lines);

        // Dismantle the PtyProcess into its components
        let handle = SessionHandle {
            id: id.clone(),
            name: name.clone(),
            command: req.command,
            cwd,
            status: SessionStatus::Running,
            created_at: now,
            updated_at: now,
            last_exit_code: None,
            last_size: size,
            controlling_client: None,
            attached_clients: Vec::new(),
            pid,
            pty_reader: Some(pty_process.master),
            pty_writer: Some(pty_process.writer),
            pty_child: Some(pty_process.child),
            master_pty: Some(pty_process.master_pty),
            vt: Some(VtState::new(size, self.config.daemon.max_scrollback_lines)),
            scrollback,
            partial_line: Vec::new(),
            subscribers: Vec::new(),
            pump_handle: None,
        };

        let details = handle.to_details();
        let shared = Arc::new(Mutex::new(handle));
        self.name_index.insert(name.clone(), id.clone());
        self.sessions.insert(id.clone(), shared.clone());

        // Persist session metadata
        if let Err(e) = persistence::save_session(
            &self.config,
            &persistence::PersistedSession {
                id: id.clone(),
                name,
                command: details.command.clone(),
                cwd: details.cwd.clone(),
                created_at: details.created_at,
            },
        ) {
            tracing::warn!(session_id = %id.0, error = %e, "failed to persist session metadata");
        }

        tracing::info!(session_id = %id.0, pid = ?details.pid, "created new session");

        Ok((id, details, shared))
    }

    /// List all sessions. Locks each handle one at a time (registry -> handle,
    /// never two handles at once) to build the summaries.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let handles: Vec<SharedSessionHandle> = self.sessions.values().cloned().collect();
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.lock().await.to_summary());
        }
        out
    }

    /// Rename a session, updating the registry `name_index` and the handle.
    pub async fn rename_session(
        &mut self,
        selector: &SessionSelector,
        new_name: String,
    ) -> Result<(), RemuxError> {
        if self.name_index.contains_key(&new_name) {
            return Err(RemuxError::SessionExists(new_name));
        }

        let id = self.resolve_selector(selector)?;
        let handle = match self.sessions.get(&id) {
            Some(h) => h.clone(),
            None => return Err(RemuxError::SessionNotFound(format!("{id:?}"))),
        };

        // Lock the single handle (registry lock is held by the caller of this
        // async method, but we never lock a second handle, so no inversion).
        let mut session = handle.lock().await;
        let old_name = session.name.clone();
        self.name_index.remove(&old_name);

        session.name = new_name.clone();
        session.updated_at = chrono::Utc::now();
        self.name_index.insert(new_name.clone(), id.clone());

        // Update persisted metadata
        if let Err(e) = persistence::save_session(
            &self.config,
            &persistence::PersistedSession {
                id: id.clone(),
                name: new_name,
                command: session.command.clone(),
                cwd: session.cwd.clone(),
                created_at: session.created_at,
            },
        ) {
            tracing::warn!(session_id = %id.0, error = %e, "failed to persist renamed session");
        }

        tracing::info!(session_id = %session.id.0, "renamed session");

        Ok(())
    }

    /// Persist a single session's scrollback to disk (when enabled), reading the
    /// already-locked handle. Used on the exit path before `mark_exited`.
    pub fn persist_scrollback_for(&self, session: &SessionHandle) {
        if !self.config.daemon.persist_scrollback {
            return;
        }
        let lines = session.scrollback.read_all();
        if let Err(e) = persistence::save_scrollback(&self.config, &session.id, &lines) {
            tracing::warn!(session_id = %session.id.0, error = %e, "failed to flush scrollback on exit");
        }
    }

    /// Flush every live session's scrollback to disk for crash-resilience.
    ///
    /// Called periodically by a background task when `persist_scrollback` is
    /// enabled. Each handle is locked one at a time (registry -> handle, never
    /// two handles simultaneously); the work is just cloning the line buffer and
    /// writing it — bounded by `max_scrollback_lines` per session. Only running
    /// sessions are flushed here; exited sessions were already flushed by the
    /// exit path.
    pub async fn flush_all_scrollback(&self) {
        if !self.config.daemon.persist_scrollback {
            return;
        }
        let handles: Vec<SharedSessionHandle> = self.sessions.values().cloned().collect();
        for h in handles {
            let session = h.lock().await;
            if session.status == SessionStatus::Exited {
                continue;
            }
            let lines = session.scrollback.read_all();
            if let Err(e) = persistence::save_scrollback(&self.config, &session.id, &lines) {
                tracing::warn!(session_id = %session.id.0, error = %e, "failed to flush scrollback");
            }
        }
    }

    /// Whether scrollback persistence is enabled in the daemon config.
    pub fn persist_scrollback_enabled(&self) -> bool {
        self.config.daemon.persist_scrollback
    }

    /// Recover prior sessions from disk on daemon startup (Option A).
    ///
    /// For each persisted session, reconstruct a **read-only** `SessionHandle`
    /// marked `Exited` with no PTY handles and no live VT. Scrollback is
    /// repopulated from the on-disk `.scrollback` file (when present) so
    /// `list`, `inspect`, and `logs`/`ReadScrollback` work after a restart.
    /// The underlying process is gone (its PTY died with the old daemon), so we
    /// never attempt to respawn it — recovery restores history and metadata
    /// only.
    ///
    /// Name collisions: if two persisted entries share a name, the first wins
    /// and the later one is skipped (it remains on disk but is not indexed). A
    /// recovered name can later collide with a new `create_session`, which
    /// already errors on duplicate names; that is acceptable and never panics.
    pub fn load_persisted(&mut self) {
        let persisted = persistence::load_sessions(&self.config);
        let mut recovered = 0usize;

        for meta in persisted {
            if self.sessions.contains_key(&meta.id) || self.name_index.contains_key(&meta.name) {
                tracing::warn!(
                    session_id = %meta.id.0,
                    name = %meta.name,
                    "skipping recovered session with id/name collision"
                );
                continue;
            }

            let mut scrollback = ScrollbackBuffer::new(self.config.daemon.max_scrollback_lines);
            if self.config.daemon.persist_scrollback {
                for line in persistence::load_scrollback(&self.config, &meta.id) {
                    scrollback.push(line);
                }
            }

            let handle = SessionHandle {
                id: meta.id.clone(),
                name: meta.name.clone(),
                command: meta.command,
                cwd: meta.cwd,
                // Clearly mark as ended-by-daemon-restart: Exited, no live state.
                status: SessionStatus::Exited,
                created_at: meta.created_at,
                updated_at: meta.created_at,
                last_exit_code: None,
                last_size: TermSize { cols: 80, rows: 24 },
                controlling_client: None,
                attached_clients: Vec::new(),
                pid: None,
                pty_reader: None,
                pty_writer: None,
                pty_child: None,
                master_pty: None,
                vt: None,
                scrollback,
                partial_line: Vec::new(),
                subscribers: Vec::new(),
                pump_handle: None,
            };

            self.name_index.insert(meta.name, meta.id.clone());
            self.sessions.insert(meta.id, Arc::new(Mutex::new(handle)));
            recovered += 1;
        }

        if recovered > 0 {
            tracing::info!(recovered, "recovered persisted sessions as Exited");
        }
    }
}

/// Shared session manager type used throughout the daemon.
pub type SharedSessionManager = Arc<Mutex<SessionManager>>;

#[cfg(test)]
mod tests {
    use super::*;
    use remux_core::config::{DaemonConfig, DataConfig};

    fn config_with_dir(dir: &std::path::Path, persist: bool) -> Config {
        Config {
            data: DataConfig {
                dir: dir.to_path_buf(),
            },
            daemon: DaemonConfig {
                persist_scrollback: persist,
                ..DaemonConfig::default()
            },
            ..Config::default()
        }
    }

    /// Helper mirroring the daemon's two-phase lookup: resolve + clone the
    /// handle under the registry lock, then read it under the handle lock.
    async fn details_for(
        mgr: &SessionManager,
        selector: &SessionSelector,
    ) -> Result<SessionDetails, RemuxError> {
        let handle = mgr.resolve_handle(selector)?;
        let details = handle.lock().await.to_details();
        Ok(details)
    }

    async fn scrollback_for(
        mgr: &SessionManager,
        selector: &SessionSelector,
        lines: usize,
    ) -> Result<ScrollbackChunk, RemuxError> {
        let handle = mgr.resolve_handle(selector)?;
        let chunk = handle.lock().await.read_scrollback(lines);
        Ok(chunk)
    }

    /// Simulates a daemon restart at the `SessionManager` level: a prior daemon
    /// persisted metadata + scrollback to disk; a fresh manager recovers them.
    ///
    /// This is the component-level stand-in for the end-to-end restart test
    /// (see the note in the WS4 implementation): it exercises the exact
    /// recovery path (`load_persisted`) that the daemon runs on startup,
    /// asserting Option A semantics — recovered session is `Exited`, its
    /// scrollback is readable via `read_scrollback`, and it is reachable by
    /// name and id.
    #[tokio::test]
    async fn load_persisted_recovers_exited_session_with_scrollback() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path(), true);

        // A prior daemon would have written these.
        let id = SessionId::new();
        persistence::save_session(
            &config,
            &persistence::PersistedSession {
                id: id.clone(),
                name: "recovered".to_string(),
                command: vec!["bash".to_string()],
                cwd: PathBuf::from("/tmp"),
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        persistence::save_scrollback(
            &config,
            &id,
            &[b"hello world".to_vec(), b"second line".to_vec()],
        )
        .unwrap();

        // Fresh manager (as on daemon startup) recovers from disk.
        let mut mgr = SessionManager::new(config);
        mgr.load_persisted();

        // Reachable by id and by name (name_index populated).
        let by_name = details_for(&mgr, &SessionSelector::Name("recovered".to_string()))
            .await
            .expect("recovered session reachable by name");
        assert_eq!(by_name.id, id);
        // Marked Exited with no live process.
        assert_eq!(by_name.status, SessionStatus::Exited);
        assert_eq!(by_name.pid, None);
        assert_eq!(by_name.last_exit_code, None);

        // Appears in `list`.
        assert_eq!(mgr.list_sessions().await.len(), 1);

        // Scrollback restored and readable via `logs`/ReadScrollback.
        let chunk = scrollback_for(&mgr, &SessionSelector::Id(id.clone()), 100)
            .await
            .unwrap();
        assert_eq!(chunk.lines, 2);
        assert_eq!(chunk.data, b"hello world\nsecond line\n");
    }

    /// With `persist_scrollback = false`, metadata is still recovered but no
    /// scrollback is loaded back.
    #[tokio::test]
    async fn load_persisted_without_scrollback_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path(), false);

        let id = SessionId::new();
        persistence::save_session(
            &config,
            &persistence::PersistedSession {
                id: id.clone(),
                name: "meta-only".to_string(),
                command: vec!["zsh".to_string()],
                cwd: PathBuf::from("/tmp"),
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        // Even if a scrollback file exists on disk, it must not be loaded when
        // persistence is disabled.
        persistence::save_scrollback(&config, &id, &[b"ignored".to_vec()]).unwrap();

        let mut mgr = SessionManager::new(config);
        mgr.load_persisted();

        let details = details_for(&mgr, &SessionSelector::Id(id.clone()))
            .await
            .expect("metadata recovered");
        assert_eq!(details.status, SessionStatus::Exited);

        let chunk = scrollback_for(&mgr, &SessionSelector::Id(id), 100)
            .await
            .unwrap();
        assert_eq!(chunk.lines, 0);
        assert!(chunk.data.is_empty());
    }

    /// Recovered sessions must not block; a duplicate persisted name is skipped
    /// (first wins) rather than panicking.
    #[tokio::test]
    async fn load_persisted_skips_name_collision() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path(), false);

        for _ in 0..2 {
            persistence::save_session(
                &config,
                &persistence::PersistedSession {
                    id: SessionId::new(),
                    name: "dup".to_string(),
                    command: vec!["bash".to_string()],
                    cwd: PathBuf::from("/tmp"),
                    created_at: chrono::Utc::now(),
                },
            )
            .unwrap();
        }

        let mut mgr = SessionManager::new(config);
        mgr.load_persisted();

        // Both files share a name; exactly one is indexed/recovered.
        assert_eq!(mgr.list_sessions().await.len(), 1);
        assert!(details_for(&mgr, &SessionSelector::Name("dup".to_string()))
            .await
            .is_ok());
    }

    /// Per-session locking smoke test: create several sessions and drive output
    /// + broadcast concurrently, each task holding only its own handle lock.
    /// Asserts no deadlock and that scrollback/VT updates land on the right
    /// session. Uses recovered (no-PTY) handles built directly so the test is
    /// fast and deterministic (no real processes).
    #[tokio::test]
    async fn per_session_locking_concurrent_output_no_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_dir(dir.path(), false);
        let mgr = Arc::new(Mutex::new(SessionManager::new(config)));

        // Build several no-PTY handles directly into the registry.
        let mut ids = Vec::new();
        {
            let mut m = mgr.lock().await;
            for i in 0..8 {
                let id = SessionId::new();
                let handle = SessionHandle {
                    id: id.clone(),
                    name: format!("s{i}"),
                    command: vec!["bash".to_string()],
                    cwd: PathBuf::from("/tmp"),
                    status: SessionStatus::Running,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                    last_exit_code: None,
                    last_size: TermSize { cols: 80, rows: 24 },
                    controlling_client: None,
                    attached_clients: Vec::new(),
                    pid: None,
                    pty_reader: None,
                    pty_writer: None,
                    pty_child: None,
                    master_pty: None,
                    vt: Some(VtState::new(TermSize { cols: 80, rows: 24 }, 1000)),
                    scrollback: ScrollbackBuffer::new(1000),
                    partial_line: Vec::new(),
                    subscribers: Vec::new(),
                    pump_handle: None,
                };
                m.name_index.insert(format!("s{i}"), id.clone());
                m.sessions.insert(id.clone(), Arc::new(Mutex::new(handle)));
                ids.push(id);
            }
        }

        // Each task does the two-phase lookup (registry -> handle), then drives
        // its OWN handle many times. Tasks run concurrently; if the hot path
        // touched the registry per chunk, this would serialize, but it must
        // never deadlock.
        let mut tasks = Vec::new();
        for id in &ids {
            let mgr = mgr.clone();
            let id = id.clone();
            tasks.push(tokio::spawn(async move {
                let handle = {
                    let m = mgr.lock().await;
                    m.get_handle(&id).expect("handle present")
                };
                for n in 0..200 {
                    let mut h = handle.lock().await;
                    let line = format!("line {n}\r\n");
                    h.append_to_scrollback(line.as_bytes());
                    h.broadcast_event(Event::Output {
                        session: id.clone(),
                        data: line.into_bytes(),
                    });
                }
            }));
        }

        for t in tasks {
            t.await.unwrap();
        }

        // Every session must have accumulated its scrollback independently.
        let m = mgr.lock().await;
        for id in &ids {
            let handle = m.get_handle(id).unwrap();
            let h = handle.lock().await;
            let chunk = h.read_scrollback(1000);
            assert_eq!(chunk.lines, 200, "session {:?} lost output", id.0);
        }
    }
}
