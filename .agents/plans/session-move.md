# Move sessions between targets and profiles

This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md`. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current throughout implementation. All paths below are relative to `/home/jonathan/Projects/hel2`.

The user authorized implementation on 2026-09-06 and subsequently requested a push. Implement and validate on the current branch, commit only the feature's files, then push to the branch's upstream.

## Purpose / Big Picture


A user can select an active coding-agent session, choose **Move…**, change its target, profile, or both, and keep the same logical session. A target is the configured place where the session runs, such as a Docker container or an SSH host. A profile selects a harness account and configuration; a harness is the coding agent program, such as Codex or Claude Code. Choosing a profile from another harness is supported.

Move combines verified stop and resume into one supervised operation. The controller daemon, Mjolnir's persistent background process, owns the operation independently of an attached terminal or browser. Move restores into a fresh execution environment; it is not migration of process memory, installed packages, or arbitrary files outside the existing recovery contract. The session ID, workspace membership, title, draft, conversation history, and recoverable repository state remain associated with the same session.

Active work is interrupted only after confirmation. By default the destination opens idle, without replaying the interrupted prompt. If queued prompts or configuration commands exist, the confirmation offers **Discard queued work** (selected by default) or **Run after move**, with a count and inspectable contents. The CLI must also expose the operation and require an explicit queue choice for unattended moves with pending work.

The observable success case is a session that changes its displayed target/profile, retains its identity and work, reports the move in its transcript, and is ready on the destination. A failed destination launch leaves a verified checkpoint and explicit recovery actions instead of losing the session.

## Progress


- [x] (2026-09-06) Inspected current stop, checkpoint, resume, worker queue, persistence, daemon lifecycle, and TUI/web action paths.
- [x] (2026-09-06) Agreed on verified stop/resume, interruption confirmation, cross-harness profile changes, all three control surfaces, and bounded performance improvements.
- [x] (2026-09-06) Resolved queue behavior: discard or run choice, default discard; no persistent paused-queue feature.
- [x] (2026-09-06) Wrote this implementation-ready planning artifact; no implementation performed.
- [x] Implement controller orchestration, durable operation state, and recovery; focused database, relay replay, and interrupted-close tests pass.
- [x] (2026-09-06) Started implementation on the clean current branch; established shared Move preparation, request, outcome, and durable operation types. Assigned independent restore optimization, CLI/TUI, and web/documentation work to Luna agents.
- [x] Validate schema 26 migrations and cancellation persistence; verify original command-ID replay after an acknowledged-command crash and reject replacement relay storage.
- [x] Implement and validate daemon/API integration, CLI, TUI, and web controls, including exact prepared confirmation, queue inspection, cancellation, and durable recovery controls.
- [x] Implement and validate bounded performance improvements: direct verified LocalBare archive reads and joined, cancellation-aware cross-harness provisioning/handoff overlap.
- [x] Complete integrated validation, user documentation, and commits on the current branch. Feature commit: `0c8006d1`; upstream integration: `a6fed911`. Push to `origin/master` is the final authorized handoff.

## Surprises & Discoveries


Stop already interrupts active work without steering the next queued prompt into that turn. `checkpoint_session_latched` in `mj-controller/src/hel_controller/checkpoint.rs` uses `LatchExclusivity::HoldThroughClose`, whose `BarrierBusyPolicy` is `InterruptWhileRunning`. It submits `BeginCheckpoint` before `CancelTurn`. `src/hel_worker.rs` contains `cancel_turn_bypasses_a_pending_checkpoint_without_steering_the_queue`. Reuse this behavior; do not implement Move by issuing the ordinary composer cancel action first.

Stop already avoids exporting an identical checkpoint through `CheckpointExportPolicy::ReuseUnchangedArchive`. Resume already overlaps projection reconstruction with provisioning and worker installation with archive transfer. These are existing optimizations, not new benefits to claim for Move.

`resume_session_controlled_with_repository_preflight` currently performs cross-harness transcript compaction before provisioning. This is a candidate for safe overlap. It also uploads the archive into a local-bare worker directory even though the controller and worker share the filesystem; local-bare restore can instead reference the verified absolute archive path.

Podman stop can retain a stopped container temporarily for deferred storage cleanup. `RuntimeState::resume_session` waits for that cleanup before reprovisioning. Retaining that container for reuse would need a new ownership and cleanup protocol, so it is explicitly deferred.

Current resume can begin executing a restored queue during worker startup. Move's explicit Run choice must not execute pending work on a destination that later fails readiness and is rolled back. Restore without the queue first, then admit queued entries after destination readiness, using their original command IDs.

Target compatibility is narrower than arbitrary host-to-host copying. `resume_compatibility` in `mj-controller/src/hel_controller/worktree.rs` supports specific raw-checkout/workspace conversions and restricts raw SSH worktrees to their existing host. Move must present these existing rules honestly rather than broaden them implicitly.

The implementation tree uses database schema version 25, including workspace pane sizes. Move adds schema version 26 through `src/hel_database/schema.rs` and the existing guarded writer.

Implementation discoveries (2026-09-06): the relay needs a persisted store identity in addition to the native session ID, because a recreated worker directory can reuse the same path and native identity but lose command deduplication. Worker snapshot version 3 adds that identity. Partial queue admission also retains a mutation hold after a failed/cancelled attempt and across daemon restart, and blocks checkpoint-floor advancement until admission finishes.

The real local-bare acceptance run exposed an existing data-preservation bug: managed raw worktrees were captured as metadata-only although Stop retires them. Their checkpoints now capture dirty/untracked state relative to the retained local branch's commit, without requiring an origin remote. Unmanaged raw checkouts still remain in place and use metadata-only capture. The acceptance harness checks committed, staged, unstaged, and greater-than-64-KiB untracked content across repeated moves.

Review found that terminal failed/cancelled Move operations can still have a Closing/Destroying source after cleanup failure. Restart recovery now finishes that source teardown independent of the stored Move phase, including deferred target cleanup, before presenting retry. Lifecycle task panics now produce a result and release transient mutation ownership even when clients disconnect.

## Decision Log


Decision (2026-09-06, user): use verified stop followed by resume, accepting that a failed destination leaves the session stopped and recoverable. This keeps the existing integrity and teardown guarantees and avoids a staged two-environment handover.

Decision (2026-09-06, user): support target-only, profile-only, combined, and cross-harness profile changes. Same-harness moves restore native state; cross-harness moves use the existing transcript handoff.

Decision (2026-09-06, user): warn about interruption and permit it in the confirmation dialog. Do not wait indefinitely for the active turn to finish naturally.

Decision (2026-09-06, user): provide TUI, web, and CLI interfaces in the first version. All must call the same daemon-owned operation.

Decision (2026-09-06, user): default to an idle destination. When work is queued, offer Discard or Run after move; Discard is the UI default. This replaces the earlier idea of preserving a durably paused queue. Do not add pause/resume-queue controls or saved multi-draft storage as part of this task.

Decision (2026-09-06, user): use safe overlap first. Rebuild the destination normally, including for profile-only changes; defer retaining containers or restarting a different profile in the existing environment.

Decision (2026-09-06, design): keep one session lifecycle reservation across the operation and persist move intent separately from the underlying session lifecycle state. This keeps the session visible as Moving while the existing stop/resume states change beneath it and permits restart recovery.

Decision (2026-09-06, design): admit retained queued commands only after verified destination readiness. Once any destination queue admission begins, recover on that destination instead of rolling back to the source checkpoint, because accepted work may already have effects.

## Outcomes & Retrospective


Implementation is complete. Move is one daemon-owned operation with durable intent, verified source teardown, ready-before-queue destination admission, same-store command deduplication on retry, and CLI/TUI/web confirmation and recovery. Source settings, workspace, draft/title, history, native identity for same-harness moves, and recoverable Git state are retained. Interrupted turns are never replayed. Final checks and the requested commit/push are recorded below.

The implementation reused normal Stop/Resume behavior but corrected managed raw-worktree capture and the ready-worker-to-session-actor handoff uncovered by the real-worker test. Managed worktrees deliberately bypass relay-frontier-only archive reuse: host Git edits do not advance that frontier, and older metadata-only archives must not be reused before deleting a checkout. No container reuse or new inference service was introduced.

## Context and Orientation


`mj-cli/src/daemon.rs` owns `RuntimeState`, `DaemonAction`, `ResumeSessionRequest`, and the in-memory lifecycle registry. `start_or_join_lifecycle` reserves one operation per session, publishes progress, supervises work, and exposes a result. `close_session` calls the controller close path and may schedule deferred cleanup. `resume_session` waits for cleanup and calls the controller resume path. Move must run within one reservation, not recursively acquire separate Close and Resume reservations.

`mj-controller/src/hel_controller/lifecycle.rs` implements `close_session_managed_controlled`, `recover_interrupted_close_managed`, and `cleanup_stopped_target`. A close persists its intent, captures and verifies an archive, seals the relay at a specific ordered event position, and only then destroys the owned target. `checkpoint.rs` owns the exclusive relay lease and checkpoint barrier. The barrier prevents queued work from overtaking checkpoint capture. Releasing or abandoning an unsealed barrier restores normal dispatch; cancellation cannot undo a turn that was already interrupted.

`mj-controller/src/hel_controller/resume.rs` implements stopped-session restoration, repository-source preflight, cross-harness handoff, provisioning, readiness, and failure rollback. `SessionResumeOptions` currently contains attachments, resource allocation, and `discard_queue`. `worktree.rs` decides compatible destination representations. `provisioning.rs` installs environments and files; `readiness.rs` verifies the worker and native agent session.

`src/hel_state.rs` defines `SessionRecord`, lifecycle states, and recovery reservations. `src/hel_database.rs` and its `schema.rs` module own durable writes and migrations. Do not bypass the daemon's SQLite writer or persist credentials in move records. Worker command acceptance is durable and deduplicated by command ID in `src/hel_worker.rs`; commands and canonical queued entries are defined in `src/hel_worker/snapshot.rs` and `src/hel_archive.rs`.

TUI entry points are `mj-tui/src/lib.rs`, `mj-tui/src/wizards.rs`, the session action/palette modules, and `mj-cli/src/dashboard/actions.rs`. `mj-controller/src/hel_server.rs` defines web actions, capabilities, and operation projections; `mj-cli/src/server.rs` dispatches actions to the daemon; browser code lives under `mj-controller/src/web/`. `mj-cli/src/main.rs` defines the public CLI. The desktop app uses the same web viewer, so it receives Move through that surface.

## Plan of Work


### Milestone 1: durable Move lifecycle and recovery


Add a controller move module, `mj-controller/src/hel_controller/move_session.rs`, exposed through `hel_controller.rs`. Extract the shared internal stop/resume phases needed by it without changing normal Stop and Resume behavior. Introduce a move request/options type that carries the resolved session, profile, target, resource/attachment choices, queue disposition, and interruption acknowledgement. Reuse `ResumeQueueDisposition` or its shared equivalent rather than create incompatible queue vocabularies across clients.

Add a durable move-operation record and additive database migration. Keep existing `SessionState` values; Moving is the higher-level operation projected over them. Store operation ID, session ID, source profile/target and exact target locator, destination selections and nonsecret options, checkpoint identity when available, phase, timestamps, and failure/cancellation information. Use phases Preparing, ClosingSource, ResumingDestination, StartingQueue, and terminal Completed, Failed, or Cancelled. The source locator identifies the exact owned resources, not merely a reusable target template name.

Retain the archive referenced by a nonterminal move, including a failed queue-admission operation, through checkpoint pruning and startup archive reconciliation. Do not copy the transcript or queue into a giant lifecycle response. Read queued entries from the pinned verified archive when needed. Release the move-specific archive retention only when queue admission is finished or explicitly abandoned and the ordinary session recovery reference no longer needs it.

Preparation performs shared compatibility, target access, destination profile configuration, worker availability, resource, and attachment checks without altering the source. Check utility-model availability for cross-harness handoff. Reuse existing harness execution-policy warnings and target prerequisites. Checks requiring the final archive or repository commit boundary run after capture while the source is still retained, before sealing and teardown. A failure there releases the unsealed barrier and reports the problem instead of destroying the source. Never claim preflight proves credentials will remain usable or that provisioning cannot subsequently fail.

At admission, compare current source identity and relevant destination configuration with the prepared selection. If a config edit changed the intended destination or invalidated prerequisites, revalidate or return a new-preparation-required error before interruption; do not silently move to a different host under the same edited target ID. Preserve current settings when compatible. Reuse resume sizing/attachment controls to resolve incompatible destination settings explicitly, never silently drop attachments or resource requests.

Hold the existing lifecycle and recovery reservations through checkpointing, required source cleanup, destination restoration, and initial queue admission. Prevent external prompt/configuration/shell mutations during the operation and keep rejected drafts intact in their clients. Reuse the existing supervised reviewer shutdown/lifecycle handling for Stop; a review must not launch or forward corrective work while Move owns the session. Source processes must be stopped before destination processes can execute session work. Do not delete working files as a substitute for stopping their process owners.

For both queue choices, restore using the equivalent of `discard_queue = true`. Discard then finishes idle. Run reads the source canonical queue, persists StartingQueue after destination identity/readiness is durable, and submits the original commands in order with original IDs. Configuration commands remain configuration commands. Unsupported cross-harness configuration must surface through the existing command error path, not silently disappear or be converted into prompt text. Queue admission errors retain the ready destination and the pinned archive for retry; they must never trigger ordinary pre-readiness rollback after accepted work might have executed.

Mark success only after readiness and the requested queue disposition are complete. A caller choosing Run is guaranteed queue acceptance, not completion of all agent turns. Record a controller-authored transcript notice identifying source and destination and whether queued work was discarded or started. Never automatically replay the interrupted active prompt.

Validate this milestone with controller tests using real local Git fixtures, hand-written process/relay fakes, and persisted state reopened across simulated daemon restarts. Prove both successful state transitions and safe failure boundaries before adding UI code.

### Milestone 2: daemon, CLI, and interactive surfaces


Add a Move lifecycle kind, request dispatch, result, and progress view in the daemon. One accepted operation owns the whole sequence. Normalize the full request before comparing duplicate requests: identical requests join the active result; conflicting target, profile, queue, or resource selections fail as busy. Do not inherit the current same-kind joining behavior if it would ignore different destinations.

Publish Moving and its destination immediately. Keep the session row and existing chat identity visible through the intermediate Stopped/Provisioning states. Use phases such as Checking destination, Stopping, Preparing destination, Restoring, and Starting queued work, together with existing detailed provisioning stages. Read-only status and other sessions remain responsive. Every filesystem scan, process execution, archive operation, and network call runs in a supervised background task or blocking worker, never on a UI/event loop or under a long-held shared state mutex.

The TUI and web viewer expose Move on active sessions. Reuse the resume wizard's profile, target, sizing, and attachment controls. The source workspace membership is fixed for Move; this task does not add moving sessions between UI workspaces. Stopped/lost sessions continue using Resume, except recovery of a failed Move may directly resume its retained checkpoint without trying to stop again.

Confirmation names both profile/target pairs, explains that a fresh environment will restore the workspace and conversation, identifies a cross-harness handoff when needed, and warns if active work will be interrupted. When there are pending commands, show their count and an inspectable list, including configuration entries and attachments, plus the Discard/Run choice. Only the interactive form defaults to Discard. Before acting, recheck activity and pending-work identity; newly active work without interruption acknowledgement or a queue changed since confirmation requires refreshed confirmation. Once admitted, block new submissions so the chosen queue disposition applies to the accepted operation's pending work.

Use the existing cancellation control for a running Move. Closing a browser tab or detaching the TUI leaves Move running. After a recoverable error show Retry move, prefilled with the failed destination, and Resume with previous settings, prefilled with the source settings; both use the shared resume controls and current compatibility checks. The latter provisions/restores as necessary and is not a promise to revive a destroyed process or instance.

Add this public command:

    mj move --session ID [--target ID] [--profile ID]
            [--queue discard|start] [--yes] [--json]

At least one of target/profile is required; the omitted selector retains its current value. Unchanged selections on a healthy session return success with an unchanged outcome and do not interrupt it. A matching retry of a failed Move is recognized before applying the unchanged-selection shortcut. Interactive invocation prompts for confirmation. Noninteractive invocation requires `--yes`, and requires `--queue` when pending work exists; `--yes` explicitly permits interrupting active work but must not imply a queue-discard choice. Human output reports phase progress and the operation ID. `--json` suppresses human progress on stdout and produces one structured final outcome with operation ID, session ID, resolved destination, outcome, and error/recovery details. Errors have a nonzero exit status. CLI Ctrl-C explicitly requests cancellation through the daemon with a bounded wait; an ordinary client connection loss alone does not cancel the operation.

Route TUI, web, and CLI through one validated daemon request. Add Move capabilities to snapshots so clients disable conflicting controls consistently. Cached web clients and old snapshots should follow existing compatibility handling for newly optional capabilities; do not implement a browser-only composition of Close and Resume.

Validate event transitions in the TUI, action payloads and rendering in existing embedded-viewer tests, CLI parsing/confirmation/JSON behavior, and daemon concurrent-request behavior. Show that a detached initiating client can reconnect and observe the same operation.

### Milestone 3: bounded performance improvements


Preserve existing archive reuse, image and Git caches, projection reuse, and parallel installation/transfer. Add phase duration instrumentation that distinguishes preflight, turn cancellation, archive capture/verification/transfer, source cleanup, destination provisioning, cross-harness handoff, and readiness. Use those observations to compare ordinary stop/resume and Move; do not infer speed from fewer API calls.

Refactor cross-harness resume so handoff generation can run concurrently with destination provisioning after the final checkpoint is verified and prerequisite checks pass. Await the handoff before starting the destination harness. Both concurrent tasks remain supervised, cancellable where supported, and joined; a failure cancels/drains its peer and cleans up any partial destination through the established rollback path. Do not abandon a provisioner that might still create or write resources.

For LocalBare only, use the controller's verified absolute archive path in `CheckpointRestoreSpec`, eliminating the upload/copy into the worker root. Keep the archive retained until restore completes and keep worker-side structural/payload verification. Containers and remote targets keep their existing transfer behavior. Respect paths as `Path`/`PathBuf` until the execution/protocol boundary.

Do not add retained-container reuse, profile replacement in a live environment, transfer deduplication across hosts, new inference services, or skipped checksum gates. Potential duplicate Git setup and repeated archive-reading optimizations discovered during research are follow-up candidates, not additional requirements of this milestone.

Validate overlap with deterministic synchronization tests, not fragile elapsed-time thresholds. Validate LocalBare restore with a real archive and repository while an executor rejects an unnecessary archive-copy attempt. Record representative live timings only as supporting evidence.

### Milestone 4: integrated acceptance and documentation


Exercise target-only, profile-only, combined, and cross-harness moves through the common operation, including changing between supported bare/workspace representations. Update human documentation under `docs/src/content/docs/`, including session lifecycle, durability, CLI reference, and relevant terminal/web descriptions. State that Move rebuilds the environment and obeys current resume compatibility; do not claim arbitrary host migration or preservation of installed packages.

Finish required Rust and web checks, record limitations of any untested live target, and commit each coherent validated checkpoint on the current branch. Do not create a branch. Push to upstream after validation, as explicitly requested by the user. Stage only files changed for this feature. The primary implementer owns persistence, concurrency, orchestration, and final integration. Once shared interfaces are settled, substantial independent UI work may be delegated to Luna with explicit file ownership under the repository delegation rules; subagents must not delegate further.

## Concrete Steps


Implementation was authorized; the initial current-branch worktree was clean. Preserve unrelated work and run repository commands from `/home/jonathan/Projects/hel2`.

Useful grounding commands are:

    git status --short
    rg -n 'start_or_join_lifecycle|resume_session|close_session' mj-cli/src/daemon.rs
    rg -n 'checkpoint_session_latched|BarrierBusyPolicy' mj-controller/src/hel_controller/checkpoint.rs
    rg -n 'resume_session_controlled|utility_handoff|discard_queue' mj-controller/src/hel_controller/resume.rs
    rg -n 'resume_compatibility' mj-controller/src/hel_controller/worktree.rs

After each milestone run focused behavior tests for the affected packages. Package names come from the workspace manifests; the relevant ones are `brokk-mj-core`, `brokk-mj-controller`, `brokk-mj-tui`, `brokk-mj-chat`, and `brokk-mjolnir` (the CLI). Every `cargo test` invocation must run with elevated permissions outside the restricted sandbox, because socket-based tests are invalid inside it. Do not redirect Cargo build output into `/tmp`.

For final validation run:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

Run Cargo tests elevated, and elevate other build commands if normal build storage requires it. The existing embedded JavaScript viewer tests in `mj-controller/src/hel_server.rs` execute through Cargo tests; add meaningful behavior coverage there or in the existing web harness rather than inventing a parallel test framework. Run applicable documentation checks if human docs change, reviewing any generated changes before staging.

For a live acceptance run, first inspect configured profiles and targets and select a disposable test session. Never guess a user's profile IDs or move unrelated active work. With a configured Linux container target and two valid same-harness profiles, create a test session through the existing UI, make committed and uncommitted changes, attach an image to a queued prompt, and then execute an explicitly confirmed move such as:

    mj move --session TEST_SESSION --target DESTINATION --profile SECOND_PROFILE --queue discard --yes --json

`TEST_SESSION`, `DESTINATION`, and `SECOND_PROFILE` are placeholders replaced with the actual disposable session and configured entries. Expect the same session ID, the requested destination, a successful structured result, retained repository changes/history, and no automatic agent turn. Repeat with `--queue start` and verify ordered execution only after destination readiness. Exercise another harness when an appropriate profile and utility model exist; otherwise record that live cross-harness validation is unavailable and retain deterministic coverage.

## Validation and Acceptance


Use behavior tests that would fail without Move. A target-only move must preserve identity, title, owning workspace, draft text and supported draft attachments, source repository changes, and conversation history. A profile-only move must actually launch with the selected profile's credentials/configuration. Same-harness readiness must verify the expected native session ID. Cross-harness readiness must install a handoff before any queued prompt runs. Do not compare native IDs across different harnesses.

Preflight tests must prove that incompatible targets, unavailable prerequisites, invalid resources/attachments, and missing cross-harness utility capability fail before destructive source transitions. Include stale confirmation tests for newly active work and changed queues. Reject conflicting mutations while allowing reads and operations on other sessions.

Queue tests must include multiple prompts, images, and configuration commands; preserve order and original IDs for Run and prove Discard starts no work. Inject a crash after the first destination acceptance but before recording progress, then retry against the same durable relay and prove the accepted command is not executed twice. If the destination cannot be proven to be the same durable relay/native session, fail visibly rather than recreate a target and replay potentially executed work. Do not promise exactly-once effects after target storage loss.

Recovery tests must reopen the database at each phase, including immediately before/after close intent, checkpoint installation, source teardown, target locator persistence, destination readiness, and partial queue admission. Prove only one recovery owner acts and that archive pruning retains needed artifacts. Add checkpoint corruption and failed-cleanup cases; the former must never permit unverified teardown, and the latter must retain exact resource identities for cleanup instead of orphaning writers.

Cancellation tests cover preflight, interruption/checkpoint capture, source sealing/cleanup, provisioning, handoff generation, and queue admission. No success is reported while an unjoined background task may still create resources. UI cancellation remains responsive and status describes ongoing bounded cleanup. Cancelling after source sealing may have to finish teardown to reach a coherent stopped state. Once queued work is accepted, cancellation stops further admission and preserves the live destination; it does not silently kill or rewind already accepted work.

Concurrency tests include two unrelated sessions moving simultaneously, an identical duplicate request, a conflicting destination request, automatic checkpoint contention, and a disconnecting initiating client. Use existing test hooks/fakes and real Git repositories where relevant. For archive/pipe streaming paths use fixtures exceeding 64 KiB so pipe-buffer deadlocks and truncation are observable.

Final acceptance requires all required checks to pass, meaningful TUI and web behavior coverage, and a recorded live container/local-bare scenario where the environment supports it. Do not run a real EC2 provisioning test without an explicitly authorized test target and cost context; report its live coverage separately from deterministic backend tests.

## Idempotence and Recovery


A move's persisted phase describes the last durable boundary, not merely the last progress label. Persist intent before a side effect and reconcile exact source/destination identities on restart. Preparing can be abandoned safely. ClosingSource uses the existing interrupted-close recovery and must finish required teardown or report the source recoverable. ResumingDestination cleans up an unready partial destination and retains a stopped retryable checkpoint after failure. A proven ready destination is retained; StartingQueue retries original IDs there and never falls back to replaying on a fresh target.

On daemon startup, move-owned sessions are claimed before generic interrupted-close and retained-cleanup recovery. Recovery is supervised and publishes progress. Where the state cannot establish the safe next action, retain a visible error with Retry move or Resume with previous settings rather than launch another writer. Explicit cancellation is persisted so a restarted daemon does not revive a cancelled move.

Retry move reuses the retained verified checkpoint and failed destination settings, repeating current prerequisites; it must not close the source a second time after teardown already succeeded. Resume with previous settings uses the previous profile/target as a selection, not an automatic rollback promise. Both actions are explicit and run under the same session reservation. Changes in configuration or missing archive/resource state result in actionable errors, not guessed substitutions.

Use the existing schema migration and guarded writer. Store no access tokens, refresh tokens, copied credential files, or credential-bearing environment values in operation records, notices, JSON responses, or checkpoints. Record only configuration selections and safe recovery metadata.

## Artifacts and Notes


Validation evidence (2026-09-06 implementation session; artifact timestamps use UTC 2026-09-07):

- `python3 tests/e2e/session_move.py --hel target/debug/mj` passed. Artifacts: `target/reliability-artifacts/session-move-seed-1-2523228/trace.json` and adjacent controller/worker logs. This uses real local-bare workers, daemon IPC, authenticated web prompt submission, actual Git repositories, and a deterministic fake ACP executable—not real-provider credentials.
- The live scenario preserves session/workspace/native identity and committed, staged, unstaged, and 126,976-byte untracked content through combined, profile-only, queue-start, and target-only moves. It verifies default discard, original queued prompts executed exactly once, unchanged-selection no-op, final Stop, database integrity, and no surviving owned worker processes. Successful Move durations were 5.501, 7.244, 7.795, and 7.071 seconds under concurrent build load. These are supporting observations, not an ordinary-resume comparison or a speedup claim.
- Focused tests cover durable migration/reopen, cancellation persistence against stale writes, draft/title preservation, configuration-fingerprint invalidation, incompatible destination rejection, mutation exclusion, matching-request joining, independent session progress, task panic reporting, failed/cancelled source cleanup recovery, and queue replay after an acknowledged-command crash. The queue fixture includes a greater-than-64-KiB image and configuration command and rejects a replacement worker store.
- Deterministic channel-handshake tests prove concurrent handoff/provisioning start and cancellation followed by joining the peer. LocalBare restore uses the verified absolute archive directly; container restore still transfers it.
- `npm run build` in `docs/` passed and checked 1,688 internal links across 24 pages. `node --check mj-controller/src/web/viewer.js` passed. Final full Rust test/lint results are recorded at completion.
- Live Podman/Docker/Apple Container, SSH, EC2, and real-provider cross-harness moves were not run. Existing backend/restore tests and new deterministic concurrency tests pass, but are not represented as live backend or provider coverage.
- Parallel full runs hit transient `ETXTBSY` in an existing worker-stop shell fixture and `WouldBlock` in an existing supervisor-lease test. Both isolated tests passed. The final full run serializes test cases; the new concurrency tests still exercise their own parallel workers. No host mount changes or production fallback was added.
- Standalone `brokk-mj-worker` test compilation also passed, ensuring persisted worker-store identity does not depend on the controller-only state module.
- `cargo test -q -- --test-threads=1` passed across the workspace. Final rollback review additionally preserves a partial destination's managed checkout until target teardown succeeds; retry restores the recorded source identity only after bounded cleanup. The affected controller suite is rerun for that correction.
- Final controller rerun passed: 674 tests, one intentionally ignored. After merging upstream ACP-resume and clipboard changes, 84 ACP tests and four clipboard tests passed. The integrated `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, JavaScript syntax check, and diff checks passed.
- Final integrated real-worker acceptance passed again, including explicit assertions that interrupted prompts are never replayed. Artifacts: `target/reliability-artifacts/session-move-seed-1-2824250/trace.json`. Test processes were stopped and isolated runtime scratch files removed; diagnostic artifacts were preserved.

Proposed human success text is: `Moved SESSION from OLD_PROFILE / OLD_TARGET to NEW_PROFILE / NEW_TARGET; ready and idle.` For Run, report that queued work was accepted, not that all queued tasks completed. Failure text must name whether the source remains available, the session is stopped with a checkpoint, or the destination is live with queue admission incomplete, and identify the applicable retry action.

## Interfaces and Dependencies


Use existing Rust workspace crates, SQLite writer, subprocess executors, session manager, relay commands, and archive verification. Do not add a workspace crate, mocking framework, or worker queue-pause protocol for this feature. The controller request/options and durable operation types belong near the controller behavior; only shared serialized shapes belong in the shared state layer.

Required public additions are the `mj move` command; one validated Move request available through daemon IPC and the web action path; a Move operation/capability in snapshots; and corresponding TUI/web controls. The request carries selected profile/target, compatible resume resource options, explicit queue disposition, and interruption acknowledgement. The result carries operation/session identity, resolved destination, completion or actionable error state. Recovery controls reuse these selections and existing lifecycle cancellation rather than bypassing the daemon.

Extend archive retention/reconciliation to honor operation-owned checkpoints and add durable move state through the standard database migration. Preserve normal Stop/Resume semantics while sharing their internal phases. Destination queue admission uses existing `RelayCommand::Prompt` and `RelayCommand::SetConfig` with durable command IDs; it does not require a new paused mode.

Revision note (2026-09-06): created from the agreed conversational plan, including the user's final Discard/Run queue choice and safe-overlap performance scope. Earlier proposals for automatic continuation, a durable paused queue, and retaining the source until destination readiness are not part of this plan.

Revision note (2026-09-06, implementation): recorded implementation and push authorization, schema 26, completed design adjustments, regression coverage, live local-bare acceptance, timings, and explicit backend/provider coverage limits.
