# Finish Destroy when target cleanup fails

This living ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries, decisions, and outcomes current.

## Purpose / Big Picture

Destroy must remove a session even when its remote host cannot be contacted. The user explicitly chose ordinary Destroy with a cleanup warning. The CLI, terminal dashboard, and browser use the same daemon operation. External cleanup failures must be visible without leaving an undeletable session. A worker whose death cannot be confirmed must keep its files: removing the session record does not authorize deleting files under a potentially live writer.

## Progress

- [x] Diagnosed the live SSH/Podman cleanup and exact-absence probe failures.
- [x] Confirmed the user's choice to proceed with a warning.
- [x] Implemented explicit cleanup outcome, strict automatic stopping, and safe daemon/browser warnings.
- [x] Focused isolated cleanup/removal regressions and browser warning/dismissal tests pass.
- [x] Completed touched crate suites, corrected the one obsolete expectation and reran the affected cases; final workspace clippy passes.
- [x] Committed the implementation as `3c523cbdd` and completed the first review.
- [ ] Validate and commit the review fixes, finish review, and push. Apply to the live session only after installation is authorized.

## Surprises & Discoveries

The host described as terminated is actually stopped according to AWS. Its resources can return when restarted. Existing Destroy keeps both the record and archive whenever cleanup cannot confirm absence. Runtime notices already survive session deletion in the terminal; browser snapshots do not yet expose them. Private runtime diagnostics must remain private.

## Decision Log

The user chose on 2026-11-10 to let Destroy proceed with a cleanup warning. Keep ordinary suspension and automatic child stopping strict: only explicit destruction authorizes abandoning remote resources. Keep local recovery archive and database errors fatal so the requested local removal remains retryable. Reuse the daemon's existing notice owner, adding an internal visibility marker for safe public notices without changing its transport protocol.

## Outcomes & Retrospective

The original implementation passed validation; review identified two warning-delivery gaps, now addressed with validation running. The full controller suite passed 2529 cases with one obsolete explicit-Destroy assertion; that assertion was corrected to cover strict automatic stopping and passed on its focused rerun. The terminal suite passed 582 cases; CLI unit tests passed 242 cases plus its enabled integration suites. The final three affected Rust cases, browser warning rendering/dismissal test, formatting, and workspace clippy all pass. No live data has been changed. Filing an issue was rejected by automatic approval review because the proposed issue included live identifiers; do not publish those details.

## Context and Orientation

`mj-controller/src/controller/lifecycle.rs` owns target teardown and record removal. `mj-controller/src/daemon/resume.rs` runs explicit destruction and automatic child stopping through the same indexed helper; the lifecycle kind distinguishes their intent. `mj-controller/src/daemon/views.rs` owns the bounded runtime notice queue, and `daemon/feed.rs` sends its existing wire representation to terminal clients. Browser projections are built in `server_runtime/run.rs`; public types live in `server/viewer_types.rs` and the browser renders them in `web/viewer.js`.

## Plan of Work

Return a destruction outcome containing safe cleanup warnings from explicit controller destruction. Catch external target and prepared destination cleanup failures, retaining process-owned files after unconfirmed target death. Continue independent cleanup where safe. Treat managed checkout cleanup failures as warnings in explicit destruction as well. Retire obsolete worker restart intent when forgetting the session. Preserve strict cleanup in automatic child stops by passing intent explicitly through the existing daemon helper.

Publish warnings from the daemon through the existing notice queue, with an internal marker permitting the safe warning text in browser snapshots. Keep private diagnostics out of browser projection. Render public notices independently of session cards so deletion cannot hide the warning. Explain the cleanup policy in destroy confirmations and CLI help. Provide `mj notices --session <id>` and an authenticated `/api/v1/sessions/{id}/notices` endpoint that reads safe recent notices without requiring the deleted record. Destroy points to this command and JSON callers receive its URL. Absence of retained notices does not imply completion or successful cleanup. Scope browser notice dismissals by feed incarnation and notice ID so a daemon restart cannot hide a new warning.

## Concrete Steps

Work from the repository root. Use existing lifecycle tests and their isolated-store helper, adding an unreachable target regression and extending notice coverage. Run focused tests while editing, then the touched crate suites and workspace clippy once at the end. All Cargo tests require escalated execution and retain existing isolated store directories; use `TZ=UTC` for terminal golden tests. Run browser tests with `node --test` from the repository root.

## Validation and Acceptance

An explicit Destroy with failed target cleanup removes its durable record and recovery archive, returns a warning, and never deletes a surviving worker's files or releases their build cache. Ordinary suspension and automatic child stopping still fail visibly when their target survives. Public warnings reach the browser after the session is absent; private runtime notices do not. Successful teardown and branch disposition tests continue passing. Run `cargo test -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui` and `cargo clippy --all-targets -- -D warnings` in the dev profile; run the relevant Node browser tests.

## Idempotence and Recovery

Destroy remains admitted by the existing lifecycle and worker owners. Database and archive failures retain the record. Failed external cleanup is logged and reported as a warning; resources on a stopped host can remain until the host is removed or manually cleaned. Do not install a new build or modify the live store without the user's installation instruction.

## Artifacts and Notes

The failure was an SSH connection timeout in both target cleanup and its exact absence probe. No new timeout, retry budget, protocol revision, or database migration is required.

## Interfaces and Dependencies

Keep the existing HTTP and daemon request shapes. Add a controller destruction outcome and an internal cleanup policy distinguishing explicit destruction from automatic stopping. Keep notice visibility internal to the daemon so old clients retain the same runtime notice wire shape. Add an optional, omitted-when-empty notice array to HTTP viewer snapshots, plus an additive notice-reading endpoint. Preserve asynchronous Destroy and use the existing safe notice owner for CLI retrieval.

Initial plan recorded after live diagnosis and the user's cleanup-policy decision.

Implementation update: explicit stopped-session destruction now uses the same path as active destruction. Automatic child stopping and archive cleanup retain strict teardown. The browser metadata projection explicitly includes notices so compact snapshots and deltas can carry warnings after record removal.

Validation update: the first full run exposed an existing test expecting the old strict explicit-Destroy behavior. It now checks strict automatic stopping instead. Extend the existing browser delta integration test to carry a warning with the deleted row and clear evicted warnings with an empty array. Rerun only that corrected assertion and the affected browser integration cases, plus final clippy.

Final validation: the corrected durable-target assertion, browser snapshot HTTP integration and browser delta integration all passed. Final workspace clippy passed. No full suites were repeated to confirm the one assertion correction.

Review update: both reviewers found missing CLI access to warnings and dismissal ID reuse after daemon replacement. Added CLI/API notice retrieval, linked it from Destroy output, and keyed browser dismissals by daemon incarnation. Test the authenticated route with no session record and extend browser dismissal coverage across incarnation changes.
