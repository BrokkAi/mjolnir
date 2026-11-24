# Finish interrupted suspend in the existing capture worker

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as work proceeds. The user requires simpler lifecycle code and asked for this plan, review by MJ Advisor, then implementation. This revision replaces the rejected target-command design with the advisor's smaller capture-worker design.

## Purpose / Big Picture


An accepted Suspend must finish when its original worker disappears during a daemon restart. The replacement daemon should resume the accepted operation using the retained checkout and command journal, without starting an agent harness on the successful path. A harness is the external coding agent. The daemon coordinates lifecycle work; a worker holds the worker-root lock, runs the harness, and owns the durable journal. The existing checkpoint-only worker reads that journal and exports a verified archive without starting a harness.

Fresh managed Suspend, a user's retry, and startup recovery will enter one capture pipeline. A live harness worker retains the current orderly interruption and stop. If that worker is gone, skip the live-only work and start the existing checkpoint-only worker. In both cases the capture worker opens its journal under the root lock, then drains remaining attributable writers and checks unknown checkout holders before becoming attachable whenever the journal is not already sealed by Close. An already sealed journal only needs verified teardown and must not run a new capture refusal. Proof and capture share one worker-root lock tenure. Remove the superseded recovery routing and harness-revival branches. There is no new control command, capability negotiation, daemon service, or lifecycle state.

## Progress


- [x] (2026-11-24 01:05Z) Manually repaired the affected retained environment and completed normal Suspend with a fresh checksum-verified archive and no error.
- [x] (2026-11-24 01:29Z) Wrote the original plan and created/self-assigned #1676 with agent-in-progress after finding no matching open issue.
- [x] (2026-11-24 01:37Z) Received MJ Advisor review: routing consolidation approved in principle; new target command rejected as unnecessary. Accepted the capture-worker design and the listed deletions and behavioral limits.
- [x] (2026-11-24 01:41Z) Verified sealed relay behavior, refusal-state mapping, and early stopped-target recovery ordering in current code.
- [x] (2026-11-24 01:43Z) MJ Advisor approved implementation with six final corrections recorded below; no further plan review is needed.
- [x] (2026-11-24 01:45Z) Added omitted-when-None capture launch context, shared writer draining and typed holder refusal before checkpoint journal open; reviewed worker diff. Integration validation remains.
- [x] (2026-11-24 01:50Z) Consolidated managed controller routing and deleted the specified dead-worker recovery branches; retained only live pending-intent reattachment, with no Harness startup from that branch.
- [x] (2026-11-24 01:50Z) Extended the existing isolated fixture with dead-worker-live, dead-worker-startup, dead-worker-error, and verified-before-destroy variants. Runtime validation is starting under one build owner.
- [x] (2026-11-24 01:56Z) Dev CLI/worker builds, launch compatibility (1), checkpoint-only worker tests (3), worker control golden (1), and sealed-capture restart E2E passed.
- [x] (2026-11-24 02:00Z) Additional targeted tests passed: older live worker (1), legacy Closing (4), SSH scope routing (1), Podman scope routing (1), uncancelled wait (1), daemon clone recovery (2). These remain valid across the private exporter correction.
- [x] (2026-11-24 02:10Z) Corrected exporter scope validation and completed 17 focused Rust tests plus 9 E2Es; all pass. No isolated test process remains.
- [x] (2026-11-24 02:14Z) Strict all-targets Clippy passed across 7 affected crates on merged a741a1061.
- [x] (2026-11-24 02:24Z) Fixed both first-review findings in the common route/post-stop decision; 2 new persisted-boundary tests plus 7 affected controller cases pass.
- [x] (2026-11-24 02:24Z) Controller strict all-targets Clippy and formatting passed after the two review fixes.
- [x] (2026-11-24 02:48Z) Fixed second-review pending-stop publication and dead sealed-capture ordering. All targeted checks pass: 6 Rust executions, 3 affected E2Es, rebuilt CLI/worker, and 5-crate strict Clippy.
- [x] (2026-11-24 02:55Z) Third-review stop-probe classification correction passed the persisted SSH test and controller strict Clippy.
- [x] (2026-11-24 03:21Z) Final simplification passed five focused Rust filters, real dead-worker-error rollback E2E, both dev builds, and four-crate strict Clippy.
- [ ] Obtain clean review and push.
- [ ] Run targeted regression/controls and affected-crate Clippy under one build owner.
- [ ] Review integrated simplicity and behavior, commit on the current branch, obtain before-push review, push origin/master, and dispatch platform CI.

## Surprises & Discoveries


The incident had durable Closing, no checkpoint frontier, no stop REQUEST or RECEIPT, and no worker. Recovery still required a relay lease from that worker. After failure became Error, retry treated the missing worker as an old live worker needing an upgrade and failed its initial lease. The actual worker/daemon death mechanism is unproven; the previous daemon stopped uncleanly. Another same-day session reached the same dead end after a clean handoff.

Successful Suspend already stops the original worker and starts a distinct checkpoint-only worker. `OriginDeltaSuspend::start_worker` installs the current build and launch config before starting that worker, and can reattach an alive/starting checkpoint-only worker after another daemon restart. That worker acquires worker.lock before reading or writing runtime state. A separate target-side quiescence command would duplicate ownership already available there.

The first integrated run found another reader of the original-worker proof in mj-checkpoint/src/checkpoint/capture.rs: verify_origin_delta_quiescence required REQUEST, RECEIPT and original PID death inside the export subprocess. All three dead-worker variants reached that check and failed, while the sealed variant passed because it correctly reused its archive. Replace duplicate proof with scope validation against the scoped capture launch and the existing accepted operation. An older unscoped capture can still be reattached before sealing, so preserve its successful request/receipt as the legacy scope source without forcing a new worker or repeating PID proof.

The existing origin_delta_suspend.py fixture still called removed framed daemon actions. Updated only its isolated record/create/wait calls to existing authenticated HTTP terminal routes before runtime assertions; no production compatibility path was added.

The daemon acceptance-publication test initially timed out waiting for its held child stop. Both baseline HEAD references prove fixture drift: it registered the key (label,delete) while production joins (label,delete,recovery). Correct only that fixture key by adding false for recovery; preserve its failure-publication expectation. This requires no new lifecycle helper or production behavior.

The live worker publishes its stop receipt before releasing its root lock. Existing live waiting must continue to verify exit; the capture worker's later acquisition proves root release. A stopped container makes launch/acceptance/Git reads fail before the current late target-recovery call. Reuse target_recovery_plan and ensure_recovery_target_running inside admitted accepted Suspend before those reads; do not add side effects to dry-run or preflight. Closed relay latch requires its exact barrier/cursor, so a sealed capture must use the existing verified artifact and cleanup path rather than latch again.

## Decision Log


Decision: replace the initial proposed ensure-quiescent command and Live/Waiting/Finished protocol with capture-worker writer draining.
Rationale: the existing current-build capture worker and its exclusive root lock already provide the required authority, survival across daemon handoff, and reattachment. No new command, probe, staging machinery, or controller polling protocol is needed.
Date/Author: 2026-11-24, parent agent accepting MJ Advisor review.

Decision: use one shared helper for launch mode Harness and held root lock; gate all live-only work through it. If live work fails, recheck once and enter capture only if the live harness is gone; otherwise return the original failure.
Rationale: only this lifecycle owner starts workers under session admission. Worker disappearance is a one-way transition while that owner holds admission, so a retry loop or separate daemon PID predicate is unnecessary. An existing checkpoint-only holder must be reattached rather than mistaken for an old harness worker.
Date/Author: 2026-11-24, parent agent accepting MJ Advisor review.

Decision: add one optional serde(default) launch field suspend_capture containing operation, checkout, and child_sessions; publish drain refusal through the existing worker exit error path unless its handling requires more code than the existing receipt path.
Rationale: the installed current-build capture worker needs the accepted operation's scope. An absent field preserves ordinary and historical launches, and new readers accept old JSON. WorkerLaunchConfig denies unknown fields, so an older reader cannot parse a present field: write it only after the existing replacement path installs the current worker, and never include it in an old live Harness launch. The worker owns failure reporting; avoid parallel durable refusal state. Confirm the selected path in implementation review and record a change here if receipt is smaller.
Date/Author: 2026-11-24, parent agent.

Decision: preserve cooperative live CancelTurn/quiescence and older live worker compatibility; accept existing crash continuation semantics for a dead worker with an in-flight prompt and no durable CancelTurn.
Rationale: live cancellation prevents a restart continuation; a worker crash without that marker already creates one at journal recovery. Successful recovery must preserve that behavior rather than silently drop the input. Explicit Resume may continue that interrupted turn. A refusal may restart a harness through existing recovery, intentionally.
Date/Author: 2026-11-24, parent agent accepting MJ Advisor review.

Decision: incorporate the advisor's final approval corrections without another review loop. Omit absent suspend_capture entirely with skip_serializing_if and extend the existing compatibility test. Keep current same-mode alive/starting reattachment and use Closing/Closed plus installed checkpoint for sealed cleanup, without new operation predicates. Add a new post-live-gate hook for the deterministic recheck race. Drain refusal must remain a typed precondition with holder text, not a generic boot failure. Bound stopped-container work to one existing recovery-plan/start call at accepted-pipeline entry, yield handoff admission around container start as existing callers do, and use existing controller fixture coverage rather than a new Podman E2E; split any larger requirement into a follow-up.
Rationale: strict older launch readers reject even a null unknown field; the extra operation guards would add unreachable branches; the existing preflight hook is too early to exercise the race; typed refusal and bounded resumable work preserve current user behavior and handoff ownership.
Date/Author: 2026-11-24, parent agent accepting MJ Advisor final approval.

Decision: retain the live-only pending Harness restart-intent reattachment, with an explicit reattach-only start option, and keep the historical verified-without-ACCEPTED relay route.
Rationale: a retained live replacement still owns its boot intent and must release it before another compatibility swap. If it dies, the existing one-time liveness recheck enters capture without reviving Harness. Historical verified archives without an origin acceptance marker may already have sealed legacy relays; every current ACCEPTED operation uses the common path, including when an older archive exists. These preserve shipped ownership and formats rather than add a second dead-worker pipeline.
Date/Author: 2026-11-24, parent agent after integrated controller review.

Decision: consolidate exporter validation around capture scope: scoped current launch plus matching ACCEPTED operation, or the existing successful request/receipt scope for an unscoped pre-upgrade launch, followed by common session/mode and canonical checkout containment checks. The held capture-worker barrier and root-lock tenure provide process proof; the exporter must not recheck original worker liveness.
Rationale: a live worker REQUEST is optional after a crash, but historical unsealed captures without the launch field remain valid under unchanged same-mode reattachment. This is persisted-format compatibility in one scope resolver, not a parallel dead-worker capture path. Add a focused integration test through the existing export fixture for both scope formats and rejected mismatches.
Date/Author: 2026-11-24, parent agent after failing E2E evidence.

Decision: keep the exporter's unscoped-launch compatibility arm for the demonstrated generic checkpoint restart path, and comment its removal condition beside the code.
Rationale: ordinary reattachment keeps the old hel and invokes its old exporter, but restart_worker_for_checkpoint can install the current hel with launch:None/prepared:false while retaining the old launch.json. The new exporter can therefore see a genuine legacy unscoped launch and successful request/receipt. This is reachable persisted compatibility, not a hypothetical fallback. References: mj-session-runtime/src/checkpoint_transfer.rs export binary, mj-controller/src/controller/checkpoint/relay.rs restart arguments, and worker_restart.rs install ordering.
Date/Author: 2026-11-24, parent agent after bounded exporter reachability trace requested by MJ Advisor.

Decision: mark failures while observing a durably requested worker stop with the existing trusted in-memory failure-reason mechanism (SuspendStopPending), and carry that reason through the existing daemon/store publication transaction to retain Closing and the close intent.
Rationale: both controller rollback and store failure publication otherwise end ownership while the worker continues the accepted stop. The marker is a non-serialized Rust error classification, not a stored flag, state, schema or protocol addition. Error reference/text still publishes normally and guides retry of Suspend; pre-request failures keep existing behavior. A dead-worker recheck still advances to capture.
Date/Author: 2026-11-24 02:32Z, parent agent after inspecting LifecycleFailureReason and save_failed_close.

Decision: open the checkpoint-only journal under the existing root lock, then apply its existing sealed_by_close() predicate before writer proof. Unsealed captures still drain before socket publication; sealed captures cannot accept a fresh barrier and only finish the accepted Close and verified teardown.
Rationale: proof and serving remain one worker ownership tenure, while reading the authoritative journal prevents a post-seal foreign checkout holder from reversing accepted cleanup. No controller seal replica or new worker mode is needed.
Date/Author: 2026-11-24 02:32Z, parent agent after runtime ordering investigation.

## Outcomes & Retrospective


Manual recovery and implementation are complete; 23 distinct focused Rust cases and 10 named E2E cases have passed across the targeted validation rounds, with unaffected results retained. Affected-crate Clippy passed on the merged tree. Both first-review preserved-behavior findings are fixed; two new persisted-boundary regressions and seven affected existing controller cases pass, as does controller Clippy. Second-review corrections also preserve pending-stop ownership through the existing failure publisher and use the journal seal before deciding whether capture proof is needed. Their focused validation and strict Clippy pass. Final review and delivery remain. The initial command-based plan was rejected before production edits. The implementation deletes the parked and failed-lease recovery reroutes, dead pending-worker startup, swap-failure Harness revival, synthetic missing receipt, dead-request early return, and exporter PID proof. One existing root-lock owner proves writers stopped and captures journal state, using a shared drain. The narrowly live reattach-only pending-intent branch remains for its shipped startup ownership contract. Current production Rust diff against upstream is +450/-407 lines; ownership and removed special cases, rather than raw size alone, are the simplicity criterion. Non-origin-delta checkouts retain their relay-first missing-worker limitation. Dead-worker process visibility, crash continuation, and refusal Harness restart remain the documented limits.

## Context and Orientation


The initial baseline is commit 401753925 on the current master branch. Stay on that branch. Bug https://github.com/BrokkAi/mjolnir/issues/1676 is self-assigned and agent-in-progress. Do not install a build or touch live user data for tests.

`mj-daemon/src/daemon/close.rs` persists accepted Closing and consent before the actual worker stop request. `mj-daemon/src/daemon/process.rs` finds interrupted operations at startup. `mj-controller/src/controller/lifecycle.rs` contains the managed stop/capture pipeline in suspend_session_controlled_with_manager and the relay-first recovery in recover_interrupted_close_managed. Origin-delta means the existing checkpoint policy preserving Git state relative to the origin repository. Verified Destroying means checkpoint verification finished and only cleanup remains.

`mj-controller/src/controller/lifecycle/origin_delta.rs` owns OriginDeltaSuspend and start_worker. The latter already reads launch.json run_mode and authoritative root-lock liveness, stops old root holders through restart_worker_with_installed_binary, installs the current hel and launch config with prepared:false, and starts/reconnects the requested worker mode. Hoist the existing mode/liveness interpretation into one helper used by both start_worker and the pipeline gate. Keep path and process interpretation in their existing owners.

`mj-core/src/worker_launch.rs` defines WorkerLaunchConfig. Add optional suspend_capture with serde(default, skip_serializing_if = "Option::is_none"), referring to a small capture-scope type in worker_suspend.rs or adjacent launch code. It contains only operation:String, checkout:PathBuf, and child_sessions:Vec<String>. Clear it for Harness mode. The controller populates it for managed origin-delta CheckpointOnly launch. Audit struct literals, writers, readers, and worker-restart launch refreshes so the field reaches the actual installed launch and old JSON still deserializes.

`mj-worker/src/worker_runtime/unix.rs` handles checkpoint_only before open_for_checkpoint_with_initial_commands. The root lock is acquired by mj-worker-bin before runtime entry. Move shared marked-process drain and unknown-holder refusal out of the live-only terminate_session_processes implementation in mj-worker/src/worker_stop/suspension.rs and call it after journal open but before socket publication for every unsealed launch carrying suspend_capture, including after a successful cooperative stop. Use the relay's existing sealed_by_close() predicate to skip this new proof for an already sealed capture, whose verified archive only needs teardown. Run blocking process inspection through the existing blocking-task helper. Live subreaper cleanup remains with the live worker. Ensure the capture worker's launcher is not mistaken for a session writer and never kill the process providing this proof.

`mj-core/src/worker_suspend.rs` retains existing ACCEPTED, REQUEST, RECEIPT, CAPABILITY and PROGRESS formats for live cooperative stop. `tests/e2e/origin_delta_suspend.py` provides isolated real daemons and workers, a recorded harness, and the origin_delta_after_preflight pause hook. Extend it rather than inventing a new framework.

## Plan of Work


### Milestone 1: settle review details before implementation


Send the revised plan to MJ Advisor for focused approval, then assign disjoint implementation slices. Verified findings: the latch requires an exact live barrier/cursor and must not run again on a sealed capture; restart_after_refused_origin_delta starts Harness and maps previous Closing to Running, previous Error to Error, and Parked back to Parked; target recovery currently runs too late for stopped containers. Preserve those refusal states and the existing Harness restart, and test the visible result. For admitted accepted owning Suspend, verify cached placement and reuse targets::target_recovery_plan plus ensure_recovery_target_running before the first target-backed policy, ACCEPTED, launch or Git read. This includes startup and retry but excludes dry-run/preflight. Missing or ambiguous/unreachable targets fail visibly and keep data. Bound this to one existing target-recovery call pair; release handoff admission during container start using the existing begin/end resumable-work pattern. Use an existing controller fixture for ordering if one fits, with no new Podman E2E. Split larger stopped-target work into a follow-up and record its exact user-visible limit. If the capture-worker design has a blocker, consult the advisor before reinstating any command.

### Milestone 2: make the capture worker own final writer proof


The worker implementer owns the optional launch context/type, necessary launch struct literals outside the controller, shared drain in worker_stop/suspension.rs, and the checkpoint_only runtime call. It owns no controller lifecycle or Python files. Reuse the existing verified process signaling, marked-session scan, and unknown checkout-holder refusal. Preserve live worker subreaper checks, scope by parent plus borrowed child sessions, and hold the existing worker root lock throughout proof and capture. Drain both after live cooperative stop and after abrupt original-worker loss. No new launch process, target command, capability, or durable receipt format is added.

Use the existing exit record to report a refusal with useful context. Existing controller error handling must restart the harness on refusal and preserve the environment. If the receipt path is demonstrably smaller, select that before editing both sides and record why. Extend existing journal/launch compatibility or process-boundary coverage only where it tests a real boundary or tricky condition; the new command tests from the rejected plan are removed.

### Milestone 3: unify controller routing and delete replaced code


The controller implementer owns lifecycle.rs, lifecycle/origin_delta.rs, necessary launch population in worker_restart.rs, tightly scoped daemon integration/hooks, and their existing Rust fixtures. A matching recorded REQUEST goes directly into the existing cancellation-aware exact-worker wait before cancellable target reads or borrower work; only the no-REQUEST arm probes and prepares the live original worker. Populate suspend_capture from the accepted operation and chosen checkout/child scope. Hoist Harness mode plus root-lock liveness into one helper; reuse it in start_worker and the pipeline. No live harness means skip upgrade, lease, CancelTurn, quiesce and live wait and enter CheckpointOnly directly. A live harness retains the current live block. In the no-REQUEST live-preparation arm, recheck once on failure; if gone enter capture, if still live return the original failure. A recorded REQUEST uses only the existing exact-worker wait, which records cancellation with the uncancelled executor before other cancellable work. Do not introduce loops or interpret socket refusal as quiescence.

Route managed origin-delta fresh Suspend, retry, and startup to this common pipeline before legacy relay-seal querying. Preserve verified Destroying cleanup and historical sealed non-origin formats. Delete recover_interrupted_close_managed's parked_close block, resumable_origin_delta failed-lease arm, and origin-delta policy choice in its Idle/Running reroute. Delete the pipeline's Harness revival after WorkerRestartLeftNoWorker and the dead-worker half of the pending-restart branch. Delete request_stop's dead-REQUEST early return and wait_for_stop's synthetic exited-without-confirming receipt: worker disappearance now enters capture. If any specified deletion must stay, justify it in Decision Log rather than quietly retaining duplicate behavior.

Generalize the currently parked-only sealed-capture block to every accepted explicit owning origin-delta Suspend; exclude Move/Restart handoffs whose checkpoint owners differ. If a daemon died after persisting the verified checkpoint but before Destroying, an already Closing/Closed capture relay must finish the same verified checkpoint and cleanup, not latch or export again. For Closing/Closed on the capture relay with an installed checkpoint, wait Closed, release it, run before_close, and use destroy_after_verified_checkpoint; no extra operation-ownership predicate is needed because sealing only moves forward. An Idle relay plus an old artifact must recapture. Add a deterministic hook after the relay Close is acknowledged/sealed and the verified checkpoint is persisted, before Destroying; the test must observe the accepted seal as Closing or Closed before killing the daemon. Keep the existing same-mode alive/starting CheckpointOnly reattachment unchanged, including pre-upgrade and parked captures with no field. ACCEPTED persists until rollback replaces capture with Harness; an extra operation-match predicate would add an unreachable branch. Keep current operation identity, latch/export/checksum verification, and cleanup order.

### Milestone 4: prove crashes, retries, and explicit behavior limits


The E2E implementer owns tests/e2e/origin_delta_suspend.py and its existing fixtures, not production code or build settings. Extend one shared fixture with three preflight-hook variants: original worker killed while the same daemon continues; original worker killed plus test daemon crashed and replaced; retry from Error. Add an explicit new hook after the live-Harness gate and before live-only work for the same-daemon variant; the existing origin_delta_after_preflight hook precedes the gate and covers the plain absent-worker variants. Add the sealed-capture crash variant after verified checkpoint persistence with exactly one verified checkpoint. Coordinate the two required production hook names and timing with the controller owner.

Each successful recovery must preserve staged/unstaged tracked edits and durable journal content, clean up a marked detached writer, reach suspended without a new harness launch, and survive explicit Resume. Include a dead in-flight prompt without CancelTurn and assert the existing restart continuation is preserved for Resume. Exercise an unknown checkout holder and assert visible refusal, retained data, the existing harness restart, and the actual recovered state for previous Closing/Error. Existing live CancelTurn and queued-prompt semantics remain controls. Repeated daemon restart during capture must reconnect to the existing capture worker; a receipt published before original worker exit must not allow premature capture.

The dead-worker drain can only see MJ_SESSION_ID-marked processes and current checkout holders. A former descendant that dropped its marker and currently holds no checkout file is invisible; only the live subreaper can catch it. Do not claim equivalent ancestry coverage. The no-harness-start promise applies to successful capture; refusal intentionally uses restart_after_refused_origin_delta. Non-origin-delta checkouts retain their historical relay-first behavior. Include existing owned-container restart through target_recovery_plan/ensure_recovery_target_running before early target-backed reads; Missing or ambiguous targets retain the environment and report the existing diagnostic.

## Concrete Steps


Run from the repository root through the existing mbx dev profile, without changing targets or cache layout. Every Rust test and E2E invocation runs outside the restricted sandbox. Use named instances for all new-build daemon, TUI, CLI, and live test invocations; never use the default instance. Fixture-owned isolated MJ_CONFIG_DIR/MJ_DATA_DIR must remain isolated. One assigned child owns all builds/tests at a time, after concurrent production writers finish.

    mbx build -p brokk-mjolnir --bin mj --features test-hooks
    mbx build -p brokk-mj-worker --bin mj-worker
    mbx test -p brokk-mj-daemon suspend_recovery_preserves_a_clones_
    mbx test -p brokk-mj-daemon accepted_suspend_failure_is_published_without_an_observer
    mbx test -p brokk-mj-controller legacy_closing_with_
    python3 tests/e2e/origin_delta_suspend.py --mj target/debug/mj --worker target/debug/mj-worker --instance suspend-restart-dead-worker-startup --case dead-worker-startup
    python3 tests/e2e/origin_delta_suspend.py --mj target/debug/mj --worker target/debug/mj-worker --instance suspend-restart-round-trip --case round-trip

Implemented regression selectors are dead-worker-live, dead-worker-startup, dead-worker-error, verified-before-destroy, and verified-before-destroy-dead-capture. Existing control selectors are detached, late-file, untracked, git-error, and raw; use distinct suspend-restart-<case> names. Run worker control/drain and affected launch/journal compatibility tests. If cancellation implementation changes, run the existing cancel_turn_bypasses_a_pending_checkpoint_without_steering_the_queue filter in brokk-mj-worker-runtime. Require a nonzero matched count per Rust filter. For behavior changes run affected-crate Clippy:

    mbx clippy -p brokk-mj-controller -p brokk-mj-worker-runtime -p brokk-mj-worker -p brokk-mj-core --all-targets -- -D warnings

Add daemon/targets only if their code changes. Rerun only failing tests after repairs. Broaden checks only for changed shared boundaries or a specific unresolved concern. Record exact commands/counts below. Commit changed files on the current branch, call request_review, resolve findings, then push HEAD to origin/master. If upstream moved fetch and merge origin/master, resolve conflicts, and validate what the merge invalidated. Dispatch nightly.yml for this platform-sensitive process change. Do not install without user authorization.

## Validation and Acceptance


The new real-process regression must reproduce the baseline worker-contact dead end and pass on the implementation. Replacement startup or same-daemon continuation must automatically report state suspended and has_error false, without an intervening Resume, and export exactly one verified checkpoint. Resume must restore staged-versus-unstaged Git state and journal content; a dead active input retains its existing crash continuation. Unknown-holder refusal must retain data, reach the user as a typed precondition refusal with the checkout-holder text, and recover through the existing harness path; generic worker-boot error is insufficient. A second restart during draining/capture must not start a competing worker or repeat an already verified export.

Review the production diff for the named branch deletions, one shared liveness/mode interpretation, and one shared writer drain. Reject a parallel dead-worker rescue path. No new command, target protocol, capability staging, database migration, lifecycle state, daemon task, or speculative timeout is allowed. The one optional launch field is absent by default, omitted entirely when None (not serialized as null), and new readers accept old JSON. Extend existing launch compatibility coverage to assert a serialized Harness launch contains no suspend_capture key. Older strict launch readers reject a present field, so the current worker must be installed before its scoped launch is written; ordinary old live Harness launch files omit it. Existing request/receipt and checkpoint formats remain unchanged. Record the limited scope and visibility guarantees in Outcomes.

## Idempotence and Recovery


Reuse the accepted operation ID and existing WorkerRunMode::CheckpointOnly reattachment. The capture worker acquires worker.lock before proof; no worker can concurrently write the journal during capture. Never delete files as a substitute for stopping writers. Keep source cleanup after archive verification. Use existing lifecycle admission and restart machinery rather than new daemon memory. On a test failure retain isolated evidence, terminate test processes, and stop the named daemon in fixture cleanup. No manual edits to the live store are needed.

## Artifacts and Notes


The original session Flashing Notification And Bug Report reached Closing/no frontier/no REQUEST and no worker. Current daemon log mj-daemon-20261124T010035.305Z-2208363.log records startup failure at 01:01:08Z and repeated user retries. Manual retained-environment Resume followed by normal Suspend launched a distinct capture worker, exported and checksum-verified at 01:05:31Z, and reached suspended/closed/has_error=false at 01:05:32Z. The exit causes remain unknown. Investigation reports under .mj/agents/07f89d6cc8c9c36b6f7f1a8409c118df and .mj/agents/58baf6a47409a5175b1ab7b81c0198bf contain source/log references but are temporary; the facts needed to implement are embedded here.

MJ Advisor reviewed baseline 401753925 without running tests. Required changes were to move proof into capture, keep live quiescence, delete named branches, cover sealed capture and stopped targets, and acknowledge the limits above. Full requested deletions and coverage are recorded in Milestones 3–4.

## Interfaces and Dependencies


Use optional WorkerLaunchConfig::suspend_capture with serde(default, skip_serializing_if = "Option::is_none") and a scope carrying operation, checkout and child_sessions. Populate it only for origin-delta CheckpointOnly; Harness launch clears it. Use the existing worker exit error record for proof failures and existing controller refusal recovery. Share existing worker stop process helpers and root owner; do not introduce a target control result. Hoist existing launch-mode/root-lock reads into one internal controller helper. Preserve frozen worker control commands and existing stored request/receipt shapes. Use Path/PathBuf, shared subprocess helpers, supervised background work and existing test isolation.

Revision note (2026-11-24 01:41Z): Replaced the unapproved target command design with the smaller capture-worker design; specified deletions, shared root tenure, sealed restart coverage, strict launch-reader compatibility, existing refusal-state mapping, early owned-container restart, and explicit process/turn/refusal/non-origin limits. These details were checked against the baseline before implementation.

Revision note (2026-11-24 01:43Z): Final advisor approval incorporated: omit None launch field, unchanged capture reattachment, monotonic sealed cleanup, bounded resumable target start, typed holder refusal, and explicit post-gate hook. Implementation is authorized and no further plan review is pending.

Revision note (2026-11-24 02:10Z): Completed implementation and behavior checks, documented stale fixture repairs and reachable legacy exporter scope, and recorded exact test totals. Clippy/review/delivery remain.

Revision note (2026-11-24 02:14Z): First code review completed with two accepted findings. Extend the shared route to accepted Parked captures under every checkout policy and consume a matching persisted stop refusal after worker exit. Keep these in the common pipeline, add persisted-boundary regressions, and retain all passing unaffected validation.

Revision note (2026-11-24 02:24Z): The common route now recognizes durable parked acceptance independently of export policy, and one common post-stop receipt decision honors recorded refusal after original-worker exit. Both new fixtures and 7 affected existing controller cases pass; fixture-only Path qualification and short session IDs were corrected during validation.

Revision note (2026-11-24 02:31Z): Second review identified pending accepted-stop state ownership and a dead sealed-capture worker restart. Keep recorded unresolved worker stops durably Closing. Move capture proof after journal open but before attachability and use existing sealed_by_close() so accepted Close proceeds to verified cleanup even if a new foreign checkout holder appears. Add focused persisted-progress failure coverage and one variant of the existing sealed E2E that kills both daemon and capture worker. No new state or mechanism is introduced.

Revision note (2026-11-24 02:48Z): Second-review fixes and all affected validation are complete. Existing lost-ACK code refreshes the in-place attempt before waiting; final read-only verification required no additional patch. Ready for committed code review and delivery.

Revision note (2026-11-24 02:53Z): Third review's initial liveness observation edge is handled at the existing stop-entry boundary, only for a matching recorded request. The shared liveness helper used for adoption and rollback is unchanged; the existing persisted SSH fixture loops over initial-probe and progress-read failures. No extra mechanism or new validation scope beyond this filter and controller Clippy.

Revision note (2026-11-24 03:06Z): Latest review is resolved by cutting the redundant initial probe for recorded requests, including the cancellable pre-wait RESTART read, rather than adding another probe exception. Unknown checkpoint-worker startup/contact and seal-observation errors retain Closing through the existing pending classification, renamed SuspendPending. Only an ephemeral readiness context derived from existing worker exit refusal plus checkout-quiescence startup stage before socket publication permits unsealed rollback; no new persisted/wire metadata is introduced. The parked outer catch must respect Pending rather than restore Parked while seal state is unknown. Focused cancellation, sealed reconnect, readiness-provenance and publisher fixtures will validate the correction.

Revision note (2026-11-24 03:21Z): Validation is complete for the recorded-request simplification and conservative seal recovery. Readiness attaches only an in-memory UnsealedSuspendRefusal marker based on its existing worker-exit/startup-stage evidence. The selected E2E originally sampled the Closing publication overlay over durable Error; the fixture now requires holder diagnostic, settled non-Closing state, and a new sanitized public close-failure reference before applying unchanged terminal assertions. No production change was needed for that sampling race.

Revision note (2026-11-24 03:31Z): Quiesce delivery now owns its lost-acknowledgement readback in request_stop. Matching durable request continues waiting; confirmed absent request permits the existing unstarted failure path; unreadable outcome yields SuspendPending. The live failure handler uses that same trusted classification instead of cached false to clear ownership. The extended SSH lifecycle/store fixture and controller strict Clippy pass, with previous evidence retained. Upstream 9ef3bc9c0 changes only unrelated review records and test import gates; daemon-test hunks are disjoint and introduce no API or behavior interaction.
