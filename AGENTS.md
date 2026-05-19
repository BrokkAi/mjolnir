# Repository Guidelines

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
- `cargo run -- --command "anvil" --cwd .` runs the TUI against an ACP server command.

## Coding Style & Naming Conventions

Use idiomatic Rust formatted by rustfmt. Prefer clear module boundaries that match the existing runtime/UI split. Name files and modules with `snake_case`; use `PascalCase` for types and enum variants, `snake_case` for functions and variables, and `SCREAMING_SNAKE_CASE` for constants. Keep comments short and useful, especially around async runtime behavior, terminal ownership, or protocol edge cases. Repository-facing text, code comments, and documentation should be written in English.

## Testing Guidelines

Add focused unit tests near the code under test using `#[cfg(test)] mod tests`. Follow the existing descriptive test naming style, e.g. `autocomplete_updates_matches_for_prefix`. For state-machine changes, test the event transition or input handling directly rather than relying only on manual TUI checks. Run `cargo test` and `cargo clippy --all-targets -- -D warnings` before submitting changes.

## Commit & Pull Request Guidelines

Recent commits use concise, imperative summaries such as `rename crate to mjolnir, binary to mj`; some include PR numbers after merge. Keep commit subjects specific and lowercase where natural. Pull requests should describe the behavior change, list validation commands run, link related issues, and include screenshots or terminal recordings when UI rendering changes.

## Security & Configuration Tips

Do not log to stderr while the TUI owns the terminal. Use `--log-file` or `BROKK_TUI_LOG` for diagnostics and `--agent-stderr` or `BROKK_TUI_AGENT_STDERR` to capture agent subprocess stderr. Avoid committing generated `target/` artifacts or local machine configuration.
