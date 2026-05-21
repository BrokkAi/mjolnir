# mjolnir

`mjolnir` is a terminal UI client for Agent Client Protocol (ACP) servers. It
spawns an ACP-speaking agent process, talks JSON-RPC over the agent's stdio, and
renders the session in a `ratatui` chat interface.

The binary is named `mj`. On first launch it shows an interactive picker so you
can choose which ACP agent to run (from the official ACP registry, the bundled
`anvil` default, or a custom command). The choice is persisted to
`~/.config/mj/config.toml` and reused on subsequent runs. Use `/mj:agents`
inside the TUI to switch to a different agent later.

## Features

- Interactive agent picker backed by the
  [agentclientprotocol/registry](https://github.com/agentclientprotocol/registry)
  with built-in binary download / extraction (24h on-disk cache).
- Streaming agent messages, reasoning blocks, plans, and tool-call cards.
- Permission prompts with keyboard selection.
- Slash-command autocomplete from commands advertised by the agent, plus a
  client-side `/mj:` namespace (`/mj:agents` to re-open the picker).
- Optional file logging for the TUI and separate stderr capture for the agent.

## Requirements

- Rust stable with Cargo.
- Network access on the *first* launch (to fetch the registry index) unless
  you only intend to use `anvil` or a custom command.
- For registry agents distributed via `npx` / `uvx`, the corresponding runtime
  (`npx` from Node.js, `uvx` from `uv`) must be on `PATH`.

## Build and Run

```bash
cargo build --release
./target/release/mj
```

Install locally from this checkout:

```bash
cargo install --path .
mj --cwd .
```

The first invocation drops you into the agent picker:

```
 mj | choose an agent
+--- agents -------------------------------------------------+
| > anvil [current]    -- default mj agent                   |
|   Claude              -- npx v0.36.1                       |
|   Codex               -- binary v0.14.0                    |
|   ...                                                      |
|   Custom command...   -- type your own command             |
+------------------------------------------------------------+
```

`Up` / `Down` to navigate, type to filter, `Enter` to confirm, `Esc` to
cancel. Binary distributions are downloaded to `~/.cache/mj/agents/<id>/<version>/`
and reused on subsequent launches.

## CLI Options

- `--cwd`: workspace directory used for the ACP session. Defaults to the current
  directory.
- `--log-file`: write TUI logs to a file. Equivalent env var:
  `BROKK_TUI_LOG`.
- `--agent-stderr`: capture the agent subprocess stderr to a file. Equivalent
  env var: `BROKK_TUI_AGENT_STDERR`.

There is no `--command` / `--agent` flag: pick the agent interactively the
first time and re-pick later with `/mj:agents`. To force a fresh picker, delete
`~/.config/mj/config.toml`.

Logging is disabled by default because the TUI owns the terminal. Set
`BROKK_TUI_LOG_LEVEL` to override the default `info` filter when `--log-file` is
enabled.

## Keyboard Controls

- `Enter`: send the current prompt, or accept the selected slash command.
- `Tab`: accept the selected slash command.
- `Up` / `Down`: move within slash-command autocomplete or permission prompts.
- `PageUp` / `PageDown`: scroll the transcript.
- `?` / `F10`: show or hide the help overlay.
- `F1`..`F9`: edit visible session config options.
- `Esc`: dismiss autocomplete, clear input, or cancel a permission prompt.
- `Ctrl-C`: cancel an in-flight prompt; when idle with an empty input, quit.
- `Ctrl-D`: quit when the input is empty.

### Built-in `/mj:` commands

- `/mj:agents`: re-open the agent picker so you can switch the current session
  to a different ACP server.

## On-disk Files

- `~/.config/mj/config.toml` — the persisted choice (program + args + env).
- `~/.cache/mj/registry.json` — cached registry index, refreshed every 24h.
- `~/.cache/mj/agents/<id>/<version>/` — extracted binary distributions.

## Development

Use the same checks as CI before submitting changes:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The crate uses inline unit tests under `src/`. Keep runtime, UI state, event,
rendering, registry, install, and picker concerns separated across the existing
modules.

## License

`mjolnir` is licensed under GPL-3.0. See [LICENSE](LICENSE).
