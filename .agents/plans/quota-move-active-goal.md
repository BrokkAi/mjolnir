# Preserve an active goal during automatic quota Move

This ExecPlan is maintained according to `.agents/PLANS.md`. Its progress and decisions describe issue #1498.

## Purpose / Big Picture

A session whose turn stopped at a provider quota limit should be able to switch to another profile of the same harness on the same target, preserving its conversation, durable goal, and managed children. An active goal alone must not look like work that this Move interrupts. A turn that starts after preparation must still prevent an automatic Move. Worker connection handshakes must stop waiting after the user-requested 60 seconds, instead of 300 seconds.

## Progress

- [x] (2026-11-11) Confirmed the failing session has idle execution and an active goal, and that the goal triggers the generic replacement predicate.
- [x] (2026-11-11) Confirmed a separate manual Move waited 163 seconds for a reconnect on a full disk. Expanded the requested host's EBS volume and filesystem separately.
- [x] (2026-11-11) Created and claimed issue #1498; delegated disjoint implementation and read-only investigation.
- [x] (2026-11-11) Implemented shared quota-Move eligibility at preparation, leased source, and non-interrupting checkpoint acquisition.
- [x] (2026-11-11) Four-mode isolated regression passed: full active-goal Move, running-turn refusal, newer completed-turn refusal, and autonomous-turn deferral without cancellation.
- [x] (2026-11-11) Corrected failed pre-stop retry classification; the regression proves a different profile stays in place with no workspace transfer.
- [x] (2026-11-11) Changed the worker handshake budget to 60 seconds.
- [x] (2026-11-11) Removed blocking recovery/upgrade result settlement from the serving loop; four focused publication/admission/lifecycle tests passed.
- [ ] Diagnose the reported `d1af5b48` project-memory snapshot relay disconnect and resolve the proven cause if it lies in this path.
- [x] (2026-11-11) With explicit user authorization, fast-forwarded the checkout by 80 upstream commits to 2fc7ec17b, preserving all in-progress edits.
- [x] (2026-11-11) Reviewed the integrated changes; completed full core/controller suites with the sole older-fixture failure repaired and rerun, workspace clippy, formatting and diff checks.
- [ ] Commit, complete push review, push to origin/master, and close the issue.

## Surprises & Discoveries

The failed automatic Move was not blocked by its 13 managed children. The broad `safe_to_replace` activity check rejects a session whenever a durable active goal owns it, even though the quota failure has already stopped the turn. The live session's goal is active, rather than limited with a usage reason. Adapter-reported goal status must not be rewritten just to pass Move admission.

The existing same-environment Move preserves managed children by closing and draining worker child-mutation admission before replacing the parent. That behavior must remain intact. Manual Move still uses its existing interruption warning.

A second observed defect explains the user's transfer chooser: an old failed pre-stop Move to one profile plus an unrelated historical source checkpoint causes a new profile choice to be classified as a restore retry. The old operation owns no checkpoint or handoff, and its source is still running. The generic historical checkpoint is not evidence that the old Move needs recovery.

The user expanded scope to stalls while publishing recovery and worker-upgrade results. The daemon's serving select loop awaits a configuration reload for every recovery result, including cancellation and deferral. One observed phase lasted 190503 ms while the runtime heartbeat remained recent. Production records already arrive through the committed-state feed, so this reload is unnecessary. Upgrade preparation-failure settlement also performs durable work on that serving loop. The logs identify the ownership defect but not the precise holder of the configuration lock; do not attribute the wait to database size without evidence.

The user also reported a Move error for `d1af5b48898e8d2a9f970b26631bb81a`: an established relay disconnected during `project_memory_snapshot` at 15:38:25 UTC. Its source remained running. This is distinct from the handshake timeout, and its worker-side cause is under investigation.

## Decision Log

Decision: Treat the durable active goal as preserved for automatic same-harness, same-environment quota Move, keeping interruption acknowledgement false and keeping the remaining activity blockers. Use one shared decision at preparation and again while the source connection is leased. Rationale: quota recovery already established a stopped turn; the durable goal is the work to continue, while newly started execution must not be interrupted. Date: 2026-11-11.

Decision: Set the existing handshake timeout to 60 seconds without changing command-acknowledgement or review-capture budgets. Rationale: the user explicitly requested this bound after observing a multi-minute reconnect stall. Date: 2026-11-11.

Decision: Initially work on the current checkout after automatic approval review rejected the advance merge. The user subsequently explicitly authorized merging upstream in advance; completed the fast-forward to 2fc7ec17b with edits preserved. Rationale: honor the clarified authorization and validate against current upstream. Date: 2026-11-11.

Decision: Use the existing `MoveOperation::checkpoint_retained()` to identify recovery retries instead of looking at the source session's arbitrary historical checkpoint. Copy the checked in-place decision into new durable operations. Rationale: the Move record owns recovery evidence, and preparation and execution must not compute conflicting environment decisions. Retain existing sealed-environment checks and partial-stop recovery adoption. Date: 2026-11-11.

Decision: Remove unnecessary recovery-triggered configuration reloads in production, and settle upgrade preparation failures within the supervised upgrade attempt before sending its result. Keep the session reservation until settlement and make serving-loop reporting synchronous. Rationale: filesystem/database work and configuration-lock waits cannot block accepting connections or shutdown; failed publication tasks must not be orphaned or dropped. Date: 2026-11-11.

Decision: Bind the automatic preparation's opaque fingerprint to its original user/completed-turn identity, and use a quota-specific non-interrupting mode of the existing checkpoint latch. Revalidate pending quota recovery against the ready checkpoint and let the existing exact Close cursor seal that checked cut. Rationale: a new turn may start and finish before lifecycle ownership, or start autonomously while a checkpoint is acquired. Current activity alone cannot prove an old quota decision is still relevant. This reuses existing formats and ownership instead of adding protocol or database fields. Date: 2026-11-11.

## Outcomes & Retrospective

Implementation and validation are in progress. No installation is authorized by this task. The live disk expansion was completed before this plan; source changes do not require touching live sessions.

The result-publication change now keeps configuration-lock waits out of the serving loop. Upgrade failure settlement retains the existing per-session attempt and a named daemon admission through the durable write. Four focused tests passed, including actual Ping/Stop traffic while the configuration lock remains held and a real SQLite write-lock wait while another session proceeds. Log: `.mj/agents/4a58f6d94bc675fcf4acd5f24798cfc1/publication-tests-retry.log`.

The full admission regression now requires a completed Move, the destination profile, unchanged native history/cache, and an active goal in the canonical checkpoint. It also proves that a running turn and a newer completed turn prevent the old automatic decision, and that an autonomous turn at checkpoint acquisition remains alive without a CancelTurn command. Intermediate fixture failures exposed missing native history and insufficient outcome assertions; both were corrected. Passing log: `.mj/agents/1cbd92b1e462d4831611d7744363a0df/retest8.log`. Full touched-crate tests and workspace clippy are running after the source edits stabilized.

Final validation: controller 2544 passed and 19 ignored, with one older native-source fixture initially failing because it supplied no pending quota recovery. That fixture now models a real quota-stopped live source and its focused rerun passed. Core 643 passed and 3 ignored; the three Jev scenario tests passed; documentation tests completed without failures. Workspace clippy passed after collapsing one nested conditional. Relevant logs are `consolidated-tests.log`, `consolidated-core-tests.log`, `consolidated-core-scenarios.log`, `consolidated-doctests.log`, and `consolidated-clippy-retry.log` in the publication agent's report directory, and `native-drain-fix2.log` in the Move agent's report directory. No full suite was repeated to confirm the fixture repair.

The separate relay disconnect is now proven to be an oversized project-memory snapshot frame on morannon, tracked in #1500. Its transfer design is under investigation; raising the global frame cap would only postpone the failure.

## Context and Orientation

The daemon is the control service owning session lifecycle operations. `mj-controller/src/daemon/session_move.rs` prepares manual and automatic Moves and checks automatic safety. `mj-controller/src/controller/move_session.rs` revalidates preparation after taking the source worker connection, then checkpoints and restores the session. A worker holds the harness, which is the provider's coding-agent process. `mj-core/src/activity.rs` decides whether observed worker state contains work. Its ordinary replacement predicate rejects an active goal even when no turn is running. `mj-controller/src/worker_client.rs` defines the connection handshake timeout; `worker_client/connect.rs` uses it for initial connections and reconnections.

## Plan of Work

Provide a shared Move activity decision that distinguishes quota recovery from ordinary replacement without duplicating the activity rules. For automatic quota Move, exclude only the durable active-goal ownership fact. Use that decision in daemon preparation and in the controller's source-lease revalidation. Keep non-preserved work, source unavailability, and child-mutation safety checks. Keep existing goal continuation behavior unless the regression proves it creates duplicate continuations.

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

The quota Move regression must demonstrate successful same-environment automatic profile switching with an idle turn and active goal while preserving child behavior. The execution-time regression must show that newly started work refuses the Move without invoking an interrupting stop path. Ordinary/manual replacement must retain its existing warning behavior. A different profile choice after a failed pre-stop Move must prepare and execute in place, with no workspace transfer; genuine interrupted Moves must still retain their recovery path. Existing handshake timeout transport coverage must still pass. Final tests and clippy must run in the development profile. Record failures and rerun only failures after repairs; full touched-crate validation runs once after the implementation stabilizes.

## Idempotence and Recovery

No database migration, protocol change, or live session mutation is planned. Regression fixtures own their private state. Source changes are reversible through version control. A failed validation leaves the checkout available for targeted repairs. Do not install the changes unless the user requests installation.

## Artifacts and Notes

The original session is `85e2d8a9f8798bc50e1b00b854c7301c`. The decisive log is `/home/jonathan/.local/share/mjolnir/logs/mj-daemon-20261111T142643.474Z-92692.log`: automatic refusal at 15:09:48 UTC; manual source lease took 163185 ms and failed at 15:16:30 UTC. Issue: https://github.com/BrokkAi/mjolnir/issues/1498.

## Interfaces and Dependencies

Reuse the existing activity facts, Move preparation, source connection lease, and goal restore formats. No new dependency or persisted field is required. The timeout remains the existing `RELAY_HANDSHAKE_TIMEOUT` constant, shared by initial connection and reconnect.

Revision note (2026-11-11): Initial plan records the confirmed cause, authorized changes, ownership, and acceptance conditions.

Revision note (2026-11-11): Added the transfer-chooser defect and recovery ownership fix, and recorded explicit advance-merge authorization and completed fast-forward.

Revision note (2026-11-11): Recorded the user's expanded recovery/upgrade publication scope and additional relay disconnect report, with confirmed evidence and the selected ownership fix.
