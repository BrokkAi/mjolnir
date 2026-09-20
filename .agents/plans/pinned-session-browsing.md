# Instant browsing with pinned sessions

This ExecPlan follows `.agents/PLANS.md`. Keep its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Run more sessions than visible panes, inspect unpinned sessions instantly in one Browse pane, and retain selected conversations as colored pins. List navigation never changes a pin. Keyboard focus, list cursor, and Browse identity are independent. Users can construct and rearrange a grid entirely with mouse or keyboard.

## Progress

- [x] 2026-09-20: inspected existing pane, attachment, rendering, migration, and control infrastructure; agreed interaction design.
- [x] 2026-09-20 M1: explicit roles and persisted pin identities, compatibility migration and tests.
- [x] 2026-09-20 M2: pane-directed browsing, safe asynchronous attachment, preserved drafts and state.
- [x] 2026-09-20 M3: mouse/keyboard pinning, menus, badges, empty slots and help.
- [x] 2026-09-20 M4: full regression validation and integration of origin/master through f8d747fd; ready for the authorized commit and push.

## Decision Log

2026-09-20: Exactly one Browse pane exists per workspace. Selecting a pin reveals its existing view without changing Browse. Enter/double-click enters the selected conversation. Pane focus does not move the list cursor. Deliberate list navigation drives Browse; background clamping does not.

2026-09-20: Follow Herdr placement: split a targeted rectangle 50/50, retaining existing content left/top and creating Browse right/below. The previous Browse becomes a pin if populated, otherwise an empty slot. New Browse is empty until deliberate browsing resumes. Splitting fails atomically when space is insufficient. Swaps carry roles and identities, not geometry.

2026-09-20: User chose to preserve empty grid slots on unpin and to move Browse beside a split pinned pane. Close removes a non-Browse rectangle without stopping a session; Browse cannot be closed. Pin controls offer directional splits or an explicit empty destination. Empty panes offer Pin selected here. No auto packing or drag-and-drop.

2026-09-20: User additionally requires clickable pin/unpin glyphs on rows and headers. Mouse context menus expose split, swap with focused pane, unpin, zoom and close. Shared action/control infrastructure owns hit testing; clicks capture their target and never leak through to another action.

2026-09-20: Pins have stable letter badges and eight theme-aware accent colors, repeating colors with unique badges beyond eight. Match row and header badges; preserve through swaps and restart. Focus and lifecycle status remain independently visible. Unicode and ASCII symbols must both work.

2026-09-20: User authorizes merge and push to origin/master after completion. Commit only this task's files; inspect remote state before integration.

## Context and Orientation

`mj-core/src/workspace.rs` defines ConversationLayout, stored as JSON by `mj-controller/src/database/workspaces.rs`. `mj-tui/src/dashboard_conversation.rs` manages the tree and pane/session mapping. `mj-cli/src/dashboard/session_state.rs` automatically follows the selected row, while `drafts.rs` starts background attachments and `io.rs` accepts their generation-tagged results. `mj-tui/src/combined.rs` draws conversations and currently couples the focused conversation to row selection. `surface_controls.rs` supplies mouse controls, `actions.rs` the command registry, and `mj-chat/src/theme.rs` shared glyphs and colors.

## Surprises & Discoveries

The split controller restores a previous conversation displaced by automatic browsing. Explicit pane roles replace this workaround. Existing background attachment generations already provide stale-result rejection but destinations and cancel/defer paths must stop depending on current focus.

## Plan of Work

M1 extends ConversationLayout with optional legacy-compatible Browse metadata and stable pin assignments. Legacy layouts use saved focus as Browse, preserve tree and empty panes, and assign remaining sessions deterministic pin identities. Add breaking migration 42 (older writers discard metadata), updating minimum compatibility transactionally; protocol 29 becomes 30. Add role validation and serialization/migration tests.

M2 centralizes role changes and targets session attachment by pane. Deliberate selection is distinct from refresh clamping. Remove selection equality checks from focused rendering and input routing. Preserve per-session drafts, question answers, transcript state, and read receipts; superseded asynchronous results cannot overwrite another pane or steal focus. Handle normal and native conversations, failed and transitioning sessions, launch placeholders, workspace switching, and startup restoration.

M3 adds pin/unpin actions, targeted split/swap menus, empty-pane placement, matching stable badges and glyphs. Existing prefix+v/minus and shifted directional swap bindings remain. Pin placement captures source session, checks capacity before mutation, and leaves one Browse. Selecting an offscreen target exits zoom. Unpinning the selected session previews it in Browse; unpinning others does not change selection. Missing sessions empty their slots. Document controls in existing user guides and help.

M4 validates the complete interface, fixes regressions, updates this plan with evidence, commits coherent validated changes on the current branch, then integrates and pushes origin/master as requested.

## Concrete Steps

Run from `/home/jonathan/Projects/hel2`. Use `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. Every cargo test runs outside the restricted sandbox. Use isolated MJ_CONFIG_DIR and MJ_DATA_DIR for migration and manual checks; never upgrade the live store for this task. Do not redirect Cargo output storage to /tmp.

## Validation and Acceptance

Behavior tests cover four-pane browsing after interacting with a pin; selecting visible pins; both split directions for Browse/pin/empty; atomic refused splits; swapping roles and stable badges; unpin preserving slots; explicit mouse glyph/menu actions without click-through; delayed/failed attachment and cancellation; drafts and question input; workspace and restart restoration; deterministic legacy conversion and incompatible older access; Unicode/ASCII, light/dark, narrow geometry and zoom. Manual acceptance uses more running sessions than panes and builds, fills, rearranges, unpins and restores a four-pane grid using both input methods.

## Idempotence and Recovery

Migrations use a new revision and isolated stores. Failed splits or attachments leave unrelated pane state intact. Session view removal never stops its process. Preserve existing asynchronous supervision and bounded cleanup. Git integration must preserve unrelated work and remote commits.

## Interfaces and Dependencies

Keep existing workspace crates and shared subprocess/component infrastructure. Persist roles in ConversationLayout. Expose explicit pane-targeted dashboard actions for pin placement and layout commands, and explicit pane targets in attachment/cancellation paths. Shared theme code supplies glyphs and pin colors; the action registry and surface controls expose identical behavior across input methods.

## Outcomes & Retrospective

The implementation now separates the list cursor, keyboard focus, and Browse identity. Pin and pane menus use shared mouse controls; numbered destination overlays accept matching press/release in an empty pane. Drafts and transcript anchors survive replacement and unpinning, while attachment completions retain explicit destinations and reject stale generations. Persistence includes stable badges and legacy conversion. No live migration has been run.

Final validation passed the full default-member `cargo test` suite (including 694 TUI tests, 1,517 controller tests, 147 CLI tests, and eight terminal PTY tests), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check`. Logs are `/mnt/optane/hel2-pin-merged-tests.log` and `/mnt/optane/hel2-pin-merged-clippy.log`. The manual four-pane interaction scenario is covered by event/render tests; an interactive live session was not launched.

2026-09-20: Integrated ten origin/master commits before final validation. Kept the upstream lifecycle terminology changes and assigned protocol 30 after upstream consumed 28 and 29.

2026-09-20 revision: recorded completed milestones and focused validation. Review found that session commands also need explicit focus-aware targets without changing list navigation; command availability and palette headings now use that same target. Late lifecycle attachment results retain pane assignments for recovery.

2026-09-20 completion: integrated two further upstream commits through f8d747fd and validated the merged implementation. No implementation work remains; deliver this result by committing on hel2 and pushing HEAD to origin/master as authorized. Full workspace checking additionally requires GTK/GDK desktop system libraries that are absent here; the required default-member checks above all passed.
