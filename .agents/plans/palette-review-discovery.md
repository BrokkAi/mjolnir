# Improve palette layout and review discovery feedback

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

F2 should fit all commands when the terminal is tall enough and show a scrollbar otherwise. Review settings belong under Settings. The tier selector should explain Quick and Extended in aligned gray text. Selecting a profile should show loading feedback and expose discovered models before slow readiness checks finish.

## Progress

- [x] 2026-09-06: Inspect shared ChoiceList scrolling and action grouping; implement palette sizing and Settings scope.
- [x] Add selected-tier descriptions and loading animation to review settings.
- [x] Publish controller capability progress before tool verification; integrate generation-checked UI updates.
- [x] Validate focused state transitions, Cargo suites, clippy, formatting, and real tmux; deliver by committing and pushing the current branch.

## Context and Orientation

`mj-tui/src/palette.rs` renders shared ChoiceList controls; its Form owns display-row offsets and mouse geometry. `mj-tui/src/actions.rs` groups registry commands for F2 and Help. `mj-tui/src/review_settings.rs` owns the editable review draft and guards asynchronous results by generation and selected profile/model/effort. `mj-controller/src/hel_review_settings.rs` stages reviewer adapters, discovers options, verifies repository analysis tooling, and cleans up temporary reviewers. `mj-cli/src/dashboard/io.rs` supervises this work and delivers results.

## Surprises & Discoveries

Model discovery previously waited for all target tooling checks and cleanup despite having the choices earlier. The probe also repeats adapter starts for explicit model and effort selections; every selector edit restarts the full probe. Cancellation already exists, but completed discovery is not cached. This change targets time to first visible choices without treating partial checks as verified readiness. Terminal inspection exposed the shared scrollbar's content-length mapping: it now passes the number of valid viewport offsets so its thumb reaches both ends. The test fixture also needed an owned worker directory ending in the session ID, matching the real placement invariant.

## Decision Log

- Decision: Use the existing ChoiceList offset for the scrollbar and the existing modal clamp for height. Rationale: preserve one source of scrolling and input geometry. Author/date: root, 2026-09-06.
- Decision: Stream capability choices from the first sorted target before verification, retain final readiness semantics and cancellation generation checks. Rationale: expose useful data sooner without weakening validation or introducing stale global caches. Author/date: root, 2026-09-06.
- Decision: Delegate palette rendering and controller progress separately; root integrates CLI/TUI updates and validation. Rationale: independent file ownership. Author/date: root, 2026-09-06.

## Plan of Work

First implement content-based palette height, a separate scrollbar rail, and Settings grouping. Then insert wrapped tier descriptions below the tier options and update viewport row calculations. Add a controller progress channel, publish model/effort choices before tooling checks, and consume progress in the supervised CLI task. TUI progress must retain probing/readiness state and reject stale, cancelled, or completed generations. Animate while probing using the existing fast UI tick.

## Concrete Steps and Validation

From `/home/jonathan/Projects/hel4`, run elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `cargo build -p brokk-mjolnir -p brokk-mj-worker`. Run elevated `python3 tests/e2e/tui_components_tmux.py --seed 516` on the matching host binaries. Logs and evidence stay in target. Confirm F2 at 140x60 fits Settings and tail commands, at 100x18 End reveals the tail with a scrollbar, filtering shrinks the palette, and Review settings remains actionable. Confirm Quick/Extended descriptions change, loading animates, and early choices do not enable unverified saving. A controller behavior test should hold tooling work pending while observing capability progress.

## Interfaces and Dependencies

Keep `probe_review_settings` for existing callers. Add `probe_review_settings_with_progress` and `ReviewCapabilityChoices` using the existing Tokio channel dependency. CLI update messages carry generation, profile, model, effort, and choices. `DashboardState::apply_review_settings_capabilities` accepts choices without clearing probing or final readiness. No new crate or network dependency is needed.

## Idempotence and Recovery

Tmux tests own private runtime/configuration and restore their target overlays. Preserve evidence and stop owned processes before cleanup. Commit only task-owned files on the current branch; upstream push is already authorized.

## Outcomes & Retrospective

Palette, tier descriptions, and progress delivery are implemented. Final TUI tests passed (322 tests), the controller early-progress test passed, and seed 518 passed 72 live tmux checks; evidence is recorded in `.agents/docs/tui-components-live.md`. Clippy, formatting, Python compilation, and all default-member Cargo suites passed across validation runs. The full workspace invocation hit an unrelated worker socket-startup timeout after chat/controller/core/TUI passed; the complete worker suite passed on retry, and CLI plus workspace doc tests then passed separately. Logs are `target/palette-review-{tests,worker-tests,cli-tests,doc-tests,clippy,live}.log`. Full readiness checks still run; this work does not claim to cache or eliminate them.

Revision: scope expanded from small palette changes after the user reported slow model loading; document the new asynchronous progress boundary.
