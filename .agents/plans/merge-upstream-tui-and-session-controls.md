# Merge upstream TUI and session controls

This ExecPlan follows `.agents/PLANS.md` and records the approved integration.

## Purpose / Big Picture

Merge origin/master 14b99cb3 into master 3d9f1fc8 while retaining live workspace filtering and readable Sessions. Integrate themes, mouse controls, status symbols, steering notices, and upstream fixes. Explicit Create opens the full wizard. Workspaces gains keyboard focus and a border; tasks become a prompt-border control.

## Progress

- [x] Verified upstream, inspected dry-run conflicts and incompatible automatic merges, and agreed product decisions.
- [x] Began the real merge without committing; assigned bounded independent implementations.
- [x] Integrate runtime/interfaces, workspace focus, and archive removal.
- [x] Review Sessions, workspace controls, configuration, and documentation changes.
- [x] Finish task-dialog interaction review, preserve full command text, and draw supporting panes before chat modals.
- [x] Run default Rust suites, final chat/TUI/CLI tests including all PTY tests, Clippy, formatting, documentation, license and terminal checks.
- [x] Commit the validated merge on master.

## Surprises & Discoveries

The merge has 28 conflicted files. Both branches independently used daemon protocol 15 for incompatible message shapes. Automatic theme merge retains aliases to deleted color constants. Surface controls overlap old mouse-down activation. Upstream restored a standalone workspace selector that local tabs replaced.

The reported pre-merge blank preview exposed two projection assumptions: agent/tool-only snapshots were gated on a user message, and an equal-frontier late startup summary could erase a richer live excerpt. Both now have regression coverage. Initial concurrent controller tests exhausted host threads (Rayon EAGAIN); the controller/core/worker suites passed with bounded threads. Final UI validation uses TOKIO_WORKER_THREADS=1 RAYON_NUM_THREADS=1 CARGO_BUILD_JOBS=1 taskset -c 0 and --test-threads=2 to avoid disturbing unrelated host processes. An extra --workspace test attempt reached optional desktop dependencies unavailable on this host (GTK/WebKit); use the repository default six-crate cargo test scope.

Live terminal tests also found cached palette availability keeping Rename disabled after creation completed, and retirement of an early warm chat leaving its attachment selection marked complete. Refreshing command availability preserves the selected command; retiring only the matching attachment permits one fresh open without affecting another selected workspace. Both have state/input regression tests. A later real target rename exposed another delayed-completion race: changed configuration still called cancel_modal and could close a newer command palette. Configuration refresh now retains the current interaction and draft; a changed-config palette test covers this path. PTY wizard tests now follow Enter from the directory field and recognize incremental terminal title updates.

## Decision Log

Keep local workspace runtime architecture and Sessions layouts; port upstream controls onto them. Use one merge commit without rebase. All explicit New/Create commands open the full wizard; automatic empty-workspace startup retains existing configuration and workspace guards. Keep stopped sessions only in Resume and remove Mjolnir archive filtering entirely. Legacy storage remains readable; provider archive data is informational and browsing never changes it. Use upstream spinner placement above the composer with local truthful activity clocks. View tasks is embedded in the prompt bottom border, not an extra row. Workspaces is a fixed three-row bordered pane above Sessions, with one styled space on each side of each tab label.

## Outcomes & Retrospective

All-target compilation passes. Controller: 726 passed/1 ignored; core: 873 passed/6 ignored; worker: 109 passed/2 ignored; their auxiliary tests also pass. Documentation check reports zero errors/warnings/hints. License regeneration matches the staged report and cargo deny passes (existing unmatched libbz2 exception warning). Final chat tests: 458 passed/1 ignored; TUI: 381 passed/2 ignored; CLI: 203 unit tests passed, plus auxiliary tests. All five PTY tests pass after updating wizard navigation. Clippy, formatting, and the full chat/TUI/CLI invocation pass after the final configuration-refresh fix. The seeded mouse scenario passes end to end (seed908). The broader component scenario passes against the final binary (seed515), including Stop/Resume, review discovery, container settings, deletion confirmation, and SIGTERM cleanup. Artifacts: target/reliability-artifacts/tui-components-seed-515-1746896 and target/reliability-artifacts/mouse-commands-seed-908-1687469. Preserve the untracked prior workspace plan. No push or release is requested.

## Context and Orientation

`mj-cli/src/dashboard.rs` owns attachment and workspace selection orchestration; `dashboard/io.rs` runs supervised background work. `mj-tui/src/lib.rs` owns focus and state, `combined.rs` layout, `render.rs` Sessions, and `workspaces.rs` tabs and integrated manager. `mj-chat/src/hel_chat/active.rs` draws the composer and receives operational snapshots. `src/hel_config.rs` owns persisted configuration; `mj-chat/src/theme.rs` owns dynamic palettes.

## Plan of Work

First resolve shared state and API conflicts. Retain local independent live sessions and selected-workspace filtering, draft persistence and guarded async completion. Remove obsolete standalone selector and Quick New UI/action path. New commands and their old shortcut aliases enter the full wizard with upstream multi-repository creation. Remove archive actions, filtering, badges and hidden-native loading; keep old database columns/record fields readable without applying their visibility policy. Keep destructive deletion and Resume search/import behavior.

Then integrate UI. Workspaces joins the focus ring before Sessions, Prompt, Targets, Quota; reverse traversal works. Left/right immediately selects an adjacent workspace, stops at ends and retains workspace focus through restoring another view. Clicking a tab focuses the pane. Its three rows occupy the sidebar above Sessions; labels retain symmetric padding, active tab scrolls into view and long Unicode names truncate inside padding. Keep min20, standard max(40,width/3), max width/2 Sessions and 80-column terminal minimum. Preserve four-line normal/two-line compact rows, badges, previews, status/queue priority and adaptive Targets/Quota grouping. Use one shared Form mouse-up path for top Create/Resume, footer and session menus; reserve title space for ellipsis. Add fixed status symbols without clipping actionable text.

Move the selected conversation spinner above composer using corrected busy predicate. Keep model/effort at prompt top border and microphone immediately after effort, or model if absent. Remove Background oldest summary. Show View tasks (N) in bottom prompt border when background_commands exist. Down from last visual input line focuses it after autocomplete/history navigation; Enter/click opens live scrollable commands plus elapsed times. Up/Esc returns focus; Esc closes dialog preserving draft. Task dialog is read-only and updates from existing snapshots without blocking I/O; empty open dialog explains all tasks finished.

Merge config schema6 accepting1..5, theme choices plus advanced detailed clocks, deprecated show_stopped_sessions accepted/preserved but hidden and ineffective. Keep integrated Setup review draft and discovery. Daemon protocol16 distinguishes incompatible15; preserve old management transcript fixtures. Worker state5 and relayprotocol8 remain, combining activity anchor and optional steering support. Port all static colors to dynamic semantic palettes preserving red error, amber activity, yellow attention/unread, blue idle. Keep slim shared scrollbars.

Finally update documentation/screenshots and tests. Retain upstream package2.4.0 and verified unused dependency removals; refresh generated dependency outputs as needed.

## Concrete Steps

Work in /home/jonathan/Projects/hel on master. Resolve owned files in the shared worktree; root coordinates Cargo and stage operations. Run cargo fmt --all -- --check, elevated cargo test, cargo clippy --all-targets -- -D warnings, documentation npm run check, git diff --check, and updated isolated real-terminal scenarios. Build outputs stay in normal target storage. Stage explicit task paths, resolve merge index and commit the validated merge without pushing.

## Validation and Acceptance

Actual render/input tests must prove workspace keyboard/mouse focus retention and Unicode padding/overflow; top Create opens wizard; session menu targets clicked session and modal/disabled pointer activation is safe. Test background task navigation, selection, live update, elapsed display, empty state and focus/draft restoration. Exercise sizes/themes with readable status/queue and quota percentages. Test background workspace operations cannot steal selection, legacy archived history is visible, config1..5 loads, protocol15 rejects ordinary current sessions, and worker timing/steering coexist. Retain supported tests and update old layout/Quick New/archive assertions to advertised behavior rather than weakening checks.

## Idempotence and Recovery

No real sessions or provider state are mutated for tests. Old archive storage is not dropped. Existing merge parents and untracked prior plan are preserved. Unexpected background failures remain reported. Stop owned test processes before removing their files. Do not change branches or push.

## Artifacts and Notes

Parents: local3d9f1fc8 and upstream14b99cb3. Full agreed behavior comes from this plan and the conversation. Each agent owns disjoint files; root owns integration and final review.

## Interfaces and Dependencies

Add Focus::Workspaces and keep support pane sizing separate. Tasks are client-side focus/modal state over existing BackgroundCommand data. Config6, daemon16, workerstate5, relay8. No new crate or external dependency.

Revision: Initial execution record of approved merge and interaction decisions.

Revision: Record preview, palette availability, and attachment retirement regressions found during integration, along with passing unit and PTY validation.

Revision: Record passing mouse workflow and the changed-configuration refresh race found by the broader terminal scenario.

Revision: Both real-terminal workflows now pass. Updated stale harness expectations for integrated Setup, direct Stop, Delete confirmation, workspace selection, and filtered palette geometry without changing the advertised application behavior. All implementation and validation work is complete. This record is included in the validated merge commit on master. No push was performed.
