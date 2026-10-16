# Keep delegation history out of session control

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture


A healthy session must stay reachable after collecting many child reports. The worker (the process running the agent) will keep durable tool requests and replies, while its connection to the daemon (the controller process) exchanges only pending work and compact completion receipts. Normal polling sends changes, and acknowledgments retire reply bodies after both recipients have finished with them. Installation must recover an existing oversized queue without deleting accepted work.

## Progress


- [x] (2026-10-15) Diagnosed a retained 256-result response exceeding the relay's 8 MiB frame limit; checked current workers and daemon logs.
- [x] Read protocol, worker socket, session actor, delegation coordinator, installation, and plan instructions. Working on current master; no branch changes authorized.
- [x] (2026-10-15 22:32Z) Implemented byte-bounded acknowledged pages, private reply files, caller/daemon receipts, legacy import, and acknowledgment-based retirement.
- [x] (2026-10-15 22:32Z) Updated compact controller observations and protocol compatibility. Delegation replication has its own supervised transport so failed polls cannot break control or idle upgrade admission.
- [x] (2026-10-15 22:32Z) Added real-wire oversized-history regression, paging/completion ordering, durable acknowledgment retirement, MCP large-reply acknowledgment, and control-isolation coverage; test execution is pending.
- [x] (2026-10-15) Focused checks passed: eight worker delegation cases including oversized history; 60 sub-agent unit tests plus one integration case; both corrected control-isolation and returned-lease regressions. Workspace Clippy, formatting, and diff whitespace checks passed.
- [x] Full suites exercised all five touched crates. Final unit results: controller 2,006; worker 683; core 502 plus three integration cases; chat 412; CLI 212. All other integration targets passed; two native-recovery startup deadline failures passed on serial rerun. Final workspace Clippy passed. Rust test deadlines were unchanged; final suite concurrency was eight.
- [x] Matching standalone binaries built successfully. The complete isolated delegation/handoff e2e passed, including parked waits, messages, legacy attachments, task types, startup cleanup, retained usage, and session overrides. Measured handoff: 0.562 seconds. Log: `/mnt/optane/delegation-feed-e2e-recorded-turn.log`.
- [x] Committed implementation `f9d9cdb88`, merged the disjoint upstream UI change as `d69be1249`, and pushed master. Merge formatting and viewer unit checks passed.
- [x] Standard release installation completed with all four matching binaries; HEAD stayed frozen throughout the builds. The daemon automatically handed off and confirmed it runs the installed executable.
- [x] Live recovery verified: the affected session is connected and running; its worker negotiates protocol 38 and its SHA-256 matches the installed portable binary. Legacy results are imported, with private reply bodies preserved and new completions progressing.

## Surprises & Discoveries


The previous implementation retransmits every cached result during every session sync and again after completing a tool. The private queue bounds result count at 256 but has no acknowledgment-based retirement. Worker-local steering can complete a wait, so completion receipts must preserve that fact without sending unrelated reports. Message delivery history also reads these cached replies; it needs a compact typed projection.

Automatic approval review rejected publishing an issue containing live-session diagnostic details. No issue was created. Implementation and evidence remain local until the authorized code push.

Upstream advanced to `c832d993f` during implementation. Its #1287 changes serialize cancellation with result completion and distinguish canceled turns from steering interruptions. Stashed this task's files, fast-forwarded the current branch, reapplied the task, and reconciled the shared queue implementation. Compact receipts preserve both interruption reasons. An earlier production `cargo check` passed; in-progress focused compiles were stopped before integration because their source inputs were changing. Final validation will use the combined stable source.

Focused Clippy passed for core, worker, and controller with all targets. The first session-manager run passed 62 of 64 cases. Both failures were the fake relay's extra journal writer: every connection eagerly opened its own durable journal, including the new queue-only connection. The fixture now opens a journal only for control requests; the two failures are being rerun. New workers explicitly reject historical bulk snapshots; only the new controller's connection to an older worker uses that old request. Paging coverage also exercises preparation with no endpoint followed by a reopened persisted queue at generation zero.

Both corrected controller cases passed on isolated rerun. The worker's eight delegation tests and all 60 sub-agent unit cases plus the matching integration case passed. `cargo clippy --all-targets -- -D warnings` passed for the default workspace. Full touched-crate suites are running; logs are `/mnt/optane/delegation-feed-full-tests.log`, `/mnt/optane/delegation-feed-clippy-final.log`, `/mnt/optane/delegation-feed-queue-focused.log`, `/mnt/optane/delegation-feed-control-recheck.log`, and `/mnt/optane/delegation-feed-lease-recheck.log`. The affected live worker's private activity summary was idle with no active or queued prompt; no live process or data has been changed.

The first full run passed all 412 chat tests and 1,987 controller tests, with 19 controller failures. The checkpoint and move fake workers had the same multiple-journal-owner assumption as the session-manager fixture; their queue-only connections now answer without opening a journal, and control-connection counters still measure control reuse. One existing lifecycle test also shared the process-wide upgrade gate with concurrent handoff tests; it now runs in an isolated process like the existing shutdown-admission regression. Focused reruns and the real-process delegation/handoff e2e are in progress. Review also tightened reply receipt ownership: a pending-tool status never advertises an acknowledgment, and the worker only accepts receipts for completed replies. The retirement regression now sends an early receipt and proves it cannot retire a later body.

The default parallel run finished with 2,005 controller passes and one existing delayed-startup timeout; that exact case passed on its isolated rerun. The worker run finished with 680 passes and three deadline failures during concurrent filesystem-heavy tests; its new oversized-history and receipt-retirement regressions passed. All core, chat, CLI, worker-main, and worker integration suites passed. The real-process delegation run reached handoff, reports, accounting, and multiple parks before its 20-second park assertion expired while a later stop was still progressing; cleanup then terminated that operation. Final validation uses eight test threads without extending deadlines, followed by a fresh e2e run.

The last source change makes acknowledgment eligibility come from the reply-producing branch itself. An early `send_input` queued placeholder is ineligible even if completion races the subsequent socket acknowledgment. Cached and durably completed replies are eligible; synthesized pending/wait status is not. The existing stored-input socket regression asserts the capability is absent, and the reply-retirement regression rejects receipts for work that has not completed. This removes the race rather than checking another snapshot of completion state.

A second real-process e2e attempt reproduced the 20-second park assertion. The trace shows control and delegation requests completing after up to 13 seconds, a recovery checkpoint being superseded, and the subsequent park still progressing when test cleanup kills its relay. `mj-controller/src/daemon/subagent_park.rs` already allows 120 seconds for the park lifecycle. The Python observation now waits the same 120 seconds and fails immediately on child Error; no production deadline changed. This is a harness correction, so only Python syntax/diff checks and the affected e2e rerun are needed.

The park-budget e2e run then exposed an accounting-baseline race in the old test: `COUNT(unfinished) == 0` can be true before a parent prompt's selection has reached the database. Destroy later allowed its real usage to arrive, changing an incorrectly early baseline. The fixture now asks the worker for the actual active/completed command identity after the user message is observed and waits for that exact command's durable usage row. It no longer decides completion from a transient absence of selection rows. Python syntax/diff checks passed, and the complete e2e subsequently passed.

## Decision Log


- Decision: Keep durable queue ownership in the worker. Add protocol 38 for a connection-scoped incremental feed with byte-bounded pages and explicit page acknowledgments. Reconnecting rebuilds the feed from worker-owned state.
  Rationale: Worker turns and pending tools must survive daemon replacement; historical reply bodies must not determine control connectivity.
  Date/Author: 2026-10-15 / Codex.
- Decision: Keep full reply bodies in worker-private files and compact receipts in the queue. Require caller receipt plus daemon receipt before retiring new reply bodies; retain compact duplicate-request identities. Import the old queue without dropping unacknowledged work.
  Rationale: A lost acknowledgment must never authorize repeating a child mutation, and completion recovery needs metadata rather than full historical reports.
  Date/Author: 2026-10-15 / Codex.
- Decision: Preserve the existing exchange only when talking to older workers. Do not raise the frame limit or discard historical reports to restore access.
  Rationale: Protocol compatibility must permit idle worker replacement during ordinary upgrades.
  Date/Author: 2026-10-15 / Codex.
- Decision: Give delegation replication a separately supervised relay connection owned by `SubagentQueueObserver`; publish queue errors independently of control connectivity and back off failed polls up to 30 seconds.
  Rationale: Separating payload shapes alone does not recover an already broken legacy worker or isolate future queue errors. The control connection must remain usable for status, steering, and atomic idle replacement even when delegation polling disconnects.
  Date/Author: 2026-10-15 / Codex.

## Outcomes & Retrospective


The redesign and Rust validation are complete. Reports stay in worker-private files, normal polls exchange bounded changes and compact receipts, and delegation transport failures leave session controls attached. The byte-boundary, lost-acknowledgment, restart, cancellation, and receipt-ordering regressions pass. The complete real-process e2e also passed with a 0.562-second daemon handoff. Implementation `f9d9cdb88` and merge `d69be1249` were pushed to master and installed through the standard release installer. The daemon automatically handed off to the installed executable. The formerly unreachable session is connected and running on protocol 38; its live worker exactly matches the installed portable binary. No implementation or delivery work remains.

## Context and Orientation


`mj-worker/src/worker_runtime/subagents.rs` owns the durable MCP tool queue and the private Unix socket used by `mj-worker/src/subagent_mcp.rs`. `mj-worker/src/worker_runtime/unix.rs` serves daemon relay requests. Shared wire types and protocol versions live in `mj-core/src/relay.rs`, `mj-core/src/relay/protocol.rs`, and `mj-core/src/subagent.rs`.

`mj-controller/src/worker_client/reviewer.rs` sends delegation requests. `mj-controller/src/session_manager/standalone.rs` synchronizes the worker and publishes a `ManagedSessionSnapshot` from `mj-core/src/state.rs`. `mj-controller/src/daemon/delegation.rs` executes queued work and persists results in SQLite until worker delivery. `mj-controller/src/server_runtime/api/subagent_input.rs` renders delivery history; `child_wait.rs` observes worker-completed steering waits. No database migration is intended.

## Plan of Work


First introduce shared compact receipts and a feed page consisting of request, completion, and retirement changes. The worker retains an unacknowledged page per connection, replays it until acknowledged, and compares acknowledged metadata against current worker state to generate the next page. Individual requests already enter through a 1 MiB socket boundary; pages use the existing 4 MiB relay replay budget.

Move reply bodies from the queue JSON into worker-private result files written before publishing their receipts. Preserve old result JSON by importing it on open. Caller acknowledgments use a separate small private socket request advertised in new replies, preserving existing socket clients. Daemon page acknowledgments confirm receipt observation. Both acknowledgments and receipt identities survive worker restart. Retire bodies only when safe; old retained replies remain a bounded local retry cache.

Update controller snapshots to carry compact receipts. Message-delivery projections use receipt metadata. Worker steering interruptions carry a small typed completion that reconstructs their exact wait reply. The controller keeps its feed cursor and applies bounded changes; new connections reconstruct metadata, and old workers use their existing protocol. A completed tool must return its direct acknowledgment without a second full-history fetch.

Then exercise the real Unix relay and persisted queue with report bodies totaling more than 8 MiB. Check page replay after a lost acknowledgment, reply retirement after acknowledgments in either order, duplicate completion, reconnect/reopen, and a request transition during pagination. Run the existing isolated delegation and upgrade regressions. Finish with normal installation, allowing its automatic handoff to replace the daemon and idle workers.

## Milestones

The first milestone is the worker-owned exchange. Protocol 38 carries bounded queue changes, replies live in private files, and both recipient acknowledgments govern retirement. The real-wire oversized-history and receipt-ordering regressions demonstrate that accepted work survives reopening without transferring report history.

The second milestone is independent controller replication and validation. A supervised queue connection publishes compact facts and failures while the control actor stays usable. The broken-queue control regression, full touched-crate suites, workspace Clippy, and the isolated delegation/handoff run must pass. Fixture relays must preserve one control-journal writer even when clients connect independently.

The final milestone is delivery to the live installation. Commit and push the validated current branch, run the standard installer without changing HEAD during its builds, then observe automatic daemon handoff and idle worker replacement. Acceptance includes a connected formerly unreachable session with its imported queue intact.

## Concrete Steps


Run all commands from the repository root. Use the existing mbx-managed `target` symlink; never redirect Cargo storage. Package names are `brokk-mj-core`, `brokk-mj-worker`, and `brokk-mj-controller`.

    cargo test -p brokk-mj-worker <focused-filter>
    cargo test -p brokk-mj-controller <focused-filter>
    cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-chat -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    git diff --check

The isolated live-process validation uses `MJ_INSTANCE=delegation-feed-38-e2e MJ_RELIABILITY_ARTIFACTS=/mnt/optane/delegation-feed-e2e python3 tests/e2e/delegation.py --mj target/debug/mj --worker target/debug/mj-worker --instance delegation-feed-38-e2e`. The Lab supplies its own config/data directories and recorded ACP harness. Keep the named instance in the parent environment as well as the CLI arguments so every helper inherits it. Cargo integration tests build the actual debug binaries; build the same two binaries with normal Cargo storage only if they are missing.

Every Cargo test runs with escalated permissions. Runtime/e2e tests use their own config/data and a named instance, for example `--instance delegation-feed-38`; never run a development binary against the default instance.

After validation, stage only changed files and commit on the current branch. Push upstream. If upstream moved, fetch and merge without rebasing, resolve and validate relevant conflicts, then push. Run `scripts/install.sh` after the final commit; do not change HEAD while it builds. Observe the installed daemon after handoff, inspect worker versions and connection state, and report any remaining limitations.

## Validation and Acceptance


A queue containing more than 8 MiB of historical result bodies must synchronize through the live wire without including those bodies or disconnecting. A subsequent unchanged poll must contain no queue changes. Lost page acknowledgments must replay the same page; completed requests must not execute again after restart or reply retirement. An old worker must remain readable by the new controller. Existing delegation tools, report collection, message status, steering, and daemon restart regressions must pass in isolated instances.

## Idempotence and Recovery


Write reply files before publishing queue metadata so interrupted writes cannot expose missing accepted results. Keep page acknowledgment state separate from durable command effects. Repeated completion and acknowledgment are idempotent. Preserve worker files and work throughout installation. A busy worker waits for idle replacement; never cancel its turn to upgrade it. If legacy bulk state prevents ordinary connection, diagnose and implement bounded upgrade admission rather than deleting state or killing an active worker.

## Delivery evidence

The release installer completed both worker builds (4m30s and 11m49s) and the host build (24m37s), then installed `mj-worker`, `mj-worker-x86_64-unknown-linux-musl`, `mj-voice-worker`, and `mj`. The source and HEAD remained at `d69be1249` throughout. Build waits included shared scheduler and local ZFS pressure; the pool was healthy and had ample free space. No mount, storage layout, or build configuration changes were made.

The installed management client reported daemon build `2.46.0+d69be124` and `This daemon runs this build.` The affected session's API relay health was `connected`, with no queue error, and its lifecycle was running. The live worker's hello negotiated protocol 38, and its executable SHA-256 equaled the installed portable worker's SHA-256.

Initial recovery verification found a 100,495-byte queue with zero legacy results, 268 compact completions, and daemon acknowledgment of all 268. There were 267 private reply bodies totaling 8,749,603 bytes, with one caller receipt permitting one consumed body to retire. A pending request and additional completions confirm processing resumed. The prior 8,440,644-byte historical response is no longer sent through session synchronization.

Evidence logs are `/mnt/optane/delegation-feed-install.log`, `/mnt/optane/delegation-installed-daemon-status.txt`, `/mnt/optane/delegation-recovered-session.json`, and `/mnt/optane/delegation-recovered-worker-verification.json`. Runtime incident identifiers remain out of the published plan.

## Artifacts and Notes


The initial worker error was `relay response frame is too large`; the daemon saw `relay proxy disconnected during subagent_requests`. Keep incident identifiers out of published tracking text. Validation evidence and final commit/install identities will be recorded here.

## Interfaces and Dependencies


Reuse serde, Tokio, the existing local socket helpers, SHA-256 for safe result-file names, and existing atomic file writes. Add no crate. Shared protocol types describe `SubagentCompletion`, `SubagentQueueChange`, and `SubagentQueuePage`. Keep connection-local replication state with the relay connection and durable delivery facts in the worker queue. Maintain the protocol-31 compatibility boundary for completion delivery status rather than comparing that older feature against the changing current protocol constant.

Revision note (2026-10-15): Initial executable plan records the authorized redesign, compatibility constraints, validation, push, and install.

Revision note (2026-10-15 22:32Z): Recorded implemented interfaces, independent transport ownership, and integration of upstream cancellation fixes before final validation.

Revision note (2026-10-15): Recorded the first focused results and fixture ownership correction. CLI and chat snapshot constructors also require the shared compact receipt fields, so their full suites join final validation.

Revision note: recorded full-suite fixture failures and the pending-status acknowledgment guard before final validation.

Revision note: recorded resource-related full-run failures and serialized reply-origin acknowledgment eligibility; final suites use bounded test concurrency.

Revision note: recorded completed Rust validation, including the unchanged-deadline serial recovery reruns; matching standalone binaries and the e2e rerun remain before delivery.

Revision note: aligned the e2e park observation with its existing production budget after tracing cleanup-induced failures; Rust source and validation remain unchanged.

Revision note: recorded the ownership-based accounting wait and successful complete e2e before commit and delivery.

Revision note (2026-10-16): recorded successful push, standard installation, automatic daemon handoff, matching live worker, and preserved private replies after recovery.
