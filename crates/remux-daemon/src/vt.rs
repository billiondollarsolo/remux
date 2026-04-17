use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize as AlaTermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};

use remux_core::terminal::{CellData, TerminalSnapshot};
use remux_core::TermSize as RemuxTermSize;

/// Wrapper around alacritty_terminal's Term that processes PTY output
/// and can produce terminal snapshots for reattach.
pub struct VtState {
    term: Term<VoidListener>,
    processor: Processor,
}

impl VtState {
    pub fn new(size: RemuxTermSize) -> Self {
        let term_size = AlaTermSize::new(size.cols as usize, size.rows as usize);
        let config = TermConfig {
            scrolling_history: 10000,
            ..Default::default()
        };
        let term = Term::new(config, &term_size, VoidListener);
        let processor = Processor::new();

        Self { term, processor }
    }

    /// Feed raw PTY output bytes through the VTE processor into the terminal state.
    pub fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    /// Resize the virtual terminal to the given dimensions.
    #[allow(dead_code)]
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
                    char: if cell.c == '\0' { ' ' } else { cell.c },
                    fg: color_to_option(cell.fg),
                    bg: color_to_option(cell.bg),
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.contains(Flags::UNDERLINE),
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

fn color_to_option(color: Color) -> Option<u8> {
    match color {
        Color::Named(_) => None,
        Color::Spec(_rgb) => None,
        Color::Indexed(idx) => Some(idx),
    }
}
