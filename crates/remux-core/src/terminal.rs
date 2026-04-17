use serde::{Deserialize, Serialize};

/// Data for a single terminal cell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellData {
    pub char: char,
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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
