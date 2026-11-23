//! Login terminal messages shared by the HTTP client and PTY runner.
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum TerminalInput {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TerminalOutput {
    Bytes(Vec<u8>),
    Exited { code: u32 },
    Failed(String),
}
