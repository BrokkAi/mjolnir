# Preserve Claude execution policy when implementing an approved plan

This ExecPlan is maintained under `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections are living records.

## Purpose / Big Picture

Selecting Implement must resume Claude in Auto on guardian deployments and bypassPermissions on unconstrained deployments. Today mj selects manual approval, then cancels the resulting edit permissions. The fix belongs to the shared ACP runtime so terminal and web clients behave identically.

## Progress

- [x] Investigated the failed anvil session and tested the installed bridge and SDK with isolated, non-persistent ExitPlanMode probes.
- [x] Finalized the mj-only workaround with the user.
- [x] Implement policy-aware option selection and the supervised continuation sequence.
- [x] Add deterministic ACP behavior tests and run repository validation: cargo test and cargo clippy --all-targets -- -D warnings passed.
- [x] Committed implementation as c5ad2128 on hel3 and merged origin/master as f475961b. Merged chat/TUI tests and the final warnings-as-errors Clippy check passed. Publication uses `git push origin HEAD:master` after committing this validation record.

## Surprises & Discoveries

Bridge 0.73.0 offers exit-plan-auto instead of exit-plan-bypass when Auto is available. Its older payloads used literal mode IDs. Selecting a clear-context option would discard conversation context. SDK 2.1.257 skips the permission callback in bypass mode, but invokes it after switching bypass to plan. An isolated bridge probe successfully returned Cancelled, awaited the turn, set bypassPermissions, and sent another prompt in the same session without another permission callback. Selecting the reject option instead produced an internal diagnostic in one probe, so the workaround uses Cancelled.

## Decision Log

The user chose an mj-only correction after considering a bridge change. Guardian approval selects the exact Auto option. Unconstrained approval selects the exact bypass option when present and otherwise cancels the native permission request, waits for the turn, explicitly changes mode, and sends a continuation containing the approved plan. No manual approval, clear-context option, bridge patch, or dependency update is part of this work.

The runtime must keep the original prompt command active across the transition and continuation. This prevents the relay from starting queued work or checkpointing in the middle. Cancellation, close, unexpected errors, and transport loss discard the continuation; it is never replayed after restart.

The implementation uses a connection-local sender captured when the permission request arrives. A per-command guard removes the sender on every exit path; an acknowledgement channel also requires successful delivery of the native permission cancellation before mode restoration. The session loop continues servicing cancellation and close while the mode request is outstanding. Both legacy session modes and the newer config-option mode interface use the existing execution-enforcement helper. A config response reporting a different mode fails the transition.

The user subsequently authorized merge and push. The current branch is hel3, tracking origin/master. Merge fetched upstream changes into hel3, validate the merged result, and push HEAD to origin/master without switching branches.

## Outcomes & Retrospective

Implementation and workspace validation are complete. The full cargo test command passed, including 756 passing core tests (four ignored) and all default workspace members. Clippy with warnings denied passed on retry; its first concurrent run encountered an mbx metadata-artifact collision, not a source diagnostic. Initial ACP validation passed 73 of 74 tests; the failing transport test half-closed its simulated pipe without reproducing the supervisor's driver teardown. The fixture now drops the driver exactly as run_bridge does when its child exits. A cancellation test also exposed the protocol library's expected $/cancel_request notification for the dropped mode future; the test now verifies that notification explicitly. No existing anvil session has been modified.

Fourteen added tests exercise native approval, old and new IDs, context preservation, an 88 KiB plan over a 4 KiB duplex stream, response ordering, duplicate answers, cancellation in both transition phases, close, timeouts in both phases, driver teardown, prompt failure, missing Auto, mode rejection, and verification of the config response's effective mode. The workaround requires an active foreground prompt to own the continuation; a permission request without one is cancelled with an actionable warning rather than starting untracked work.

The upstream merge included only the separate dashboard Ctrl-C fix. `cargo test -p brokk-mj-chat -p brokk-mj-tui` passed on the merged tree (308 chat tests, 282 TUI tests, two TUI tests ignored), followed by a successful `cargo clippy --all-targets -- -D warnings`. The implementation's core files were unchanged by the merge. The working branch remains hel3 and the requested publication target is origin/master.

## Context and Orientation

`src/hel_acp.rs` contains the permission callback, policy-independent `permission_plan_response`, and `serve_session`, which serializes commands while a prompt is running. `src/hel_acp/tests.rs` contains in-process duplex ACP fakes. `mj-worker/src/hel_worker_runtime/unix.rs` records runtime events into the durable relay. Existing PromptFinished events release the active relay command; therefore the workaround must delay this event until the implementation continuation finishes or fails. ExecutionPolicy::ConfiguredApprovals is guardian and ExecutionPolicy::Unconstrained is full access.

## Plan of Work

First change Claude approval selection to consume harness and execution policy and return either a native selected option, a deferred bypass continuation, or an explanatory error. Match exact IDs (auto / exit-plan-auto and bypassPermissions / exit-plan-bypass); preserve non-Claude and non-implementation behavior. Add focused selection tests.

Then connect permission callbacks to the session command loop with a connection-local handoff. Record the continuation before returning Cancelled so prompt completion cannot race ahead of it. During an active command, wait for its ACP prompt to settle with a bounded deadline, then await session/set_mode acknowledgement and publish updated mode state before sending the approved-plan continuation. Keep the original command active throughout. Service cancel and close while switching modes, and never submit a continuation after cancellation or failure. Report progress and errors through existing runtime events. A connection restart must discard all pending continuation state.

Finally exercise the actual ACP messages with a handwritten fake: assert no second prompt before old-turn completion and mode acknowledgement, then exactly one continuation with the same session and plan. Cover cancellation, close, duplicate answers, unavailable modes, failures, and unrelated harness behavior. Update this plan with evidence and commit only changed files.

## Concrete Steps

Work in `/home/jonathan/Projects/hel3`. Use apply_patch for edits. Run focused tests with `cargo test -p brokk-mj-core hel_acp`, followed by `cargo test` and `cargo clippy --all-targets -- -D warnings`. All cargo test commands require escalated execution. Use normal build storage. Run rustfmt and check the final diff, then stage explicit paths and commit on the current branch. Merge origin/master and push HEAD:master to origin after validating the merged result, as subsequently requested by the user.

## Validation and Acceptance

Tests must prove guardian Auto selection, unconstrained bypass selection, and the absent-bypass sequence across real JSON-RPC boundaries. Reorder options and include clear-context choices to prove exact selection. Use a plan exceeding 64 KiB in the stream test. Delay prompt completion and mode acknowledgement independently. Verify one continuation only, preservation of the plan and session, and no continuation after cancel, close, timeout, transport loss, unexpected prompt failure, or rejected mode change. Existing generic plan approvals, revision, and decline tests must still pass.

## Idempotence and Recovery

The continuation is connection-local and consumed once. Duplicate elicitation answers already fail because the pending answer sender is removed. Interrupted transitions report failure instead of replaying implementation after restart. No database migration, release, branch change, or live-session repair is part of this implementation. Merge and push were subsequently authorized.

## Artifacts and Notes

The successful isolated probe observed a permission callback in plan mode, a completed turn, an acknowledged bypass mode change, and a completed subsequent turn without another callback. Probe errors and max-turn limits were diagnostic bounds, not successful implementation tests.

## Interfaces and Dependencies

Keep the public relay command protocol unchanged where possible. Use the existing ACP SetSessionModeRequest, PromptRequest, RuntimeEvent mode/configuration observations, cancellation timeout, and supervised session loop. Introduce only small private selection/handoff types near their consumers. Do not add a workspace crate or new dependency.

Revision: implementation choices, initial test evidence, and the user's merge/push authorization are recorded above so the remaining validation and delivery steps are reproducible.
