# Dialog action completion and terminal audit

This ExecPlan follows `.agents/PLANS.md` and extends the completed reusable component migration.

## Purpose / Big Picture

Successful Create, Resume, Save, and confirmation actions must close their initiating dialog. Pending validation must retain the draft, and validation failures must remain editable. The user reported that final New session Create performed its action but left the dialog open, and requested thorough tmux testing of migrated dialogs.

## Progress

- [x] Read wizard adapters and identify restoration after explicit close as a likely source.
- [x] Seed 503 reproduced a durable session being created while final review stayed open; rebuilt seed 504 verified dismissal.
- [x] Fix New/Resume ownership and retain the draft during mount/repository checks. Audit other migrated dialog lifecycles and repair background selection.
- [x] Expand live submissions, validation failures, cancellation, and persistence checks; seed 515 passed 69 recorded checks.
- [x] Run full Cargo tests, clippy, formatting, and final live acceptance.
- [x] Commit validated fixes on the current branch; push to the already-current upstream as the final delivery step.

## Context and Orientation

`mj-tui/src/component_events.rs` moves the active mode out before dispatch. `mj-tui/src/wizards/dashboard.rs` adapts shared control actions to New/Resume state transitions. Its activate helpers previously restored a cloned wizard when the mode became Dashboard, even though explicit cancellation and successful submission intentionally use that mode. `mj-tui/src/dialogs.rs`, `dialogs/container.rs`, `review_settings.rs`, `resume.rs`, and `palette.rs` own the other migrated dashboard lifecycle transitions. Chat components have an existing live submission workflow in `tests/e2e/tui_components_chat.py`.

## Plan of Work

First reproduce final Create in the existing isolated tmux harness, replacing API fixture creation with actual wizard navigation. Record both the durable session and the terminal state after submission. Fix wizard ownership explicitly: handlers that retain a draft must restore it themselves, while adapters must preserve deliberate close transitions. Add tests at event boundaries for successful launch, pending validation, failure, Cancel, and Back.

Independently inspect the other migrated dialogs for similar mode restoration or domain-action inference. Extend the real terminal harness to submit saves and confirmations, asserting persisted effects as well as disappearance of the modal title. Waiting for a background Sessions label is not sufficient because it is visible behind dialogs. Keep the existing keyboard/mouse, resize, asynchronous discovery, Help, and shutdown scenarios.

## Validation and Acceptance

From the repository root run elevated `cargo test`, then `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all -- --check`. Build matching host executables with `cargo build -p brokk-mjolnir -p brokk-mj-worker`. Run elevated `python3 tests/e2e/tui_components_tmux.py --seed 502` or a fresh seed for later runs. Evidence belongs under target/reliability-artifacts; short disposable runtime paths stay under /tmp to satisfy Unix socket limits. The pre-fix run must demonstrate an action occurring while the final review remains visible; the fixed run must show exactly one new session and no stale review dialog.

## Decision Log

Use existing real CLI/worker, fake ACP, and isolated tmux infrastructure. Only test-owned sessions and config are modified. Delegate wizard implementation and the independent dialog audit with disjoint file ownership; root owns terminal acceptance and integration. No UI-loop I/O or new runtime architecture is required.

## Surprises & Discoveries

The previous live suite exercised new-wizard cancellation but created its main session through the API. Several disappearance assertions waited only for background dashboard text. This follow-up must test actual submission and explicit modal absence.

The expanded live workflow also found inconsistent focus after clicking Target Next: the new-project field accepted fallback typing while the shared form still focused Next, so paste was ignored. Advancing now focuses content, and field steps no longer edit unfocused fields through the legacy fallback. Resume rendered Next disabled for bare targets because rendering incorrectly required an allocation that event handling did not require; both now use one eligibility method. Cancelled launch checks carry a generation so late replies cannot launch a session or dismiss a newer dialog. Resume keeps its draft between mount checking and repository checking.

## Outcomes & Retrospective

The final binary passed seed 515's 69 recorded tmux checks; see `.agents/docs/tui-components-live.md` for artifact location and SHA-256. Create now closes after creating exactly one session, Resume closes after preflight, and pending drafts remain editable. Unchanged config replies no longer dismiss a newly opened palette. The audit also repairs project-field focus, bare-target Next eligibility, and selection after background import-row removal. Full `cargo test`, `cargo clippy --all-targets -- -D warnings`, formatting, and Python compilation passed. Validation logs are `target/tui-dialog-audit-{tests,clippy,live-final}.log`. Commit and upstream push complete delivery.

## Idempotence and Recovery

Each tmux run owns a private server and disposable configuration. Stop owned processes before deleting runtime state, preserve failure captures, and never use personal provider sessions. Commit only owned files on the current branch. The preceding user authorization to merge and push the completed migration applies to these corrective changes after validation.

Revision: initial plan records the reported regression, the previous coverage gap, and the action-lifecycle acceptance criteria.
