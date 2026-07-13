# mjolnir

Mjolnir (`mj`) is a native Rust ACP client with a model-first coding Council:

- **Thor** coordinates the user turn.
- **Eitri** implements delegated, forgeable coding tasks or explores a codebase.
- **Loki** optionally reviews Thor and Eitri at streaming step boundaries.

Models are ranked with the public DeepSWE Pass@1 and average-cost data. Mjolnir
selects models first, then routes them through locally available ACP adapters.
The active adapter remains an implementation detail, so the transcript,
permissions, tools, terminals, session handling, and keyboard workflow stay
consistent.

![Mjolnir inline chat showing streaming agent output and tool activity](docs/readme-images/default-ui.png)

## Install and run

Install the latest `mj` release:

```bash
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash
```

The installer writes to `~/.local/bin` by default and offers to add that
directory to your shell profile when needed. Set `INSTALL_DIR` or
`MJOLNIR_INSTALL_DIR` to install somewhere else.

Desktop users can instead install both executables from crates.io. The worker
must be installed with `mj` for Ctrl-R dictation to be available:

```bash
cargo install --locked brokk-mjolnir brokk-mj-voice-worker
```

Installing only `brokk-mjolnir` is supported, but leaves voice dictation
disabled. Android installs should omit `brokk-mj-voice-worker`.

Then open a repo and run `mj`. The short binary name is intentional; nobody
wants to type `mjolnir` every time they ask an agent to look at a diff.

```bash
mj
```

Mjolnir discovers these built-in routes automatically:

- `codex` on `PATH` enables OpenAI models through `codex-acp`.
- `claude` on `PATH` enables Anthropic models through `claude-agent-acp`.
- a non-empty `OPENROUTER_API_KEY` enables Anvil through `uvx brokk acp`.
- `opencode` on `PATH` enables OpenCode through `opencode acp`.

Council adapters must advertise ACP HTTP MCP support because Thor invokes
Eitri through Mjolnir's embedded `code_agent` and `explore_agent` MCP tools.
Mjolnir probes each route once per process, never logs API-key values, and
selects High reasoning when the adapter advertises that control.

## Council configuration

The default is automatic model selection for every role:

```toml
[thor]
model = "auto"
discrete_review = true

[loki]
model = "auto"
streaming_review = true

[eitri]
model = "auto"
```

Put this in `~/.config/mj/config.toml`. Automatic Thor selection chooses the
strongest launchable DeepSWE High/default row. Loki chooses the strongest
launchable model from a different provider. Eitri chooses a cost-efficient
model on the DeepSWE Pass@1/cost Pareto frontier at the current Sonnet High
quality floor.

Use `/models` to see saved preferences, active resolved models, benchmark data,
adapter routes, custom unranked models, and disabled reasons. Changes made with
`/models <thor|loki|eitri> <auto|model-id>` apply to the next session. Use
`/reviews`, `/reviews thor on|off`, and `/reviews loki on|off` to inspect or
change review policy.

Thor's remaining ACP session controls appear on F1–F9. Model and Thought Level
are owned by the Council and therefore omitted from those controls; the other
values are session-only and are not persisted.

### Custom ACP servers

Custom servers are launched directly without a shell, inherit Mjolnir's
environment, and run in the workspace directory:

```toml
[[acp.servers]]
name = "company"
command = "/opt/company/bin/acp-server"
args = ["--stdio"]
```

Custom routes take precedence over built-ins in configuration order. Their
DeepSWE-matched models participate normally. Additional advertised models are
shown as `Unranked` and can be selected explicitly with an ID such as
`custom/company/provider/model`, but never participate in Auto or Ragnarok.
Configured servers without HTTP MCP support remain in the config and are
reported as unavailable.

## Delegation and review

Mjolnir appends Thor's session-level coordinator policy to the first user
message of a new session. It is not sent as a standalone turn or injected into
resumed or compacted sessions. Thor should use direct tools for small, tightly
coupled edits and delegate self-contained implementation tasks with clear inputs
and acceptance criteria to `code_agent`. It uses `explore_agent` for open-ended,
multi-file research where locations or execution flow are not yet known.

During a handoff, Thor's foreground ACP lane is detached and Eitri's fresh ACP
session streams directly through the normal Mjolnir UI. Ctrl-C cancels the
currently active Eitri request, not the paused Thor request. Eitri's final
message and invocation diff return to Thor through MCP, then Thor resumes.

Loki runs in its own fresh, best-effort read-only ACP session. A streaming Loki
intervention is intentionally expensive: it causes Mjolnir to cancel at the next
safe step boundary and re-prompt the target with the critique. Loki's prompt
therefore requires intervention only for material correctness, safety, scope,
or strategy problems. Thor performs the optional discrete workspace review at
the end of a turn; a turn containing only one implementation handoff skips that
extra review.

## Worktrees and resume

Use `--worktree` to create an isolated linked Git worktree below
`<project>/.mjolnir/worktrees/`, or pass a prior worktree name:

```bash
mj --worktree
mj --worktree quiet-forge
mj resume <session-id> --worktree quiet-forge
```

Mjolnir stores session provenance atomically in its state directory so a resume
returns to the original adapter and model even if Auto later resolves
differently. `mj resume` opens the searchable session picker;
`mj resume --list --format json` provides machine-readable output.

![Mjolnir resume picker listing prior ACP sessions](docs/readme-images/session-picker.png)

## Headless and remote use

Use `--print` for one-shot prompts with the same Council runtime:

```bash
mj --print "summarize the current diff"
git diff | mj --print -
```

`--output-format json` returns Thor's final result. `stream-json` additionally
labels Thor, Loki, and Eitri activity. `--permission-mode` controls permissions
for both Thor and Eitri; its default rejects prompts so automation cannot hang.

`mj server` starts Mjolnir's remote-control server with the same resolved
Council. Nested permission IDs are namespaced so remote clients can safely
answer the active Thor or Eitri prompt.

## Reference

Common options:

- `--cwd PATH`: primary workspace directory.
- `--additional-directory PATH`: expose another workspace root; repeatable.
- `-p, --print [PROMPT]`: run once; omit the value or pass `-` to read stdin.
- `--output-format text|json|stream-json`: headless output format.
- `--permission-mode default|acceptEdits|bypassPermissions`: headless policy.
- `-w, --worktree [NAME]`: create or reuse a linked worktree.
- `--debug-file PATH`: capture Mjolnir diagnostics.
- `--agent-stderr PATH`: capture ACP adapter stderr.
- `--fullscreen-tui`: use the alternate-screen UI instead of inline chat.

Keyboard basics:

- `Enter`: send a prompt or accept the selected action.
- `Up` / `Down`: navigate autocomplete and permission choices.
- `PageUp` / `PageDown`: scroll the transcript.
- `F1`-`F9`: edit visible Thor session controls.
- `F10`: toggle help.
- `Esc`: dismiss autocomplete, clear input, or cancel a permission prompt.
- `Ctrl-C`: cancel the active Thor or Eitri prompt; idle Ctrl-C quits.
- `Ctrl-D`: quit when input is empty.
- `Ctrl-R` (non-Android): start or stop microphone dictation into the prompt.
  Official desktop releases include the `mj-voice-worker` sidecar, which uses
  sherpa-onnx speech recognition with Silero VAD and
  the multilingual Parakeet TDT v3 model; the model (~0.7 GB) is downloaded and
  cached under `~/.cache/mj/voice/` on first use.

Persistent data includes:

- `~/.config/mj/config.toml`: Council, review, theme, spinner, and custom ACP
  preferences.
- the platform state directory's `mj/session-provenance.json`: resume routing.
- `~/.cache/mj/deepswe-v1.1.json`: 24-hour DeepSWE cache.
- `<project>/.mjolnir/worktrees/`: linked worktrees created by Mjolnir.

## Development

You only need Rust when building from source or contributing. Ordinary `mj`
builds do not compile the native speech stack. To build the optional voice
worker on Linux, install the ALSA development headers first (e.g.
`sudo apt-get install libasound2-dev` on Debian/Ubuntu).

```bash
cargo build --release
./target/release/mj
```

For local dictation development, build the sidecar into the same target
directory:

```bash
cargo build --release -p brokk-mj-voice-worker
```

Use the same checks as CI before submitting changes:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Tests are colocated under `src/`; deterministic PTY fixtures live in
`tests/e2e/`.

## License

Mjolnir is licensed under GPL-3.0. See [LICENSE](LICENSE).
