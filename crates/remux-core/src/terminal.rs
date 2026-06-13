use serde::{Deserialize, Serialize};

/// Color of a terminal cell's foreground or background.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CellColor {
    /// The terminal's default color.
    Default,
    /// 16-color / 256-color palette index.
    Indexed(u8),
    /// 24-bit truecolor.
    Rgb(u8, u8, u8),
}

/// Data for a single terminal cell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellData {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strikethrough: bool,
}

/// Snapshot of the full terminal screen state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<CellData>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub alternate_screen: bool,
}
