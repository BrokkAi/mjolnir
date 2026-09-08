# Make web Resume session-first

This ExecPlan follows `.agents/PLANS.md` and must be maintained through implementation and validation.

## Purpose / Big Picture

People should browse saved sessions before answering configuration questions. Web Resume will show a searchable compact list for the current workspace, then one selected session's review card. Error status and failed-move recovery belong in that card rather than above the dashboard or in browsing rows.

## Progress

- [x] (2026-09-07) Inspected web and TUI flows and settled the interaction with the user.
- [x] (2026-09-07) Implemented list, card, navigation, and recovery relocation with Luna.
- [x] (2026-09-07) Added browser and embedded behavior coverage; integrated primary-agent review fixes.
- [x] (2026-09-07) Passed browser, Cargo, and live integration validations; prepared the current-branch commit and authorized upstream push.

## Surprises & Discoveries

The existing web Resume list intentionally crosses workspace boundaries, with a Rust-embedded JavaScript test asserting that behavior. The user explicitly chose current-workspace scope instead. The existing public projection carries `has_error`, not raw error text; generic checkpoint claims in the old card cannot be justified by that flag.

Visual review caught full timestamps consuming most of a phone row and truncating titles. Titles now span the row, with compact recency alongside location/profile below. Browser checks also exposed the need to preserve document scrolling, rather than the non-scrolling list element. A removed choice with one alternative must still offer an explicit replacement dropdown; a static value would strand a disabled Resume button.

## Decision Log

On 2026-09-07 the user chose compact review after selection instead of a step wizard, current-workspace browsing, and error status only within the selected card. Preserve the existing public action API and safe status projection. Native import, archives, expanded diagnostics, and additional provisioning controls are outside this task. The user subsequently authorized pushing when complete; the current branch is `hel2` with upstream `origin/master`.

## Outcomes & Retrospective

The session-first flow is complete and validated. The integrated browser fixture matrix passed 34 checks, the final Resume suite passed 17, the web unit suite passed 15, and Clippy completed with warnings denied. The full final Cargo suite passed. The real browser/TUI stop-resume-reconnect scenario passed, followed by all 22 layout/interaction checks, with database integrity intact and no leaked processes. No implementation work remains. Publication uses the current branch and its authorized upstream.

The useful review lesson was to verify the actual phone geometry and state transitions beyond the initial green tests: full timestamps truncated titles, replacing an entire cached card lost errors/focus, and a missing selection with one remaining option needed an explicit escape from the disabled state. The integrated regressions now cover each behavior.

## Context and Orientation

`mj-controller/src/web/viewer.js` renders snapshots supplied by the controller and submits actions asynchronously. `viewer.html` and `viewer.css` beside it define the shell and appearance; `service-worker.js` caches those assets and requires a version bump when they change. `mj-controller/src/hel_server.rs` embeds assets and includes small JavaScript behavior checks. `tests/e2e/web/` contains Playwright browser tests with fake snapshot/action endpoints as well as a live reliability harness. `mj-tui/src/resume.rs` provides the session-first reference interaction. No persistence or server protocol change is needed.

## Plan of Work

First implement compact two-line rows with title, project/location, previous profile, and recency, ordered newest first with an ID tie break. Filter to the route workspace and search title, location/project, and profile locally. Include resume-capable sessions and retained failed/cancelled move recoveries. Never frontload errors or include generic error badges in this list. Distinguish an empty list from no search matches.

Selecting a row opens `#workspace/<workspace>/resume/<session>`, leaving `/resume` as the list route. Preserve search, scroll, and focus when returning during the visit. The selected card shows identity/status and labeled profile/target dropdowns, with a single available choice displayed as text. Prefer previous valid settings. If previous settings are unavailable and several replacements exist, require a choice. Ask about queued work only when it exists, retaining the default to run it. Keep drafts and DOM focus stable through unrelated snapshot updates and never silently replace invalidated selections.

An option disappearing during a visit clears its selection and requires explicit replacement, including when just one alternative remains. Lost and destroyed-with-data-loss sessions without retained recovery checkpoints explain their unavailability and offer no Resume action. An active session offers navigation to its conversation or dashboard status. Store request errors on the session draft so later status updates or navigation cannot lose a background failure.

Move persistent recovery cards out of `renderLaunchFailures`, retaining only dismissible launch-failure notices. The selected card shows safe error status and existing failed-move actions, reusing the move route, retained source mounts/resources, pinned destination, and queue-admission restrictions. Keep unavailable sessions discoverable with explanatory details. Do not promise a checkpoint on the strength of `has_error` alone.

Submission captures the session, workspace and choices, disables duplicate submission, preserves the card on errors and returns to the originating dashboard on success only if the user has not left that card. Vanished or newly active sessions replace stale controls with explanatory navigation. Direct card links must validate workspace membership. Increment the service worker cache version.

## Milestones

The first milestone delivers the new browser interaction and recovery placement in the four web asset files. A phone-sized browser should show several sessions regardless of configured profile count and no profile controls before selection. Delegate that bounded implementation to Luna while the primary agent prepares independent tests and updates the embedded checks.

The second milestone proves browsing, navigation, live-update stability, action behavior and recovery safety through Playwright. The primary agent reviews actual changes, resolves integration issues, runs required checks and commits only task files on the existing branch.

## Concrete Steps

From the repository root, run `node --check mj-controller/src/web/viewer.js`, `cargo test` with elevated permissions, `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. Cargo output remains in the normal configured build storage. From `tests/e2e/web`, run `npm run test:unit` and `MJ_BROWSER_SPEC='@(resume|new-session|compact-cards|project-groups).spec.js' npx playwright test`. Adapt the live reliability test to select a row before resuming and run its established harness when available. Review actual browser geometry at 390 by 844 and a desktop viewport.

## Validation and Acceptance

Browser coverage must demonstrate multiple visible two-line rows with many profiles and long titles without horizontal overflow; current-workspace filtering; search and recency; direct-link membership; keyboard/touch selection; and Back restoring browsing state. Prove persistent selections and focus on snapshot updates, unavailable options requiring replacement, queue payloads, duplicate prevention, request failures preserving drafts, and delayed responses not redirecting another page. Verify generic error details are absent from dashboard/list and present only in the selected card. Verify move retry links, retained source settings, and pinned-queue restrictions. Replace the obsolete global-workspace embedded test and update existing resume interactions. Required suites should finish without failures; record exact results below.

## Idempotence and Recovery

All changes are source and test changes with no data migration. Tests use isolated browser fixtures where possible. Retrying checks is safe. Do not change branches or include unrelated files in the commit. After validation and commit, push to the current branch's upstream as explicitly authorized. Errors from submission remain visible and never trigger automatic retries.

## Artifacts and Notes

Validation evidence will be recorded here after integration.

The fixture matrix command passed 34 tests in 31.8 seconds. The final Resume suite passed 17 tests in 6.5 seconds; an additional targeted run verified the pinned-move Retry action opens the existing move route. `npm run test:unit` passed all 15 checks. `cargo clippy --all-targets -- -D warnings` finished successfully in 47.23 seconds, and `cargo test` passed against the final assets with results in `target/resume-cargo-test-final.log`. Visual inspection used `/tmp/hel2-resume-populated.png` and `/tmp/hel2-resume-detail.png`; test and build logs are under the ignored `target/` directory.

The live harness initially expected a removed startup workspace dialog, old palette footer, and removed live-session badges/visible Stop buttons. Updated its readiness and interaction assertions to the current dashboard, palette footer, Open session accessibility label, and session action menu. These are harness repairs required to reach the real Resume flow, not runtime workarounds.

Final live command: `tests/e2e/run-browser-reliability.sh --seed 20260907 target/debug/mj`. Result: `passed clients=2 sse_reconnect=1 leaks=0`. Its real browser scenario passed in 25.9 seconds and the 22-check layout matrix passed in 14.3 seconds. Artifacts reside in `target/reliability-artifacts/browser-tui-convergence-seed-20260907-3565902/`; integrity output is `integrity_check=[('ok',)]` and `foreign_key_check=[]`. Syntax checks, Python AST parsing, `cargo fmt --all -- --check`, and `git diff --check` passed.

## Interfaces and Dependencies

Keep `/api/actions` resume request fields and existing move preparation/confirmation interfaces unchanged. The only added public interface is the selected-session hash route. Reuse existing timestamp, route, DOM and request helpers rather than new dependencies. The public snapshot remains the source for capabilities and compatible targets; raw stored errors stay private.

Revision note: Created from the accepted conversational plan on 2026-09-07 before implementation.

Revision note: Recorded subsequent authorization to push the completed, validated changes to upstream.

Revision note: Recorded integration findings, browser evidence, explicit replacement behavior, and per-session error retention after primary-agent review.

Revision note: Recorded final unit/browser/Cargo validation and repaired stale live-harness expectations.

Revision note: Recorded successful real stop/resume/reconnect, layout checks, database integrity, and bounded cleanup; implementation is ready for its required commit and authorized push.
