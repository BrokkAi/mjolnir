# Redraw the TUI only when visible state changes

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Typing and focusing the prompt should respond immediately. A press inside the prompt currently waits for release because text selection consumes it. The event loop also redraws after every terminal event, including inert mouse movement. Preserve all event routing and action execution while drawing only when visible content, cursor, focus, selection, geometry, or time-dependent text changes. Do not discard mouse events by kind.

## Progress

- [x] (2026-09-08) Investigated event loop, selection routing, rendering, and history; user confirmed prompt focus occurs only on release.
- [x] (2026-09-08) Settled shared change-reporting design and delegated disjoint handler/component/background work.
- [x] (2026-09-08) Implemented dashboard, chat, shared-control, selection, and background change reports; integrated agent handoffs and reviewed actual changes.
- [x] (2026-09-08) Integrated reports into batching, background feeds, clocks, and immediate mouse-down focus; focused press/drag regression passes.
- [x] (2026-09-09) Validated behavior tests, all default-member test groups, formatting, Clippy, and final live tmux acceptance; task files are ready for the accompanying commit.

## Surprises & Discoveries

The shared controls already expose `EventResult<A>` with `Outcome::Continue`, `Unchanged`, and `Changed`. Outer event routing discards that information. `DashboardAction::None` and `ChatEventOutcome::None` do not indicate that nothing changed: typing and cursor movement can return them. Ratatui already emits only changed terminal cells, but building a frame remains expensive. Timers do not gate input; the focus delay comes from replaying clicks on release.

## Decision Log

Use existing EventResult/Outcome rather than a new event-kind filter. Keep action payload independent from repaint information. Preserve compatibility wrappers for action-only callers where needed. Accumulate changes reported by focused setters; do not clone, hash, or render the entire application to decide whether to draw. Keep timers for real displayed animation/clock changes. Background completions still apply actions and report failures when no frame is required.

## Outcomes & Retrospective

Implementation and validation are complete. The live tmux workflow confirmed prompt typing before mouse release and exercised chat model/effort, question forms, reviewer setup and turn-review controls. It exposed missing workspace-manager completion and target-ID modal invalidation, which were fixed with explicit mutation reports. Preview formatting performance is outside this change unless validation shows it prevents the advertised behavior.

## Context and Orientation

`mj-cli/src/dashboard.rs` owns the terminal loop, waits on terminal/background events, batches input, applies actions, and calls `render_combined`. Its `dirty` flag gates actual draws. `mj-tui/src` owns dashboard and modal state. `mj-chat/src/hel_chat.rs` and its submodules own composer/chat state and action dispatch. `mj-chat/src/components` contains reusable controls and their EventResult type. `mj-chat/src/hel_selection.rs` owns pointer selection. Background feeds in the CLI apply prepared state through dashboard/chat setters. A visible change means the next drawn cells or terminal cursor would differ; internal bookkeeping alone is not a visible change.

## Plan of Work

First add `handle_event_result` entrypoints on DashboardState and ActiveChat, returning existing EventResult with the original action payload. Preserve existing action-only entrypoints when necessary. Shared forms must distinguish no-op editing and pointer capture from actual displayed changes. Dashboard setters accumulate changes for `take_render_changed`; chat pump returns Outcome for actual applied changes. Selection exposes a cheap comparison of its displayed highlight state.

Then consume those reports in the dashboard loop. Do not mark dirty simply because an event/feed arrived. Execute actions regardless of change outcome. Combine input changes into a single frame; before a pointer uses hitboxes changed by earlier events, draw pending changes. Immediately focus a visible normal prompt on mouse-down before selection consumes it, preserving click replay for cursor placement and drag selection.

Finally make time invalidation reflect displayed clock/animation values, and suppress duplicate or hidden background updates. Integrate carefully with notice generation and action-specific existing dirty flags. Preserve initial draw, resize, modal geometry, shutdown notices, and background error visibility.

## Concrete Steps

Work in `/home/jonathan/Projects/hel` on the current branch. Parent owns CLI loop integration and the plan. Independent Luna agents own chat handlers, dashboard handlers, shared controls/selection, and dashboard background/time invalidation, with nonoverlapping files.

Run focused behavioral tests as interfaces converge. Every cargo test must use elevated permissions with normal build storage. Final commands:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

Stage only task files and commit to the current branch without pushing. Leave `.agents/plans/restore-tui-workspaces-and-status.md` untouched.

## Validation and Acceptance

Tests must prove ignored/consumed-unchanged inputs and duplicate background updates do not request frames; edits, cursor-only movement, focus, selection, scrolling, modal/control changes and visible background content do. Verify a batch requests one draw, pointer hitboxes follow preceding layout changes, and mouse-down focuses the prompt before release while drag selection still works. Exercise visible time progression and idle/hidden content. Use frame counts/change decisions rather than fragile wall-clock timing thresholds; live tmux acceptance sends mouse-down and mouse-up separately, typing before release to prove focus is immediate; it also exercises normal chat controls and drag/release routing in an isolated fake-ACP lab.

Final validation covered the default-member test set through `cargo test` and focused reruns: chat 466, controller 727, core 878, dashboard 395, worker library 109, worker executable 4, CLI 205, and CLI integration tests 11. Documentation tests passed. The new cursor-only test exposed a shared adapter that mapped cursor motion to Unchanged; fixing `apply_field_edit` made it pass. Existing worker child-process timeouts passed with the complete worker test binary run serially. The quota wall-clock boundary test passed in the serial CLI run; one PTY startup timeout passed in isolation after builds finished. `cargo clippy --all-targets -- -D warnings`, rustfmt, and diff checks passed. No build output was relocated.

The final tmux run used `python3 tests/e2e/tui_components_tmux.py --seed 90830`. Keep live acceptance separate from Cargo builds: Cargo test also rebuilds the executable that the harness reattaches later.

## Idempotence and Recovery

This is a local code change with no data migration or external publication. Keep unrelated work untouched. Re-run only checks affected by integration fixes. Do not remove working files of running test processes.

## Artifacts and Notes

The source investigation found mouse-down consumption in `route_selection_event` and unconditional input dirty marking in `run_dashboard_for_workspace`. These are the initial regression targets. Intermediate live tmux evidence is under `target/reliability-artifacts/tui-components-seed-90827-744357/`; that run passed held-button focus and chat controls before revealing the target-ID transition gap. Final live tmux acceptance passed with seed `90830`; evidence and captures are under `target/reliability-artifacts/tui-components-seed-90830-1279394/`. This final run followed all builds and included the shared cursor-reporting correction. This includes typing before release, Unicode form paste, drag-outside release, reviewer controls, all tested modal saves/reopens, viewport sizes, and SIGTERM shutdown.

## Interfaces and Dependencies

Reuse `mj_chat::components::{EventResult, Outcome}`. DashboardState and ActiveChat gain change-aware event entrypoints with their existing action enums as payloads. DashboardState exposes accumulated visible invalidation and clock/animation change checks. ActiveChat pump reports Outcome. Selection exposes small visible-state comparison. No new crate, network protocol, or dependency is needed.
