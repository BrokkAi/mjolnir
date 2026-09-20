# Separate interruption, suspension, destruction, and pane dismissal

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Issue #1107 reports accepted graceful closes that appear idle indefinitely, followed by destructive escalation. People must be able to Interrupt turn without releasing an environment, Suspend session with a verified recovery copy for Resume, Destroy session explicitly, and Close pane without affecting execution. The CLI, terminal UI, and web UI must use these meanings consistently.

## Progress

- [x] 2026-09-20: Inspected lifecycle, CLI, HTTP API, terminal dialogs, browser actions, and existing failure reporting.
- [x] 2026-09-20: Implemented separate CLI/HTTP actions, renamed public lifecycle projections and configurable terminal commands, and advanced daemon protocol to 29.
- [x] 2026-09-20: Implemented terminal and browser confirmations, checkpoint-specific discard, explicit web Destroy for live and retained sessions, and persistent pending/failure presentation.
- [x] 2026-09-20: Persisted suspension intent before HTTP acceptance, supervised startup recovery, and passed isolated restart, missing-worker, stale-error, and child-failure tests.
- [x] 2026-09-20: Passed full dev-profile Rust tests, all terminal tests after the final label correction, Clippy, rustfmt, diff checks, Python harness parsing, and browser tests; reviewed and prepared the implementation for commit on the current branch.

## Surprises & Discoveries

The terminal currently has two commands for the same graceful operation. The CLI's `close --force` destroys recovery data, whereas the terminal's Force stop retains an older verified checkpoint. These must stay separate. `record_failed_close` refuses to replace an older internal error, while the live-session API only publishes specially composed lifecycle errors: the combination hides a new failure. The original reported restart trigger has not been reproduced.

HTTP acceptance previously preceded durable intent. Admission now writes the existing Closing state before acceptance; restart resumes that intent through the same supervised suspension path. Child failures name the affected child in the parent outcome. Current and older safe lifecycle failure prefixes remain readable without changing stored enum encodings or adding a migration.

Validation exposed narrow terminal confirmation buttons being clipped and retained-session web destruction navigating away too early. Confirmations stack buttons when needed, and retained history stays on its detail page to show destruction progress. The bundled agent command guide is validated against CLI subcommands and needed the same clean command rename.

## Decision Log

2026-09-20, user: Make a clean break with old commands rather than compatibility aliases; add Destroy to the web UI. Retain the terminal's older-checkpoint recovery action with explicit loss confirmation and checkpoint time.

2026-09-20, implementation: Preserve durable session/checkpoint serialization and worker relay terminology; translate public lifecycle presentation. No database upgrade is authorized or needed for terminology alone. Keep branch deletion an explicit choice and preserve the existing verified-checkpoint gate.

## Outcomes & Retrospective

CLI, TUI, and WUI now distinguish interruption, suspension, destruction, and pane dismissal. Suspension never automatically escalates to destruction. The terminal's checkpoint-discard choice carries the displayed checkpoint identity to the daemon, which rejects a changed copy before teardown; children are suspended safely first. Source inspection establishes and fixes reporting gaps, not the historical cause of all eighteen failures. Implementation and validation are complete. No release, push, live database migration, or destructive operation on a real session was performed.

## Context and Orientation

`mj-cli/src/api_commands.rs` implements script commands, backed by HTTP handlers in `mj-controller/src/server/api/turns.rs`. The long-lived daemon runs lifecycle work in `mj-controller/src/daemon/close.rs` and supervises outcomes in its lifecycle module. Controller lifecycle code checkpoints and verifies data before releasing an environment. Session state is durable database data; viewer operation state describes work currently in progress. Terminal commands are defined in `mj-tui/src/actions.rs` and executed by the CLI dashboard. The web viewer lives in `mj-controller/src/web/viewer.js` and uses authenticated API requests.

## Plan of Work

Milestone one separates `mj suspend`, `mj destroy`, and `mj interrupt-turn`, with corresponding HTTP endpoints, typed actions, and explicit admission responses. Remove old public commands rather than aliases, rename keybindings, and advance the daemon protocol when changing its messages. Preserve stored encodings and low-level relay Close/CancelTurn commands.

Milestone two consolidates the terminal command into Suspend session with confirmation, updates all visible lifecycle terminology, adds web Destroy through its dedicated endpoint, and shows destructive branch choices clearly. The terminal's Discard changes since checkpoint action requires a second confirmation including checkpoint age. Pending work is visible immediately and remains visible until authoritative completion; pane dismissal remains independent.

Milestone three fixes failure recording across the full suspension request, including child and pre-registration failures. Inspect durable admission and restart recovery; accepted suspension must either progress or report failure. Tests use isolated configuration/data and disposable resources, never the live store. Verify interruption leaves the environment usable, suspension retains resumable work, destruction is distinct, and failed checkpoints preserve resources.

## Concrete Steps

Work in `/home/jonathan/Projects/hel4`. Run `cargo fmt --all -- --check`, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings` in the dev profile. Run `npm test` in `tests/e2e/web`. Review `git diff --check` and the diff; explicitly stage only changed files and commit on the current branch without pushing.

## Validation and Acceptance

Add behavior tests for clean command/route boundaries, idle and active suspension, a stale internal error followed by checkpoint failure, parent/child failure, duplicate requests, and restart recovery. Test terminal command dispatch and the two destructive confirmations. Browser tests must exercise confirmation cancellation, request admission followed by delayed completion, persistent errors, Destroy, and viewer dismissal without lifecycle requests. Successful suspension must leave captured work available for Resume. Failure must keep the environment and prior recovery archive.

## Idempotence and Recovery

Retries join existing supervised operations or retry retained state. Never escalate suspension to destruction automatically. Existing teardown terminates processes before deleting files. Tests isolate configuration and data. Existing stored records remain readable; only explicit Destroy or confirmed checkpoint rollback may lose work.

## Artifacts and Notes

The complete browser command `npm test` passed 40 JavaScript unit tests and 86 deterministic browser tests, with 3 browser cases skipped. The targeted suspension, destruction, and resume browser run passed all 58 cases. Full `cargo test` passed. A final setup label correction was verified with `cargo test -p brokk-mj-tui --lib`: 683 passed and 2 ignored. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed; all modified Python harnesses parsed successfully. The broader Rust suite uses its existing ignored tests; live infrastructure lab scenarios were not run. Use default workspace members for Cargo validation: explicitly adding `--workspace` also builds the optional desktop crate, whose GTK development libraries are not installed here. Historical agent plans describe past behavior and are not bulk-rewritten as product documentation.

## Interfaces and Dependencies

The public verbs are suspend, destroy, and interrupt-turn. Destroy accepts delete_branch; Suspend never accepts a force flag. Both surfaces use the daemon's supervised lifecycle and shared state projections. The browser uses the dedicated authenticated Destroy endpoint rather than making destruction representable on the generic action endpoint. No new crate or external dependency is required.

Revision 2026-09-20: Created from the approved plan and explicit compatibility/recovery decisions.

Revision 2026-09-20: Recorded implemented behavior, durable admission and recovery findings, UI fixes discovered by tests, and browser validation evidence.

Revision 2026-09-20: Completed implementation review and final validation evidence; prepared the commit on branch `hel4`.
