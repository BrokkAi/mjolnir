# Offer to repair stale Git tracking during session creation

This plan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

When a local branch names a removed remote but origin (or the only configured remote) offers valid network fetch and push URLs, session creation should offer to repair tracking and continue. The person reviews the actual destination and approves the local Git configuration change instead of receiving an unrecoverable error or having to run Git commands.

## Progress

- [x] (2026-09-10) Trace startup creation, terminal wizard preflight, and web preflight.
- [x] (2026-09-10) Implement shared repair discovery and guarded application.
- [x] (2026-09-10) Connect repair confirmation and background retry to terminal and web creation.
- [x] (2026-09-10) Add core, terminal input/render, and web approval regression tests; web unit tests pass.
- [x] (2026-09-10) Trace recurring container permission error to a stale npm daemon and implement macOS development daemon refresh.
- [x] (2026-09-10) Full `cargo test --quiet`, Clippy with warnings denied, rustfmt, shell/JS syntax, and web unit checks pass; host executable built.
- [x] (2026-09-10) Commit macOS daemon refresh as 75712ae4 and gracefully restart the old installed daemon with the local build (PID 3834).
- [x] (2026-09-10) Complete the validated Git repair flow for the current-branch commit accompanying this plan.

## Surprises & Discoveries

Startup creation bypasses the terminal wizard preflight and resolves sources inside `spawn_dashboard_create_session`. Both entry points need the same repair discovery. Web preflight already runs in a cancellable background task.

The user reported the earlier container chmod failure during this work. Process inspection showed the npm daemon PID 41688 had started at 15:17, before ownership fix commit ab258373 at 15:38; the development binary was rebuilt at 15:39. `scripts/run.sh` and `maybe_replace_stale_development_daemon` enabled executable replacement checks only on Linux. On macOS the UI could therefore use the new build while provisioning still ran in the old installed daemon.

## Decision Log

Offer an explicit local configuration repair, as requested by the user, instead of silently substituting a remote. Select origin or the sole configured remote; preserve errors for ambiguous choices and unusable URLs. Preserve explicit push destinations and the branch merge ref. Recheck the proposal against current configuration before writing so an old confirmation cannot overwrite a changed setting.

Extend the existing opt-in development daemon refresh to macOS using sysinfo's executable path and process start time, filesystem identity, and modification time. Run process inspection in a blocking task. Keep Linux's running-inode comparison. The existing container ownership fix already has a passing real Docker integration test recorded in `.agents/plans/launch-failure-recovery.md`; do not add another ownership workaround. Validate and build the current binary, then gracefully replace the user's stale daemon through the supported daemon command.

## Outcomes & Retrospective

The offered Git repair is implemented across startup, terminal wizard, and web creation. Cancellation preserves the previous screen; accepted repairs recheck current configuration, write only the stale remote setting, and retry in supervised background work. Web unit tests pass (26 tests), the full Rust suite passes, and Clippy with warnings denied, formatting, shell syntax, JS syntax, and diff checks pass. The first full run found one new test expecting unsanitized SSH user text in a display URL; the expectation was corrected to use the existing display sanitizer. The actual Git URLs remain unchanged.

The stale macOS daemon fix is committed as 75712ae4. `cargo build -p brokk-mjolnir --bin mj` succeeded and `target/debug/mj daemon restart` replaced the old installed daemon with PID 3834. This addresses the recurring container permission error by using the already-fixed ownership code. The npm installation is unchanged; use the repository's `scripts/run.sh` for the development UI and automatic daemon refresh. A new container session was not provisioned as part of this work.

## Context and Orientation

`src/hel_local_git.rs` resolves fetch and push endpoints using the shared `CommandExecutor`, which supervises subprocesses. `mj-cli/src/dashboard/actions.rs` starts terminal wizard checks, and `mj-cli/src/dashboard/io.rs` runs startup creation and applies background results. `mj-tui/src/dialogs.rs` owns confirmation controls. `mj-cli/src/server.rs` executes web preflight and `mj-controller/src/hel_server.rs` carries its request and response; `mj-controller/src/web/viewer.js` presents the web creation flow.

## Plan of Work

First add serializable repair proposals to the existing local Git module. Discovery verifies the replacement through the existing URL resolver without writing configuration. Application regenerates and compares the proposal, then writes only the stale branch remote setting through the shared executor. Bundle-scoped application must reject proposals outside that bundle.

Next surface proposals before terminal startup registration and wizard source resolution. Confirmations preserve the previous UI mode, show the repository, branch, replacement, fetch and push endpoints, and dispatch supervised background repair. Retry the original operation after success; report failure without losing the pending operation. Web preflight returns proposals and accepts explicitly approved proposals on a later request; the viewer asks for confirmation and repeats preflight.

## Concrete Steps

Work in `/Users/ryansvihla/code/mjolnir`. Add focused tests using temporary Git repositories and direct terminal state transitions. Run `cargo fmt --all -- --check`, `cargo test` outside the restricted sandbox, `cargo clippy --all-targets -- -D warnings`, and the applicable web syntax/behavior checks. Review the diff and stage only this task's files, then commit to the current branch without pushing.

## Validation and Acceptance

A branch configured with `branch.main.remote=missing` and a usable origin should produce a proposal without changing Git configuration. Approval should change tracking to origin, preserve merge and push settings, and permit source resolution. Cancellation should produce no repair action and preserve the creation screen. Changed configuration or an unrelated repository proposal must be rejected. Startup and wizard creation must both offer repair before creating a session; web creation must echo approved proposals before any mutation. Existing valid remotes keep their existing behavior.

## Idempotence and Recovery

Discovery is read-only. Each approved repair changes one Git configuration key atomically through Git. If several repositories need repair and a later repair fails, earlier completed repairs remain valid; retry discovery offers only remaining problems. No branch changes, remote pushes, or worktree content changes are involved.

## Artifacts and Notes

Original failure: current branch names Git remote "upstream", but that remote is not configured. The reported checkout had a valid origin for both fetch and push.

## Interfaces and Dependencies

Keep repair types and operations in `hel::hel_local_git`, using existing serde, anyhow, PathBuf, ProjectBundle, and CommandExecutor dependencies. Use the existing terminal confirmation and supervised background task infrastructure; do not add a crate or run subprocesses on UI loops.

Revision 2026-09-10: initial plan reflects the user's explicit request for an offered, applied repair.

Revision 2026-09-10: record implementation, initial test evidence, and the user's subsequent recurring permission failure caused by stale macOS development daemon selection.

Revision 2026-09-10: record completed validations, the daemon fix checkpoint, and live daemon replacement.
