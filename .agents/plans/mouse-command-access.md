# Make workspace and session commands usable with the mouse

This ExecPlan is maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Users should be able to create and manage workspaces and sessions without remembering shortcuts. The sidebar will offer New… (the session options wizard), Quick new (saved session defaults), Resume, and Commands. Session rows will offer an actions menu. Workspace selection, creation, rename, draft recovery, deletion, and cancellation will have mouse controls. Dashboard shortcut hints will become clickable, and help and the command palette will have explicit close controls. Typing names and deletion confirmation text still uses the keyboard.

## Progress

- [x] (2026-09-08) Audited existing mouse paths and confirmed origin's default branch is master, matching the clean current branch.
- [x] (2026-09-08) Added persistent dashboard actions, clickable pane/composer command hints, session actions, and complete help/palette mouse dismissal.
- [x] (2026-09-08) Migrated the workspace picker to reusable controls; its focused tests pass.
- [x] (2026-09-08) Validated rendered interactions, full Cargo tests, Clippy, formatting, documentation, and 12 live mouse workflow assertions.
- [x] (2026-09-08) Committed the mouse changes as `213602c9`, merged concurrent upstream work, repeated full Cargo/Clippy and live validation, and pushed merge commit `c0370837` to origin/master.

## Surprises & Discoveries

The workspace picker draws a Create new row but filters all mouse input except preview scrolling. The command palette selects rows by mouse but previously had no mouse submission or close control. Existing reusable `Form` controls already implement press/release capture, disabled controls, text-field cursor placement, and list scrolling.

The remote advanced during implementation to v2.3.1, including F4 Web/F7 Setup and shared composer shortcut labels. We preserved these changes by stashing only task files, fast-forwarding master, and resolving the footer integration. The terminal lifecycle test previously searched raw ANSI bytes for a phrase that differential rendering split with cursor movements; it now waits for the unique intact suffix. Live acceptance confirmed that notice dismissal retains the existing four-second minimum reading period.

A second upstream update added session titles and moved conversation activity above the prompt. It merged cleanly; the full Cargo suite, Clippy, host build, and all 12 live mouse assertions passed again on the combined tree before pushing.

## Decision Log

- Decision: Label the two session paths New… and Quick new, preserving their current keyboard shortcuts.
  Rationale: The ellipsis advertises a dialog; Quick new communicates immediate creation from saved defaults.
  Date/Author: 2026-09-08, Codex.
- Decision: Reuse the dashboard command registry and shared form controls. Keep workspace work in the existing asynchronous selector/controller boundary.
  Rationale: Mouse actions must have the same availability and lifecycle behavior as keyboard actions, and background discovery must remain responsive.
  Date/Author: 2026-09-08, Codex.

## Outcomes & Retrospective

Implementation, validation, and delivery to origin/master are complete. The real terminal verified workspace management, both session creation paths, session rename, help, four terminal dimensions, and the global composer footer shortcut. Full Cargo tests and Clippy passed, including after the final upstream merge; the documentation check and build passed with 1,700 internal links checked. No implementation work remains. Mouse gestures must retain ownership through release, and command availability must be checked again when the user activates a control; both were central to keeping these controls correct during resizing and background updates.

## Context and Orientation

`mj-tui/src/actions.rs` defines named dashboard commands, availability, and dispatch. `mj-tui/src/combined.rs` lays out the sidebar and conversation, while `render.rs` draws session rows and shortcut hints. `component_events.rs` routes reusable controls ahead of text selection. `palette.rs` and `help.rs` own their overlays. `mj-cli/src/dashboard.rs` routes mouse events between dashboard controls, chat controls, and text selection.

`mj-cli/src/workspace_selector.rs` owns the separate workspace selection screen. Its `preview` module loads workspace metadata and session previews asynchronously. The selector returns `SelectorOutcome` values; the caller performs the requested workspace operation. `mj-chat/src/components/` supplies `Form`, `Button`, `ButtonRow`, `TextField`, and `ChoiceList`; use these instead of another pointer implementation.

## Plan of Work

First add a dashboard form for visible command buttons and row actions. Register geometry during rendering and route its complete pointer gesture before selectable text and chat. Build buttons from command identities, recheck availability when activating, and keep Commands reachable while the composer or notices use the footer. Add a session-scoped palette opened by each session row's actions button. Turn displayed dashboard shortcut segments into buttons without inferring commands from arbitrary rendered strings. Add Close controls and help wheel scrolling.

Next extract workspace selection/edit/confirmation state into a small colocated module. Render the workspace list with the shared choice list, using click to preview and an Open button to switch. Add New, Rename, Recover draft, Delete, and Cancel actions. Create and rename show a real text field with Save/Create and Cancel; deletion retains the existing typed-name requirement for workspaces with active sessions or drafts. Metadata availability disables actions until their prerequisites arrive. Keep preview polling in the existing supervised asynchronous loop.

Finally add focused tests that draw screens and send mouse press/release events, including release outside a button, resized layouts, modal isolation, and disabled operations. Extend or add an isolated tmux scenario using the existing fake-provider reliability lab to exercise real terminal input and durable workspace/session effects.

## Concrete Steps

Run commands from `/workspace/mjolnir`. After each coherent implementation milestone run focused Cargo tests. Before the final commit run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. This environment has unrestricted execution; Cargo tests can open their required loopback and Unix sockets. Build the host CLI and worker with `cargo build -p brokk-mjolnir -p brokk-mj-worker` for terminal acceptance. Keep logs and evidence under `target/`.

Inspect the final diff, stage only task files, and commit on master. Fetch origin before pushing; if origin advances, integrate without rebasing or changing branches, rerun affected validations, and push `HEAD:master` to origin. Never force-push.

## Validation and Acceptance

Mouse-only navigation must open Commands from a focused composer, open New… and Quick new through distinct existing actions, open session-specific actions for the clicked session, run displayed footer commands, and close palette/help. Help must scroll with the wheel. Clicking hidden or covered controls must do nothing, and dragging outside an armed control must cancel activation.

In the workspace picker, click a row to preview it, Open to switch, New to create with a typed name, Rename to save an edited name, Recover draft when available, Delete to open the existing confirmation, and Cancel to return. Wrong typed deletion names must never enable destructive submission. Resize and asynchronous metadata changes must not activate stale controls. Tests and a live terminal scenario must prove these behaviors, not merely count registered controls.

## Idempotence and Recovery

Use isolated fake-provider fixtures for terminal tests and stop owned processes before removing their files. The product changes introduce no data migration. Preserve existing confirmations and backend outcomes. Failed checks should be fixed and rerun before committing; a rejected non-fast-forward push should be resolved by inspecting origin and integrating its work without force.

## Artifacts and Notes

Live seed 914 passed all 12 assertions before the final upstream merge. Seed 915 repeated all 12 successfully on the combined tree at `target/reliability-artifacts/mouse-commands-seed-915-101000/`; the tested CLI SHA-256 was `2d168b229376a68c819543eabf6b0703d5a2e1ffa5e79c29e9d9646b8208e972`. Logs are in `target/mouse-merged-tests.log`, `target/mouse-merged-clippy.log`, `target/mouse-merged-build.log`, `target/mouse-merged-live.log`, and the corresponding earlier docs logs. The deterministic screenshot generator refreshed the four documentation SVGs. Do not commit generated build output.

## Interfaces and Dependencies

Use existing Ratatui rendering and `mj_chat::components::Form` controls. Dashboard interactions return existing `DashboardAction` values through `dispatch_command`. Workspace interactions return existing `SelectorOutcome` values. No new crate or dependency is needed.

Revision: Initial plan records the authorized implementation, validation, commit, and push scope.

Revision: Records the implemented behavior, origin/master integration, focused validation, and live findings.

Revision: Records completed full validation and the final live acceptance evidence. A later concurrent upstream change moves conversation activity and adds the session title; preserve it during delivery.

Revision: Records successful validation after the final upstream merge and confirmed delivery to origin/master, completing the authorized work.
