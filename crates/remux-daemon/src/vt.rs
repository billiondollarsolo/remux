use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize as AlaTermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

use remux_core::terminal::{CellColor, CellData, TerminalSnapshot};
use remux_core::TermSize as RemuxTermSize;

/// Wrapper around alacritty_terminal's Term that processes PTY output
/// and can produce terminal snapshots for reattach.
pub struct VtState {
    term: Term<VoidListener>,
    processor: Processor,
}

impl VtState {
    pub fn new(size: RemuxTermSize, scrollback_lines: usize) -> Self {
        let term_size = AlaTermSize::new(size.cols as usize, size.rows as usize);
        let config = TermConfig {
            scrolling_history: scrollback_lines,
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
