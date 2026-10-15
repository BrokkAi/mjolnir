# Attach host directories during web session creation

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

The web new-session wizard will offer attached directories on its final Review screen, matching the terminal wizard. Users can add, edit, and remove host paths, choose a container destination and read-only, copy-on-write, or read-write access, and start the session with those mounts already present.

## Progress

- [x] (2026-10-15) Confirmed the terminal edits attachments on Review and the web creation action currently discards them.
- [x] (2026-10-15) Carry mounts through the creation action and cancellable background preflight.
- [x] (2026-10-15) Add a shared web mount editor and expose it on creation Review.
- [x] (2026-10-15) Extend browser goldens and HTTP boundary coverage, verify both goldens, pass all web unit checks and the focused HTTP tests.
- [x] (2026-10-15) Full controller suite: 2000 passed, 12 ignored; workspace clippy with warnings denied passed.
- [ ] Commit on the current master branch and push upstream as requested.

## Surprises & Discoveries

The controller Cargo package is `brokk-mj-controller` (its directory is `mj-controller`). All 30 creation browser checks pass, including extended goldens. Source-sliced JavaScript tests need to load the new shared serialization helper explicitly.

The archive-compatible mount JSON uses `read_only: true` for read-only and `read_only: false` for copy-on-write; read-write additionally carries `access: "rw"`. The existing Container settings dialog already handles that representation. Normal Cargo resolves to the installed mbx wrapper; preserve its build storage. Sandboxed file reads stall, while escalated reads complete immediately.

## Decision Log

- Decision: Put attachments inside Review, without another top-level wizard step.
  Rationale: This matches the terminal and the user's explicit request.
  Date/Author: 2026-10-15, Codex.
- Decision: Reuse the mount editor between creation and Container settings and keep disk/SSH validation in the existing supervised preflight task.
  Rationale: One editor preserves the mount wire representation and filesystem work must stay off UI and daemon event loops.
  Date/Author: 2026-10-15, Codex.

## Outcomes & Retrospective

The Review editor, creation mount propagation, HTTP validation, documentation, and existing-session settings regression coverage are implemented. The requested live session is running and idle with its verified read-only /repos bind mount. All 30 initial creation browser checks passed; both updated goldens passed without rewriting, the final keyboard-submit golden passed, the web unit suite passed, and the two new HTTP boundary checks passed. The full controller suite passed (2000 passed, 12 ignored), and the final workspace lint pass passed with warnings denied. The user then requested a separate correction so Restart recreates containers when their mounts change; retain that work as a separate validated checkpoint before the final push.

## Context and Orientation

`mj-controller/src/web/viewer.js` owns browser creation state (`freshDraft`), Review rendering (`renderNewForm`), project checking (`preflightNew`), and launch (`commitNew`). Its `openContainerSettings` function already edits mounts after creation. `mj-controller/src/server/actions.rs` defines the JSON action contract and in-process preflight messages. `server/handlers.rs` validates browser requests; `server_runtime/run.rs` supervises cancellable background checks; `server_runtime/actions.rs` converts New into the daemon's `CreateSessionRequest`. `Controller::validate_mount_source` checks directories on the target's actual host. Bare targets use a selected project directory and do not accept mounts. Container targets offer mounts on Review. Tests live in `tests/e2e/web/new-session.spec.js` and colocated Rust modules.

## Plan of Work

First add an optional-on-the-wire empty-by-default `additional_mounts` list to New and to new-session preflight requests. Validate path shape and target compatibility at the HTTP boundary; check directory existence on the supervised task before repository checks. Carry the selected list into `CreateSessionRequest` and update in-process constructors without changing other API behavior.

Then extract the existing mount controls into a reusable editor that keeps unfinished input in draft state and emits the established JSON format. Render that editor on container creation Review. Adding, editing, or removing an attachment invalidates the old preflight and starts a new cancellable check, so a stale response cannot enable Start for another selection. Keep selections associated with the selected target, and do not send them for bare targets. Preserve edit inputs across snapshot renders.

Finally extend the existing browser golden case to show mounts on Review and their launch request, including editing, removal, default destinations, access modes, target switching, and checking/error states. Extend HTTP contract coverage for request propagation and refusal of unsupported target/path combinations. Review all changed files, commit only task files, and push the current branch.

## Concrete Steps

Run from the repository root, with no Cargo output relocation:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

Run Cargo tests outside the restricted sandbox. From `tests/e2e/web`, run browser tests outside the sandbox with the existing installed Chromium:

    PLAYWRIGHT_BROWSERS_PATH=$HOME/.cache/ms-playwright npx playwright test new-session.spec.js --project deterministic
    node --test *.unit.test.mjs

The deterministic browser tests intercept HTTP and never contact the live default instance. Rust tests retain their existing isolated data directories. Update goldens only after inspecting the resulting user-facing output.

## Validation and Acceptance

On container Review, entering a host directory and adding it shows its container destination and access mode. Editing and removing an entry update both the displayed list and subsequent preflight/launch JSON. Start stays disabled while the current selection is checking. An invalid host directory reports an actionable failure and starts no session. Switching to a bare target shows no attachments and sends none. Existing Container settings continues to save the same wire representation. Controller tests, browser tests, formatting, and workspace clippy must pass before commit and push.

## Idempotence and Recovery

The implementation adds no migrations, and its automated tests do not touch live session state. The separately authorized live repair of session d8443f17cb24506c5ee26277fcf319f4 is complete: suspend/resume applied its existing saved mount at /repos. Repeat deterministic tests freely. Preserve mbx configuration and target directories. If upstream moved, fetch and merge origin/master, resolve conflicts, validate affected code, and push again; do not rebase or change branches.

## Artifacts and Notes

Initial evidence: `CreateSessionRequest.additional_mounts` in the web New arm was always `Vec::new()`. The existing editor states that settings apply only when the container is next created; creation Review will avoid needing a restart.

## Interfaces and Dependencies

Use the existing `mj_core::targets::AdditionalMount`, shared mount-path validation, `Controller::validate_mount_source`, `CancellableProcessExecutor`, and supervised preflight job set. No dependency or database changes are needed. Omitted mounts continue to deserialize as an empty list for older clients.

Revision note (2026-10-15): Created the plan after locating terminal parity, creation request ownership, and test coverage.

Revision note (2026-10-15): Prioritized and completed the requested live-session repair. Its saved mount was already correct at /repos, but Restart retained the old container. Suspend/resume recreated it and Podman confirmed the read-only bind mount. Update the settings dialog to describe the working apply procedure.

Revision note (2026-10-15): Implemented the editor, request propagation, cancellable checks, and HTTP coverage. The 30-test creation browser suite passed; controller compilation and remaining validation continue.

Revision note (2026-10-15): Browser layout validation caught content-box input overflow on phones; scoped border-box sizing fixed it. Enter in mount fields now adds/saves an attachment rather than launching the session. Final browser goldens and all web unit checks pass.

Revision note (2026-10-15): Creation work is fully validated. Commit it as a coherent checkpoint; complete the newly requested Restart correction before pushing both changes.
