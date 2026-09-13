# Make managed worktree creation explicit


This ExecPlan is maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture


People must be able to start a session in the directory they selected instead of silently receiving another Git worktree. Add a Create managed worktree checkbox to terminal and web new-session confirmation and terminal import confirmation. A managed worktree is a separate Git checkout and branch owned and cleaned up by the session. Unchecking uses the selected directory directly, including after resume.

## Progress


- [x] (2026-09-13) Inspected creation, provisioning, import, resume, database, and UI paths; user approved both interfaces and existing defaults.
- [x] Persist and enforce the worktree preference; share background eligibility inspection.
- [x] Connect terminal, web, and import confirmation.
- [x] Add behavior tests for persistence, provisioning, terminal review, clean imports, and web review.
- [x] Remove automatic startup session creation and its settings following the user’s explicit scope update.
- [x] Refresh Setup documentation capture and check documentation and Python fixtures.
- [x] Fix the separately reported worker build failure by declaring tokio-util’s compat feature directly in mj-worker; native and musl worker builds now pass.
- [x] Complete final Rust validation and include the session changes in the required commit.

## Surprises & Discoveries


Imports are saved stopped without managed worktree metadata, but first resume calls the same provisioning function as new sessions. Therefore import needs a persisted preference too. Existing linked checkouts are currently skipped by `prepare_managed_raw_worktree`.

## Decision Log


Decision: Preserve automatic defaults: main checkout checked, linked checkout unchecked; both editable. Plain directories and isolated targets are unchecked and disabled. SSH bare follows local bare rules. Rationale: expose and override the existing behavior without changing defaults. Date/Author: 2026-09-13, user and Codex.

Decision: Use an optional boolean named `create_managed_worktree` throughout launch and durable session state. Missing means legacy automatic, true requests creation, and false requires direct use. Rationale: existing callers and stored sessions remain compatible while explicit selections survive reload. Date/Author: 2026-09-13, Codex.

## Outcomes & Retrospective


Implementation and validation are complete. New-session review in both interfaces and dashboard import confirmation expose the persisted worktree choice. Automatic startup creation, Quick New, and their settings are removed; old startup settings are discarded safely. The separately reported standalone worker build is fixed in commit `76d6cebf` by declaring the compat feature directly.

The full workspace suite passes with `cargo test --quiet --no-fail-fast -- --test-threads=4`. Normal-concurrency runs exposed intermittent OS `Text file busy` and cache-lease collisions in unchanged subprocess tests; all tests pass with bounded concurrency. Two setup test fixtures were corrected to use `advanced.show_stopped_sessions`, since the similarly named top-level field is deprecated and intentionally omitted on save. `cargo clippy --all-targets -- -D warnings` and rustfmt pass. All 13 focused browser tests pass. The 9 run-script tests and 5 install-script tests pass outside the sandbox; the sandbox suppressed diagnostics from the run-script fake subprocesses. `scripts/run.sh -- --version` successfully builds native and musl workers plus the CLI. Astro reports zero diagnostics, Python fixture syntax passes, and the Setup screenshot is regenerated.


## Context and Orientation


`mj-controller/src/controller/worktree.rs` resolves directories, inspects Git checkouts, and creates owned worktrees. `mj-controller/src/controller/provisioning.rs` invokes that preparation for creation and resume. Session records in `mj-core/src/state.rs` are persisted through `mj-controller/src/database.rs` and its `schema.rs` module. `mj-client/src/daemon.rs` defines requests to the long-running controller process.

The terminal wizard is in `mj-tui/src/wizards.rs` and `wizards/dashboard.rs`; CLI dashboard actions and background results connect it to the controller. Web confirmation is in `mj-controller/src/web/viewer.js`, backed by preflight and action types in `server.rs` and execution in `server_runtime.rs`. Terminal import preparation is in `mj-cli/src/import.rs`, and its confirmation is in `mj-tui/src/dialogs.rs`. Web has no import flow.

## Plan of Work


First add the optional preference to SessionRecord and creation interfaces, with an additive nullable database column and protocol version bump. Extend worktree provisioning to obey false without creation, true even for linked checkouts, and missing with existing behavior. Create from the selected checkout's HEAD and upstream and preserve the relative directory. Keep existing cleanliness and cleanup behavior. Reject explicit true for unsupported inputs. Share one controller query returning whether creation is available and whether it is the default; run it only in existing supervised background work.

Next extend terminal directory validation and web preflight with that query's result. Render an accessible checkbox and explanatory text in final review, for explicit New sessions. Preserve selections across Back and refresh; clear them when directory or target changes, and reject stale replies. Carry the selected value through retries and requests. Imported raw sessions must always receive confirmation even if there are no safety warnings; combine the checkbox with existing warnings and untracked-file selection and persist the choice before publishing the session.

Finally prove defaults and overrides, linked checkout commit selection, directory preservation, durability, import first-resume behavior, disabled targets, cancellation, and both UIs' interaction. Keep normal command-line callers on the missing-value default and leave move semantics unchanged.

## Concrete Steps


Work from `/home/jonathan/Projects/hel`. Run `cargo fmt --all -- --check`, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings`. Run the focused browser suite from `tests/e2e/web` with `MJ_BROWSER_SPEC=new-session.spec.js npx playwright test`. All commands must exit successfully; record failures and their resolution here. Do not redirect Cargo output directories into `/tmp`.

Stage only changed files and commit directly to the current branch. Do not push or include the pre-existing untracked `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, or `mj.sqlite3`.

## Validation and Acceptance


Selecting a main checkout shows a checked checkbox. Uncheck and launch: the stored project directory is the selected directory, no owned branch or checkout is created, and stop/resume preserves this. Select a linked checkout on a different commit and check: the new owned checkout starts at that commit, with the same relative subdirectory and upstream. Existing dirty-source rejection still applies when creating. Plain directories and containers/VMs cannot enable the checkbox. Test local and SSH inspection with real repositories or hand-written process fakes. Test database migration and round-trip for missing, true, and false. Import a clean raw session and confirm it presents the choice; uncheck and verify first resume stays unmanaged. Test UI keyboard/mouse toggles and stale background responses.

## Idempotence and Recovery


The database change is additive and preserves old rows as automatic. Existing managed metadata remains the authority for cleanup and restoration; preference alone never authorizes deleting a checkout. Failed creation and cancellation use existing bounded cleanup. Tests use temporary repositories. Re-run failed checks after fixing their causes and preserve unrelated workspace changes.

## Artifacts and Notes


The commands and results above record the completed validation. The session implementation, documentation, and this plan are committed together; the independent worker dependency fix is commit `76d6cebf`. Pre-existing untracked files remain outside both commits.

## Interfaces and Dependencies


Use existing Git subprocess helpers and background task supervision. Share a small serializable `ManagedWorktreeOptions` value with `available` and `default_create` booleans in mj-core, and a controller method that computes it for a target and selected directory. Extend existing preflight responses rather than adding a new endpoint. Add `create_managed_worktree: Option<bool>` to SessionRecord, CreateSessionRequest, SessionLaunchOptions, and creation actions. Use existing checkbox components in the terminal and native checkbox input on the web. No new crate or dependency is needed.

Revision: 2026-09-13 — initial implementation plan based on the approved design and repository inspection.

Revision: 2026-09-13 — Quick New was a helper used only by automatic startup, not a separate UI entry point. The user explicitly requested removal of automatic startup sessions. Removed that creation path, helper, defaults selection/provisioning module, setup controls, and related configuration references. Legacy `[startup]` sections deserialize into a discarded value and disappear on save; no setting can re-enable automatic creation. Opening an existing live conversation remains separate from creating a session.

Revision: 2026-09-13 — The user reported scripts/run.sh could not compile tokio_util::compat and asked to check install.sh too. mj-worker used compat but declared only rt; workspace tests obtained compat indirectly from its controller dev-dependency. Declaring compat on the worker dependency fixes independent native and portable builds, including scripts/install.sh’s release builds. The root install.sh downloads binaries and has no affected Rust build path. Validate through scripts/run.sh -- --version and the existing run/install script tests, without replacing the user’s installed binaries.
