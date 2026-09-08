# Restore readable Sessions and consistent activity


This ExecPlan follows `.agents/PLANS.md` and is updated during implementation.

## Purpose / Big Picture


Make Sessions useful at an 80-column terminal without hiding questions, queue counts, or operational details. Preserve local workspace tabs and adaptive support panes. Normal sessions use four lines (name, metadata, two wrapped output lines), minimized sessions use two lines, and clocks and animations agree about actual activity.

## Progress


- [x] (2026-09-08) Agreed widths, four-line rows, two-line compact cells, continuous turn timing, selected-session animation, and review integration.
- [x] (2026-09-08) Assigned independent Sessions renderer, Setup/review, shared scrollbar, and documentation/dependency work.
- [x] (2026-09-08) Implemented shared activity classification and a durable turn-start field retained through background work; focused relay test passed.
- [x] (2026-09-08) Integrated Sessions controls/rows, Setup review discovery, compact status reservation, shared scrollbars, and clock preferences. Reviewed actual diffs; focused TUI suite passed 365 active tests before final additions.
- [x] (2026-09-08) Focused web-viewer original-port retry test passed; improved diagnostics preserve its HTTP and address assertions. Core suite passed 868 tests with six existing ignored tests.
- [x] (2026-09-08) Full integrated cargo test, clippy with warnings denied, formatting, documentation, and diff checks passed. Implementation and this completed plan form the final commit on current master.

## Surprises & Discoveries


The old grid had three columns across a horizontal panel; a 20-column sidebar needs its status-reserving cells in one vertical column. The preceding normal renderer changed representation at width 70; normal rows now use one representation at all supported widths. Review Settings duplicated Setup fields but uniquely supplied capability discovery and profile filtering, which are now retained inside Setup. The web already has New/Resume above Sessions. There is no current standalone chat entry point. Removing the workspace renderer did not eliminate a crate boundary.

## Decision Log


On 2026-09-08 the user chose minimized width 20, standard max(40, width/3), and maximized width/2; standard and maximized coincide at width 80. Normal rows remain name/metadata/two output lines, not wrapped metadata. Minimized rows use two lines. Default clocks continue from originating turn start through its background work, while optional detailed clocks separate turn, step, and background timing. Keep the upper-right indicator for confirmed activity of the visible session. Merge rich review settings into Setup with one Save/Cancel workflow. Standardize existing terminal scrollbars only.

## Outcomes & Retrospective


Implemented the agreed Sessions layouts and controls, shared activity clocks and animation evidence, Setup-integrated review discovery, slim terminal scrollbars, and verified unused dependency removal. Web creation/resume controls were already at the top. Config schema 4 and worker snapshot schema 5 preserve supported older configuration and live-turn timing. Final cargo test passed across all workspace suites: chat 449, controller 714, core 869, TUI 366, worker 109, CLI 203, and all enabled integration/terminal tests; existing ignored tests remain ignored. cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, documentation npm run check, and diff checks passed. The previous workspace commit’s viewer retry test is resolved and also passed twenty consecutive CLI suites. Prior workspace work remains in 2f268023; its untracked planning artifact is preserved separately. No push is performed.

## Context and Orientation


`mj-tui/src/render.rs` prepares session rows and support-table scrollbars; `combined.rs` allocates panes. `lib.rs` and component events own state and mouse geometry. `mj-chat/src/usage_format.rs` derives activity and clocks shared by views. `hel_chat.rs` and `hel_chat/transcript.rs` render the selected conversation. Core relay snapshots and materialized projections carry durable activity evidence; the daemon is the persistent process owning execution. `mj-tui/src/setup.rs` owns a configuration draft, while `review_settings.rs` currently has the richer standalone review editor.

## Plan of Work


Milestone one unifies rows at all normal widths, retains permission badges and actionable status before truncating identity, and wraps only the two output lines. Use the current reply after latest prompt, otherwise current tool/thought, otherwise clearly labeled user prompt. Retain grouping/folding and mouse selection. Place Create/Resume in a fixed top row using existing commands; compact title controls reserve the pending count. Preserve the existing 80-column minimum and adaptive Targets/Quota decision. Actual rendering must show readable status at 20/40 columns.

Milestone two retains the originating turn timestamp in shared activity state through background work, with durable reconstruction on reconnect. New turns reset it and settled activity displays Idle. Missing timestamps never invent a duration. Shared activity classification drives clocks, session animation, conversation animation and redraw scheduling. Questions or queues alone do not animate, nor do stale phase flags; foreground, background, lifecycle and review work do. Closed primary state overrides stale primary work. Setup advanced.detailed_activity_clocks defaults false; enabled normal rows and conversation header show T/S or BG, compact cells keep compact clocks.

Milestone three extracts the conversation scrollbar rail/geometry for all existing terminal rails, preserving interactions. Merge asynchronous review capability discovery and compatible-profile filtering into the Setup draft with stale-response guards and conflict-aware save. Remove duplicate editor while preserving any shortcut as navigation into Setup. Keep per-session model/effort and second-opinion controls. Remove only verified unused direct dependencies in affected core/controller/chat/TUI manifests and update documentation.

## Concrete Steps


Work in /home/jonathan/Projects/hel on current master. Use bounded agents with separate ownership and root integration; do not run competing Cargo builds. Run cargo fmt --all -- --check, elevated cargo test, and cargo clippy --all-targets -- -D warnings, plus documentation checks and git diff --check. Build outputs use the normal target directory. Commit validated checkpoints and final changes on current branch without pushing.

## Validation and Acceptance


Actual-render tests cover 80/120/wide terminals, both sidebar sides, min/std/max, longUnicode names, question and queue counts, badges, wrapping, controls and resizing. Test turn→background→idle, new turn during background, reconnect, missing timestamps, pending questions, lifecycle/review work and stale closed snapshots. Test scrollbar endpoints/content-fit/drag and Setup discovery races, offline discovery, Save/Cancel and concurrent config updates. The previous web-viewer port retry test must pass without masking its behavior. Full required checks must pass.

## Idempotence and Recovery


Keep old configs valid through defaulted fields. Do not create sessions or mutate real user data for validation. Tests use isolated state. Preserve the untracked prior ExecPlan. Stage only task changes; no branches or pushes.

## Artifacts and Notes


The earlier workspace commit is 2f268023 and input/animation checkpoint is bbf47f93. This plan records the new Sessions work independently.

## Interfaces and Dependencies


Use HelConfig.advanced.detailed_activity_clocks with default false. Shared activity projection and formatting belong alongside SessionActivity; views must not maintain divergent timer logic. Reuse existing Form controls, action dispatch and background discovery/persistence helpers. No new crate or external dependency is needed.

Revision (2026-09-08): Initial execution plan records the final user choices and bounded delegation.

Revision (2026-09-08): Recorded durable activity timing and chat validation. The worker snapshot carries an optional activity_turn_started_at_ms through background work and clears it at settled idle; operational snapshots carry it to reconnecting clients. A shared predicate ignores stale Running flags and waiting-only questions. Header rendering also suppresses primary animation for unreachable/closed views.

Revision (2026-09-08): Advanced settings advance the config schema to 4, upgrading versions 1–3 on normal load/save and preserving older-build read-only handling. Review subsection Back retains validation; unsaved changed account definitions must not reuse or probe saved capabilities. Controller suite passed 714 tests with one existing ignored test. Shared scrollbar and activity review additionally checked disconnected snapshot-less views and live SDK steps without explicit turn markers.

Revision (2026-09-08): The first integrated TUI run compiled and passed 341 tests but exposed 24 stale layout assertions and review-flow test failures; correction remains in progress. Documentation checks pass. Focused viewer retry and full core suites pass. Final validation must use the completed renderer and Setup changes.

Revision (2026-09-08): Final review removed obsolete test-only renderers and redirected metadata tests through the production row formatter. Added actual minimized Create/Resume click tests, Unicode question visibility with workspace-specific counts, and long running-clock queue visibility. Setup tests cover changed-account cache/refresh guards and validation retention across unrelated or unchanged account edits. Worker snapshot schema advances from 4 to 5; the legacy snapshot upgrade/reopen test passes, and active prompt timing takes precedence over the retained background anchor.

Revision (2026-09-08): Repeated CLI suites exposed a transient listener-release race in the prior web-viewer retry test. A failed retry was immediately followed by a successful bind with no remaining socket owner, consistent with fork/exec descriptor inheritance during concurrent process tests. The test now waits up to three seconds for a non-listening reuse-address bind to succeed before issuing exactly one Retry, preserving same-port, HTTP, and shutdown assertions. Twenty consecutive complete CLI-suite runs passed (203 tests each). No production retry behavior changed. Final clippy, formatting, and documentation checks pass.
