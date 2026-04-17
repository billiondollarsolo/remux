use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use remux_core::{Request, Response, ScrollbackChunk, SessionSelector};

use crate::client::RemuxClient;
use crate::tui::Tui;
use crate::ui;

/// Main application state for the TUI session manager.
pub struct App {
    pub client: RemuxClient,
    pub sessions: Vec<remux_core::SessionSummary>,
    pub selected: usize,
    pub scrollback_preview: Option<Vec<u8>>,
    running: bool,
}

impl App {
    pub fn new(client: RemuxClient) -> Self {
        Self {
            client,
            sessions: Vec::new(),
            selected: 0,
            scrollback_preview: None,
            running: true,
        }
    }

    /// Run the main event loop.
    ///
    /// Draws the UI, polls for keyboard input, periodically refreshes
    /// the session list, and dispatches key events.
    pub async fn run(&mut self, terminal: &mut Tui) -> Result<(), Box<dyn std::error::Error>> {
        // Initial session refresh
        self.refresh_sessions().await?;

        let refresh_interval = Duration::from_secs(2);
        let mut last_refresh = std::time::Instant::now();

        while self.running {
            // Draw UI
            terminal.draw(|f| ui::draw(f, self))?;

            // Poll for events with a short timeout so we can also check refresh timing
            let poll_timeout = Duration::from_millis(100);
            if event::poll(poll_timeout)? {
                match event::read()? {
                    Event::Key(key_event) => {
                        self.handle_key_event(key_event).await?;
                    }
                    Event::Resize(_, _) => {
                        // Terminal resize is handled automatically by ratatui on next draw
                    }
                    _ => {}
                }
            }

            // Periodic session list refresh
            if last_refresh.elapsed() >= refresh_interval {
                self.refresh_sessions().await?;
                last_refresh = std::time::Instant::now();
            }
        }

        Ok(())
    }

    /// Refresh the session list from the daemon.
    async fn refresh_sessions(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        match self.client.send_request(&Request::ListSessions).await {
            Ok(Response::SessionList(sessions)) => {
                self.sessions = sessions;
                // Clamp selection to valid range
                if !self.sessions.is_empty() {
                    self.selected = self.selected.min(self.sessions.len() - 1);
                } else {
                    self.selected = 0;
                }
                // Update scrollback preview for selected session
                self.update_scrollback_preview().await?;
            }
            Ok(Response::Error(e)) => {
                // Daemon error -- keep existing sessions, don't crash
                eprintln!("Refresh error: {e}");
            }
            Ok(_) => {
                // Unexpected response type, ignore
            }
            Err(e) => {
                // Connection error -- keep existing sessions
                eprintln!("Connection error: {e}");
            }
        }
        Ok(())
    }

    /// Update the scrollback preview for the currently selected session.
    async fn update_scrollback_preview(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.sessions.is_empty() {
            self.scrollback_preview = None;
            return Ok(());
        }

        let session = &self.sessions[self.selected];
        let selector = if session.name.is_empty() {
            SessionSelector::Id(session.id.clone())
        } else {
            SessionSelector::Name(session.name.clone())
        };

        match self
            .client
            .send_request(&Request::ReadScrollback {
                session: selector,
                lines: 5,
            })
            .await
        {
            Ok(Response::Scrollback(ScrollbackChunk { data, .. })) => {
                self.scrollback_preview = Some(data);
            }
            _ => {
                self.scrollback_preview = None;
            }
        }
        Ok(())
    }

    /// Handle a keyboard event.
    async fn handle_key_event(
        &mut self,
        key: KeyEvent,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match (key.modifiers, key.code) {
            // Quit (Ctrl-Q only)
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.on_quit();
            }
            // Navigate up
            (_, KeyCode::Up) => {
                self.on_move_up();
            }
            // Navigate down
            (_, KeyCode::Down) => {
                self.on_move_down();
            }
            // Attach
            (_, KeyCode::Enter) => {
                self.on_enter().await?;
            }
            // Kill
            (_, KeyCode::Char('k')) => {
                self.on_kill().await?;
            }
            // Manual refresh
            (_, KeyCode::Char('r')) => {
                self.refresh_sessions().await?;
            }
            _ => {}
        }
        Ok(())
    }

    fn on_move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn on_move_down(&mut self) {
        if !self.sessions.is_empty() && self.selected < self.sessions.len() - 1 {
            self.selected += 1;
        }
    }

    /// Attach to the selected session.
    ///
    /// Sends an AttachSession request to the daemon. In a full implementation,
    /// this would proxy PTY I/O. For now, it verifies the session and refreshes state.
    async fn on_enter(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.sessions.is_empty() {
            return Ok(());
        }

        let session = &self.sessions[self.selected];
        let selector = if session.name.is_empty() {
            SessionSelector::Id(session.id.clone())
        } else {
            SessionSelector::Name(session.name.clone())
        };

        let client_id = remux_core::ClientId::new();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let size = remux_core::TermSize {
            cols,
            rows,
        };

        match self
            .client
            .send_request(&Request::AttachSession {
                session: selector,
                size,
                mode: remux_core::AttachMode::Control,
                client_id,
            })
            .await
        {
            Ok(Response::Attached(_bootstrap)) => {
                // Successfully attached in protocol sense.
                // In a full implementation, we would proxy I/O here.
                // For now, we just refresh the session list to show updated state.
                self.refresh_sessions().await?;
            }
            Ok(Response::Error(e)) => {
                // Show error in scrollback preview area
                self.scrollback_preview = Some(format!("Attach failed: {e}").into_bytes());
            }
            Ok(_) => {
                self.scrollback_preview = Some(b"Unexpected response".to_vec());
            }
            Err(e) => {
                self.scrollback_preview = Some(format!("Connection error: {e}").into_bytes());
            }
        }

        Ok(())
    }

    /// Kill the selected session.
    async fn on_kill(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.sessions.is_empty() {
            return Ok(());
        }

        let session = &self.sessions[self.selected];
        let selector = if session.name.is_empty() {
            SessionSelector::Id(session.id.clone())
        } else {
            SessionSelector::Name(session.name.clone())
        };

        match self
            .client
            .send_request(&Request::KillSession {
                session: selector,
                signal: None,
            })
            .await
        {
            Ok(Response::Ok) => {
                self.refresh_sessions().await?;
            }
            Ok(Response::Error(e)) => {
                self.scrollback_preview = Some(format!("Kill failed: {e}").into_bytes());
            }
            Ok(_) => {
                self.scrollback_preview = Some(b"Unexpected response".to_vec());
            }
            Err(e) => {
                self.scrollback_preview = Some(format!("Connection error: {e}").into_bytes());
            }
        }

        Ok(())
    }

    fn on_quit(&mut self) {
        self.running = false;
    }
}
