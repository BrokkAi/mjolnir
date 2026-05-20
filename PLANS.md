# mjolnir plan

This document is a working plan for `mjolnir`, extracted from the earlier
Brokk-side ACP client work and adjusted for this standalone repository.

## Background

`mjolnir` started as `brokk-tui-rust` inside `BrokkAi/brokk`.

Relevant Brokk history:

- Brokk PR #3562 scaffolded `brokk-tui-rust` as "Phase 0 of Rust ACP TUI".
- Brokk PR #3592 replaced the stub with a working interactive ACP chat client.
- Brokk PR #3600 added slash-command autocomplete.
- Brokk PR #3666 removed `brokk-tui-rust` from `BrokkAi/brokk` after extracting
  it to a standalone repo with history preserved.
- The repo was briefly named `hammer`; it is now `mjolnir`, with binary `mj`.

The original intent was broader than "a Brokk TUI": build a native Rust terminal
client that speaks the Agent Client Protocol (ACP) to any conformant agent. It
defaults to `anvil`, but the client should stay agent-agnostic.

## Current status

The current crate is already a usable MVP:

- Spawns an ACP agent process, defaulting to `anvil` on `PATH`.
- Talks ACP JSON-RPC over the child process stdio.
- Opens a new ACP session for `--cwd` or the current directory.
- Sends text prompts and receives prompt responses.
- Streams ACP `SessionUpdate` values into a ratatui transcript.
- Renders user messages, agent messages, reasoning/thought chunks, plans, and
  tool calls.
- Handles `session/request_permission` with a keyboard-driven modal.
- Supports slash-command autocomplete from `AvailableCommandsUpdate`.
- Supports prompt cancellation with `Ctrl-C`.
- Keeps TUI logging out of stderr via `--log-file`.
- Captures or discards agent stderr via `--agent-stderr`.
- Has focused unit tests around ACP lifecycle and UI state transitions.

Current command-line surface:

- `mj --cwd /path/to/repo` to choose the ACP session root.
- `mj --log-file /path/to/mj.log` for TUI logs.
- `mj --agent-stderr /path/to/agent.err` for child-process stderr.

There is no `--command` / `--agent` flag. The agent is chosen interactively in
a picker on the first launch (or whenever `~/.config/mj/config.toml` is missing
the `agent` block), and can be changed later with the in-TUI `/mj:agents`
command. The picker is backed by the official
[agentclientprotocol/registry](https://github.com/agentclientprotocol/registry)
index (24h on-disk cache) with native binary install, plus the bundled `anvil`
default and a `Custom command...` entry for arbitrary launch strings.

M1 hardening landed (PR #34): an explicit `ConnectionState` lifecycle drives
the header label, a `LaunchError` enum surfaces spawn / initialize /
`session/new` failures with one-line action hints, permission prompts queue
FIFO instead of silently overwriting, and an unexpected agent exit raises a
single Fatal instead of an unbounded "prompt failed" stream.

## Product goal

Make `mjolnir` the best small terminal client for ACP agents:

1. Agent-agnostic: works with Brokk, Codex-style agents, Claude ACP agents,
   Gemini agents, Goose, and any other ACP-compatible server.
2. Terminal-native: fast startup, reliable keyboard UX, readable output, and no
   GUI assumptions.
3. Protocol-faithful: use ACP primitives directly instead of inventing
   Brokk-only control paths.
4. Safe by default: permission prompts are clear, never auto-accepted, and do
   not block the JSON-RPC dispatch loop.
5. Easy to install and run: one binary, simple launch flags, sane defaults.

## Non-goals

These were present in earlier Brokk plans, but should not be v1 scope for this
repository:

- Recreating the Python/Textual `brokk-code` feature set one-for-one.
- Owning Brokk's Java/Python launcher migration.
- Implementing Brokk subcommands such as `issue`, `pr`, `provider`, `install`,
  `github`, or `commit`.
- Building a Tauri/Svelte desktop app like the removed `brokk-foreman` plan.
- Building an ACP registry browser with install/uninstall flows in v1.
- Multi-repo project management.
- Multiple concurrent ACP sessions in one UI.
- Agent-side credential management beyond surfacing ACP auth failures clearly.

## Architecture

Runtime shape:

```text
+------------+     UiCommand      +-------------+      stdio      +-----------+
| ratatui UI | -----------------> | ACP runtime | --------------> | ACP agent |
|            | <----------------- |             | <-------------- | process   |
+------------+      UiEvent       +-------------+    JSON-RPC     +-----------+
```

Key constraints:

- The UI task owns the terminal alternate screen and crossterm input stream.
- The ACP runtime owns child-process stdio and JSON-RPC dispatch.
- UI and ACP runtime communicate over channels.
- Permission requests cross into the UI through a oneshot responder so the ACP
  dispatch loop is not blocked by terminal input.
- The client currently advertises no ACP filesystem or terminal capabilities.
  That keeps the MVP simple, but it limits agents that expect client-provided
  file or terminal operations.

## Milestones

### M0: Preserve the extracted client

Status: done.

- Rename crate and binary to `mjolnir` / `mj`.
- Keep the Brokk TUI history useful while removing Brokk monorepo assumptions.
- Add README, license, and contributor guidance.
- Keep CI checks simple: fmt, clippy, tests, release build.

### M1: Make the MVP dependable

Status: done (PR #34, 2026-05-20). Follow-up: issue #35 (unify
`TurnState` with `ConnectionState::Streaming`).

Goal: the current feature set should feel stable enough for daily local use.

Deliverables:

- ✅ Tighten error messages when agent launch, initialize, or `session/new`
  fails — `LaunchError` enum classifies five distinct failures
  (`CommandNotFound`, `SpawnFailed`, `StderrFileOpen`, `InitializeFailed`,
  `AuthRequired`, `SessionCreateFailed`) and each renders a headline plus a
  `hint:` line.
- ✅ Add visible connection states for launching, initializing, ready,
  streaming, cancelled, closed, and fatal — `ConnectionState` enum drives
  the header label.
- ✅ Improve shutdown so child processes are reliably reaped on normal exit
  and cancellation paths — `run()` races `drive_client` against
  `child.wait()` and surfaces an unexpected agent exit as a single Fatal.
- ✅ Make transcript scrolling predictable during active streaming and after
  resize — integration test `streaming_chunks_and_resize_preserve_user_scroll_anchor`
  locks in the reconciler composition.
- ✅ Keep permission modal behavior deterministic under streaming, resize,
  and autocomplete interactions — permission prompts queue FIFO; the modal
  header shows `(1 of N)` when more are queued; runtime close fans out
  `Cancelled` to every queued responder.
- ✅ Add regression tests for the state transitions above — test count went
  from 40 → 88, including portable integration tests on Linux / macOS /
  Windows for the agent-exit and stderr-blame paths.

Exit criteria:

- ✅ `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, and `cargo build --release` pass on all three CI targets.
- ✅ Manual smoke test against `anvil` can launch, send a prompt, handle a
  tool permission, cancel a prompt, and exit without a leftover child
  process.

### M2: Improve protocol coverage

Goal: render and respond to common ACP features well enough that `mjolnir` is
useful with multiple agents, not only the default one.

Deliverables:

- Render more `ContentBlock` variants with clear fallbacks:
  `resource_link`, embedded `resource`, `image`, `audio`, and unknown variants.
  Today they show labelled placeholders (`[image]`, `[resource]`, `[link …]`,
  `[audio]`) — M2 should make them rich.
- Improve tool-call rendering for diff, terminal, and structured content.
- Render config option updates in a way that lets users understand model, mode,
  reasoning, and other agent-provided session settings.
- Add command support for ACP session config changes if the protocol surface and
  advertised commands make that practical.
- ✅ Handle ACP auth-required responses with actionable UI text — shipped
  early in M1 as `LaunchError::AuthRequired`, classified at both
  `initialize` and `session/new`.
- Track compatibility quirks discovered with at least two non-Brokk ACP
  agents — partial: `@agentclientprotocol/claude-agent-acp` 0.36.1 done
  (see Compatibility section below), one more (Gemini or Goose) to go.

Exit criteria:

- Manual compatibility matrix documents which ACP agents were tested and which
  features worked.
- Unsupported ACP features degrade visibly and politely instead of becoming
  silent no-ops.

### M3: Make command entry and session workflow pleasant

Goal: reduce friction in everyday terminal use.

Deliverables:

- Prompt history across the current process.
- Optional persisted prompt history under a user config directory.
- Better multiline editing, including newline insertion and submit semantics.
- Search or filter over the transcript.
- Copy-friendly transcript output mode or an export command.
- Session title display and clearer session metadata.
- ✅ Interactive agent picker backed by the ACP registry with native binary
  install, plus a hardcoded `anvil` default and a `Custom command...` entry,
  reachable on first launch and via `/mj:agents`.

Exit criteria:

- A user can start `mj`, pick an agent in the picker, carry out a few turns,
  recover recent prompts, and copy or export useful output without leaving the
  terminal.

### M4: Installation and distribution

Goal: make `mj` easy to install without cloning the repo.

Deliverables:

- GitHub release workflow for Linux x86_64, macOS aarch64, and Windows x86_64.
- Release artifacts named consistently, with checksums.
- Document `cargo install --git`, release binary install, and local build paths.
- Decide whether to provide a shell installer, Homebrew formula, or both.
- Decide whether `mj` should ever be installed by Brokk's installer, or remain
  independent.

Exit criteria:

- Fresh machine install path works for at least macOS aarch64 and Linux x86_64.
- `mj --version` and `mj --cwd .` work after install.

### M5: Optional client capabilities

Goal: decide whether `mjolnir` should become more than a prompt/permission UI.

Candidate capabilities:

- ACP filesystem operations (`fs/read_text_file`, `fs/write_text_file`) backed
  by local disk.
- ACP terminal operations backed by a managed subprocess view.
- ACP registry lookup and agent launch presets.
- Session persistence or `session/load` UI.

These are intentionally later because they can expand the blast radius quickly.
Each should start with a separate design note before implementation.

## Feature backlog

Near-term:

- Multiline input.
- Prompt history.
- More complete `SessionUpdate` rendering (image/audio/resource go beyond
  placeholders; structured tool-call output for diff/terminal).
- Compatibility smoke tests against more non-Brokk ACP agents (one done in
  M1; see the Compatibility section).

(M1 closed: fatal/error rendering, child-process cleanup, transcript
scrolling.)

Medium-term:

- Named agent presets.
- Persisted local settings.
- Export transcript.
- Rich diff rendering.
- Config option picker.
- Session list/load/fork support if agents expose it usefully.

Later:

- Release packaging and installers.
- ACP registry integration.
- Filesystem capability support.
- Terminal capability support.
- Multiple sessions.

## Risks and open questions

1. **How agent-agnostic should the UI stay?** Brokk-specific affordances can make
   Brokk better, but they should not turn the core into a Brokk-only client.
2. **Do we want a config file?** Launch presets and history need persistence, but
   a config format adds compatibility burden.
3. **Should `mj` implement filesystem capabilities?** Local-disk reads are easy;
   doing it safely and predictably with permissions is harder.
4. **Should `mj` implement terminal capabilities?** It fits the app domain, but
   a nested terminal view inside a TUI is a substantial feature.
5. **How much of the old Brokk launcher plan still matters?** The standalone
   repo should not inherit the entire Java/Python migration unless that becomes
   an explicit product decision.
6. **What are the first-class target agents?** We need a small compatibility
   matrix to avoid designing only against one agent.

## Compatibility

Smoke-tested against non-Brokk ACP agents to validate the
"agent-agnostic terminal client" goal (PLANS.md goal #1). Each entry
records the date, agent version, and what worked at the protocol layer.
Update this table when re-running against newer versions or new agents.

### `@agentclientprotocol/claude-agent-acp` 0.36.1 — 2026-05-20

Source: npm package in the official `@agentclientprotocol` scope, OIDC-
published from GitHub Actions by Conrad Irwin et al. (Apache-2.0).
Wraps the Claude Agent SDK; uses Claude Code's local credentials, so no
`ANTHROPIC_API_KEY` in the env is needed if Claude Code is already
authenticated on the machine.

Launch:

```text
mj --command "npx -y -p @agentclientprotocol/claude-agent-acp@0.36.1 claude-agent-acp"
```

Verified at the protocol layer (driven by a hand-rolled JSON-RPC probe,
not a full interactive prompt round-trip, to avoid burning model tokens
in a smoke test):

| Feature | Result |
| --- | --- |
| `initialize` handshake (ACP v1) | works; `protocolVersion: 1` returned, matches our advertised version |
| `agentInfo` (name + version) | populated; our `Connected` event renders `Claude Agent 0.36.1` |
| `authMethods` | `[]`; no auth-required path triggered for this configuration |
| `session/new` with `cwd` | works; returns `sessionId`, `models`, `modes`, `configOptions` |
| `configOptions` categories | `mode`, `model`, `thought_level` — all map to our existing `SessionConfigOptionCategory` variants and render via the inline shortcut row |
| `available_commands_update` notification | streams immediately after `session/new`; populates the slash autocomplete |
| `loadSession`, `sessionCapabilities` (resume/fork/list/close/delete) | advertised by the agent; mjolnir does not yet drive any of these (M5 territory) |
| `promptCapabilities.image`, `embeddedContext` | accepted by the agent; mjolnir still renders these `ContentBlock` variants as `[image]` / `[resource]` placeholders pending M2 |
| `mcpCapabilities.http`, `sse` | advertised; mjolnir does not currently let the user specify `mcpServers` at `session/new` (sends none) |

Known gaps to file as follow-ups when the matrix expands:

- We do not surface the agent's `sessionCapabilities` to the user, so
  there's no UI hint that this agent supports resume/fork/list.
- We send `session/new` without `mcpServers`. If users want to plug in
  MCP servers via `mj`, that requires a CLI flag or config-file entry.
- Effort levels (`low/medium/high/xhigh/max`) come through the
  `thought_level` config category and render with the auto-titlecased
  name (`Xhigh`). Cosmetic and agent-side, not blocking.

Not yet exercised (would consume model tokens or require interactive
testing): `session/prompt` round-trip, tool-call permission flow,
prompt cancellation against a live agent, agent-initiated errors mid-
turn.

### Next targets

- Gemini CLI (auth-required path test).
- Goose (self-hosted, no auth dance).

Each future entry should follow the same shape: source / launch
command / verified table / known gaps / not-yet-exercised.

## Discussion checklist

Before turning this into an implementation roadmap, decide:

- Is `mjolnir` primarily a Brokk companion, or a general ACP terminal client?
- Should v1 include named launch presets?
- Should v1 include persisted prompt history?
- Should v1 include session list/load, or only `session/new`?
- Which agents must be in the compatibility matrix?
- What install channel should be first: GitHub releases, Homebrew, shell
  installer, or `cargo install` only?
