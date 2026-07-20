# mjolnir

Mjolnir (`mj`) is a native Rust ACP client with a model-first coding Council:

- **Thor** coordinates the user turn.
- **Eitri** implements delegated, forgeable coding tasks or explores a codebase.
- **Loki** optionally reviews Thor and Eitri's implementation runs at streaming step boundaries.

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

- Existing Codex credentials enable OpenAI models through `codex-acp`.
- Existing Claude credentials enable Anthropic models through `claude-agent-acp`.
- Existing Kimi Code credentials enable Moonshot models through the official
  Kimi ACP server, which Mjolnir installs from the ACP registry when needed.
- Pinned Anvil `0.23.0` is a managed pseudo-builtin and enables its exposed models.

Council adapters must advertise ACP HTTP MCP support because Thor invokes
Eitri through Mjolnir's embedded `code_agent` and `explore_agent` MCP tools.
Codex and Claude discovery checks local credential files and supported token
environment variables without launching either CLI or their npm ACP bridges.
Other ACP routes are probed once and the results are cached on disk for 24
hours (invalidated when the adapter binary changes), so interactive startup
binds the Council instantly from credentials and cached capabilities. Adapters
without a fresh cache entry are probed in the background: their models appear
in `/models` and the ACP Servers tab when the probe lands, and take effect for
role selection in the next session. Startup only waits on a probe when the
configured Council cannot be bound any other way, so a wedged ACP server never
blocks an otherwise-launchable session. Mjolnir never logs API-key values and
selects High reasoning when the adapter advertises that control.
Interactive startup installs pinned Anvil in the background when no development
override, bundled sibling, or managed copy is available. Resolution precedence
is `--anvil-path`, `MJ_ANVIL_PATH`, an `anvil` sibling beside `mj`, then the
managed copy under the platform data directory (`~/.local/share/mj/agents` on
Linux). Development overrides are never updated or replaced.

## Council configuration

The default is automatic model selection for every role:

```toml
version = 2

[thor]
model = "auto"
discrete_review = true

[loki]
model = "auto"

[eitri]
model = "auto"
```

Put this in `~/.config/mj/config.toml`. Automatic Thor selection chooses the
strongest launchable DeepSWE High/default row. Loki prefers the strongest model
from a different provider, then another model, and finally reuses Thor if
needed. Eitri prefers a distinct cost-efficient model on the DeepSWE Pass@1/cost
Pareto frontier at the current Sonnet High quality floor, but may reuse a model
when no distinct qualifying choice exists. Set `model = "disabled"` under
`[loki]` or `[eitri]` to turn that role off.

Use `/mjconfig` to configure Council models, accounts, ACP servers, review
policy, and appearance. `/models` opens the same editor directly on the Council
tab. Model and ACP server changes apply to the next session. The ACP Servers tab
can sign in to OpenAI, Anthropic, and Kimi through their official CLIs. OpenAI
credentials also enable Anvil's Codex routes, while Kimi credentials enable both
Kimi Code and Anvil's Kimi routes. Mjolnir requires an installed `codex` or
`claude` CLI for those vendors; it can install the official Kimi CLI itself.

Only available native servers are listed. Choose `+ Add server` to browse the
ACP registry or enter a custom command. Managed servers show their managed
version without repeating their internal install path. Registry binary
distributions are owned under Mjolnir's platform data directory. Configure automatic Thor review with the
`Thor review` switch in `/mjconfig`. Use `/review` to choose a findings-only review of the
most recent change-producing turn, all uncommitted changes, or `HEAD`; direct forms are
`/review recent`, `/review uncommitted`, and `/review head`.

The config schema is versioned. A missing or incompatible `version` starts from
fresh defaults instead of attempting field-by-field migration.

For one non-interactive invocation, `--thor MODEL`, `--loki MODEL`, and
`--eitri MODEL` override the saved Council selection when used with `--print`.
Loki and Eitri also accept `disabled` or `none`; these overrides are never
written to config.

Thor's remaining ACP session controls appear on F1–F9. Model and Thought Level
are owned by the Council and therefore omitted from those controls; the other
values are session-only and are not persisted.

### Custom ACP servers

Custom servers are launched directly without a shell, inherit Mjolnir's
environment, and run in the workspace directory:

```toml
version = 2

[[acp.servers]]
id = "custom:company"
label = "company"
command = "/opt/company/bin/acp-server"
args = ["--stdio"]
origin = "custom"
```

Custom routes take precedence over built-ins in configuration order. Their
DeepSWE-matched models participate normally. Additional advertised models are
shown as `Unranked` and can be selected explicitly with an ID such as
`custom/company/provider/model`, but never participate in Auto or Ragnarok.
Configured servers without HTTP MCP support remain in the config and are
reported as unavailable.

## Delegation and review

Mjolnir appends Thor's session-level coordinator policy to the first user
message of a session, including resumed and loaded sessions. It is never sent
as a standalone turn. A drop in the agent's reported context usage is treated
as history compaction and re-arms the policy for the next user message, so the
delegation contract survives context replacement. Thor should use direct tools for small, tightly
coupled edits and delegate self-contained implementation tasks with clear inputs
and acceptance criteria to `code_agent`. It uses `explore_agent` for open-ended,
multi-file research where locations or execution flow are not yet known.
Read-only exploration runs in a bounded background pool and may overlap other
explorations or an implementation run. Loki never observes or interrupts these
scouts; its Eitri review path is reserved for `code_agent` implementation work.

During a handoff, Thor's foreground ACP lane is detached and Eitri's fresh ACP
session streams directly through the normal Mjolnir UI. Ctrl-C cancels the
currently active Eitri request, not the paused Thor request. Eitri's final
message and invocation diff return to Thor through MCP, then Thor resumes.

Loki runs in one long-lived, best-effort read-only ACP session and reviews
asynchronously at his own pace: transcript checkpoints — including each turn's
concluding message — stream into his queue, and his advice is pulled off at
natural turn boundaries. Loki never interrupts running work and nothing ever
waits on him. Advice ready when an Eitri invocation returns rides back inside
the tool result; advice ready when Thor's turn completes either folds into the
discrete review brief or starts one follow-up Thor turn; and advice that lands
after the turn already finished interjects one council-initiated follow-up
("thoughts on the work you just did") unless the user has started a new prompt,
in which case it waits for that turn's boundary. Delivered advice is always
labeled with the turn and span it reviewed, since later work may have
superseded it. Thor performs the optional discrete workspace review at the end
of a turn; a turn containing only one implementation handoff skips that extra
review. Per-role token and cost telemetry is shown in the header and
`/council` status.

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
- `--permission-mode manual|auto|yolo`: headless policy. The former
  `default`, `acceptEdits`, and `bypassPermissions` values remain aliases.
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
- `~/.cache/mj/acp-probes-v1.json`: 24-hour ACP adapter capability cache.
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

## License and third-party notices

Mjolnir and its voice worker are licensed under `GPL-3.0-only`. See
[`LICENSE`](LICENSE). Official release archives also include
[`licenses/SOURCE.md`](licenses/SOURCE.md), a generated Rust dependency report,
supplemental notices for native libraries and embedded fonts, and the legal
bundle shipped with the pinned Anvil binary.

The generated report covers the locked production workspace graph across every
native release target. The supplemental generator also fails when a new
native-linking crate, standalone Cargo `NOTICE` file, embedded web font, or
sherpa-onnx native payload version appears without review.

To validate or regenerate the reports after changing `Cargo.lock` or shipped
assets:

```bash
cargo deny --workspace --config licenses/deny.toml --locked check licenses
cargo about generate --workspace --offline --config licenses/about.toml \
  --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html
node scripts/generate-supplemental-third-party-notices.mjs
```
