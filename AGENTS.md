# Repository Guidelines

## Developer Experience

Other things being equal, prefer to follow Claude Code conventions, e.g. in commandline parameters.

## Project Structure & Module Organization

This repository is a Rust 2024 crate named `mjolnir` that builds the `mj` binary. Source code lives in `src/`:

- `src/main.rs` parses CLI flags, initializes logging, and wires the runtime to the UI.
- `src/acp.rs` manages Agent Client Protocol process startup and JSON-RPC communication.
- `src/app.rs` contains the UI state machine and most unit tests.
- `src/event.rs` defines messages shared between the ACP runtime and UI task.
- `src/ui.rs` renders and drives the ratatui/crossterm terminal interface.

There is no separate `tests/` directory today; tests are colocated in module-level `#[cfg(test)]` blocks.

## Build, Test, and Development Commands

- `cargo fmt --check` verifies Rust formatting without changing files.
- `cargo fmt` applies standard rustfmt formatting.
- `cargo clippy --all-targets -- -D warnings` runs lints with warnings treated as errors, matching CI.
- `cargo test` runs unit tests.
- `cargo build --release` builds the optimized `mj` binary.
- `cargo run -- --cwd .` runs the TUI in the current workspace and opens the agent picker when no agent is configured yet.

## Coding Style & Naming Conventions

Use idiomatic Rust formatted by rustfmt. Prefer clear module boundaries that match the existing runtime/UI split. Name files and modules with `snake_case`; use `PascalCase` for types and enum variants, `snake_case` for functions and variables, and `SCREAMING_SNAKE_CASE` for constants. Keep comments short and useful, especially around async runtime behavior, terminal ownership, or protocol edge cases. Repository-facing text, code comments, and documentation should be written in English.

## Testing Guidelines

Add focused unit tests near the code under test using `#[cfg(test)] mod tests`. Follow the existing descriptive test naming style, e.g. `autocomplete_updates_matches_for_prefix`. For state-machine changes, test the event transition or input handling directly rather than relying only on manual TUI checks. Run `cargo test` and `cargo clippy --all-targets -- -D warnings` before submitting changes.

## UI Safety Requirements

Permission dialogs must never truncate requested permission content. Long commands, titles, descriptions, and option labels must remain fully readable through wrapping, scrolling, paging, resizing, or an equivalent explicit expansion path. This applies to both inline and fullscreen UI modes.

Do not recover from inline UI failures by falling back to the fullscreen TUI. That is a jarring mode switch and a poor user experience; inline terminal problems should be retried, degraded, or surfaced within inline mode instead.

Inline UI code must never exit, bail, return a fatal error, or switch to fullscreen solely because the terminal cursor position could not be read in time. Treat cursor-position/CPR timeouts as transient terminal conditions: retry, skip the affected redraw/repair/resize, or surface the issue within inline mode while keeping the session alive.

## Commit & Pull Request Guidelines

Recent commits use concise, imperative summaries such as `rename crate to mjolnir, binary to mj`; some include PR numbers after merge. Keep commit subjects specific and lowercase where natural. Pull requests should describe the behavior change, list validation commands run, link related issues, and include screenshots or terminal recordings when UI rendering changes.

## Security & Configuration Tips

Do not log to stderr while the TUI owns the terminal. Use `--debug-file` (or compatibility alias `--log-file`) and `BROKK_TUI_LOG` for diagnostics, and `--agent-stderr` or `BROKK_TUI_AGENT_STDERR` to capture agent subprocess stderr. Avoid committing generated `target/` artifacts or local machine configuration.
