# mjolnir

`mjolnir` is a terminal UI client for Agent Client Protocol (ACP) servers. It
spawns an ACP-speaking agent process, talks JSON-RPC over the agent's stdio, and
renders the session in a `ratatui` chat interface.

The binary is named `mj` and defaults to launching `anvil` from `PATH`.

## Features

- Interactive ACP chat session over stdio.
- Streaming agent messages, reasoning blocks, plans, and tool-call cards.
- Permission prompts with keyboard selection.
- Slash-command autocomplete from commands advertised by the agent.
- Optional file logging for the TUI and separate stderr capture for the agent.

## Requirements

- Rust stable with Cargo.
- An ACP server executable, such as `npx @zed-industries/codex-acp`, available on `PATH` or passed
  with `--command`.

## Build and Run

```bash
cargo build --release
./target/release/mj
```

Run against an ACP server command explicitly:

```bash
cargo run -- --command "npx @zed-industries/codex-acp" --cwd /path/to/workspace
```

Install locally from this checkout:

```bash
cargo install --path .
mj --cwd .
```

## CLI Options

- `--command`, `-c`: ACP server command to spawn. Defaults to `anvil`.
- `--cwd`: workspace directory used for the ACP session. Defaults to the current
  directory.
- `--log-file`: write TUI logs to a file. Equivalent env var:
  `BROKK_TUI_LOG`.
- `--agent-stderr`: capture the agent subprocess stderr to a file. Equivalent
  env var: `BROKK_TUI_AGENT_STDERR`.

Logging is disabled by default because the TUI owns the terminal. Set
`BROKK_TUI_LOG_LEVEL` to override the default `info` filter when `--log-file` is
enabled.

## Keyboard Controls

- `Enter`: send the current prompt, or accept the selected slash command.
- `Tab`: accept the selected slash command.
- `Up` / `Down`: move within slash-command autocomplete or permission prompts.
- `PageUp` / `PageDown`: scroll the transcript.
- `Esc`: dismiss autocomplete, clear input, or cancel a permission prompt.
- `Ctrl-C`: cancel an in-flight prompt; when idle with an empty input, quit.
- `Ctrl-D`: quit when the input is empty.

## Development

Use the same checks as CI before submitting changes:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The crate uses inline unit tests under `src/`. Keep runtime, UI state, event, and
rendering concerns separated across the existing modules.

## License

`mjolnir` is licensed under GPL-3.0. See [LICENSE](LICENSE).
