# Compose multi-repository projects in the browser


This ExecPlan is maintained under `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture


The browser must offer the terminal's optional ability to open several repositories together without requiring ordinary users to create a bundle. On the isolated target's Project step, users can browse or enter a repository, select Add repository to keep it in a list, and repeat. The first repository is primary (the agent's starting directory); Next includes any remaining source and prepares the project before review. Saved projects remain directly selectable. Removing a listed repository and correcting a failed request must preserve the rest of the draft.

## Progress


- [x] (2026-09-24) Inspected browser, HTTP route, controller persistence, and terminal shared creation helper.
- [x] (2026-09-24) Implemented ordered multi-source requests through the existing supervised controller jobs; all four endpoint tests passed.
- [x] (2026-09-24) Implemented the optional repository list, removal, primary labeling, duplicate feedback, and draft preservation; updated documentation. All 26 focused browser scenarios passed.
- [x] (2026-09-24) Reviewed the diff; formatting and whitespace checks passed. Full browser suite passed (45 unit tests, 98 interaction tests, 3 existing skips). The user requested a PR and delegated final full Rust tests and Clippy to CI.
- [ ] Commit task changes and publish the requested PR, leaving CI running.

## Surprises & Discoveries


The existing HTTP route accepts only `{source}` and the controller job invokes the older single-source helper. The terminal already uses `create_bundle_from_sources`, which validates every repository before persisting, rejects duplicates, preserves primary order, and reuses an exact configured group. The controller already supervises these jobs and accounts for them during upgrades, so the new request can use the existing path.

## Decision Log


2026-09-24: Match the terminal's optional composition flow: Add repository keeps a source in an ordered removable list; Next includes the current field. Selecting a saved project or recent directory replaces the composed draft, as an explicit project choice. Preserve legacy `{source}` HTTP compatibility and add `{sources}` for ordered groups. Route both through the shared exact-group helper to avoid silently selecting a larger group when a user chooses a single repository.

2026-09-24: Preserve the existing bounded supervised job pipeline, config-mutation lock, and publication-before-reply behavior. No database, worker protocol, or upgrade handoff format changes are needed.

## Outcomes & Retrospective


Browser composition now uses the shared atomic repository-group creation path. The first member is primary, members can be removed, and Next includes the current input. Tests confirm preservation across refresh, Back, retries, duplicate submissions, and navigation away. Single-repository creation remains direct. Final full Rust tests and Clippy are delegated to PR CI at the user's explicit request.

## Context and Orientation


`mj-controller/src/web/viewer.js` owns the browser wizard draft and rendering, path browsing, preparation, and review. `tests/e2e/web/new-session.spec.js` runs the actual page in an isolated intercepted browser with fake API responses and a phone viewport.

`mj-controller/src/server/handlers.rs` validates `/api/bundles` requests and sends an internal `BundleRequest` from `server/actions.rs`. The receiver in `server_runtime/run.rs` runs supervised asynchronous jobs through `daemon/state.rs`; filesystem work runs on a blocking worker task, and the result is published to the browser snapshot before success is returned. `controller::create_bundle_from_sources` is the shared atomic config writer already used by the terminal. A bundle is internal saved configuration for one or more repositories and their primary directory.

## Plan of Work


First extend the HTTP handler to accept exactly one of source or sources and validate every source before dispatch. Represent internal requests with an ordered source vector, adapt the daemon job to the shared creation function, and keep the existing single-source API helper as an adapter. Add authenticated route behavior tests for ordered forwarding, empty members, ambiguous requests, and failure responses. Existing shared helper tests cover atomicity, duplicates, primary choice, and reuse.

Then extend the browser draft with staged sources. Render an ordered list with primary labeling and accessible remove buttons only when needed. Reuse the existing path field and Add repository action; Next sends all staged sources plus the current field. Preserve draft identity checks, pending controls, live-refresh focus, retry state, and exact saved-project reuse after success. Add browser tests for composition, removal, duplicate handling, refresh and Back preservation, retries, single-flight behavior, and successful launch using the returned project. Document the optional browser control in `docs/src/content/docs/workspaces-bundles.md`.

## Concrete Steps


From `/home/ryan/.codex/worktrees/dc4c/mjolnir`, run:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-controller bundle_endpoint --quiet
    cargo clippy --all-targets -- -D warnings
    cargo test --quiet --no-fail-fast
    git diff --check

Run every cargo test outside the restricted sandbox, keeping existing automated isolated configuration and data paths. Run from `tests/e2e/web`:

    npx playwright test new-session.spec.js --project deterministic
    npm test

These browser tests serve intercepted static assets and do not start or contact a live daemon. Any manual invocation of a new application build must use `--instance browser-multi-project-test` throughout. The user subsequently requested a PR and explicitly delegated remaining validation to CI. Stage only changed files and commit on the current checkout, then publish a dedicated PR head and open a PR targeting master. Do not merge it; leave CI running.

## Validation and Acceptance


Enter `example/app`, Add repository, enter `example/api`, then Next. Review must show both sources, one project must be prepared with app first, and Start must launch with the returned project ID. Remove the first repository and verify that the next member becomes primary. Failed preparation must leave every source editable. Live snapshot updates and Back navigation must retain composed sources; duplicate submissions must issue only one preparation. Selecting an existing project must reuse it without creating a new configuration. A lone entered source must continue directly to review without Add repository.

## Idempotence and Recovery


Config creation is atomic and reuses an exact repository group, so retrying after a lost response is safe. Keep errors visible and retain draft inputs. Tests operate only on fixtures and existing isolated directories. Do not touch live session data or unrelated worktree changes.

## Artifacts and Notes


Completed: `cargo test -p brokk-mj-controller bundle_endpoint --quiet` (4 passed), `npx playwright test new-session.spec.js --project deterministic` (26 passed), `npm test` (45 unit tests and 98 browser tests passed, 3 existing skips), `cargo fmt --all -- --check`, and `git diff --check`. Full workspace Rust tests and Clippy for this revision are left to CI per the user's request.

## Interfaces and Dependencies


Keep POST `/api/bundles` accepting `{source: string}` and also accept `{sources: string[]}`. Return the existing `{bundle_id: string}` response. `BundleRequest.sources` carries the ordered vector; the daemon calls `controller::create_bundle_from_sources(&sources)` inside its existing config mutation lock and blocking worker. No additional dependencies or crates are needed.

Revision 2026-09-24: Created this plan for the user's request to make browser multi-repository creation available.

Revision 2026-09-24: Recorded completed implementation and validation. The user requested immediate PR publication and delegated remaining checks to CI.
