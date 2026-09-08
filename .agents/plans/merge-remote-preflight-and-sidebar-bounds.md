# Merge remote preflight and sidebar bounds

This living plan follows `.agents/PLANS.md`.

## Purpose and context

Complete the user's in-progress merge on master, combining local commit 29310d2d (daemon-lifetime worker pinning and the preceding live-workspace TUI integration) with incoming df0d994e. Incoming changes add independent network clones for isolated sessions, repository-plan review before creation, bounded sidebar widths, utility inference changes, and fixture daemon cleanup. Preserve the local bordered workspace tabs, explicit full creation wizard, active-only Sessions list, readable status rows, and pinned worker sources.

## Progress

- [x] Inspect merge parents and nine conflicted files; assign independent resolution slices.
- [x] Resolve conflicts and review automatic integration across workspace ownership and preflight.
- [x] Run formatting, Rust tests and strict Clippy, docs and web checks.
- [x] Prepare the validated merge on master for the authorized upstream commit and push.

## Plan of work

Resolve renderer files together to retain live workspace tabs while adopting incoming width caps and ineffective-control hiding. Resolve wizard and CLI preflight together so the action retains the selected workspace through asynchronous completion. Combine test daemon cleanup with the existing full-wizard PTY interactions. Retain the incoming shorter README and accurate detailed terminal documentation. Review automatic merges around worker pinning and daemon startup before validation.

## Milestones and acceptance

First, all conflict markers disappear and both sets of intended behavior remain represented in source and behavior tests. Creation and resume dialogs capture their destination workspace at opening. Workspace switches only filter the view and must not cancel pending preflight or reassign its destination. Second, from the repository root run `cargo fmt --all -- --check`, elevated `cargo test --quiet -- --test-threads=2`, and `cargo clippy --all-targets -- -D warnings`; use TOKIO_WORKER_THREADS=1, RAYON_NUM_THREADS=1, CARGO_BUILD_JOBS=1 and taskset -c 0 to limit host thread pressure. Keep logs in target. Run `npm run check` from docs and the web unit test script. All applicable checks must pass. Finally explicitly stage resolved files and this plan, commit the existing merge, and push origin master, without rebasing or forcing.

## Surprises & Discoveries

Some incoming conflict hunks contained obsolete workspace-picker and stopped-history tests because both branches evolved from the previous TUI. Those were removed without undoing live workspace navigation or Resume-only history. The sidebar cap also changes the hypothetical maximized width used for adaptive Targets/Quota placement; that calculation now uses the shared bounded-width function.

The first full run passed chat (458) and controller (727), then core passed 877 with one bridge fixture failure during cleanup (repeated startup exits). The unchanged bridge test passed in isolation in 0.55 seconds. The final full suite passed: chat 458, controller 727, core 878, TUI 387, worker 109, CLI 204, all auxiliary suites and six PTY tests. The bridge test also passed in this full rerun. Logs are target/merge-final-tests.log, target/merge-final-clippy.log, target/merge-docs-check.log, and target/merge-web-tests.log. Strict Clippy, formatting, docs diagnostics, and all 22 web unit tests pass.

## Decision Log

Retain incoming standard sidebar bounds of 40–80 columns and maximum half-width capped at 100, with unavailable maximum hidden. Preserve local workspace navigation and readable rows. These features are independent and can be combined.

## Idempotence and recovery

Do not abort the user's merge, change branches, force-push, or include the unrelated untracked restore-tui-workspaces-and-status.md plan. Retry failed validation after addressing its cause. Any new remote advancement requires inspection and normal integration.

## Outcomes & Retrospective

All merge conflicts are resolved and the integrated tree passes validation. Create and Resume now capture their destination workspace when opened; switching tabs does not cancel pending preflight or change that destination. The incoming network clone behavior, sidebar bounds, and fixture cleanup coexist with the live workspace TUI and daemon-lifetime worker pinning. User clarified that workspace switching must never cancel work; an initial cancellation-based integration fix was removed in favor of capturing destination workspace ownership. Worker source pinning and its daemon launch call survived automatic merging; the incoming worker file change only updates target guidance.
