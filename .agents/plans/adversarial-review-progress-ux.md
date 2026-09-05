# Make review progress clear and cancellation deliberate


This ExecPlan follows `.agents/PLANS.md` and records the implementation follow-up to `.agents/docs/adversarial-review-live-2026-09-05.md`.

## Purpose / Big Picture


A running adversarial review must appear as review activity in the session list and selected chat, instead of idle. Its default pane should explain progress without flooding the user with internal prompts. Users must still be able to inspect raw reviewer transcripts. Pressing Enter again after starting a review must not cancel it accidentally. Global reviewer configuration remains one shared TOML setting; the user asks for design advice on its menu entry, severity calibration, and failure prevention, while explicitly authorizing progress/status and cancellation fixes.

## Progress


- [x] Inspected the current review UI, command palette, global configuration, and previous live-test report.
- [x] Delegated independent progress/input and session-status implementation to Luna agents, plus a read-only reliability audit.
- [x] Integrated and reviewed Overview/input handling, authoritative session activity, actual conversation header, and shared role-completion labels. Repaired the narrow active-reviewer cancellation wait defect.
- [x] Focused UI, TUI, conversation-title, and worker reviewer lifecycle tests passed. Reviewed global F2 configuration entry point, stable severity guidance, and acknowledged-forwarding/preflight follow-up designs.
- [x] Full Cargo suite passed: 2,271 tests, 0 failed, 12 ignored. Focused tests exercised actual render/key handlers, compact/unselected rows, header restoration, and an active scripted reviewer; formatting and diff-whitespace checks passed.
- [x] Clippy with warnings denied passed. Prepared the validated implementation and notes for commit on `hel2` without pushing.
- [x] Recorded configuration, prompt-calibration, and failure-prevention recommendations with remaining work explicitly distinguished from shipped behavior.

## Surprises & Discoveries


The previous repair selected the only enabled running action, Cancel, as the default. That made a duplicate Enter cancel useful work. Action availability and default keyboard focus need separate representation. `RoleState::Clean` also sometimes means a role completed successfully rather than that its judgment was clean, so presentation should say done when it represents completion.

The reliability audit found a concrete cancellation defect: `ReviewerRole::pause` takes `self.running` before its `wait_for` call, but `wait_for` immediately fails when `self.running` is absent. This bypasses the intended acknowledgement interval. It also found that forwarding closes the review and advances its baseline before primary submission is acknowledged; that broader handoff design is recorded as a remaining hardening recommendation rather than concealed inside presentation work.

## Decision Log


Keep one global `[review]` configuration and leave primary model selection unchanged, as requested. Treat configuration/severity questions as design work rather than silently introducing new settings or rewriting the prompt protocol. Use authoritative runtime review state for activity overlays; preserve the underlying primary lifecycle state. Use an overview tab for progress and raw transcript tabs for inspection. Make cancellation explicit through Escape, a click, or deliberate action navigation; do not rely on timing or debounce windows.

Extend the implementation only to the narrow confirmed pause defect: retain the runtime handle during cancellation acknowledgement, then take it for teardown. An active-turn fixture must prove that the harness sees cancellation and can resume without a stale cancel. Do not introduce retries or silently ignore configured model/effort selectors. Keep the larger forwarding transition and target-capability preflight as explicit future designs requiring dedicated actor/controller tests.

## Outcomes & Retrospective


Implemented authoritative activity labels for all session rows and the active conversation header, a scrollable Overview with transcript/verdict tabs, explicit absent default action while running, and accurate role-completion labels. Full Cargo validation passed 2,271 tests with 12 ignored. The worker cancellation regression proves cancellation reaches an active harness before teardown and is not replayed after compatible restart. This follow-up used deterministic terminal rendering/input tests and the scripted ACP harness; it did not create another paid-model container session.

Global configuration remains one TOML section and primary model selection is unchanged. `.agents/docs/adversarial-review-ux-follow-up.md` records the F2 global-dialog design, shared severity rubric/evaluation approach, target-specific readiness, and the remaining acknowledged-forwarding transition. The latter is a concrete unimplemented reliability issue, not covered by the UI fixes.

## Context and Orientation


`mj-chat/src/hel_chat/turn_review.rs` owns review tabs, actions, input handling, and rendering. `mj-chat/src/hel_chat/active.rs` consumes daemon views and attaches reviewer journals. The controller daemon is the background process that runs reviews independently of an open terminal. Its authoritative `RuntimeReviewView` is in `mj-controller/src/hel_review_host.rs`. `mj-tui` owns the session list and command palette, while `mj-cli` connects controller views to the combined terminal. `src/hel_config.rs` defines and saves the global review configuration. `src/hel_review/lanes.rs` holds shared prompts and qualification rules; `verdict.rs` classifies their output.

## Plan of Work


Milestone one changes the running review's default from raw transcript to compact progress. Retain raw journals behind Tab, switch to the verdict when available, and separate enabled actions from keyboard selection. Behavioral input tests must prove repeated Enter is harmless during startup/running and deliberate cancellation still works. Rendering tests must prove progress is initially visible and findings/failed actions still work.

Milestone two projects runtime review activity onto the session list and selected status. Store it as an overlay rather than changing primary lifecycle state. Verify unselected sessions, phase changes, and clearing a review. Coordinate any controller display helper with the parent agent so all surfaces use one interpretation.

Milestone three audits failure prevention and global settings integration. Read the exact save/reload and reviewer launch paths, distinguish observed defects from recommendations, and fix only concrete defects that justify expanding implementation. Keep severity advice centered on shared semantic guidance without new brittle output parsing. Review all changes and perform the required checks before committing.

## Concrete Steps


Work in `/home/jonathan/Projects/hel2`. Use focused `cargo test -p <affected-package> <behavior-filter>` invocations outside the sandbox. Then run `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Build the CLI if a live terminal check is needed. Use isolated state and a separate tmux socket for any live check and clean up only owned resources.

## Validation and Acceptance


The session list must show review activity even when the primary is idle and the session is unselected. Removing the review restores underlying primary status. Starting review shows compact stage/role progress; raw transcript tabs remain inspectable. Enter without deliberate action selection during running does nothing. Escape and an explicit Cancel click cancel. Findings default to Forward and failures default to Dismiss. The full suite and Clippy must pass, and only task-owned files are committed.

## Idempotence and Recovery


Do not change branches or push. Keep user profile homes and unrelated sessions untouched. All blocking operations stay in background tasks. A UI failure must not terminate controller-owned review work. Any new test resources must be stopped before their working files are removed.

## Artifacts and Notes


The prior live report contains the original reproduction and exact requested models: primary codex3/Sol medium and reviewer claude2/Opus medium (`opus[1m]`). This follow-up tests presentation and input behavior; it does not require re-spending model turns for unchanged review logic.

## Interfaces and Dependencies


Reuse `RuntimeReviewView`, `TurnReviewPhase`, `RoleStatus`, existing palette scopes and configuration save/reload paths. Do not introduce a workspace crate or additional third-party dependency. Agents own disjoint implementation files; the parent owns shared controller interpretation, integration, and final review.

Plan created 2026-09-05 for the user's requested UX follow-up and related design questions. Updated after integration to record the additional narrow cancellation repair, deterministic test coverage, and the explicit boundary between implemented fixes and remaining configuration/forwarding designs.
