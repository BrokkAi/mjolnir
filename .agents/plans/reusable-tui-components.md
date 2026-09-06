# Reusable TUI components and live terminal acceptance

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current throughout implementation.

## Purpose / Big Picture

Buttons, fields, lists, and tabs must behave consistently in the dashboard and chat. The user explicitly requested a reusable component layer, adoption of rat-focus/rat-event without rat-widget or rat-salsa, migration of all standard controls, and live tmux testing. Standard controls own presentation and interaction; screens retain validation and domain actions. The specialized readline, composer, transcript, asynchronous task, and persistence engines remain in use.

## Progress

- [x] (2026-09-06) Reviewed published rat-focus 2.1.1 and rat-event 2.1.0 and ran 13 standalone behavior probes against existing Ratatui artifacts.
- [x] (2026-09-06) User approved all standard dashboard and chat controls and required live tmux validation.
- [x] Shared scope and container editor implemented with behavior tests.
- [x] Validate the representative form through the actual TUI in isolated tmux: seed 416 passed 55 assertions, including open-form resizing and SIGTERM.
- [x] Palette, resume/import, review settings, and dashboard dialogs migrated; focused dashboard suite reached 293 passing tests with integration fixes underway.
- [x] Chat forms, pickers, review controls, and microphone use the shared layer.
- [x] Integrated pointer routing, selection, focus restoration, Help, and dynamic layout behavior.
- [x] Complete automated and live acceptance, license maintenance, and final review; full cargo test, clippy, fmt, license checks, and live seed 417 passed.
- [x] Final acceptance fixes and this record form the completion commit on the merged current branch; publication is the authorized push to origin/master.

## Surprises & Discoveries

The existing dashboard drops mouse events for most modal modes. Selection intercepts gestures and replays a simple click as mouse-down, which must not be applied to new press/release controls. Most modal key dispatch clones Mode; migrated live control state must be moved or handled in place instead.

Published rat-focus performs traversal and hit testing but not pointer capture or modal ownership. A focus-changing mouse press returns a consumed Changed result, so preprocessing must still deliver it to the target control. Removing a focused entry clears its flag without assigning a replacement. Changing its navigation to None leaves it focused and can prevent traversal. Rebuild with the previous Focus to clear removed flags; repair eligibility centrally. FocusFlag clones share identity, and identities are local to the UI thread.

The complete CLI build confirmed that ActiveChat previously crossed a Tokio channel from the attach task. Live rat-focus state is not Send. PreparedChat now carries only initialization data across that boundary: stored review reads remain in spawn_blocking, and the UI constructs live control identities after receiving the result. Inline custom answers also need exact editor geometry inside a selectable option list; the shared layer now supports that compound control without adding a separate Tab stop.

## Decision Log

- Decision: Put the layer in `mj-chat/src/components/`, exported as `mj_chat::components`. Rationale: mj-tui already depends on mj-chat for common modal, editor, and selection utilities, so this avoids a dependency cycle and unnecessary workspace crate. Date: 2026-09-06.
- Decision: Use rat-focus and rat-event only, preserving our editing/runtime engines. Rationale: standalone primitives fit; higher-level library widgets change editing behavior and the runtime duplicates existing task ownership. Date: 2026-09-06.
- Decision: Controls and modal scopes own common behavior; screens emit existing DashboardAction/ChatAction values. Rationale: removes duplicated mechanics without moving filesystem/network work onto the UI loop. Date: 2026-09-06.
- Decision: Use persistent per-control identity and one authoritative scope for each migrated surface; domain drafts store values, not live focus flags. Rationale: stable focus and capture across redraws and safe restoration of help overlays. Date: 2026-09-06.

## Context and Orientation

`mj-chat/src/hel_text_input.rs` provides the nonblocking readline editor, Unicode grapheme editing, limits, filters, and history. `hel_modal.rs` and `hel_selection.rs` provide shared modal geometry and content-space text selection. `mj-tui/src/dialogs.rs`, `wizards.rs`, `wizards/dashboard.rs`, `resume.rs`, `review_settings.rs`, and `palette.rs` currently implement their own controls. `mj-chat/src/hel_chat/elicitation.rs`, `config_picker.rs`, `second_opinion.rs`, and `turn_review.rs` contain chat forms and controls. `mj-cli/src/dashboard.rs` owns the Tokio loop, terminal dispatch, and selection routing. Package names are brokk-mj-chat, brokk-mj-tui, and brokk-mjolnir; the CLI executable is target/debug/mj.

## Interfaces and Dependencies

Add rat-focus 2.1.1 and rat-event 2.1.0 to mj-chat. Export an EventResult<A> containing Outcome and an optional typed action; it implements ConsumedEvent. Do not infer propagation from whether a domain action is None.

The component layer supplies persistent control state in `Form<K>` and button, button-row, text-field, checkbox, single/multiple-choice list, tab-strip, dialog layout, and form viewport primitives. Use Ratatui rendering conventions and existing TextInput rather than a new editor. Record geometry during rendering and use it for focus, hit testing, horizontal text viewport, and cursor placement. Keep library focus flags internal. Form rebuilds with the previous tree, preserves a valid control, and selects the next eligible entry when the current one disappears or becomes disabled. Metadata updates use begin_update/declare/end_frame and preserve drawn hitboxes; render passes use begin_frame/register/end_frame. Do not rebuild a form from scratch or treat a domain action as proof that an event was unconsumed.

Ordinary Tab/Shift-Tab navigate eligible controls in visual order. Controls receive editing/navigation first, preserving composer completion. Button Enter/Space act on press only, ignoring reported repeats/releases. Mouse-down focuses and arms; release inside activates once and release outside cancels. Button rows allow left/right. Text fields preserve readline, filtering, limits, history, paste, and configured Enter submission. Lists and tab strips are one Tab stop with arrow/Home/End navigation; checkbox and multi-choice toggle with Space. Tab activation changes the visible panel while work remains asynchronous.

Only the active modal receives ordinary input. Store and restore its invoking control; Escape first closes a nested popup. Hidden controls do not participate. Visible disabled controls consume clicks but never activate. Forms scroll focused controls into view. One pointer owner retains drag/release outside its bounds, and capture is cancelled on disappearance or a superseding modal. New interactive controls take precedence over surrounding selectable body text. Refresh geometry before dispatching pointer input after structural changes or resize.

## Plan of Work

First implement the shared scope and basic controls and migrate the container editor, including input, checkbox, lists, buttons, and nested help. Validate its behavior using focused tests and the live TUI. Then migrate dashboard confirmations, rename/configuration/origin editors, new/resume wizards, review settings, resume/import tabs, and palette. Migrate chat elicitation forms, config pickers, reviewer setup/review tabs and actions, and the microphone. Keep specialized composer/autocomplete/transcript engines, connecting them at the shared routing boundary. Remove superseded standard-control helpers and update key hints. Each migrated screen has one interaction authority; coexistence is only between migrated and unmigrated screens.

Root owns integration, focus/routing, the representative form, and review. Once interfaces are concrete, independent component/dashboard/chat implementation may be delegated to gpt-5.6-luna with xhigh reasoning and disjoint file ownership. Delegates must not delegate further.

## Concrete Steps

From /home/jonathan/Projects/hel4, use cargo fmt --check, cargo test, and cargo clippy --all-targets -- -D warnings. Every cargo test runs with elevated sandbox permissions. Focused package tests validate checkpoints; the complete required suite validates the integrated result. Build with cargo build -p brokk-mjolnir for live acceptance. Keep build artifacts under target, never /tmp. Update generated dependency-license reports using the existing repository tooling after dependencies settle.

Use tests/e2e/prepare-luna-lab.py and the disposable fake-ACP/local-bare infrastructure, correcting old runbook package/binary names where necessary. Launch a unique tmux socket/server with isolated runtime state at a short /tmp/hel-r-* path to stay within Unix socket limits; retain captures and copied logs beneath target. Run socket-based lab preparation and tmux control outside the restricted sandbox. Do not use personal sessions or paid harnesses.

## Validation and Acceptance

Colocated component tests exercise activation, Space vs text entry, press/release cancellation, Unicode cursor placement, paste, filters/limits/history, navigation, disabled controls, and empty/clipped layouts. Scope tests exercise wrap, eligibility transitions, modal isolation/restoration, shared identity, and captured pointer release. Screen/router tests verify that first-click activation reaches the target once, text never runs unrelated commands, and modal input does not reach background controls. Preserve existing composer, selection, and PTY termination coverage.

Live tmux is required for the representative form and final migration. Drive the actual binary with keys and SGR mouse reports through its PTY; inspect capture-pane output after transitions and verify actual actions. Exercise every migrated screen including nested help, fields, disabled buttons, tabs, list selection, and drag-outside release. Repeat at 40x10, 72x18, 140x40, and 200x60 with resize during interaction. Run delayed fake replies/discovery updates while interacting; test cancellation, detach/reattach, normal quit, and SIGTERM terminal restoration. Record exact inputs, pauses, build hash, dimensions, expected/actual behavior, relevant logs, and captures under target. Promote discovered defects into regression tests and rerun affected scenarios.

## Idempotence and Recovery

Stay on the current hel4 branch and stage only owned changes. Commit each coherent validated checkpoint and the final result. No new branch, rebase, or pull request. This implementation request does not require a release. Stop test-owned process groups before removing working state and terminate only the owned tmux server. Keep unrelated user state untouched.

The user subsequently explicitly authorized merge and push when complete. After all validation, integrate the current branch with master and push the completed result to origin/master, inspecting existing worktrees and remote state before any Git mutation.

## Artifacts and Notes

The earlier source assessment executable at target/rat-focus-assessment/probes records 13 passing behavior probes; it is supporting evidence, not a substitute for tests of the integrated product. Live evidence will be recorded under target and summarized in an agent-facing note in .agents/docs/.

## Outcomes & Retrospective

Shared controls, dashboard dialogs, and chat controls are implemented and committed as f3f2ad5e. Upstream master merged cleanly as dcf76eea. Final full cargo test passed, including 402 chat tests, 306 dashboard tests, 143 CLI tests, and PTY termination coverage. cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, git diff --check, and the dependency license check passed. Final live seed 417 passed 56 assertions through real tmux keyboard and SGR mouse input, including reviewer Back-navigation, asynchronous discovery, captured release, four-size open-form resizing, detach/reattach, and SIGTERM. See .agents/docs/tui-components-live.md for reproducible commands and the exact binary hash. Microphone hardware and emulator-specific glyph appearance remain device-dependent and are not established by the fake-provider terminal harness. Final acceptance fixes are ready to commit and push.

Revision: initial executable plan records the approved scope and live tmux requirement, 2026-09-06.

Revision: records implemented integrations and outstanding validation, 2026-09-06.

Revision: records the implementation checkpoint, upstream merge, and live findings. Web dialog width, captured plan restoration, pointer capture during metadata reconciliation, and cancellation of review discovery now have source fixes and regression coverage. Final review also identified stage-transition focus and clipped button visibility for correction before final acceptance.

Revision: final automated and terminal acceptance passed on the merged implementation. Shared input state required explicit separation between rendered geometry, metadata updates, and domain transitions; the regression tests now cover those boundaries.
