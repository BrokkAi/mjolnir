# Open autoreview conversations through a Reviewers control

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Autoreview should use the same child-session workspace as subagents. Users click Reviewers on the prompt's bottom border to read full reviewer conversations, then return through the existing workspace close control or Escape in the Sessions list. The parent conversation keeps review status and resolution actions. The parent transcript remains visible at full width throughout review. Status, a short findings summary and resolution actions stay in the prompt area; reviewer conversations use ordinary session panes in the shared virtual workspace.

## Progress

- [x] (2026-11-04) Trace review rendering, child kinds, workspace navigation and golden coverage.
- [x] (2026-11-04) Extend the shared virtual workspace to select workers or reviewers with one navigation state.
- [x] (2026-11-04) Add the prompt control, mouse and keyboard routing, and remove split rendering.
- [x] (2026-11-04) Extend golden and navigation regression coverage; run affected crate suites and workspace clippy. The only failures are three existing CLI workspace-startup tests, detailed below.
- [x] (2026-11-04) Review the integrated diff and prepare the validated implementation for commit. Publication requires the pre-push review described below.

## Surprises & Discoveries

Golden fixtures require TZ=UTC; a first run without it changed unrelated timestamps, which were restored. A rendered running review needs an explicit neutral focus stop; without it the form selected Cancel. The existing harmless-Enter regression now renders first and protects this condition.

Reviewer sessions already use `SubagentKind::Reviewer` in durable child relations. Existing workspace membership and counts combine these with ordinary workers. Mjolnir delegation is disabled for this session (spawn returned that explicit error), so implementation and validation run in the parent.

## Decision Log

Use the durable child kind to filter one shared workspace implementation. Store parent and kind together, so membership and return navigation cannot disagree. Preserve the daemon-owned synchronous review and its resolution actions. The user clarified that the old review pane must be removed entirely. Remove its renderer, tabs, viewports, transcript polling, and selection surface. Keep the ordinary parent transcript visible in every review state. Review questions still come from the daemon projection, and reviewers open as ordinary session panes.

## Outcomes & Retrospective

Reviewers now opens the shared child workspace filtered to reviewer records; worker children remain in Subagents. Both share selection, scrolling, draft retention and parent-return navigation. The split layout is gone. The parent transcript stays visible at full width through running, findings and failure states. All legacy reviewer-pane code and hidden transcript polling are removed; findings summaries and failure reasons appear only beside the prompt resolution controls. One prompt-focus enum prevents simultaneous focus on multiple border controls.

Initial validation before deleting the legacy pane: 420 chat tests, 562 TUI tests and 231 CLI unit tests passed (existing ignored tests remain ignored). CLI integration results: ACP 2 passed/1 failed; daemon startup 15 passed/2 failed; instance 4 passed; logging 2 passed; store divergence 3 passed; terminal PTY 14 passed. The three failures are the unchanged tests `an_unknown_workspace_is_refused_at_start_from_the_store_without_a_daemon`, `new_without_a_workspace_refuses_without_starting_a_daemon`, and `new_with_an_unknown_workspace_refuses_without_starting_a_daemon`. They expect direct workspace listing from the store without a daemon. The unchanged `listed_workspaces_without_starting` in `mj-cli/src/main.rs` explicitly only asks an existing daemon and returns None when none runs; this source mismatch explains all three. The ACP failure repeated in isolation. No startup or integration-test source was changed for this task.

Both final workspace clippy and rustfmt checks passed. Logs are `/tmp/reviewers-tests.log`, `/tmp/reviewers-chat-final.log`, `/tmp/reviewers-cli-integration.log`, `/tmp/reviewers-cli-remaining.log`, `/tmp/reviewers-acp-retry.log` and `/tmp/reviewers-clippy-final.log`. The macOS golden variants were synchronized after verifying their only platform difference is Ctrl-V versus Cmd-V; they have not been executed on macOS.

## Context and Orientation

`mj-chat/src/chat/active/render.rs` now always draws the ordinary parent transcript and projects review status/actions into the prompt; its former split and full-width verdict branches are deleted. `mj-chat/src/chat/turn_review.rs` owns the projected review state, rendering and resolution input. `mj-tui/src/dashboard_workspaces.rs` opens a virtual workspace (a Sessions list filtered to one parent's children rather than a stored workspace). `mj-tui/src/dashboard_sessions.rs` supplies its rows, and `mj-tui/src/workspaces.rs` draws its return control. `mj-cli/src/dashboard/chat_tasks.rs` connects chat outcomes to workspace navigation. Child records in `mj_core::subagent` distinguish Worker and Reviewer.

## Plan of Work

First extend the virtual workspace state with a child kind and reuse its selection, scrolling and return logic. Keep ordinary subagent counts separate from reviewers. Then add Reviewers to the shared prompt border controls, route it through chat outcomes to the shared workspace, and render review content at full width. Retain findings, failure notices and resolution behavior. Update existing golden cases and add coverage for mixed child kinds and nested return navigation.

## Concrete Steps

From the repository root, edit the three crates described above. Run focused tests during fixes with `cargo test -p <crate> <filter>`. Run final `TZ=UTC cargo test -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir` and `cargo clippy --all-targets -- -D warnings` in the dev profile outside the sandbox. Use existing isolated test data directories; any live invocation must use `--instance reviewers-workspace`. Do not change build storage or Cargo configuration.

## Validation and Acceptance

Golden output shows no side-by-side primary/reviewer split and shows Reviewers on the prompt border. Mouse and keyboard activation produce the reviewer workspace outcome. Mixed worker/reviewer records produce disjoint workspace lists; closing a reviewer workspace restores its parent and the correct enclosing workspace. Findings and review failures remain readable and actionable. All touched crate tests and workspace clippy pass.

## Idempotence and Recovery

No store, protocol or configuration changes are needed. Tests use isolated state. Stage only task-owned changes, commit on the current branch, request the required review, then push HEAD to origin/master. If upstream moved, fetch and merge origin/master before retrying.

## Artifacts and Notes

The review golden is `mj-chat/tests/golden/review-prompt-controls.txt`; its platform variant uses `-macos.txt`. The former turn-review-pane fixtures and pane-only tests are deleted with their implementation. Extend the existing test rather than adding a family of examples.

## Interfaces and Dependencies

Reuse `SubagentKind`, `DashboardState` child navigation, `ChatAction`/`ChatEventOutcome`, ratatui and existing review actions. Introduce no crates or external dependencies.

Initial plan recorded after tracing the existing implementation.

Completion note: the plan now records the implemented behavior, targeted focus and pointer-routing corrections, and the unrelated startup-test limitation. Git commit, required review and publication follow as delivery operations.

Review follow-up (2026-11-04): review of 298330b19 found PageUp/PageDown scrolling hidden review content. The added golden assertions failed before correction (`/tmp/reviewers-page-before.log`). The user then clarified that the legacy pane must be gone entirely. Removing that alternate content and routing page keys directly to ordinary transcript scrolling eliminates this mismatch by construction. The golden now covers parent paging both while running and after findings, and Tab/BackTab use ordinary host focus navigation. Revised full validation and a new pre-push review follow this removal.

Removal validation completed: 413 chat tests, 562 TUI tests, 231 CLI unit tests and all enabled CLI integration/doc tests passed with the three previously reproduced startup failures excluded from the repeat run. The legacy pane and viewport modules, pane-only rendering helper, background reviewer transcript polling and secondary selection surface are deleted. `cargo clippy --all-targets -- -D warnings`, rustfmt and diff checks pass. Evidence: `/tmp/reviewers-no-pane-tests.log`, `/tmp/reviewers-no-pane-clippy.log`, and `/tmp/reviewers-no-pane-golden.log`.

Upstream integration: merge origin/master at 65606a5f8. Child relationships now use compact metadata and session records load by subscribed parent scope. Both Reviewers and Subagents call the same scope loader; reviewer loading has its own visible notice. Select the filtered child relationship before its full record arrives. The extended mixed-kind/nested-return test passes with a missing reviewer record and an intermediate refresh (`/tmp/reviewers-merge-test.log`). Workspace clippy on the merged source passes (`/tmp/reviewers-merged-clippy.log`). The full-suite repeat uses the same three documented startup-test exclusions, with results recorded in `/tmp/reviewers-merged-tests.log`.
