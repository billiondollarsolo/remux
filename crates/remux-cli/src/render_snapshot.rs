//! Repaint a `TerminalSnapshot` onto the user's terminal by emitting SGR and
//! cursor-positioning escape sequences. This is the client side of "faithful
//! reattach": instead of replaying raw bytes, we reconstruct the visible screen
//! from parsed VT state.

use std::fmt::Write as _;

use remux_core::terminal::{CellColor, CellData, TerminalSnapshot};

/// Produce a byte sequence that repaints `snap` onto the user's terminal.
pub fn paint_snapshot(snap: &TerminalSnapshot) -> Vec<u8> {
    let mut out = String::new();

    // 1. If the snapshot is on the alternate screen, enter it so we don't
    //    pollute the user's primary scrollback.
    if snap.alternate_screen {
        out.push_str("\x1b[?1049h");
    }

    // 2. Clear the screen and home the cursor.
    out.push_str("\x1b[2J\x1b[H");

    let cols = snap.cols as usize;
    let rows = snap.rows as usize;

    for row in 0..rows {
        let row_start = row * cols;
        let row_cells = &snap.cells[row_start..row_start + cols];

        // Trim trailing blank cells so we don't emit full-width padding.
        let last = row_cells.iter().rposition(|c| !is_blank(c));
        let painted = match last {
            Some(idx) => &row_cells[..=idx],
            None => &[][..],
        };

        if painted.is_empty() {
            continue;
        }

        // Position cursor at the start of this row (1-based).
        let _ = write!(out, "\x1b[{};1H", row + 1);

        // Walk cells, coalescing runs with identical SGR state.
        let mut run_start = 0;
        while run_start < painted.len() {
            let mut run_end = run_start + 1;
            while run_end < painted.len() && same_sgr(&painted[run_start], &painted[run_end]) {
                run_end += 1;
            }

            out.push_str(&sgr_for(&painted[run_start]));
            for cell in &painted[run_start..run_end] {
                out.push(cell.ch);
            }

            run_start = run_end;
        }

        // Reset SGR at end of each row so attributes never bleed.
        out.push_str("\x1b[0m");
    }

    // Restore cursor visibility and position it where the snapshot says.
    out.push_str("\x1b[?25h");
    let _ = write!(
        out,
        "\x1b[{};{}H",
        snap.cursor_row as usize + 1,
        snap.cursor_col as usize + 1
    );

    out.into_bytes()
}

/// Render a snapshot as plain text: each row is its cells' `ch` joined, with
/// trailing blank cells trimmed, and rows joined by `\n`. No escape sequences
/// are emitted, so this is safe for scripting and matching.
pub fn snapshot_to_text(snap: &TerminalSnapshot) -> String {
    let cols = snap.cols as usize;
    let rows = snap.rows as usize;
    let mut out = String::new();

    for row in 0..rows {
        if row > 0 {
            out.push('\n');
        }
        let row_start = row * cols;
        let row_cells = &snap.cells[row_start..row_start + cols];

        // Trim trailing blank cells so we don't emit full-width padding.
        let last = row_cells.iter().rposition(|c| c.ch != ' ');
        if let Some(idx) = last {
            for cell in &row_cells[..=idx] {
                out.push(cell.ch);
            }
        }
    }

    out
}

/// Render a snapshot as text decorated with SGR color/attribute runs. Like
/// `snapshot_to_text`, but each run of identically-styled cells is wrapped in
/// an SGR escape, and each line ends with a reset (`\x1b[0m`). Crucially this
/// emits NO cursor-movement or clear sequences, so it is safe to pipe.
pub fn snapshot_to_ansi(snap: &TerminalSnapshot) -> String {
    let cols = snap.cols as usize;
    let rows = snap.rows as usize;
    let mut out = String::new();

    for row in 0..rows {
        if row > 0 {
            out.push('\n');
        }
        let row_start = row * cols;
        let row_cells = &snap.cells[row_start..row_start + cols];

        // Trim trailing blank cells so we don't emit full-width padding.
        let last = row_cells.iter().rposition(|c| !is_blank(c));
        let painted = match last {
            Some(idx) => &row_cells[..=idx],
            None => &[][..],
        };

        if painted.is_empty() {
            continue;
        }

        // Walk cells, coalescing runs with identical SGR state.
        let mut run_start = 0;
        while run_start < painted.len() {
            let mut run_end = run_start + 1;
            while run_end < painted.len() && same_sgr(&painted[run_start], &painted[run_end]) {
                run_end += 1;
            }
            out.push_str(&sgr_for(&painted[run_start]));
            for cell in &painted[run_start..run_end] {
                out.push(cell.ch);
            }
            run_start = run_end;
        }

        // Reset SGR at end of each line so attributes never bleed.
        out.push_str("\x1b[0m");
    }

    out
}

/// A blank cell: a space with default colors and no attributes.
fn is_blank(c: &CellData) -> bool {
    c.ch == ' '
        && c.fg == CellColor::Default
        && c.bg == CellColor::Default
        && !c.bold
        && !c.dim
        && !c.italic
        && !c.underline
        && !c.reverse
        && !c.strikethrough
}

/// Whether two cells share identical SGR state (so they can be in the same run).
fn same_sgr(a: &CellData, b: &CellData) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.dim == b.dim
        && a.italic == b.italic
        && a.underline == b.underline
        && a.reverse == b.reverse
        && a.strikethrough == b.strikethrough
}

/// Build a full SGR sequence (`\x1b[...m`) describing a cell's attributes.
/// Always starts from a reset so terminal state is never assumed.
fn sgr_for(cell: &CellData) -> String {
    let mut params: Vec<String> = vec!["0".to_string()];

    if cell.bold {
        params.push("1".to_string());
    }
    if cell.dim {
        params.push("2".to_string());
    }
    if cell.italic {
        params.push("3".to_string());
    }
    if cell.underline {
        params.push("4".to_string());
    }
    if cell.reverse {
        params.push("7".to_string());
    }
    if cell.strikethrough {
        params.push("9".to_string());
    }

    push_color(&mut params, cell.fg, false);
    push_color(&mut params, cell.bg, true);

    format!("\x1b[{}m", params.join(";"))
}

/// Append the SGR parameters for a foreground (`background = false`) or
/// background color.
fn push_color(params: &mut Vec<String>, color: CellColor, background: bool) {
    match color {
        CellColor::Default => {
            params.push(if background { "49" } else { "39" }.to_string());
        }
        CellColor::Indexed(i) => {
            if i < 8 {
                let base = if background { 40 } else { 30 };
                params.push((base + i as u16).to_string());
            } else if i < 16 {
                let base = if background { 100 } else { 90 };
                params.push((base + (i - 8) as u16).to_string());
            } else {
                params.push(if background {
                    "48;5".to_string()
                } else {
                    "38;5".to_string()
                });
                params.push(i.to_string());
            }
        }
        CellColor::Rgb(r, g, b) => {
            params.push(if background {
                "48;2".to_string()
            } else {
                "38;2".to_string()
            });
            params.push(r.to_string());
            params.push(g.to_string());
            params.push(b.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> CellData {
        CellData {
            ch: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strikethrough: false,
        }
    }

    #[test]
    fn text_trims_trailing_blanks() {
        // 5 cols x 2 rows: "Hi" + 3 blanks on row 0; "ok" + blanks on row 1.
        let mut cells = vec![blank(); 10];
        cells[0].ch = 'H';
        cells[1].ch = 'i';
        cells[5].ch = 'o';
        cells[6].ch = 'k';
        let snap = TerminalSnapshot {
            cols: 5,
            rows: 2,
            cells,
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: false,
        };
        assert_eq!(snapshot_to_text(&snap), "Hi\nok");
    }

    #[test]
    fn text_blank_row_is_empty_line() {
        // 3 cols x 2 rows, all blank.
        let snap = TerminalSnapshot {
            cols: 3,
            rows: 2,
            cells: vec![blank(); 6],
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: false,
        };
        assert_eq!(snapshot_to_text(&snap), "\n");
    }

    #[test]
    fn ansi_contains_sgr_for_colored_cell() {
        // 3 cols x 1 row: red bold 'X' then blanks.
        let mut cells = vec![blank(); 3];
        cells[0] = CellData {
            ch: 'X',
            fg: CellColor::Indexed(1),
            bg: CellColor::Default,
            bold: true,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strikethrough: false,
        };
        let snap = TerminalSnapshot {
            cols: 3,
            rows: 1,
            cells,
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: false,
        };
        let s = snapshot_to_ansi(&snap);
        // SGR for red bold (foreground 31), then the char, then a reset.
        assert!(s.contains("\x1b[0;1;31;49m"));
        assert!(s.contains('X'));
        assert!(s.ends_with("\x1b[0m"));
        // No cursor-movement or clear sequences (safe to pipe).
        assert!(!s.contains("\x1b[2J"));
        assert!(!s.contains("H\x1b["));
        assert!(!s.contains(";1H"));
    }

    #[test]
    fn red_bold_cell_golden() {
        // 3 cols x 2 rows. One red, bold 'X' at (0,0); everything else blank.
        let mut cells = vec![blank(); 6];
        cells[0] = CellData {
            ch: 'X',
            fg: CellColor::Indexed(1),
            bg: CellColor::Default,
            bold: true,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strikethrough: false,
        };
        let snap = TerminalSnapshot {
            cols: 3,
            rows: 2,
            cells,
            cursor_row: 1,
            cursor_col: 2,
            alternate_screen: false,
        };

        let bytes = paint_snapshot(&snap);
        let s = String::from_utf8(bytes).unwrap();

        // Clear + home, then row 1 positioned, SGR for red bold, the char,
        // row reset, cursor show, final cursor position.
        let expected = "\x1b[2J\x1b[H\
                        \x1b[1;1H\x1b[0;1;31;49mX\x1b[0m\
                        \x1b[?25h\x1b[2;3H";
        assert_eq!(s, expected);
    }

    #[test]
    fn alt_screen_enters_1049h() {
        let snap = TerminalSnapshot {
            cols: 2,
            rows: 1,
            cells: vec![blank(); 2],
            cursor_row: 0,
            cursor_col: 0,
            alternate_screen: true,
        };
        let s = String::from_utf8(paint_snapshot(&snap)).unwrap();
        assert!(s.contains("\x1b[?1049h"));
        assert!(s.starts_with("\x1b[?1049h"));
    }

    #[test]
    fn trailing_blanks_trimmed() {
        // 5 cols x 1 row: "Hi" followed by 3 blanks. Only "Hi" should paint.
        let mut cells = vec![blank(); 5];
        cells[0].ch = 'H';
        cells[1].ch = 'i';
        let snap = TerminalSnapshot {
            cols: 5,
            rows: 1,
            cells,
            cursor_row: 0,
            cursor_col: 2,
            alternate_screen: false,
        };
        let s = String::from_utf8(paint_snapshot(&snap)).unwrap();

        // Default-attr cells use an SGR reset run "\x1b[0;39;49m".
        let expected = "\x1b[2J\x1b[H\
                        \x1b[1;1H\x1b[0;39;49mHi\x1b[0m\
                        \x1b[?25h\x1b[1;3H";
        assert_eq!(s, expected);
        // No padding spaces emitted after "Hi".
        assert!(!s.contains("Hi   "));
    }
}
