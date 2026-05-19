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
- Named agent presets from a local registry (`~/.config/mj/agents.toml`).

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

- `--command`, `-c`: ACP server command to spawn. Takes precedence over
  `--agent`. Defaults to `anvil`.
- `--agent`, `-a`: named agent preset from the registry. Ignored when
  `--command` is also set.
- `--list-agents`: list available agent presets from the registry and exit.
- `--cwd`: workspace directory used for the ACP session. Defaults to the current
  directory.
- `--log-file`: write TUI logs to a file. Equivalent env var:
  `BROKK_TUI_LOG`.
- `--agent-stderr`: capture the agent subprocess stderr to a file. Equivalent
  env var: `BROKK_TUI_AGENT_STDERR`.

Logging is disabled by default because the TUI owns the terminal. Set
`BROKK_TUI_LOG_LEVEL` to override the default `info` filter when `--log-file` is
enabled.

## Agent Registry

`mj` can load named agent presets from `~/.config/mj/agents.toml` (or the XDG
config directory on your platform). This lets you define short names for
commonly-used agent commands instead of retyping the full command every time.

Example `agents.toml`:

```toml
[agents.anvil]
command = "anvil"

[agents.local]
command = "/path/to/custom-agent --flag"
description = "My local dev agent"
```

Then run with a preset name:

```bash
mj --agent local
```

Command resolution precedence:

1. `--command` (explicit, highest priority)
2. `--agent` (named preset from the registry)
3. Default: `anvil`

List available presets:

```bash
mj --list-agents
```

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
