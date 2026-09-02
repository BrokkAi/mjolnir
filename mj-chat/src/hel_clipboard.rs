//! Small cross-platform clipboard boundary shared by terminal text editors.

use std::sync::Mutex;

use anyhow::{Context, Result};

// Linux clipboards are owned by the process that last supplied their contents.
// Keeping one handle alive prevents copied text from disappearing and avoids
// arboard printing a debug-build warning directly over Ratatui's screen when a
// freshly written handle is dropped. Serializing access also avoids the
// platform contention arboard documents on Windows.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn with_clipboard<T>(operation: impl FnOnce(&mut arboard::Clipboard) -> Result<T>) -> Result<T> {
    let mut clipboard = CLIPBOARD
        .lock()
        .map_err(|_| anyhow::anyhow!("system clipboard lock was poisoned"))?;
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new().context("open system clipboard")?);
    }
    operation(clipboard.as_mut().expect("clipboard was initialized"))
}

pub fn read_text() -> Result<String> {
    with_clipboard(|clipboard| {
        clipboard
            .get_text()
            .context("read text from system clipboard")
    })
}

/// Writes `text` to the system clipboard.
///
/// Callers must run this off the render loop: opening the platform clipboard
/// blocks.
pub fn write_text(text: &str) -> Result<()> {
    with_clipboard(|clipboard| {
        clipboard
            .set_text(text)
            .context("write text to system clipboard")
    })
}
