# Unify new-session preflight and open web Review immediately

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Both terminal and browser creation use one controller-owned project preflight. The browser shows Review immediately while Git/SSH checks run, keeps navigation usable, and permits launch only after the current check succeeds. Wizard-step unification is deferred by user choice.

## Progress

- [x] Inspected existing orchestration, cancellation, review rendering, and tests.
- [x] Consolidate controller preflight and migrate terminal, web, and startup callers.
- [x] Implement immediate browser Review with retry and stale-result protection.
- [x] Add controller, TUI, and browser behavior coverage; web checks passed (40 unit, 83 browser, 3 lab-dependent skips).
- [x] Full dev-profile `cargo test`, `cargo clippy --all-targets -- -D warnings`, rustfmt check, and diff check passed.
- [x] Prepare the validated changes for a current-branch commit and the authorized fast-forward push to origin/master.

## Surprises & Discoveries

The TUI already runs checks through a cancellable blocking worker. The browser awaits preflight before advancing, although Back aborts it. CLI startup also repeats repository resolution. Daemon directory validation is a launch-time safety check, not duplicate review orchestration.

## Decision Log

On 2026-09-19 the user selected shared preflight plus web UX, deferring shared wizard-step definitions. Keep HTTP JSON compatible and presentation types as thin adapters. Keep repair application explicit and separate from read-only preflight. Preserve authoritative launch validation. Use existing supervised workers and their 30-second deadlines. On 2026-09-19 the user authorized pushing the completed work to origin/master.

## Context and Orientation

`mj-cli/src/dashboard/actions.rs` starts terminal background work; `mj-cli/src/dashboard/io/spawn.rs` handles startup creation. `mj-controller/src/server_runtime/preflight.rs` runs browser checks on the server's supervised blocking tasks. `mj-controller/src/controller/worktree.rs` owns directory validation and worktree inspection. `mj-controller/src/web/viewer.js` owns browser draft state, navigation, and preflight requests. Rust tests live beside their modules; browser tests live in `tests/e2e/web`.

## Plan of Work

### Milestone 1: Shared controller orchestration

Add `mj-controller/src/controller/new_session_preflight.rs` and expose `Controller::preflight_new_session(&self, bundle_id: &str, target_id: &str, project_directory: Option<&Path>, executor: &impl CommandExecutor) -> Result<NewSessionPreflight>`. Its controller-owned result carries directory, managed-worktree options, repair proposals, sanitized repository previews, and local-change exclusion. Bare targets resolve/validate a directory then inspect worktree options. Isolated targets reject a directory, discover repairs, and only when no repairs are needed resolve repositories and remote default branches. Check cancellation before each stage. Adapt results to existing terminal and HTTP types. Replace duplicated terminal, server, and startup sequencing. Keep daemon launch validation. Prove inputs, repairs, outputs, and failures with behavior tests and existing fakes.

### Milestone 2: Immediate browser Review

Advance from Project after only immediate selection checks. Render Review then start preflight. Show checking state, disable Start, keep navigation and independent controls available, and defer worktree selection until options arrive. Failed checks show Retry; declined repair shows an explanation and Retry. Submit itself checks readiness. Back, route/workspace changes, and replacement requests abort old work and invalidate readiness. Check both draft and request identity before accepting results; old completion cannot clear a newer request. Reentering Review checks again. Keep existing repair approval and worktree-choice preservation. Extend unit and deterministic browser tests with withheld responses, retry, stale results, snapshot refresh, and simultaneous clients.

### Milestone 3: Validate and deliver

From `/home/jonathan/Projects/hel4`, run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. Every Cargo test must run with elevated sandbox permissions. From `tests/e2e/web`, run `npm test`. Record failures and resolutions here. Stage only task changes, commit on the current branch, and push `HEAD:master` to origin without force.

## Validation and Acceptance

Shared tests must exercise valid and invalid bare directories (including supported non-Git local directories), worktree defaults, isolated sources and sanitized URLs, repair proposals without mutation, repository errors, and cancellation. Existing TUI progress/retry/gating behavior must remain intact; add Back/Cancel coverage where missing. Browser tests must observe Review before releasing a held response, disabled Start and guarded submit, successful resolved values, retry after failure, repair decline, cancellation/stale-result ownership, stable snapshot refresh, and concurrent clients. All required checks must pass before final delivery.

## Idempotence and Recovery

No database migration, cache, new dependency, or live-store manipulation is involved. Tests use isolated data and fake network responses. Use existing subprocess supervision. Retry failed validation after fixing its cause; do not force-push or overwrite unrelated changes.

## Interfaces and Dependencies

Keep `/api/preflight/new` request and response JSON unchanged. The shared controller method uses existing `Path`, `PathBuf`, `CommandExecutor`, `ManagedWorktreeOptions`, and `LocalRemoteRepair` types. Presentation adapters retain current TUI and server preview shapes. Repair writes remain in existing explicitly confirmed handlers.

## Outcomes & Retrospective

Shared orchestration and immediate Review are implemented. Compilation and the web suite pass. The initial focused Rust run passed six of seven tests; the remaining test exposed a short invalid commit hash in the fake ls-remote response. The fixture now advertises a valid 40-character hash. The full Rust suite subsequently passed, including all seven shared-preflight tests, the terminal navigation regression, and all eight PTY termination tests. Clippy, rustfmt, and diff checks passed. No production defect was hidden by that fixture correction. Browser tests passed with the final Back invalidation behavior; the three skipped browser cases require the existing provisioned lab. Delivery is a commit on the current branch followed by a normal push to origin/master. Shared wizard-step definitions remain intentionally deferred.

Validation note (2026-09-19): implementation and all required checks are complete. The GitHub issue is left unchanged.
