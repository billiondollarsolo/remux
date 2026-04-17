use remux_core::TermSize;

/// Get the current terminal size (cols, rows).
pub fn get_terminal_size() -> TermSize {
    match crossterm::terminal::size() {
        Ok((cols, rows)) => TermSize { cols, rows },
        Err(_) => TermSize {
            cols: 80,
            rows: 24,
        },
    }
}
