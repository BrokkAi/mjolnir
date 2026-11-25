# Add the `mj-claude` MCP server for non-Claude harnesses

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This document follows `.agents/PLANS.md` from the repository root and must be maintained in accordance with it.

## Purpose / Big Picture

Claude Code gives sessions four built-in utility capabilities that other harnesses Mjolnir drives often lack: fetching and reading a web URL (`WebFetch`), streaming asynchronous notifications from a background command (`Monitor`), scheduling local-time recurring or one-shot prompts (`CronCreate`, `CronList`, `CronDelete`), and pacing dynamic wakeups (`ScheduleWakeup`). On harnesses without native equivalents—most notably Codex, which lacks all four, and Antigravity, OpenCode, DSH, Kimi, Grok, and Muse, which each lack a subset—agents fall back to blocking shell `sleep` loops or raw `curl` HTML dumps.

After this change, Mjolnir workers automatically run a worker-local MCP server named `mj-claude` whenever the session's harness lacks one or more of those four capabilities. The server selectively advertises only the capabilities missing from that harness: Claude receives no `mj-claude` server at all; Codex receives all four; Antigravity receives `web_fetch` and `monitor`; OpenCode, DSH, and Grok receive `monitor`, `cron`, and `schedule_wakeup`; Kimi receives `monitor` and `schedule_wakeup`; and Muse receives `schedule_wakeup`. Isolated guardian and before-push reviewers never receive `mj-claude`. On harnesses that support deferred MCP discovery (such as Codex), `mj-claude` is not eager-loaded; on every harness where Mjolnir auto-approves its own MCP servers (Codex, Antigravity, DSH), `mj-claude` is eager-whitelisted alongside `mj-agents`, `mj-memory`, and `mj-review`.

A user can verify the behavior end to end by starting a non-Claude session (or running the new integration tests), calling `web_fetch` against a local HTTP server to read clean Markdown with offset paging, starting a `monitor` command whose stdout lines wake an idle worker via Mjolnir's existing relay mailbox (and whose process group is killed cleanly on `monitor_stop` or worker exit), creating/listing/deleting a local-time cron job, and scheduling/cancelling a dynamic wakeup. Because the service lives entirely inside the session's worker (`mj-worker`) and injects wakes through the worker's own durable relay mailbox, it survives daemon restarts and requires no daemon, store, or wire-protocol changes.

## Progress

- [x] (2026-11-25 15:37Z) Completed read-only reconnaissance of Mjolnir's MCP server wiring (`mj-agents`, `mj-memory`, `mj-review`), worker relay mailbox wake path, pinned Claude Code `2.1.326` SDK schemas, and per-harness native tool surfaces across all eight harnesses.
- [x] (2026-11-25 15:43Z) Authored ExecPlan `.agents/plans/mj-claude-mcp-server.md`.
- [x] (2026-11-25 15:48Z) Milestone 1: Add `mj-core::claude_mcp` capability registry, harness capability matrix, and JSON-line RPC protocol types (`mj-core/src/claude_mcp.rs`).
- [x] (2026-11-25 15:48Z) Milestone 2: Add `mj-worker-mcp::claude_mcp` stdio MCP server (plus optional `instructions` in `mcp_stdio.rs`) and `hel worker claude-mcp` CLI subcommand in `mj-worker-bin`.
- [x] (2026-11-25 15:58Z) Milestone 3: Implement worker-side `ClaudeMcpService` (`web_fetch`, `monitor`, `cron`, `schedule_wakeup`) and `DurableRelay::deliver_agent_wake` in `mj-worker`, with clean process-group teardown and durable cron persistence.
- [x] (2026-11-25 16:09Z) Milestone 4: Wire `mj-claude` launch, selective registration, deferred loading, and eager-whitelisting across Codex, Antigravity, DSH, Kimi, OpenCode, Grok, Muse (and explicit exclusion for Claude and isolated reviewers), plus integrated-review fixes (`anyOf` schema removal, `monitor` `env_clear()`, durable one-shot cron reload, `web_fetch` self-closing script & `data-href` guards).
- [ ] Milestone 5: Run before-push review via `request_review`, address or override findings, finalize commit, and push to `origin/master`.

## Surprises & Discoveries

- Observation: Pinned `@anthropic-ai/claude-code` 2.1.326 (`@anthropic-ai/claude-agent-sdk` 0.3.326) has 54 built-in tools and no GitHub-specific tool; Claude composes GitHub polling from `Monitor` (shell command stdout streaming) and `CronCreate`/`ScheduleWakeup`.
  Evidence: `/home/jonathan/.cache/mjolnir/harnesses/claude/claude-agent-acp-0.108.0_claude-2.1.326_pkg3/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs:134` (`BUILTIN_TOOL_NAMES`) and `sdk-tools.d.ts:2895-2991`.

- Observation: Local caches of all eight Mjolnir harnesses show a nuanced native-tool matrix rather than "only Claude has these": Antigravity (`agy` 1.5.11) already has native `schedule` (5-field cron and one-shot timers via `manage_task`) but no `web_fetch` and no stdout-line-streaming `monitor`; OpenCode (2.2.7), DSH (`dsh-acp`), Kimi (2.6.3), Grok (1.0.89), and Muse (1.4.25) all have native URL fetching; Kimi and Muse also have native cron; Muse also has native `monitor`; Codex (0.170.0) has none of the four.
  Evidence: Local harness bundle inspection recorded in reconnaissance report `.mj/agents/a1757d1df137bf7be536c5fe97d9a0b6/tool-capability-investigation.md`.

- Observation: `mj-worker` already has a worker-local mailbox injection path (`DurableRelay::deliver_agent_message` in `mj-worker/src/relay/commands.rs:425`) and idle wake promotion (`mailbox_wake_is_allowed` / `enqueue_mailbox_wake` at `mj-worker/src/relay/commands.rs:3013-3134`), triggered by `dispatch_wake.try_send(())`. Reusing it lets `mj-claude` wake idle sessions and deliver mid-turn/turn-boundary events with zero daemon or store changes.
  Evidence: `mj-worker/src/relay/commands.rs:425-439`, `3013-3134`, and `mj-worker/src/worker_runtime/unix.rs:3926`.

- Observation: On Codex and Kimi, staged profile MCP configuration in `mj-worker/src/worker_runtime/subagents.rs:728-817` and `mj-worker/src/worker_runtime/unix.rs:3115` currently keys off `subagent_role.is_some()`. If a session has no `mj-agents` role, `mj-claude` must still be registered independently of `subagent_role`.
  Evidence: `mj-worker/src/worker_runtime/subagents.rs:729-737` and `mj-worker/src/worker_runtime/unix.rs:3115-3146`.

- Observation: `mj-transcript/src/transcript.rs:621-654` normalizes any MCP server whose name begins with `mj-` or `mj_` to `mj_<tool>` automatically, so `mj-claude` tool calls render cleanly without transcript parser edits.
  Evidence: `mj-transcript/src/transcript.rs:621-654`.

## Decision Log

- Decision: Host `mj-claude` execution inside `mj-worker` over a private session-local Unix socket (`claude-mcp.sock`), with `mj-worker-mcp::claude_mcp` acting as a thin synchronous stdio-to-socket JSON-line bridge.
  Rationale: `mj-worker` already owns the Tokio runtime, `reqwest`, `chrono`, the session's `cwd` and `environment`, the relay mailbox (`RelayCell` + `dispatch_wake`), and worker shutdown teardown. Running the service inside `mj-worker` keeps `mj-worker-mcp` dependency-free, keeps monitors and timers alive across MCP stdio restarts and daemon handoffs, and avoids adding any daemon/control-plane or database state.
  Date/Author: 2026-11-25 / parent agent.

- Decision: Do not add new third-party crates for cron parsing or HTML-to-Markdown conversion; implement compact, self-contained modules inside `mj-worker/src/worker_runtime/claude_mcp/`.
  Rationale: `mj-worker` already has `reqwest`, `chrono`, `tokio`, `tokio-util`, `libc`, and `serde_json`. A 5-field POSIX/Vixie local-time cron evaluator and a streaming/tag-based HTML-to-Markdown converter are small, deterministic, auditable, and avoid expanding `Cargo.lock` or workspace release license reports.
  Date/Author: 2026-11-25 / parent agent.

- Decision: Support both bounded (`timeout_ms`, default 300,000 ms) and persistent (`persistent: true` or `timeout_ms: 0`) monitors, plus explicit `monitor_stop` and `monitor_list` tools under the `Monitor` capability.
  Rationale: Non-Claude harnesses lack Claude's built-in `TaskStop` integration with `Monitor`, so agents need `monitor_stop` and `monitor_list` to manage watches they start. Allowing `persistent: true` (or `timeout_ms: 0`) lets agents compose long-lived subscriptions (such as a GitHub search poll loop) from `monitor` without arbitrary 30-minute expiration churn, while keeping a sensible 5-minute default for ad-hoc command waits.
  Date/Author: 2026-11-25 / parent agent.

- Decision: Eager-whitelist all `mj-claude` tools on every harness that supports Mjolnir-owned MCP auto-approval (`Codex` `default_tools_approval_mode = "approve"` / ACP `defaultToolsApprovalMode = "approve"`, `Antigravity` `mcp(mj-claude/*)` grant, `DSH` `OwnedMcpApprovals`), leave OpenCode's ambiguous `<server>_<tool>` title policy unchanged, and do not eager-load `mj-claude` on Codex (omit `omit_tools_from = ["deferred"]`, matching `mj-review`).
  Rationale: Directly fulfills the user's explicit instruction ("doesn't have to be eager-loaded, should be eager-whitelisted [like all mj tools should be]") while preserving OpenCode's existing title-ambiguity security invariant documented in `mj-worker/src/acp/permissions.rs:506-510`.
  Date/Author: 2026-11-25 / parent agent.

- Decision: Exclude `mj-claude` completely from isolated guardian and before-push reviewers (`LaunchSpec.claude_mcp_socket = None`, and `reviewer_profile.rs` strips any `mj-claude` entry), while enabling it for both top-level and managed child sessions on harnesses that need it.
  Rationale: Isolated reviewers must stay strictly read-only with no extra MCP capabilities; managed children on Codex/other harnesses benefit from `web_fetch`/`monitor` during work just as Claude children have those built-ins natively, and worker shutdown/parking cleanly kills all child monitors and timers.
  Date/Author: 2026-11-25 / parent agent.

- Decision: Manage `ClaudeMcpService` through a cloneable `ClaudeMcpController` reconciled on initial startup and committed harness reload, and remove the initial speculative 5 MiB `web_fetch` body truncation cap.
  Rationale: Pre-push review round 1 identified that in-place harness reloads can refresh credentials/environment or change harness capabilities (including Claude to/from non-Claude), and that a fixed 5 MiB body cap made later pages unreachable via `offset`. `ClaudeMcpController::reconcile` updates active service harness/environment in place, starts the service on Claude-to-non-Claude reload, and stops it on non-Claude-to-Claude reload; reading the full HTTP body under the 30s timeout keeps `offset`/`next_offset` and `truncated` consistent.
  Date/Author: 2026-11-25 / parent agent.

- Decision: Do not modify `go/`.
  Rationale: `AGENTS.md` states "Only work on go/ if you are explicitly instructed to do so."
  Date/Author: 2026-11-25 / parent agent.

## Outcomes & Retrospective

To be completed after Milestone 5 validation and review.

## Context and Orientation

Mjolnir splits responsibilities between a control-plane daemon (`mj-daemon`) and one data-plane worker process per live session (`mj-worker`, packaged inside the `hel` / `mj-worker-bin` executable). Each worker launches an Agent Client Protocol (ACP) bridge for a coding harness (`HarnessKind` in `mj-core/src/config/harness.rs`: `Claude`, `Codex`, `Antigravity`, `OpenCode`, `Dsh`, `Kimi`, `Grok`, `Muse`) and attaches Mjolnir-owned Model Context Protocol (MCP) stdio servers by spawning itself (`hel worker <subcommand>`).

Today, Mjolnir defines three owned MCP servers:
- `mj-agents` (`SUBAGENT_MCP_SERVER` in `mj-core/src/subagent.rs`), served by `hel worker subagent-mcp`. On Claude and Codex private staged profiles it is written into `.claude.json` or `config.toml` with eager-loading (`alwaysLoad: true` on Claude, `omit_tools_from = ["deferred"]` on Codex). On Kimi it is written into `mcp.json`. On Antigravity, DSH, OpenCode, Grok, Muse, and legacy shared Codex profiles it is delivered over ACP in `mj-worker/src/acp/launch.rs`.
- `mj-review` (`REVIEW_MCP_SERVER` in `mj-core/src/subagent.rs`), served by `hel worker subagent-mcp --review-server`. It is intentionally not eager-loaded (no `alwaysLoad` on Claude, no `omit_tools_from` on Codex), while still eager-whitelisted under `ExecutionPolicy::ConfiguredApprovals`.
- `mj-memory` (`MEMORY_MCP_SERVER` in `mj-worker/src/acp/launch.rs`), served by `hel worker memory-mcp`, which bridges stdio JSON-RPC to a local Unix socket (`history.sock`).

Worker-owned MCP approval grants work differently per harness under `ExecutionPolicy::ConfiguredApprovals`:
- Codex staged profiles set `default_tools_approval_mode = "approve"` per server in `mj-worker/src/worker_runtime/subagents.rs:760-775`, and ACP fallback sets `_meta.codex.defaultToolsApprovalMode = "approve"` in `mj-worker/src/acp/launch.rs:544-555`.
- Antigravity stages `mcp(<server>/*)` entries in `config/projects/default-cli-project.json` via `stage_antigravity_grants` in `mj-worker/src/acp/launch.rs:563-735`, driven by `OWNED_MCP_SERVERS` and `owned_acp_mcp_tools(spec)`.
- DSH auto-approves exact `mcp__<server>__<tool>` titles (with underscores rendered as spaces) in `OwnedMcpApprovals::for_launch` (`mj-worker/src/acp/permissions.rs:524-570`), also driven by `owned_acp_mcp_tools(spec)`.
- OpenCode intentionally does not auto-approve MCP calls by title because `<server>_<tool>` titles cannot distinguish a foreign server whose name contains `_` (`mj-worker/src/acp/permissions.rs:506-510`).

Worker-local asynchronous wake delivery works through `DurableRelay` (`mj-worker/src/relay/commands.rs`). Submitting `RelayCommand::DeliverMailboxEvent { event }` with `event.wake = true` appends a durable mailbox event to the worker's relay journal; sending a wakeup token on the worker's `dispatch_wake: tokio::sync::mpsc::Sender<()>` wakes the relay coordinator (`mj-worker/src/worker_runtime/unix/dispatch.rs`), which immediately promotes a `RelayCommand::MailboxWake` turn if the session is idle, or retains the event in `pending_mailbox_events` until the harness mailbox hook drains it at a tool boundary or the active turn ends.

## Plan of Work

The implementation is organized into four clean, independently verifiable milestones followed by integrated validation and pre-push review.

### Milestone 1: Core capability model and RPC wire types (`mj-core`)

Create `mj-core/src/claude_mcp.rs` and export it as `pub mod claude_mcp;` from `mj-core/src/lib.rs` (also re-export `CLAUDE_MCP_SERVER` in `mj-core/src/subagent.rs` if convenient so callers looking alongside `SUBAGENT_MCP_SERVER` and `REVIEW_MCP_SERVER` find it immediately). Define `CLAUDE_MCP_SERVER = "mj-claude"`, `ClaudeMcpCapability` (`WebFetch`, `Monitor`, `Cron`, `ScheduleWakeup`), and `ClaudeMcpCapabilities` with:

    pub fn for_harness(harness: HarnessKind) -> Self
    pub fn is_empty(&self) -> bool
    pub fn contains(&self, capability: ClaudeMcpCapability) -> bool
    pub fn tool_names(&self) -> Vec<&'static str>
    pub fn supports_tool(&self, name: &str) -> bool
    pub fn server_args(socket: &Path, harness: HarnessKind) -> Vec<String>

Implement the exact per-harness capability selection:
- `HarnessKind::Claude`: empty (`&[]`).
- `HarnessKind::Codex`: `[WebFetch, Monitor, Cron, ScheduleWakeup]`.
- `HarnessKind::Antigravity`: `[WebFetch, Monitor]`.
- `HarnessKind::OpenCode`: `[Monitor, Cron, ScheduleWakeup]`.
- `HarnessKind::Dsh`: `[Monitor, Cron, ScheduleWakeup]`.
- `HarnessKind::Kimi`: `[Monitor, ScheduleWakeup]`.
- `HarnessKind::Grok`: `[Monitor, Cron, ScheduleWakeup]`.
- `HarnessKind::Muse`: `[ScheduleWakeup]`.

Define the newline-delimited JSON RPC request/response structs (`ClaudeMcpRpcRequest` and `ClaudeMcpRpcResponse`) in `mj-core/src/claude_mcp.rs` so both `mj-worker-mcp` and `mj-worker` share one schema with zero drift.

At the end of Milestone 1, `cargo check -p mj-core` succeeds cleanly.

### Milestone 2: Stdio MCP server (`mj-worker-mcp`) and CLI entry (`mj-worker-bin`)

Create `mj-worker-mcp/src/claude_mcp.rs` and export `pub mod claude_mcp;` from `mj-worker-mcp/src/lib.rs`. Implement `serve_stdio(socket: &Path, harness: HarnessKind) -> Result<()>` using `crate::mcp_stdio::run_stdio_server`, mirroring `memory_mcp.rs`:
- `initialize` returns `serverInfo` with name `mj-claude` and concise instructions tailored to the capabilities enabled for `harness`.
- `tools/list` returns full JSON Schemas only for the tools in `ClaudeMcpCapabilities::for_harness(harness)`:
  - `web_fetch`: fetches an `http`/`https` URL and returns clean Markdown, plain text, or raw body with `offset` / `max_chars` paging.
  - `monitor`: starts a background shell command (`command`) with a short human-readable `description`, optional `timeout_ms` (default 300,000 ms; `0` or `persistent: true` runs until stopped or session end), delivering stdout lines as wake notifications.
  - `monitor_stop`: stops a running monitor by `task_id`.
  - `monitor_list`: lists active monitors.
  - `cron_create`: schedules a 5-field local-time cron expression (`cron`) with `prompt`, optional `recurring` (default `true`; recurring non-durable jobs expire after 7 days), and optional `durable` (default `false`; persisted under the worker root across restarts).
  - `cron_list`: lists active cron jobs with their next local fire time.
  - `cron_delete`: removes a cron job by `id`.
  - `schedule_wakeup`: schedules a one-shot dynamic-loop wakeup after `delay_seconds` (clamped to `60..=3600`) with `prompt`, `reason`, optional `noop` (default `false`), or cancels pending wakeups when `stop: true` without affecting cron jobs.
- `tools/call` checks `capabilities.supports_tool(name)` first, sends a single JSON-line `ClaudeMcpRpcRequest` over the Unix socket to `mj-worker`, reads one JSON-line `ClaudeMcpRpcResponse`, and returns MCP text content (or `isError: true` with a clear error message if the socket/tool returns an error).

In `mj-worker-bin/src/main.rs`, add the `WorkerCommand::ClaudeMcp { socket: PathBuf, harness: mj_core::config::HarnessKind }` subcommand and dispatch it to `mj_worker_mcp::claude_mcp::serve_stdio(&socket, harness)`.

At the end of Milestone 2, `cargo check -p mj-worker-mcp -p mj-worker-bin` succeeds cleanly.

### Milestone 3: Worker-side `ClaudeMcpService` and wake delivery (`mj-worker`)

First, in `mj-worker/src/relay/commands.rs`, add `pub fn deliver_agent_wake(&mut self, source: &str, key: &str, text: String) -> Result<()>` right beside `deliver_agent_message` (factoring a shared private helper `deliver_agent_mailbox_event(&mut self, source: &str, key: &str, text: String, wake: bool)` so there is one path).

Second, create `mj-worker/src/worker_runtime/claude_mcp/` with four focused modules:
- `mod.rs`: owns `CLAUDE_MCP_SOCKET = "claude-mcp.sock"`, `ClaudeMcpService`, `ClaudeMcpShutdownGuard`, Unix socket listener (bound with mode `0600` after unlinking any stale socket), RPC dispatch, `schedule_wakeup` state (at most one active dynamic wakeup; replacing or `stop: true` cancels the previous timer handle cleanly), and `deliver_wake(key, text)` which calls `relay.lock().deliver_agent_wake("mj-claude", key, text)` followed by `let _ = dispatch_wake.try_send(())`. On shutdown (`CancellationToken` cancelled by `ClaudeMcpShutdownGuard::drop`), every active monitor process group is terminated and every cron/wakeup task is aborted.
- `web_fetch.rs`: performs `http`/`https` GET via `reqwest::Client` (30s request timeout, up to 10 redirects, realistic `User-Agent`, streaming body read capped at 5 MiB so a giant binary download cannot OOM the worker), converts HTML to readable Markdown or plain text (stripping `<script>`, `<style>`, `<noscript>`, `<template>`, and HTML comments; decoding standard and numeric entities; preserving headings, links, lists, paragraphs, and preformatted/code blocks), and slices UTF-8 characters safely by `offset` and `max_chars` (default 32,768), returning `url`, `final_url`, `status`, `status_text`, `content_type`, `bytes`, `duration_ms`, `format`, `content`, `offset`, `next_offset`, `total_chars`, and `truncated`.
- `monitor.rs`: spawns `sh -c <command>` in the session `cwd` with the session `environment` and its own Unix process group (`process_group(0)`). Drains stdout and stderr concurrently (keeping a bounded tail of stderr for error diagnostics so stderr can never block a 64 KiB pipe). Coalesces bursts of stdout lines over a short 200 ms window (or immediately when buffered output reaches 16 KiB) and delivers each batch via `deliver_wake`. Enforces `timeout_ms` unless `persistent: true` or `timeout_ms == 0`. Sends a final wake notification when the process exits or times out, and kills the full process group (`SIGTERM` then `SIGKILL` after 500 ms via `libc::killpg`) on `monitor_stop`, timeout, or worker shutdown.
- `cron.rs`: implements a self-contained 5-field POSIX/Vixie local-time cron parser (`minute hour dom month dow`, handling `*`, `*/n`, `a-b`, `a-b/n`, comma lists, day-of-week `0` and `7` as Sunday, Vixie OR semantics when both DOM and DOW are restricted) and DST-safe `next_fire_after(DateTime<Local>)` computation. Manages active cron tasks, auto-expires non-durable recurring jobs after 7 days, persists `durable: true` jobs atomically to `<worker_root>/claude-cron.json` (and reloads/re-arms them on worker startup), and fires prompts through `deliver_wake`.

Add focused tests covering tricky conditions (per `AGENTS.md` "Which tests to add"):
- Cron parser and local-time next-fire edge cases (Vixie DOM/DOW OR rule, `dow` 0/7 Sunday, step ranges, month-end wraparound, leap day, invalid expressions).
- Monitor pipe backpressure (>64 KiB stdout and concurrent >64 KiB stderr without deadlock), process-group teardown killing descendant child processes on `monitor_stop` and shutdown, and timeout delivery.
- `web_fetch` against a local loopback HTTP server covering HTML-to-Markdown conversion, `offset` / `next_offset` UTF-8 paging, non-`http`/`https` scheme rejection, and redirect handling.
- End-to-end MCP stdio-to-socket integration test driving `mj-worker-mcp::claude_mcp` against a live `ClaudeMcpService` and verifying `RelayCell` receives `wake: true` mailbox events and `dispatch_wake` fires, plus `schedule_wakeup` clamping/replacement/`stop: true` leaving cron jobs intact.

### Milestone 4: Selective harness registration, deferred loading, and eager-whitelisting (`mj-worker`)

Wire `ClaudeMcpService` into `mj-worker/src/worker_runtime/unix.rs` and `mj-worker/src/acp/`:
- Add `pub struct ClaudeMcpSocket { pub path: PathBuf, pub profile_registration: bool }` and `pub claude_mcp_socket: Option<ClaudeMcpSocket>` to `LaunchSpec` in `mj-worker/src/acp/launch.rs` (keeping it `None` for all isolated reviewer/guardian launches and for `HarnessKind::Claude`).
- In `mj-worker/src/worker_runtime/unix.rs`:
  - When `!ClaudeMcpCapabilities::for_harness(config.harness).is_empty()`, bind and start `ClaudeMcpService` on `root.join(CLAUDE_MCP_SOCKET)` with the session's `cwd`, `environment`, `relay`, `dispatch_wake`, and a shutdown guard stored alongside `subagent_socket_guard` in `PreparedAcpSetup` and `StartedBridgeTasks` so every exit path (normal exit, cancel, suspend, SIGTERM) cleanly stops all monitors and timers and removes the socket.
  - On Codex staged private profiles (`configure_codex_mcp_servers` in `mj-worker/src/worker_runtime/subagents.rs`), register `mj-claude` independently of `role` (so even `role: None` gets `mj-claude`), WITHOUT `omit_tools_from = ["deferred"]` (deferred discovery, like `mj-review`), and WITH `default_tools_approval_mode = "approve"` under `ExecutionPolicy::ConfiguredApprovals`. Remove `mj-claude` when not offered.
  - On Kimi staged private profiles (`configure_kimi_subagent_mcp` in `mj-worker/src/worker_runtime/subagents.rs` and `unix.rs`), register `mj-claude` in `mcp.json` independently of `subagent_role` (or remove it when not offered).
  - On Claude staged profiles (`configure_claude_subagent_mcp` in `mj-worker/src/worker_runtime/subagents.rs`) and isolated reviewer profiles (`mj-worker/src/acp/guardian/reviewer_profile.rs`), remove any stale `mj-claude` entry if present.
- In `mj-worker/src/acp/launch.rs`:
  - Add `claude_acp_mcp(spec: &LaunchSpec) -> Option<McpServer>` that returns `McpServer::Stdio(approve_owned_mcp(spec, ...))` when `spec.claude_mcp_socket` is `Some` and `!socket.profile_registration` and `!ClaudeMcpCapabilities::for_harness(spec.harness).is_empty()`.
  - Extend `extra_mcp(spec)` with `claude_acp_mcp(spec)`.
  - Extend `owned_acp_mcp_tools(spec)` with `(CLAUDE_MCP_SERVER, capabilities.tool_names())` whenever `claude_acp_mcp(spec)` is `Some`.
  - Add `mj_core::claude_mcp::CLAUDE_MCP_SERVER` to `OWNED_MCP_SERVERS` so `stage_antigravity_grants` automatically writes `mcp(mj-claude/*)` for Antigravity and cleans it up if ever unregistered.
  - Because `OwnedMcpApprovals::for_launch(spec)` in `mj-worker/src/acp/permissions.rs` reads `owned_acp_mcp_tools(spec)`, DSH automatically eager-whitelists every offered `mj-claude` tool under `ConfiguredApprovals`.
- Add integration/contract tests in `mj-worker/src/acp/tests.rs` and `mj-worker/src/worker_runtime/subagents.rs` verifying:
  - Per-harness selective exposure across all 8 harnesses (Claude omits `mj-claude`; Codex gets all 4 tools families, deferred, `default_tools_approval_mode = "approve"`; Antigravity gets `web_fetch` + `monitor` and `mcp(mj-claude/*)` grant; DSH gets `monitor` + `cron` + `schedule_wakeup` and `OwnedMcpApprovals` allows those exact titles while rejecting `web_fetch` or spoofed titles; Kimi gets `monitor` + `schedule_wakeup`; OpenCode/Grok get `monitor` + `cron` + `schedule_wakeup`; Muse gets `schedule_wakeup`).
  - Reviewers never receive `mj-claude`.

### Milestone 5: Validation, pre-push review, commit, and push

Run targeted tests and strict clippy across all affected crates (`mj-core`, `mj-worker-mcp`, `mj-worker`, `mj-worker-bin`) outside the restricted sandbox, update this ExecPlan's living sections, commit on `master`, call `request_review`, resolve any review findings cleanly, and push to `origin/master`.

## Concrete Steps

All commands run from the workspace root `/home/jonathan/Projects/mjolnir/.mj/clones/68b5a4c0c71f11a34887c78ea6ccb211`. Per `AGENTS.md`, run `cargo test` invocations outside the restricted sandbox (`BypassSandbox: true`) because loopback TCP and Unix domain sockets are exercised.

    cargo test -p mj-core claude_mcp
    cargo test -p mj-worker-mcp claude_mcp
    cargo test -p mj-worker claude_mcp
    cargo test -p mj-worker owned_mcp
    cargo test -p mj-worker antigravity_grants
    cargo test -p mj-worker configure_codex
    cargo test -p mj-worker configure_kimi
    cargo clippy -p mj-core -p mj-worker-mcp -p mj-worker -p mj-worker-bin --all-targets -- -D warnings

## Validation and Acceptance

Acceptance is met when all of the following are demonstrated:
1. Per-harness tool selection matches the verified matrix: `mj-claude` is omitted on Claude, exposes all four capabilities on Codex, `web_fetch` + `monitor` on Antigravity, `monitor` + `cron` + `schedule_wakeup` on OpenCode/DSH/Grok, `monitor` + `schedule_wakeup` on Kimi, and `schedule_wakeup` on Muse.
2. Deferred loading and eager-whitelisting match existing `mj-*` standards: Codex registers `mj-claude` without `omit_tools_from = ["deferred"]` and with `default_tools_approval_mode = "approve"` (both staged `config.toml` and ACP fallback); Antigravity stages `mcp(mj-claude/*)`; DSH `OwnedMcpApprovals` permits every offered `mj-claude` tool and refuses unoffered/foreign ones; isolated reviewers never receive `mj-claude`.
3. `web_fetch` fetches an HTTP page, converts HTML to clean Markdown, supports `offset`/`next_offset` UTF-8 paging, and rejects non-`http`/`https` schemes.
4. `monitor` streams stdout lines (tested with >64 KiB output and concurrent >64 KiB stderr without deadlock) into `RelayCell` with `wake: true` and fires `dispatch_wake`, and `monitor_stop` / worker shutdown terminates the child's entire process group.
5. `cron_create` / `cron_list` / `cron_delete` evaluate 5-field local-time cron schedules accurately, persist and reload `durable: true` jobs under the worker root, and wake the session on trigger.
6. `schedule_wakeup` clamps `delay_seconds` to `60..=3600`, replaces prior pending dynamic wakeups, wakes the session when due, and cancels pending dynamic wakeups on `stop: true` without cancelling cron jobs.
7. All targeted test suites pass with non-zero executed test counts and `cargo clippy --all-targets -- -D warnings` passes cleanly on the touched crates.

## Idempotence and Recovery

All changes are additive to `mj-core`, `mj-worker-mcp`, `mj-worker`, and `mj-worker-bin` with no database migrations, no daemon wire-protocol changes, and no default-instance state modifications. Staged profile configuration writers (`configure_codex_mcp_servers`, `configure_kimi_subagent_mcp`, `configure_claude_subagent_mcp`, `stage_antigravity_grants`) are idempotent: rerunning them updates `mj-claude` in place or removes stale entries cleanly. Socket binding unlinks any leftover `claude-mcp.sock` before binding, and worker shutdown always tears down monitor process groups and removes the socket.

## Artifacts and Notes

Reconnaissance reports supporting the design:
- `.mj/agents/99cb78f93d62ce39540d203d82049fb5/mcp-mj-claude-touchpoint-map.md`
- `.mj/agents/a1757d1df137bf7be536c5fe97d9a0b6/tool-capability-investigation.md`

## Interfaces and Dependencies

In `mj-core/src/claude_mcp.rs`:

    pub const CLAUDE_MCP_SERVER: &str = "mj-claude";
    pub const SCHEDULE_WAKEUP_MIN_SECONDS: u64 = 60;
    pub const SCHEDULE_WAKEUP_MAX_SECONDS: u64 = 3600;
    pub const MONITOR_DEFAULT_TIMEOUT_MS: u64 = 300_000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ClaudeMcpCapability {
        WebFetch,
        Monitor,
        Cron,
        ScheduleWakeup,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ClaudeMcpCapabilities {
        capabilities: &'static [ClaudeMcpCapability],
    }

    impl ClaudeMcpCapabilities {
        pub fn for_harness(harness: crate::config::HarnessKind) -> Self;
        pub fn is_empty(&self) -> bool;
        pub fn contains(&self, capability: ClaudeMcpCapability) -> bool;
        pub fn capabilities(&self) -> &'static [ClaudeMcpCapability];
        pub fn tool_names(&self) -> Vec<&'static str>;
        pub fn supports_tool(&self, name: &str) -> bool;
        pub fn server_args(socket: &std::path::Path, harness: crate::config::HarnessKind) -> Vec<String>;
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ClaudeMcpRpcRequest {
        pub tool: String,
        pub arguments: serde_json::Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ClaudeMcpRpcResponse {
        pub ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
    }

In `mj-worker-mcp/src/claude_mcp.rs`:

    pub fn serve_stdio(
        socket: &std::path::Path,
        harness: mj_core::config::HarnessKind,
    ) -> anyhow::Result<()>;

In `mj-worker/src/relay/commands.rs`:

    impl DurableRelay {
        pub fn deliver_agent_wake(
            &mut self,
            source: &str,
            key: &str,
            text: String,
        ) -> anyhow::Result<()>;
    }

In `mj-worker/src/worker_runtime/claude_mcp/mod.rs`:

    pub const CLAUDE_MCP_SOCKET: &str = "claude-mcp.sock";

    pub struct ClaudeMcpShutdownGuard { /* cancels token, kills monitors, removes socket */ }

    pub fn start_claude_mcp_service(
        worker_root: &std::path::Path,
        cwd: std::path::PathBuf,
        environment: std::collections::BTreeMap<String, String>,
        harness: mj_core::config::HarnessKind,
        relay: std::sync::Arc<crate::relay::RelayCell>,
        dispatch_wake: tokio::sync::mpsc::Sender<()>,
        runtime: &tokio::runtime::Handle,
    ) -> anyhow::Result<ClaudeMcpShutdownGuard>;

No new workspace crate dependencies are added. `mj-worker-mcp` stays free of async/HTTP crates; `mj-worker` reuses its existing `reqwest`, `chrono`, `tokio`, `tokio-util`, `libc`, `serde`, and `serde_json` dependencies.
