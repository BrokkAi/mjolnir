//! Interactive wrapper around frontend-neutral worktree support.

use anyhow::Result;
use std::io::Write;

pub use mj_core::worktree::*;

/// Ask whether to remove the worktree after the session ends, using an
/// inline arrow-key menu when stdio is an interactive terminal and the
/// line-based [y/N] prompt otherwise. Returns Ok(true) when the worktree
/// was successfully removed.
pub fn prompt_remove_on_exit_menu(worktree: &CreatedWorktree) -> Result<bool> {
    let label = worktree_exit_label(worktree);
    let options = [
        crate::menu::MenuOption {
            label: "Keep",
            hint: format!("leave it at {}", worktree.worktree_root.display()),
            shortcuts: &['n', 'k'],
        },
        crate::menu::MenuOption {
            label: "Remove",
            hint: "delete the worktree, including any uncommitted changes".to_string(),
            shortcuts: &['y', 'r'],
        },
    ];
    let choice = crate::menu::select_inline(
        &format!("Worktree '{label}' — keep or remove?"),
        "↑/↓ choose · enter confirm · esc keep",
        &options,
        0,
    )?;
    let Some(choice) = choice else {
        // Not an interactive terminal: fall back to the line-based prompt.
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        return prompt_remove_on_exit(worktree, &mut input, &mut output);
    };

    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if choice != 1 {
        writeln!(
            output,
            "Keeping worktree: {}",
            worktree.worktree_root.display()
        )?;
        return Ok(false);
    }
    remove_with_feedback(worktree, &label, &mut output)
}
