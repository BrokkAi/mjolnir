# Preserve idle goals during lifecycle operations and worker upgrades

This ExecPlan is maintained according to `.agents/PLANS.md`. Its progress and decisions describe issue #1498.

## Purpose / Big Picture

A session whose turn stopped at a provider quota limit should be able to switch to another profile of the same harness on the same target, preserving its conversation, durable goal, and managed children. An idle durable goal alone must never count as work in flight, including for worker upgrades, manual Move, and checkpointing. A running turn, a goal actually executing, or a pending goal decision must still prevent an automatic interruption. Already-running old workers must be able to receive this correction without hitting the oversized legacy memory export. Worker connection handshakes stop waiting after the user-requested 60 seconds, instead of 300 seconds.

## Progress

- [x] (2026-11-11) Confirmed the failing session has idle execution and an active goal, and that the goal triggers the generic replacement predicate.
- [x] (2026-11-11) Confirmed a separate manual Move waited 163 seconds for a reconnect on a full disk. Expanded the requested host's EBS volume and filesystem separately.
- [x] (2026-11-11) Created and claimed issue #1498; delegated disjoint implementation and read-only investigation.
- [x] (2026-11-11) Implemented shared quota-Move eligibility at preparation, leased source, and non-interrupting checkpoint acquisition.
- [x] (2026-11-11) Four-mode isolated regression passed: full active-goal Move, running-turn refusal, newer completed-turn refusal, and autonomous-turn deferral without cancellation.
- [x] (2026-11-11) Corrected failed pre-stop retry classification; the regression proves a different profile stays in place with no workspace transfer.
- [x] (2026-11-11) Changed the worker handshake budget to 60 seconds.
- [x] (2026-11-11) Removed blocking recovery/upgrade result settlement from the serving loop; four focused publication/admission/lifecycle tests passed.
- [x] (2026-11-11) Proved the `d1af5b48` disconnect is a project-memory response exceeding the relay frame limit; separated implementation into #1500 and `.agents/plans/chunk-project-memory-transfers.md`.
- [x] (2026-11-11) With explicit user authorization, fast-forwarded the checkout by 80 upstream commits to 2fc7ec17b, preserving all in-progress edits.
- [x] (2026-11-11) Reviewed the integrated changes; completed full core/controller suites with the sole older-fixture failure repaired and rerun, workspace clippy, formatting and diff checks.
- [x] (2026-11-11 16:16Z) Committed the validated changes, completed push review, merged upstream, and pushed d73304e73; issues #1498 and #1499 closed.
- [x] (2026-11-11 16:27Z) Completed the explicitly requested installation with source and Git HEAD frozen throughout all builds. Ordinary installed CLI invocation handed off the live daemon, which reports build 2.70.0+d73304e7.
- [x] (2026-11-11 19:06Z) Installed the subsequent project-memory transport fix from 1fab3c8d2. The separate local node-capacity stopgap was installed and then stashed at the user's request; it must not be reapplied or shipped.
- [x] (2026-11-11 19:20Z) Confirmed session 9b3f22a6 still uses an older worker, whose project-memory response exceeded the frame limit. Its idle active goal prevents automatic upgrade. The suspected queued quota-Move defect was investigated below; #1498 remains open.
- [x] (2026-11-11 19:25Z) User authorized fixing the reported failures with idle-goal activity corrected globally, followed by push and installation. Fetched upstream; origin/master still matches 1fab3c8d2. Delegated core activity semantics and disjoint controller regression/Move changes.
- [x] (2026-11-11 19:28Z) Disproved the proposed queued quota-Move state: SetQuotaRecovery requires an empty accepted prompt queue, and accepting Prompt/SetConfig clears existing quota recovery. No supported frozen checkpoint writer produces both. Corrected the claim to the user and dropped the proposed queue exception and fabricated fixture.
- [x] (2026-11-11 19:40Z) Removed idle-goal ownership from the shared work predicate while preserving autonomous-driver intent and actual running/pending work. Activity regressions passed, including corrected running-goal classification and closed-session precedence.
- [x] (2026-11-11 19:40Z) The existing worker-upgrade lease regression passed for protocol 58/59/60 with exact goal preservation, accepted review refusal and cleanup, a completed autonomous turn after readiness, and ordered queued submissions. Existing quota Move/refusal/replay and busy steering tests also passed.
- [x] (2026-11-11 19:40Z) Implemented the old-worker goal admission bootstrap without canceling the goal or exporting memory. The protocol-peer regression passed; delivery to the actual historical worker remains to be verified after installation.
- [x] (2026-11-11 19:50Z) Reviewed the integrated fixes, completed the full development tests plus targeted environment repair (4040 passed, 34 ignored), and passed workspace clippy, formatting and diff checks.
- [ ] Merge upstream (including node removal when landed), commit, request push review, and push.
- [ ] Install from frozen source and verify daemon activation and affected worker upgrade without interrupting actual work.

## Surprises & Discoveries

The failed automatic Move was not blocked by its 13 managed children. The broad `safe_to_replace` activity check rejects a session whenever a durable active goal owns it, even though the quota failure has already stopped the turn. The live session's goal is active, rather than limited with a usage reason. Adapter-reported goal status must not be rewritten just to pass Move admission.

The existing same-environment Move preserves managed children by closing and draining worker child-mutation admission before replacing the parent. That behavior must remain intact. Manual Move still uses its existing interruption warning.

The latest manual Move for session 9b3f22a6 passed admission at 19:12 UTC but its old worker logged `relay response frame is too large` during project_memory_snapshot. The new daemon correctly used the old protocol with that retained worker. Automatic worker replacement had refused because an idle active goal maps to work in the common activity rules. This is a global semantic defect, not a reason to add another Move exception.

The suspected queued-follow-up defect is not reachable through the supported worker owner. In mj-worker/src/relay/commands.rs, quota_recovery_admissible requires queued_prompts.is_empty(). In mj-core/src/relay/snapshot/apply.rs, accepting a new Prompt or SetConfig clears quota recovery, and a user prompt replaces continuation identity immediately. Restore formats can syntactically represent both fields, but the actual checkpoint writer captures one coherent frozen state. Do not weaken the predicate or invent a restored state merely to test the original mistaken hypothesis.

Old workers independently apply their compiled safe_to_replace predicate inside ReserveIdle. The daemon-side global correction cannot alone upgrade such a worker; investigate the existing checkpoint/reservation primitives and preserve atomic admission and native goal state when resolving this bootstrap.

A second observed defect explains the user's transfer chooser: an old failed pre-stop Move to one profile plus an unrelated historical source checkpoint causes a new profile choice to be classified as a restore retry. The old operation owns no checkpoint or handoff, and its source is still running. The generic historical checkpoint is not evidence that the old Move needs recovery.

The user expanded scope to stalls while publishing recovery and worker-upgrade results. The daemon's serving select loop awaits a configuration reload for every recovery result, including cancellation and deferral. One observed phase lasted 190503 ms while the runtime heartbeat remained recent. Production records already arrive through the committed-state feed, so this reload is unnecessary. Upgrade preparation-failure settlement also performs durable work on that serving loop. The logs identify the ownership defect but not the precise holder of the configuration lock; do not attribute the wait to database size without evidence.

The user also reported a Move error for `d1af5b48898e8d2a9f970b26631bb81a`: an established relay disconnected during `project_memory_snapshot` at 15:38:25 UTC. Its source remained running. This is distinct from the handshake timeout, and its worker-side cause is under investigation.

## Decision Log

Decision: An idle goal is durable intent, not work in flight, for every consumer of shared activity facts and states. Keep active goals as autonomous drivers so continuation scheduling does not submit duplicate work; retain actual goal execution, pending resume and pending decision as blockers. Remove the old quota-only goal exception. Rationale: the user explicitly broadened the rule after the old worker could neither upgrade nor Move. Date: 2026-11-11 19:25Z.

Decision: Do not add a quota queued-command exception. Rationale: the actual worker serializes quota admission and command acceptance, so the alleged queued-prompt recovery state has no supported writer. Retain all existing user/turn identity protections and correct the earlier issue comment instead of creating impossible regression input. Date: 2026-11-11 19:28Z.

Decision: Mark corrected idle-goal admission with relay protocol 60. New workers and old workers without an active idle goal keep ReserveIdle. Goal-bearing older workers use the existing BeginCheckpoint upgrade bootstrap, selected before any request, with live activity rechecks and original-ready-cursor validation. Do not fall back after a ReserveIdle refusal. Confirm the already-accepted barrier with ReserveIdle using the identical command ID before retaining a legacy lease: its existing idempotence bypasses the obsolete goal predicate while its separate reviewer-admission check still refuses accepted work. Rationale: an old binary applies its old goal predicate inside initial ReserveIdle and cannot receive the fix through that request. BeginCheckpoint does not clear the goal, cancel a turn, export memory, or call the provider; it freezes relay dispatch and blocks new reviewer admissions. Historical workers only provide an observed-idle bootstrap, as allowed by AGENTS.md, and must not be described as having the new admission guarantee. Date: 2026-11-11 19:32Z.

Decision: Treat the durable active goal as preserved for automatic same-harness, same-environment quota Move, keeping interruption acknowledgement false and keeping the remaining activity blockers. Use one shared decision at preparation and again while the source connection is leased. Rationale: quota recovery already established a stopped turn; the durable goal is the work to continue, while newly started execution must not be interrupted. Date: 2026-11-11.

Decision: Set the existing handshake timeout to 60 seconds without changing command-acknowledgement or review-capture budgets. Rationale: the user explicitly requested this bound after observing a multi-minute reconnect stall. Date: 2026-11-11.

Decision: Initially work on the current checkout after automatic approval review rejected the advance merge. The user subsequently explicitly authorized merging upstream in advance; completed the fast-forward to 2fc7ec17b with edits preserved. Rationale: honor the clarified authorization and validate against current upstream. Date: 2026-11-11.

Decision: Use the existing `MoveOperation::checkpoint_retained()` to identify recovery retries instead of looking at the source session's arbitrary historical checkpoint. Copy the checked in-place decision into new durable operations. Rationale: the Move record owns recovery evidence, and preparation and execution must not compute conflicting environment decisions. Retain existing sealed-environment checks and partial-stop recovery adoption. Date: 2026-11-11.

Decision: Remove unnecessary recovery-triggered configuration reloads in production, and settle upgrade preparation failures within the supervised upgrade attempt before sending its result. Keep the session reservation until settlement and make serving-loop reporting synchronous. Rationale: filesystem/database work and configuration-lock waits cannot block accepting connections or shutdown; failed publication tasks must not be orphaned or dropped. Date: 2026-11-11.

Decision: Bind the automatic preparation's opaque fingerprint to its original user/completed-turn identity, and use a quota-specific non-interrupting mode of the existing checkpoint latch. Revalidate pending quota recovery against the ready checkpoint and let the existing exact Close cursor seal that checked cut. Rationale: a new turn may start and finish before lifecycle ownership, or start autonomously while a checkpoint is acquired. Current activity alone cannot prove an old quota decision is still relevant. This reuses existing formats and ownership instead of adding protocol or database fields. Date: 2026-11-11.

## Outcomes & Retrospective

The initial implementation, validation, push, and installation are complete, as is the live disk expansion. Project-memory paging from #1500 was subsequently pushed and installed, but old workers can remain in use until replacement admission succeeds. The newly authorized global idle-goal correction and old-worker upgrade follow-up below are in progress; this plan is not complete until they are pushed and installed.

Follow-up focused validation passed: 19 shared activity tests are accounted for by the final run and the single corrected old assertion rerun. Four controller filters passed in 3.24 seconds, including the existing genuine quota Move four-mode regression and the upgrade protocol matrix. A first doctest invocation failed because its referenced mj_core rlib was absent while source was changing; all library tests from that invocation passed. The final stable three-crate run owns remaining full tests, documentation tests and workspace clippy. Protocol-peer tests emulate legacy goal admission and private reviewer refusal; they do not claim to execute a historical full worker swap or a real review capture process. Logs are in the activity and Move child report directories.

The follow-up full three-crate development run completed with 4039 passed and one settings golden failure. The inherited validation instance name deterministically changed the default viewer port; the golden fixture owns private data/configuration roots and expects the default port. Rerunning only that failed filter without MJ_INSTANCE and with explicit private outer data/configuration roots passed, accounting for 4040 passing tests and 34 ignored across the full run and repair. Controller is 2546 passed/19 ignored, core 645 passed/3 ignored plus 3 integration scenarios, worker 817 library and 5 binary tests passed/11 ignored plus 24 integration tests. Documentation tests passed with one ignored core example. Initial workspace clippy found two mechanical style lints; range containment and a nested if were corrected without changing behavior, and the clippy retry owns final acceptance. No full suite was repeated for these repairs. Evidence is integrated-tests.log, settings-env-corrected.log, clippy.log and clippy-final.log under .mj/agents/daefe5367af1d2ac98cedda8a4bd9ef2/.

Final workspace clippy exited successfully in 1m04s after the two style repairs. Parent applied rustfmt's requested wrapping to the range expression, then cargo fmt --all -- --check and git diff --check both passed. The only upstream changes available at that point were the documentation-domain change through 71a2cea74; node removal had not yet landed.

The result-publication change now keeps configuration-lock waits out of the serving loop. Upgrade failure settlement retains the existing per-session attempt and a named daemon admission through the durable write. Four focused tests passed, including actual Ping/Stop traffic while the configuration lock remains held and a real SQLite write-lock wait while another session proceeds. Log: `.mj/agents/4a58f6d94bc675fcf4acd5f24798cfc1/publication-tests-retry.log`.

The full admission regression now requires a completed Move, the destination profile, unchanged native history/cache, and an active goal in the canonical checkpoint. It also proves that a running turn and a newer completed turn prevent the old automatic decision, and that an autonomous turn at checkpoint acquisition remains alive without a CancelTurn command. Intermediate fixture failures exposed missing native history and insufficient outcome assertions; both were corrected. Passing log: `.mj/agents/1cbd92b1e462d4831611d7744363a0df/retest8.log`. Full touched-crate tests and workspace clippy are running after the source edits stabilized.

Final validation: controller 2544 passed and 19 ignored, with one older native-source fixture initially failing because it supplied no pending quota recovery. That fixture now models a real quota-stopped live source and its focused rerun passed. Core 643 passed and 3 ignored; the three Jev scenario tests passed; documentation tests completed without failures. Workspace clippy passed after collapsing one nested conditional. Relevant logs are `consolidated-tests.log`, `consolidated-core-tests.log`, `consolidated-core-scenarios.log`, `consolidated-doctests.log`, and `consolidated-clippy-retry.log` in the publication agent's report directory, and `native-drain-fix2.log` in the Move agent's report directory. No full suite was repeated to confirm the fixture repair.

The separate relay disconnect is proven to be an oversized project-memory snapshot frame on morannon, tracked in #1500 and `.agents/plans/chunk-project-memory-transfers.md`. Raising the global frame cap would only postpone the failure.

Push review completed against d73304e73. One P2 alleging quota recovery replacement during Move was declined: MoveMutationGuard owns lifecycle admission before execution; continuation claiming returns Claim::Move and the sole quota-recovery publisher checks that ownership before publishing. Already-prepared recovery also carries an exact expected relay cursor, which rejects a newer turn. The late-turn regression independently proves non-interrupting deferral. The review override was recorded before pushing.

The normal release-profile installer completed all native and musl worker builds without source or Git changes, and installed matching worker compatibility stamps. An ordinary installed `mj sessions` invocation completed daemon handoff; `mj daemon status` reports pid 1646927, build 2.70.0+d73304e7, started 2026-11-11T16:27:39Z, and "This daemon runs this build."

## Context and Orientation

The daemon is the control service owning session lifecycle operations. `mj-controller/src/daemon/session_move.rs` prepares manual and automatic Moves and checks automatic safety. `mj-controller/src/controller/move_session.rs` revalidates preparation after taking the source worker connection, then checkpoints and restores the session. A worker holds the harness, which is the provider's coding-agent process. `mj-core/src/activity.rs` decides whether observed worker state contains work. Its ordinary replacement predicate rejects an active goal even when no turn is running. `mj-controller/src/worker_client.rs` defines the connection handshake timeout; `worker_client/connect.rs` uses it for initial connections and reconnections.

## Plan of Work

The first stage introduced a shared Move activity decision and a quota-specific idle-goal exception. The follow-up replaces that exception with the global rule in mj-core/src/activity.rs: an active goal alone is not owned running work. Update both state-only and fact-based work decisions, retaining the separate autonomous-driver meaning and actual goal execution/resume/decision blockers. Both Move purposes use the global decision, while quota-specific checkpoint control still prevents interrupting a newly started turn. Keep queued-command rejection and all existing user/completed-turn identity checks. Keep source unavailability and child-mutation safety checks. Inspect old-worker idle reservation and existing checkpoint primitives before selecting the upgrade bootstrap correction.

In mj-core/src/relay.rs, advance the protocol to 60 and name its corrected idle-goal ReserveIdle semantics. In mj-controller/src/controller/checkpoint/workspace_lease.rs, select the existing BeginCheckpoint bootstrap for goal-bearing workers older than that boundary, retaining the single IdleWorkspaceLease owner and worker lifecycle permit. Retain initial ReserveIdle for corrected workers and ordinary old idle workers. Once the legacy barrier is ready and activity remains idle, confirm ReserveIdle with the same accepted barrier command ID. Existing pending or active barriers block new reviewer admissions, while this idempotent confirmation checks that no earlier reviewer admission remains. A refusal drops the lease and leaves the worker alive. Store the barrier's initial ready cursor and reuse validate_checkpoint_barrier_snapshot before final replacement so an autonomous turn that begins and finishes after readiness still invalidates the reservation. Never use interruption, archive export, memory synchronization, or a refused-request fallback to make legacy upgrade succeed. Extend the existing upgrade regression with old 58/59 peers that reject initial goal-only ReserveIdle but allow accepted-ID confirmation, current 60 admission, exact goal preservation, admitted reviewer refusal, and a completed autonomous turn after the ready cursor.

Use the existing fingerprint to carry the quota-stopped turn identity from preparation to the leased source recheck. In `controller/checkpoint/lease.rs`, add an internal quota-close mode that defers when new work is observed instead of cancelling the turn. Thread it through `controller/lifecycle.rs`, `checkpoint/latched.rs`, and `checkpoint/barrier.rs`; share the same preserved-goal activity decision. Validate the original pending quota recovery against the ready barrier before Close. No new serialized Move field is needed.

Extend existing relay/Move regressions near `mj-controller/src/controller/move_session/tests.rs` to demonstrate that a quota-stopped session with an active goal can Move and that a turn starting after preview still refuses automatic interruption. Mark defect regressions with the required hard-won #1498 comment. Change the existing handshake constant and its explanatory comment to 60 seconds.

In `prepare_move_session_controlled`, `move_in_place_for_record`, and actual retry selection, use the old Move's retained artifact as recovery evidence. For a newly created operation use `checked.in_place`, matching the workspace-transfer decision. Extend the real in-place profile Move regression with a historical session checkpoint and failed old Move to a different profile, proving there is no workspace transfer and the environment remains intact.

In `mj-controller/src/daemon/process.rs`, recovery result publication should log outcomes without reloading configuration; record publication is already observed atomically by `RuntimeState::owner`. Move upgrade preparation-failure settlement from `daemon/support.rs` and `daemon/snapshot.rs` into the supervised task in `mj-controller/src/worker_upgrade.rs`. Keep revision checks and failure propagation, reserve the session through durable settlement, and retain bounded shutdown ownership. Test result publication while the configuration mutation lock is held, and test settlement against a superseding lifecycle revision. No new concurrency cap or timeout is authorized for this fix.

## Concrete Steps

From the repository root, use the existing Cargo and mbx configuration, without redirecting build outputs. Run all `cargo test` commands with elevated permissions. Keep the same package selection for focused and final tests:

    cargo test -p brokk-mj-core -p brokk-mj-controller <affected-test-filter>
    cargo test -p brokk-mj-core -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Use the regression fixtures' existing private configuration/data directories. Any test invoking an actual binary or daemon must use `--instance quota-move-1498`; never use the default live instance. Review the diff, stage only owned changed files, commit on the current branch, request the configured push review, and push HEAD to origin/master when that review is satisfied. If upstream rejects the push, fetch and merge upstream into this same branch, without rebasing or creating a branch.

## Validation and Acceptance

The quota Move regression must demonstrate successful same-environment automatic profile switching with an idle turn and active goal while preserving child behavior. Its running, newer-completed-user and autonomous-turn modes must still refuse or defer without CancelTurn. An idle goal must not produce an interruption warning for ordinary/manual replacement or block the existing atomic worker-upgrade lease; a running goal and pending goal operations must remain blockers. Upgrade proof must distinguish a new-core fake speaking an old protocol from an actual old binary with the defective admission predicate. A different profile choice after a failed pre-stop Move must prepare and execute in place, with no workspace transfer; genuine interrupted Moves must retain their recovery path. Existing handshake timeout transport coverage must still pass. Final tests and clippy run in the development profile with consistent core/controller/worker package selection. Record failures and rerun only failures after repairs; full touched-crate validation runs once after the implementation stabilizes.

## Idempotence and Recovery

The initial stage required no database migration or protocol change. This follow-up advances the relay protocol to 60 to distinguish corrected worker-owned idle-goal admission, with no new wire request or persisted format. Regression fixtures own their private state. Source changes are reversible through version control. A failed validation leaves the checkout available for targeted repairs. The user explicitly requested each installation, including this global correction. Keep source and Git HEAD unchanged during each installer build so controller and worker identities agree.

## Artifacts and Notes

The original session is `85e2d8a9f8798bc50e1b00b854c7301c`. The decisive log is `/home/jonathan/.local/share/mjolnir/logs/mj-daemon-20261111T142643.474Z-92692.log`: automatic refusal at 15:09:48 UTC; manual source lease took 163185 ms and failed at 15:16:30 UTC. Issue: https://github.com/BrokkAi/mjolnir/issues/1498.

## Interfaces and Dependencies

Reuse the existing activity facts, Move preparation, source connection lease, and goal restore formats. No new dependency or persisted field is required. The timeout remains the existing `RELAY_HANDSHAKE_TIMEOUT` constant, shared by initial connection and reconnect.

The follow-up adds RELAY_IDLE_GOAL_INTENT_PROTOCOL = 60 as a semantic boundary, not a new wire shape. IdleWorkspaceLease owns its original RelayCursor and validates the existing checkpoint barrier against it. Existing protocol-58 and protocol-59 goal-bearing workers can bootstrap with BeginCheckpoint and same-ID ReserveIdle confirmation; new workers continue to use initial ReserveIdle. The strict worker rule rejecting envelopes below its current protocol remains unchanged.

Revision note (2026-11-11): Initial plan records the confirmed cause, authorized changes, ownership, and acceptance conditions.

Revision note (2026-11-11): Added the transfer-chooser defect and recovery ownership fix, and recorded explicit advance-merge authorization and completed fast-forward.

Revision note (2026-11-11): Recorded the user's expanded recovery/upgrade publication scope and additional relay disconnect report, with confirmed evidence and the selected ownership fix.

Revision note (2026-11-11): Recorded completed validation, push review, upstream merge, push, explicit installation authorization, successful daemon handoff, and the separate follow-up memory transport plan.

Revision note (2026-11-11 19:25Z): Reopened execution for the user's global idle-goal rule and the explicitly requested follow-up installation. The user owns the separate node-removal work; merge its landed upstream result before installing rather than restoring the stashed capacity patch.

Revision note (2026-11-11 19:28Z): Worker ownership disproved the queued quota-Move hypothesis. Removed its proposed exception and acceptance condition; added actual old-worker admission as required delivery work, because changing the daemon does not replace the old worker's compiled predicate.

Revision note (2026-11-11 19:32Z): Selected a protocol-marked use of the existing historical-worker upgrade bootstrap and original barrier-cursor validation, preserving the old path's observed-idle limitation rather than claiming atomic admission from a binary that cannot provide it.
