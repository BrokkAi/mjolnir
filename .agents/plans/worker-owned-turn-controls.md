# Publish worker-owned turn controls


This ExecPlan is a living document maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


A user who changes model or effort during a running turn and then queues a prompt must see a control that can actually advance that queue. The worker (the process running one session's agent) owns queue order and delivery. It will publish the permitted controls, and the browser and terminal will render those controls. On a narrow mobile screen, the primary button says “Apply queued”; nearby status text explains that this interrupts the parent turn and applies pending changes. An independent interrupt action remains available without stopping child sessions.

## Progress


- [x] (2026-10-17) Trace the production refusal and inspect queue ownership, Web rendering, and TUI control selection.
- [x] (2026-10-17) Publish worker-owned control descriptors and reuse their decision in admission and automatic steering.
- [x] (2026-10-17) Carry typed queue items and control descriptors to Web and TUI; keep mobile actions compact.
- [x] (2026-10-17) Add regressions for queue barriers, stale targets, child isolation, reconnect, and narrow-screen rendering.
- [x] (2026-10-17) Run affected tests, full suites of all six touched crates, workspace Clippy, browser regressions, and format checks.
- [x] (2026-10-17) Prepare the validated current-branch commit and the authorized push/install follow-through. Execute them after this final source update so installed binaries share one commit stamp.

## Surprises & Discoveries


The Web viewer's queue projection removes `QueuedCommandKind`. The browser then treats its first queue item as a prompt, whereas the worker's queue also contains settings changes. The TUI retains kinds but recomputes the permitted action itself. In the affected session two setting changes blocked automatic steering, and both manual Steer requests were rejected as “The queued prompt changed”. Parent-only interruption successfully applied settings and started the preserved prompt while both running child turns retained their start times.

## Decision Log


- Decision: Preserve the single ordered command queue and its settings barriers.
  Rationale: A prompt accepted after Fable/High changes must run with those settings. Steering around the barrier would violate acceptance order.
  Date/Author: 2026-10-17, Codex.
- Decision: Publish action descriptors from the worker, with commands bound to exact turn and queue identities. Admission uses the same decision; stale actions are refused and the clients refresh.
  Rationale: Independent client predicates caused this bug. Serialized worker admission prevents a click from moving onto a later turn or queue entry.
  Date/Author: 2026-10-17, Codex.
- Decision: Keep short mobile button labels and put the interruption explanation in adjacent wrapping status text. Preserve explicit parent-only interruption separately.
  Rationale: The user explicitly requested mobile space constraints, and steering must never silently turn into cancellation.
  Date/Author: 2026-10-17, Codex.

## Outcomes & Retrospective


The worker now publishes and admits the same exact controls. The browser and terminal consume that decision, preserve settings barriers, and offer parent-only interruption. At 320 pixels, the primary action fits without horizontal overflow and the separate interrupt remains available in a compact menu. The isolated real-daemon/worker scenario passed, including child-turn preservation, stale-action rejection, and daemon handoff in 0.356 seconds. Browser unit tests (56), all compact browser cases (18, including corrected legacy labels), focused worker/TUI/API regressions, format checks, and workspace Clippy passed. Full suites of all six touched Rust crates and the final shared-whitelist Clippy check passed. Commit, push, and installation remain. Validation has not changed the host daemon or live session data.

## Context and Orientation


`mj-worker/src/relay/commands.rs` serializes command acceptance, validates targeted steering, and chooses automatic steering. `mj-worker/src/relay.rs::operational_state` publishes current process facts. `mj-core/src/relay/snapshot.rs` defines commands and the operational wire snapshot; its `apply.rs` folds durable journal events. A typed queue contains prompts and `SetConfig` commands, which run only between turns. `mj-controller/src/server_runtime/snapshot.rs` converts worker state and transcript materializations for the browser; `mj-controller/src/server/viewer_types.rs` defines those browser shapes. `mj-controller/src/web/viewer.js` renders controls. `mj-chat/src/chat/turn_control.rs` and `status.rs` implement terminal controls. Child sessions have independent workers; parent cancellation must never invoke lifecycle suspension or recursive child cancellation.

## Plan of Work


First add serializable control descriptor types to mj-core and an optional operational field populated by the worker. A descriptor contains the actual permitted command and a compact action kind. Include a targeted interrupt-and-advance command with both the active turn ID and queue-head ID so an old click cannot apply a different queued command. Extend relay protocol compatibility for the new command without changing existing durable formats. Compute controls in one worker method, reusing it from targeted admission and automatic steering. Preserve explicit interrupted, uncertain, and unavailable states rather than treating submission as delivery.

Next project the descriptors without recomputing their meaning. Preserve queue kinds in browser queue rows. Update Web and TUI to use published commands; older workers may expose only explicit interruption and must not regain inferred steering. Use short labels (“Steer”, “Apply queued”, “Interrupt”) and wrapping explanatory text. Keep an always-accessible parent interruption when primary steering is offered. Existing failure and uncertain-delivery recovery remains durable.

Finally extend existing real-worker and browser regressions. The essential ordering is running parent, setting changes, queued prompt, apply action, old turn cancellation, settings completion, prompt start. Assert child turns remain untouched. Exercise a stale click after turn completion and after queue-head replacement. Test mobile layout with a 320–390 pixel viewport and reconnect restoration.

## Milestones


First, publish the worker decision and admit that same command in its serialized queue owner. The worker relay tests verify that model and effort changes finish before the following prompt starts, a stale target cannot cancel a later turn, and an admitted action is checked again before dispatch.

Second, pass those choices unchanged through the daemon to Web and terminal controls. The browser tests verify reconnect restoration and a 320-pixel header. A pointer held across a refresh retains its original command; its eventual click cannot select the replacement turn. Ctrl-Esc in the terminal and the browser menu provide a separate parent-only interrupt.

Third, verify the user-visible behavior against a real isolated daemon and worker, complete the full touched-crate suites and workspace Clippy, commit to the current branch, push to its upstream, and run the normal installation script. The real-worker fixture must preserve the running child while applying the parent's ordered settings and prompt. Installation begins only after the commit and push, so every installed binary has the same build stamp.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir4`. Inspect the named modules before editing; use existing streamed subprocess helpers and test isolation. Run focused tests while iterating, for example `cargo test -p brokk-mj-worker <filter>` and `node --test tests/e2e/web/viewer.turn-control.unit.test.mjs`. All Cargo tests require elevated execution outside the restricted sandbox. Run final full suites for each touched Rust crate and `cargo clippy --all-targets -- -D warnings` on the dev profile. Use the existing mbx Cargo wrapper and build directories unchanged. Run browser tests through the existing Playwright configuration with an isolated named instance whenever running a new mj binary. Record exact outcomes here after execution.

## Validation and Acceptance


The worker advertises Apply queued for a settings barrier and Steer only for a steerable prompt at the queue head. Clicking the advertised command advances the intended queue in order. A stale action cannot cancel a new turn or consume a replacement queue entry. Web and terminal controls reflect the published decision after reconnect. The mobile composer fits without horizontal overflow and clearly states when Apply queued interrupts the parent. Explicit parent interruption leaves active child session turns running. Existing upgrade/handoff tests retain their isolated data directories. No database migration is expected because control availability is an operational snapshot field, not a schema change.

## Idempotence and Recovery


Control submissions retain command IDs for existing idempotency behavior. Never replay a command merely because its acknowledgement was lost. The worker rechecks target identities in the serialized owner before admitting controls. Preserve uncertain steering recovery rather than guessing delivery. Tests use temporary directories or a named isolated instance, never the default daemon/store. Commit only task files on the current branch; the user explicitly authorized push and installation after validation. Complete all commits before starting installation so daemon and worker build stamps match.

## Artifacts and Notes


Production evidence: a queue containing model=fable, effort=high, then a prompt produced two manual Steer refusals. Parent-only `mj interrupt-turn` advanced that exact queue and left both running children on their original turns. This is the regression to protect.

## Interfaces and Dependencies


Use existing serde wire types, the relay journal/state machine, `RelayCommand`, and `QueuedCommandKind`. Add no crate. The worker's operational control descriptor is optional for older snapshots and contains enough information that clients never choose queue targets themselves. `TurnControls`, `TurnControlAction`, `TurnControlKind`, and `TurnControlStatus` are defined in `mj-core/src/relay/snapshot/turn_control.rs`. Relay protocol 40 adds `ApplyQueuedFor { active_prompt_id, queued_command_id }`, dispatched using the existing targeted cancellation request after exact admission. Older workers retain explicit interruption only; harness-initiated turns without an owned prompt ID retain their pre-existing explicit interruption surface. All queue-steering choices for owned turns now come from the worker. The browser must carry descriptors unchanged through `ViewerSession` and the browser wire projection.

Plan created 2026-10-17 after confirming the production failure and the user's mobile constraint.

Update 2026-10-17: implemented protocol 40 and compact Web/TUI controls. The 320-pixel browser regression initially caught one pixel of header overflow; narrowing the mobile model selector resolved it. Browser unit suite: 56 passed. Focused browser steering/reconnect and mobile settings cases pass. Full compact browser suite found two legacy label expectations (“Interrupt turn” versus the new “Interrupt”); rerunning those cases after correcting their expectations. Rust compilation and focused state-machine/API tests are in progress. Added real daemon/worker coverage to `tests/e2e/delegation.py` for parent-only settings advancement and stale clicks.

Update 2026-10-17: workspace Clippy passed after carrying queue kinds through the terminal dashboard. The first API regression enabled a capability by numeric index after adding a child to a sorted map; it now selects `session-1` by ID. Added HTTP refusal diagnostics to prevent opaque channel-closed failures. The browser now freezes its published command at pointer/key-down so refresh during a touch cannot select a later turn; the 320-pixel test changes turn and queue identities while the button is held and confirms the original command is submitted. Focused browser checks: four passed, including legacy interruption and reconnect. Corrected worker tests distinguish connection reconnection from process restart, preserve admitted steering after a hook disappears, and require a turn boundary for attachment prompts.

Update 2026-10-17: the three corrected relay regressions pass; the targeted TUI and API tests pass. Targeted controls now bypass dashboard activity/capability guesses at the daemon boundary: the daemon checks session existence and command type, and the worker alone admits exact turn/queue targets. Web and TUI likewise use published choices even when older activity data lags. Browser regressions cover stale capability data and press ownership. The real daemon/worker regression successfully advanced settings, started the prompt, rejected the old target, and preserved the child's active command. Its subsequent existing wait case exposed fixture coupling (the newly created parent turn ended while that wait was attached); the new case now explicitly settles its parent before restoring the existing tests. Full six-crate validation and the cleaned-up delegation script are running.

Update 2026-10-17: the complete real daemon/worker delegation test passed in named instance `turn-controls-20261017-r3`; artifacts are in `target/reliability-artifacts/delegation-seed-1171-2618567`. The final daemon command whitelist is shared by API admission, action validation, and dispatch. Final full Rust validation uses `cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir`; Clippy uses `cargo clippy --all-targets -- -D warnings`. The API turn-control regression deliberately leaves dashboard capabilities stale to confirm the worker alone owns action admission.

Update 2026-10-17: full Rust validation completed successfully, including all touched-crate unit/integration suites and doctests. Main suite counts are chat 412, controller 2,009, core 502, terminal 525, worker 688, and CLI 212 passed; the CLI and core integration binaries also passed. Final workspace Clippy and format/diff checks passed. The test log is `/home/jonathan/.cache/mjolnir/build/turn-controls-final-tests.log`. The source is complete and ready to commit and push before the normal release-profile installation.

The implementation and validation milestones are complete. Operational follow-through is `git commit` on the current branch, `git push origin HEAD:master`, and `scripts/install.sh`, followed by checking the installed daemon build. Record those results in the user response rather than making a post-install documentation commit that would change the build stamp. If upstream moves, fetch and merge `origin/master` before installation; never commit or merge while installation is building.
