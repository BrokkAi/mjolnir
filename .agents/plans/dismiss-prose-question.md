# Dismiss an inferred prose question from session Content actions


This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture


The Sessions row's existing actions menu must offer **Dismiss question** under **Content**. Keep it visible but disabled when no inferred prose question is active. Dismissal clears that turn's question priority without sending input to the agent or granting permission. The worker records it durably so every client agrees and restart cannot resurrect it. A later question must still appear normally.

## Progress


- [x] Inspect the session command registry, asynchronous dashboard actions, relay command submission and durable assessment state.
- [x] Add an identity-bound, relay-local dismissal command and replay its effect using the existing assessment status.
- [x] Add the Content command, disabled availability, asynchronous submission and visible in-flight/error handling.
- [x] Extend the existing session-menu golden and add persistence/staleness regressions across the relay boundary.
- [x] Run changed-crate suites outside the sandbox, workspace Clippy, formatting and diff checks; record unrelated baseline failures.
- [x] Review the complete change for commit on master, push to origin/master and the subsequently authorized local install.

## Surprises & Discoveries


The daemon already accepts generic worker commands through `submit_session_command`, so a new daemon action or independent persistence path is unnecessary. The worker's completed-turn assessment is already durable. The preceding prose-question propagation fix carries that assessment with its materialized projection through the background queue. The controller persists existing assessment fields and the existing `RecordNotice` command kind; the new raw command lives in worker journals. No database shape changes are needed. The full validation initially exposed an existing chat navigation golden that assumes UTC; Central time rendered 18:00 instead of 00:00. Keep that golden unchanged and run timestamp-sensitive checks with `TZ=UTC`. Relay protocol 48 advertises the operation, and worker state revision 21 is breaking because older readers cannot replay the new command. Older snapshots upgrade through the existing generic version advancement. Full CLI validation also found `an_unknown_workspace_is_refused_at_start_from_the_store_without_a_daemon` failing with exit 0 instead of 1. Commit `0bd61a289` removed the direct store read from `listed_workspaces_without_starting`; that function and the failing ACP test are unchanged by this feature. The two daemon-startup cases `new_without_a_workspace_refuses_without_starting_a_daemon` and `new_with_an_unknown_workspace_refuses_without_starting_a_daemon` also fail at their old workspace-listing/no-daemon expectations for the same reason. The other 15 startup cases passed, including isolated daemon upgrade regressions. Preserve the daemon-only ownership boundary and report this pre-existing failure rather than restoring a client-side store read or weakening the test in this change.

## Decision Log


Use the existing serialized worker owner and the existing `Superseded` assessment status with a dismissal reason. Bind the request to the completed command ID and completion ordinal. Check identity and whether the question remains active before accepting the command, then apply the status transition during command replay. This prevents a delayed request or classifier response from affecting a later turn and survives partial journal writes without issuing a prompt.

Only inferred completed-turn questions are dismissible. A pending structured elicitation or permission form keeps its own response semantics. Reading a conversation remains independent of dismissal. Advertise the new worker command through a relay protocol revision and guard its persisted command format against older worker readers; inspect controller persistence before deciding whether database compatibility needs to advance.

## Outcomes & Retrospective


Implementation and validation are complete. The menu snapshots cover the idle, active, submitting and dismissed states; the real worker journal test covers replay, retries, old targets, delayed classifier answers, authorization preservation and pending forms. Core, client, chat, TUI and worker suites passed, as did all 231 CLI unit tests, the remaining CLI integration targets and doctests. The only unresolved checks are the three pre-existing workspace-validation integration tests described above. The UTC chat rerun passed without changing that golden. Workspace Clippy, the final TUI Clippy check, formatting and diff checks passed.

Commit the reviewed files on master and push to origin/master; merge a moved upstream without rebasing. The user subsequently requested `scripts/install.sh` after pushing. The current local installation is 2.64.1, older than this 2.65.0 checkout, and no install was running at the pre-commit check. Finish all commits and merges before starting the installer, then run an ordinary installed `mj` command to trigger the supported daemon handoff. Verify daemon metadata and its log without cancelling worker turns. Report publication and installation outcomes in the final response.

## Context and Orientation


`mj-core/src/assessment.rs` owns completed-turn verdicts and their validity. `mj-core/src/relay/snapshot.rs` defines durable worker commands and `snapshot/apply.rs` replays them. `mj-worker/src/relay/commands.rs` admits commands under the relay owner. `mj-tui/src/actions.rs` owns command availability and dispatch; `palette.rs` groups session actions under Content. `mj-cli/src/dashboard/actions.rs` and `dashboard/io.rs` run supervised background work and consume its result. All tests must retain their isolated data directories; no new build may use the default live instance.

## Plan of Work


First add the worker command and replay effect. Reuse assessment identity matching, represent dismissal as terminal for that assessment, and reject stale requests while preserving command-ID retries. Ensure a classifier result already in flight cannot overwrite the dismissal. Prove journal reopen and a later turn preserve the intended behavior.

Then carry the dismissible completed-turn identity through the TUI's existing projection instead of introducing a separate interpretation of the question. Add one registry command under Content and expose the same action through the command palette. Its availability must be blocked, with a reason, when there is no question, when a structured request owns input, or while dismissal is in flight. Submit through the existing daemon client in a background task. Report errors and use authoritative worker publications to clear the marker.

Finally extend the checked-in session-menu golden, including the disabled row, and verify a real relay's command, serialization, restart and stale-target behavior. Do not add ordinary unit examples. Run focused tests while changing code, then the full suites of changed crates and workspace Clippy once the implementation is settled.

## Milestones


The first milestone is a durable worker command. The worker is the process that owns the agent turn and its append-only relay journal; the daemon is the control process clients contact. The new command names an exact completed turn and changes its assessment during journal replay. Run `cargo test -p brokk-mj-worker dismissed_questions_survive --quiet` outside the sandbox. The test must pass after rebuilding the worker state from its journal, accepting a duplicate command ID, rejecting an old target, ignoring a late classifier result and preserving a pending structured form.

The second milestone is a visible session-menu action with asynchronous submission. The TUI is the terminal interface, and its existing Content group owns this command. Run `MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-tui palette::tests::golden --quiet` outside the sandbox and review both changed text snapshots. The menu must show a disabled row without a question, an enabled row for a completed prose question, a disabled in-flight row and the cleared priority after a worker publication. The same test verifies publication before acknowledgement, acknowledgement before publication and a delayed acknowledgement for an older question.

The final milestone is repository validation and publication. Run the complete changed-crate suites and workspace Clippy given below, review formatting and the staged diff, commit on the existing branch and push to origin/master. The user now explicitly authorized the local installation after the push. Run `scripts/install.sh` only after all commits and merges finish; do not change HEAD during the build. Then use the installed `mj workspaces list --json` to coordinate daemon handoff, and inspect daemon metadata and its newest log until it remains running.

## Concrete Steps


Work from the repository root. Use ordinary `cargo` commands so mbx retains ownership of build storage. Run `cargo fmt --all`; run every `cargo test` outside the sandbox. Begin with dismissal-filtered tests, then run `TZ=UTC cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir --quiet` and `cargo clippy --all-targets -- -D warnings`. Existing exhaustive activity fixtures in mj-chat also need the new capability field; their behavior and expected output do not change. Review the golden diff rather than accepting unrelated output changes. Stage only changed files; commit on the current branch and push to `origin/master`. If upstream moves, fetch and merge without rebasing.

## Validation and Acceptance


The session menu shows Dismiss question under Content even on an idle session, where it is disabled. With a prose question it is enabled. Selecting it does not emit a user prompt, marks the exact assessment dismissed and removes question priority after the worker publication. A worker reopen preserves dismissal. A later assessed question is eligible again. An old dismissal and a delayed classifier result cannot clear or resurrect a newer question. A pending form cannot be dismissed by this action. Background errors are visible and the UI remains responsive.

## Idempotence and Recovery


Give the operation a stable command ID tied to the target completion. Replaying an already accepted command must finish its durable transition without repeating work. Do not clear the local marker on a failed request. Keep durable test roots and sockets isolated. Never install or exercise a new build on the user's default instance.

## Artifacts and Notes


Update this plan with implementation decisions, focused regression results, full validation and commit details as the work completes. The existing `session-action-menu` golden is the visual acceptance artifact.

## Interfaces and Dependencies


Reuse `RelayCommand`, `TurnAssessment`, `DaemonClient::submit_session_command`, the TUI command registry and the dashboard background-result channel. New shared data types must represent an actual completed-turn identity or control state rather than duplicate the inference predicate. No dependency or crate additions are needed.
