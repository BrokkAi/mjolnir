```md
# Session mailbox and GitHub watch

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

Agents in mj sessions cannot learn about things that happen outside the session, such as a new GitHub issue that concerns their work or a review comment on a pull request they opened. After this change each session has a mailbox. External events go into the mailbox, and the agent receives them quickly: during a turn, at the next tool boundary through a harness hook; when idle, either with the next prompt or, for urgent events, through a prompt that mj starts itself.

The first producer is a GitHub watcher in the daemon. It polls the GitHub repositories of projects that have live primary sessions. For each new issue or pull request it asks Jev (mj's hosted classifier) two questions per session: is this session interested in the item, and did this session create it. An interested session gets a mailbox note with the item's number and title. A creator session gets its item watched: every new comment or review on that item goes to the creator's mailbox and wakes it if idle. Watches, cursors and undelivered events are stored in the daemon database, so they survive restarts.

To see it working: in an isolated instance, start a Codex session and a Claude session, send `mj event --session ID --wake "test event"` while each agent runs a long command sequence, and observe the event arrive inside the running turn at the next tool call (transcript notice "Delivered 1 mailbox event"), then send one while idle and observe a new turn start. With the watcher enabled and a fake GitHub server, create an item and a comment and observe the notes.

## Progress

- [x] (2026-10-06) Mapped relay queue, steering, harness hooks, Jev, compaction rendering, GitHub credentials, database and daemon services. Agreed design with the user.
- [x] Milestone 1: worker mailbox (relay command, drain request, hidden prompt attachment, idle wake, hook client command, transcript notices). Implementation and targeted tests pass; requested all-target clippy awaits a test-target compile fix in the concurrent GitHub watcher work.
- [x] Milestone 2: install drain hooks in staged Codex and Claude profiles.
- [ ] Milestone 3: Implementation complete (core, proxy, controller client, renderer); proxy checks, core tests/clippy, and live classifications pass; controller test/clippy remain pending on concurrent milestone 4 compile fixes.
- [x] (2026-10-06) Milestone 4: daemon outbox, external event API and CLI, GitHub watcher, migration 75; full touched-crate suites and touched-crate clippy pass.
- [ ] Milestone 6: replace the `mj-agents` `interrupt` tool with `send_message`, delivered through the child's mailbox.
- [ ] Milestone 5: live validation in an isolated instance with Codex and Claude; full test and clippy run.
- [ ] Merge to master, push, run `scripts/install.sh`.

## Surprises & Discoveries

(none yet)

## Decision Log

2026-10-06: Deliver during a turn through harness tool hooks, not only through steering. Automatic steering is skipped for harness-started turns, after a failed or returned steer in the same turn, behind checkpoints, and for harnesses without steering (`mj-worker/src/relay/commands.rs::automatic_steer_target`), so an event could wait hours. A `PostToolUse` hook fires at every tool boundary. Both pinned harnesses support command hooks that return `additionalContext`: Codex `PostToolUse` (`codex-rs/hooks/src/engine/discovery.rs`), Claude `PostToolBatch`.

2026-10-06: The worker owns the mailbox. It owns the relay journal and keeps running without a daemon, and one owner must decide whether an event was delivered by hook, attached to a prompt, or carried by a wake prompt. All three paths claim events inside `DurableRelay`, so the same event cannot be delivered twice.

2026-10-06: Comment events wake an idle session; interest notes do not. Interest notes are speculative, and starting turns for them would spend tokens. They ride along with the next prompt (hidden prompt context, the same mechanism as `pending_user_shell_contexts`) or the next hook drain.

2026-10-06: Deliver every comment with its author login, including comments posted by the agent itself through the user's account. The agent can ignore its own; the user's own comments, which may be instructions, still arrive.

2026-10-06: Classify new items only against live primary sessions (not children, not stopped sessions). Comment events for a stopped creator wait in the daemon outbox until it has a live worker again.

2026-10-06: External content is untrusted. Mailbox text is rendered inside an explicit untrusted-data wrapper that tells the agent the content comes from a third party and is not an instruction from the user.

2026-10-06: The Jev request puts the GitHub item before the session context so the provider's prompt prefix is shared across the sessions classified for one item. Session context is the compaction rendering (`mj-controller/src/compaction.rs`) of the session's last three user turns.

2026-10-06: Add a generic external event API (`POST /api/v1/sessions/{id}/events`, `mj event`). It is the general answer to "push external events into a session", and it makes the mailbox testable without GitHub.

2026-10-06: The user asked to replace the `mj-agents` `interrupt` tool with `send_message`, which delivers a message from the parent to a child through the child's mailbox. A running child receives it at its next tool boundary without its turn being cancelled. An idle child is woken (`wake: true`), and a parked child is unparked first, as `send_input` already does (`mj-controller/src/server_runtime/api/subagent_input.rs::deliver_subagent_input`). Cancelling a child's turn is no longer a separate tool; `close` stops a child. `send_input` keeps its meaning: queue a new turn.

2026-10-06: Milestone 2 installs Claude `PostToolBatch` in the staged `settings.json`, merging existing hook groups idempotently. The worker resolves its command to the target's absolute `hel` executable and `<worker_root>/control.sock`, so local, container and remote workers use their own paths. Codex injects `PostToolUse` into the per-thread `CODEX_CONFIG` passed as `thread/start.config`; the worker resolves the same target-local executable and socket. In a temporary `CODEX_HOME` config, pinned Codex CLI 0.159.1 `hooks/list` discovered the equivalent generated command and reported its exact hash as trusted; the pinned ACP bridge source confirms the `CODEX_CONFIG` to `thread/start.config` path.

2026-10-06: Codex source inspection found that `bypass_hook_trust` in `thread/start.config` is lifted into a global `ConfigOverrides` value and applied by discovery to every hook source. It would admit enabled untrusted hooks from user and project layers too. Use the supported per-handler `hooks.state` entry keyed by `/<session-flags>/config.toml:post_tool_use:<group>:<handler>` and the Codex normalized-handler hash instead; this trusts only the generated mailbox handler. A pinned Codex 0.159.1 probe reported that handler `trusted` while an untrusted project hook remained `untrusted`. Evidence is in `.mj/agents/2f2860a6b895602f4394b9b03d5c88ad/codex-hook-probe.md`.

2026-10-06: The GitHub item request bounds the serialized item prefix at 48 KiB and reserves 16 KiB for session context. The item body budget depends only on the item, so transcript length cannot change the prefix shared across sessions; excess context is removed from its oldest end.

2026-10-06: Controller continuation and GitHub classification share one bounded JSON transport for the 10-second timeout, disabled redirects, and 64 KiB request/response limits, while retaining separate direct and hosted payload shapes.

2026-10-06: The relay owner creates an idle wake prompt only at a turn boundary, with no queued prompt, cancellation, checkpoint, or close barrier. A wake is an ordinary queued prompt, so the existing automatic-steering rules apply after it starts; a later user prompt may steer that active turn when normal admission permits.

2026-10-06: Hook and hidden-prompt deliveries journal `MailboxEventsDelivered` with their path. An idle wake claim is the journaled `CommandQueued(MailboxWake { events })`, which persists the exact claimed event contents across worker restart.

2026-10-06: Mailbox adds relay protocol 33 and breaking relay state revision 16. Pending events, delivered-key deduplication, and prompt claims are worker-owned durable state; an older writer must not silently erase them.

2026-10-06: Keep delivered mailbox event keys only for the shared 512-entry lost-ACK retry window used by terminal command IDs. Pending event keys remain represented by the pending queue without pruning; delivered-key pruning is deterministic by journal ordinal, then event key. This stays within unreleased relay state revision 16, whose reader upgrades revision-15 snapshots and replays their existing journal.

2026-10-06: Migration 75 is compatible: it adds only mailbox/GitHub tables with no session foreign keys, so an older session writer cannot cascade-delete pending events. The current daemon prunes rows after observing a destroyed session; the compatibility floor remains 74.

2026-10-06: Keep the GitHub issues request URL stable for ETag validation and apply the creation watermark locally. Process new items oldest-first; when the per-poll classifier cap is reached, advance only through fully classified items and clear the ETag so the remaining backlog is fetched again.

2026-10-06: The outbox service is the sole mailbox delivery owner. Producers wake it after committing rows, session-view publication wakes it after worker readiness changes, and a five-second sweep recovers missed notifications. It retries with an event-key-derived command ID and only submits to protocol-33-or-newer workers; unpark is an explicit outbox option.

2026-10-06: A GitHub watch is unique per repository item because the specified comment event key is item-global and the outbox has one target per key. If multiple sessions classify an item as created, the first serialized database commit owns that watch.

## Outcomes & Retrospective

(not yet)

## Context and Orientation

mj has a daemon (crate `mj-controller`, the control plane: database, HTTP API, lifecycle) and one worker per session (crate `mj-worker`, the data plane: runs the harness over ACP and owns the relay journal). Shared types live in `mj-core`. The transcript projection lives in `mj-transcript`. Read `AGENTS.md` sections "Control plane and data plane" and "Testing Guidelines" before starting.

The relay is the worker's durable command queue. The daemon submits a `RelayCommand` (`mj-core/src/relay/snapshot.rs`) through `RelayRequest::Submit` (`mj-core/src/relay/protocol.rs`); the worker journals it in `DurableRelay::submit_command` (`mj-worker/src/relay/commands.rs`) and dispatches prompts to the harness in `claim_pending_commands_up_to`. When a prompt is claimed, pending hidden contexts are attached to it (`hidden_prompt_context`, around `commands.rs:942`). `RelayCommand::RecordNotice` adds a transcript line that the agent never sees. `RelayCommand::HandbackReminder` is an example of an mj-authored prompt with its own command type and stable command ID. The relay protocol version is 32 (`mj-core/src/relay.rs`); requests carry `minimum_protocol()`.

The worker serves relay requests on `<worker_root>/control.sock` (mode 0600). `mj-memory` reaches it through `hel worker memory-mcp --history-socket <control.sock>` (`mj-worker/src/acp/launch.rs::project_history_mcp`, `mj-worker/src/memory_mcp/history.rs::Client::send`); new connection-only requests are handled near the `HistoryQuery` branch in `mj-worker/src/worker_runtime/unix.rs::serve_worker_socket`, and must be listed in `mj-core/src/worker_protocol.rs::is_served_relay_method`.

Harness profiles are staged per session. Codex gets `CODEX_HOME` with a session-private `config.toml`, where `mj-worker/src/worker_runtime/subagents.rs::configure_codex_mcp` registers `mj-agents`. Claude gets `CLAUDE_CONFIG_DIR` with staged `.claude.json` and `settings.json`, edited by `mj-controller/src/controller/worker_binary/staging.rs::configure_claude_subagent_mcp`. Codex hooks are read from `[hooks]` in config layers or `hooks.json` beside `config.toml`; non-managed hooks need hook trust or `bypass_hook_trust`. Codex `PostToolUse` input and output schemas are in `codex/codex-rs/hooks/schema/generated/post-tool-use.command.*.schema.json`; the output is `{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"..."}}`. Claude settings command hooks include `PostToolBatch`, which runs once after all tools of a batch and before the next model request, with the same output shape (`hookEventName":"PostToolBatch"`). Hook commands run outside the harness tool sandbox but on the same target as the worker.

Jev is an HTTP classifier. The worker and the daemon call TypeSafe SystemOne directly when `TYPESAFE_API_KEY` or `~/.secrets/typesafe_api_key` exists, and otherwise call the Cloudflare proxy in `services/jev-proxy`. The daemon already does this for continuation (`mj-controller/src/continuation.rs::classify`, `mj-core/src/continuation.rs`, `services/jev-proxy/src/continuation.ts`); that is the pattern for a new question family. Requests and responses are capped at 64 KiB. Decisions are logged as JSONL by `mj_core::jev::DecisionLog`.

Compaction renders a `CanonicalSessionSnapshot` into `<turn>` blocks in `mj-controller/src/compaction.rs` (`turns_from_snapshot`, `render_turns`). The daemon loads a full projection with `database::load_materialized_session` and converts it with `mj_transcript::projection::canonical_session_from_materialized` (as in `mj-controller/src/controller/checkpoint/latched.rs`).

GitHub credentials for the host come from `GH_TOKEN`, `GITHUB_TOKEN`, then `gh auth token` (`mj-controller/src/controller/backend.rs`), or a GitHub App installation token (`mj-controller/src/controller/github_app.rs`). A session's repositories are in its accepted project snapshot (`SessionRecord.project`, `RepositoryIdentity::Github(owner, repo)`). The daemon schema is revision 74 (`mj-controller/src/database/schema.rs`); migration 73 (`session_restart_intents`, accessors in `database/session_restart.rs`) is the pattern for a small compatible table. The periodic-service pattern is `mj-controller/src/controller/mbx/service.rs`, started in `mj-controller/src/daemon/process.rs`.

## Plan of Work

Milestone 1 adds the mailbox to the worker. A new `mj-core/src/mailbox.rs` defines `MailboxEvent` and the single rendering function. A new `RelayCommand::DeliverMailboxEvent` adds an event to the relay snapshot's pending mailbox, deduplicated by key, and records a transcript notice. A new connection request `RelayRequest::DrainMailbox` claims all pending events, journals them as delivered with the path that delivered them, and returns the rendered text. At prompt claim, pending events are attached as hidden prompt context for every harness. When the worker is idle with no queued prompt and any pending event has `wake`, the relay creates a `RelayCommand::MailboxWake` prompt carrying all pending events. A new `hel worker mailbox-hook` command reads the hook's stdin, sends `DrainMailbox` to the control socket, and prints the hook output. The relay protocol advances to 33.

Milestone 2 installs the hook. Codex sessions get a `PostToolUse` hook in the session-private configuration with trust bypassed for this hook only if Codex allows it, otherwise for the session config layer. Claude sessions get a `PostToolBatch` hook in the staged `settings.json`, merged with any hooks the user's profile already has. Both run `hel worker mailbox-hook` with the absolute control socket path, resolved the same way as the `mj-agents` MCP paths for local, container and remote targets.

Milestone 3 adds the Jev contract. `mj-core/src/github_item.rs` defines the evidence (item first, then session context), loads `mj-core/src/github_item/questions.json`, and parses answers into `GithubItemVerdict { interested, created }`. The proxy gets a `/v1/github-item-verdict` route with tests that assert item fields precede session fields in the upstream `state`. The workflow `.github/workflows/jev-proxy.yml` path filter includes the new questions file. The controller gets a client modelled on `continuation.rs`. `compaction.rs` exposes a renderer for the last N user turns.

Milestone 4 adds the daemon side. Migration 75 (compatible: new tables only, which older daemons ignore) adds tables for repository cursors, classified items, watches and a mailbox outbox. The outbox is the single daemon owner of event delivery: rows are submitted to the session's worker with a stable command ID derived from the event key, marked delivered on acceptance, and retried when a session gains a live worker that speaks relay protocol 33. `POST /api/v1/sessions/{id}/events` and `mj event` write to the outbox. The GitHub watcher is a supervised periodic service. Each poll groups live primary sessions by GitHub repository, fetches issues and pull requests created after the repository's watermark (the first poll of a repository only sets the watermark), classifies each new item for each session with bounded concurrency, writes classification, watch and outbox rows in one transaction, then polls repository-wide issue comments, pull request review comments, and reviews of watched open pull requests after the comment cursor, and writes outbox rows for comments on watched items. The service holds no handoff admission; its state is durable, so cancellation is safe.

Milestone 6 replaces the `mj-agents` `interrupt` tool (`mj-worker/src/subagent_mcp.rs`, `mj-worker/src/worker_runtime/subagents.rs`, the daemon's sub-agent API under `mj-controller/src/server_runtime/api/`) with `send_message { child_session_id, message }`. The daemon writes the message to the outbox for the child as a waking mailbox event with source `parent` and a stable key from the request ID, unparking a parked child first. The tool description and the delegation instructions that mention `interrupt` are updated. `wait` and `list_agents` report pending and delivered messages the way they report pending input.

Milestone 5 validates end to end in an isolated instance and runs the full checks.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir4`. Run cargo commands outside the sandbox. Use `--instance mailbox-test` (or another non-default name) for every daemon and CLI run of the new build. For each round, run only the tests for what changed (`cargo test -p <crate> <filter>`); the full suites of touched crates and `cargo clippy --all-targets -- -D warnings` run once at the end. Proxy checks: `cd services/jev-proxy && npm run check && npm test`.

## Validation and Acceptance

Mailbox: with a Codex session and a Claude session in an isolated instance, `mj event --instance mailbox-test --session ID --key k1 --wake "hello"` during a turn that runs several tool calls produces a transcript notice that one event was delivered by hook, and the agent's next message shows it saw the text. The same command while idle starts a turn. Without `--wake`, an idle session shows the event queued, and the next user prompt carries it. Re-sending the same key does not deliver twice. Restarting the daemon or the worker between enqueue and delivery loses nothing.

GitHub watch: an integration test runs the watcher against a fake GitHub HTTP server and a fake Jev endpoint, and checks that a new item produces an interest note, a created item produces a watch, a later comment produces a waking event for the creator, a restart in between resumes from the database without re-classifying, and the first poll of a repository does not backfill. A live read-only poll against a real repository in the isolated instance shows the watermark being set and no errors.

## Idempotence and Recovery

Every delivery uses a stable command ID derived from the event key, so resubmission after a lost acknowledgement is accepted as the same command. Classification rows are written in the same transaction as the outbox and watch rows they produce; an interrupted poll classifies the item again, which is harmless. Migration 75 is compatible and additive.

## Artifacts and Notes

Mapping reports from 2026-10-06 are under `.mj/agents/` (not committed): Jev and compaction, hook installation and socket reachability, GitHub polling and database.

## Interfaces and Dependencies

In `mj-core/src/mailbox.rs`:

    pub struct MailboxEvent {
        pub key: String,          // producer-chosen dedup key, e.g. "github:owner/repo#12:comment:345"
        pub source: String,       // "github", "api"
        pub wake: bool,
        pub text: String,         // already summarized by the producer; untrusted
        pub created_at_ms: u64,
    }
    pub fn render_mailbox_events(events: &[MailboxEvent]) -> String;

In `mj-core/src/relay/snapshot.rs`: `RelayCommand::DeliverMailboxEvent { event: MailboxEvent }` and `RelayCommand::MailboxWake { events: Vec<MailboxEvent> }` (prompt blocks are the rendered events). In `mj-core/src/relay/protocol.rs`: `RelayRequest::DrainMailbox { hook_event: String }` returning `RelayResponsePayload::MailboxDrained { text: Option<String>, count: usize }`; both new and the new commands require relay protocol 33.

Hook command: `hel worker mailbox-hook --socket <abs control.sock> --event <PostToolUse|PostToolBatch>`. It reads and discards stdin, prints `{}` when nothing is pending, otherwise the hook output JSON. If the socket is unreachable it prints `{}`, writes a diagnostic to stderr, and exits 0; the events stay pending and are delivered by the next prompt or wake, so no event is lost.

In `mj-core/src/github_item.rs`: `GithubItemEvidence { item: GithubItem { repo, kind, number, title, body, author, url }, session: SessionContext { recent_turns: String } }`, `fn questions() -> serde_json::Value`, `GithubItemVerdict::parse`. Controller client: `mj_controller::github_item_verdict::classify(&GithubItemEvidence) -> Result<GithubItemVerdict>`. Compaction: `mj_controller::compaction::render_recent_turns(snapshot: &CanonicalSessionSnapshot, turns: usize) -> String`.

HTTP API: `POST /api/v1/sessions/{id}/events` with `{ "key": string, "text": string, "wake": bool }`, returns 202. CLI: `mj event --session ID [--key KEY] [--wake] TEXT` (a missing key defaults to a random one).

Configuration: `[github.watch]` with `enabled` (default true) and `interval_seconds` (default 60), plus an `api_base` used by tests.
```
