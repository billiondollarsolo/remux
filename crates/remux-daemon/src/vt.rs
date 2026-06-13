use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlaEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize as AlaTermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

use remux_core::terminal::{CellColor, CellData, TerminalSnapshot};
use remux_core::TermSize as RemuxTermSize;

/// Event listener that captures the bytes the terminal wants written back to
/// the PTY (e.g. replies to Device Attributes / cursor-position queries).
///
/// `alacritty_terminal` would normally route these through the UI/PTY; with a
/// `VoidListener` they are dropped, which means a backgrounded TUI that queries
/// the terminal while detached would hang forever waiting for a reply. We
/// instead push every `Event::PtyWrite` payload into a shared buffer so the
/// daemon can answer the query itself. All other events are ignored.
///
/// `send_event` takes `&self`, so we need interior mutability. `VtState` lives
/// in a `Send` async context (behind a tokio `Mutex`), so we use
/// `Arc<Mutex<Vec<u8>>>` rather than `Rc<RefCell<..>>`.
#[derive(Clone, Default)]
pub struct ResponseCapture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl ResponseCapture {
    /// Drain the captured response bytes, leaving the buffer empty.
    fn take(&self) -> Vec<u8> {
        match self.buffer.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            // A poisoned lock would mean another thread panicked while holding
            // it; recover the data rather than propagating the panic.
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl EventListener for ResponseCapture {
    fn send_event(&self, event: AlaEvent) {
        if let AlaEvent::PtyWrite(text) = event {
            if let Ok(mut buf) = self.buffer.lock() {
                buf.extend_from_slice(text.as_bytes());
            }
        }
        // All other events (Title, Bell, Wakeup, ClipboardStore, ...) are not
        // relevant to detached query answering and are intentionally ignored.
    }
}

/// Wrapper around alacritty_terminal's Term that processes PTY output
/// and can produce terminal snapshots for reattach.
pub struct VtState {
    term: Term<ResponseCapture>,
    processor: Processor,
    /// Shared buffer of bytes the terminal wants written back to the PTY.
    responses: ResponseCapture,
}

impl VtState {
    pub fn new(size: RemuxTermSize, scrollback_lines: usize) -> Self {
        let term_size = AlaTermSize::new(size.cols as usize, size.rows as usize);
        let config = TermConfig {
            scrolling_history: scrollback_lines,
            ..Default::default()
        };
        let responses = ResponseCapture::default();
        let term = Term::new(config, &term_size, responses.clone());
        let processor = Processor::new();

        Self {
            term,
            processor,
            responses,
        }
    }

    /// Feed raw PTY output bytes through the VTE processor into the terminal state.
    pub fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    /// Drain any bytes the terminal generated in response to queries (Device
    /// Attributes, cursor-position / device-status reports, etc.) since the
    /// last call. The daemon writes these back to the PTY when no real client
    /// is attached to answer them.
    pub fn take_responses(&mut self) -> Vec<u8> {
        self.responses.take()
    }

    /// Resize the virtual terminal to the given dimensions.
    pub fn resize(&mut self, size: RemuxTermSize) {
        let term_size = AlaTermSize::new(size.cols as usize, size.rows as usize);
        self.term.resize(term_size);
    }

    /// Build a TerminalSnapshot from the current terminal state.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let grid = self.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let display_offset = grid.display_offset();

        let mut cells = Vec::with_capacity(cols * rows);

        for row in 0..rows {
            // Map visible row to grid line, accounting for scrollback display offset
            let line = Line(-(display_offset as i32) + row as i32);
            for col in 0..cols {
                let column = Column(col);
                let cell = &grid[line][column];
                let cell_data = CellData {
                    ch: if cell.c == '\0' { ' ' } else { cell.c },
                    fg: convert_color(cell.fg),
                    bg: convert_color(cell.bg),
                    bold: cell.flags.contains(Flags::BOLD),
                    dim: cell.flags.contains(Flags::DIM),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.contains(Flags::UNDERLINE),
                    reverse: cell.flags.contains(Flags::INVERSE),
                    strikethrough: cell.flags.contains(Flags::STRIKEOUT),
                };
                cells.push(cell_data);
            }
        }

        let cursor = self.term.grid().cursor.point;
        let cursor_col = cursor.column.0 as u16;
        let cursor_row = if cursor.line.0 >= 0 {
            cursor.line.0 as u16
        } else {
            (cursor.line.0 + rows as i32).max(0) as u16
        };

        TerminalSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cells,
            cursor_row,
            cursor_col,
            alternate_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
        }
    }
}

fn convert_color(color: Color) -> CellColor {
    match color {
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(idx) => CellColor::Indexed(idx),
        Color::Named(named) => match named {
            NamedColor::Black => CellColor::Indexed(0),
            NamedColor::Red => CellColor::Indexed(1),
            NamedColor::Green => CellColor::Indexed(2),
            NamedColor::Yellow => CellColor::Indexed(3),
            NamedColor::Blue => CellColor::Indexed(4),
            NamedColor::Magenta => CellColor::Indexed(5),
            NamedColor::Cyan => CellColor::Indexed(6),
            NamedColor::White => CellColor::Indexed(7),
            NamedColor::BrightBlack => CellColor::Indexed(8),
            NamedColor::BrightRed => CellColor::Indexed(9),
            NamedColor::BrightGreen => CellColor::Indexed(10),
            NamedColor::BrightYellow => CellColor::Indexed(11),
            NamedColor::BrightBlue => CellColor::Indexed(12),
            NamedColor::BrightMagenta => CellColor::Indexed(13),
            NamedColor::BrightCyan => CellColor::Indexed(14),
            NamedColor::BrightWhite => CellColor::Indexed(15),
            _ => CellColor::Default,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_vt() -> VtState {
        VtState::new(RemuxTermSize { cols: 80, rows: 24 }, 100)
    }

    #[test]
    fn primary_device_attributes_query_is_answered() {
        let mut vt = small_vt();
        // CSI c -> Primary Device Attributes request.
        vt.process(b"\x1b[c");
        let resp = vt.take_responses();
        assert!(!resp.is_empty(), "DA query should produce a reply");
        // Reply is a CSI sequence: ESC '['.
        assert_eq!(&resp[..2], b"\x1b[", "DA reply should begin with ESC [");
    }

    #[test]
    fn cursor_position_report_is_answered() {
        let mut vt = small_vt();
        // CSI 6 n -> Device Status Report (cursor position).
        vt.process(b"\x1b[6n");
        let resp = vt.take_responses();
        assert!(!resp.is_empty(), "DSR query should produce a reply");
        // The CPR reply has the form ESC [ <row> ; <col> R.
        assert!(
            resp.contains(&b'R'),
            "cursor-position report should contain 'R', got {resp:?}"
        );
        assert_eq!(&resp[..2], b"\x1b[", "CPR reply should begin with ESC [");
    }

    #[test]
    fn take_responses_drains_the_buffer() {
        let mut vt = small_vt();
        vt.process(b"\x1b[c");
        let first = vt.take_responses();
        assert!(!first.is_empty());
        // A second drain with no new query yields nothing.
        let second = vt.take_responses();
        assert!(
            second.is_empty(),
            "buffer should be drained, got {second:?}"
        );
    }

    #[test]
    fn ordinary_output_produces_no_response() {
        let mut vt = small_vt();
        vt.process(b"hello world\r\n");
        assert!(vt.take_responses().is_empty());
    }
}
