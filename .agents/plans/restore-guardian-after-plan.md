# Restore execution policy after Claude planning

This plan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Leaving Plan mode must restore a Claude session's configured Guardian or YOLO execution policy. Operators must also be able to list and change the permission mode with `mj set-config`, which currently exposes only model and effort despite the harness advertising other settings.

## Progress

- [x] (2026-09-30) Diagnose session cf1a32d7fcf1b32a5bbfbd247604a7a3: it started in auto, entered plan, and the Plan toggle explicitly selected default (Manual). API validation filters out mode.
- [x] (2026-09-30) Implement durable worker-owned execution-mode restoration and publish advertised settings.
- [x] (2026-09-30) First full Cargo suite passed, including API mode recovery and Guardian/YOLO transport regressions.
- [x] (2026-09-30) Final worker/transcript regressions report passing results, including idle admission, replay, and failure delivery.
- [x] (2026-09-30) User authorized terminating the two exact deadlocked rvector wrappers; SIGTERM released the locks and the final Cargo test command exited successfully.
- [x] (2026-09-30) Strict all-target clippy, rustfmt, and diff checks pass; both fixes are ready for the required current-branch commit and authorized upstream push.

## Surprises & Discoveries

Claude's default mode means Manual. Codex's default collaboration mode means ordinary work. Treating these values alike changed permissions when planning ended. The API also validates against the browser's model/effort-only projection rather than the harness's complete selector catalogue. Restoration failures must enter the existing configuration-result projection too; otherwise clients would wait for a success that never arrives. During validation the live session was observed back in auto with no pending forms; this work did not change it.

Final validation encountered an independent mbx deadlock in rvector. PID 2526545 held `/mnt/optane/mbx-cache/incremental/.locks/registrar.lock` and waited for a lease held by PID 2461430; PID 2461430 waited for the registrar. Both wrappers had no Cargo child. Our final test wrapper and clippy wrapper waited on that registrar too. The user authorized terminating the two exact wrappers under the host-build rule. After verifying their identities and lack of children, SIGTERM released the locks; the final test command exited successfully and clippy began. No cache files, build configuration, mounts, or live session state were changed.

## Decision Log

The worker's saved LaunchSpec execution policy decides the restored mode using the existing HarnessKind execution_enforcement helper. Clients send a semantic RestoreExecutionMode command for Claude Plan exit; they never guess permissions from a UI snapshot. Explicit set-config mode choices remain explicit, including Manual. The new relay command requires a protocol increment so older workers reject it before delivery and upgrade normally when idle.

## Outcomes & Retrospective

Both fixes are implemented and validated. The full Cargo test run, the final worker/transcript suites, strict all-target clippy, rustfmt, and diff checks passed. The independent build-cache deadlock was released with the user's authorization. The terminal and web share worker-owned restoration, and API validation now sees the harness's advertised settings. The release-installed binary will gain these changes when Mjolnir is updated; this task delivers a current-branch commit to origin/master under the user's explicit push instruction. No live store migration or session restart was performed.

## Context and Orientation

`mj-core/src/acp/surface.rs` selects Plan controls used by terminal and web clients. `mj-client/src/session.rs` waits for accepted control commands. The relay command and its durable projection live in `mj-core/src/relay/`. `mj-worker/src/acp/session.rs` executes controls against the harness and owns the saved execution policy. `mj-controller/src/server/config_view.rs` publishes settings used by API validation and the viewer. The daemon controls sessions, while workers retain harness state across daemon restarts.

## Plan of Work

First add a RestoreExecutionMode Plan control and relay command, requiring protocol 28. Claude Plan exit selects this semantic command; other harnesses retain their existing collaboration controls. Dispatch it only while idle. The worker applies and verifies the mode derived from its execution policy, then reports the confirmed mode as a normal configuration outcome. Client waiting uses that durable outcome, including rejection, before submitting implementation prompts.

Then expand the configuration projection to include every advertised select option while preserving canonical model and effort aliases and their projected current values. Add behavior regressions for Guardian and YOLO Plan exit, failed restoration, and API mode listing and changes.

## Milestone 1: Permission ownership and API recovery

The Plan toggle now selects RestoreExecutionMode for Claude exit. The worker resolves and acknowledges the saved execution policy, records the actual effective mode, and completes the existing durable configuration outcome. Both terminal and web clients use this same command. The settings projection now includes every advertised selector and preserves model/effort aliases. Transport and API tests prove the changes at their public boundaries.

## Milestone 2: Validation and publication

Run the full dev-profile Cargo tests and strict all-target clippy. Additional regressions exercise deferred restoration, reopening the relay journal, protocol refusal on older workers, and configuration failure delivery. Review the complete diff, commit only changed files on hel3, and push HEAD to its configured origin/master upstream. Completion means all checks passed and the pushed commit contains both fixes.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir3` on the current branch. Use the existing build cache configuration without redirecting Cargo output. Run `cargo fmt --all --check`, elevated `cargo test`, elevated `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. Stage only changed files, commit, and push the configured upstream. Any live test of a new binary uses `--instance guardian-plan-regression` or an existing isolated unit-test directory.

## Validation and Acceptance

Transport tests must show Claude entering Plan and exiting into auto under configured approvals and bypassPermissions under unconstrained execution, with the mode acknowledged before completion. A refused restoration must report failure and must not submit a follow-up prompt. API tests must list mode with the harness's current value and accept an advertised auto value while refusing unadvertised choices. Existing Codex collaboration-mode tests must continue to pass. Protocol coverage must reject the new command on protocol 27.

## Idempotence and Recovery

Relay command IDs retain the existing retry semantics: an accepted mutation is not replayed under a new identity. No database migration is introduced. Tests keep their existing isolated directories. Do not interrupt or upgrade the user's active session as part of testing.

## Artifacts and Notes

The session journal records auto at startup, plan at 04:41:43 UTC, and an explicit mode=default command at 04:42:59 UTC, followed by repeated tool-permission requests.

## Interfaces and Dependencies

Add unit variants RestoreExecutionMode to PlanControl, RelayCommand, and CommandRequest. Use RelayCommandOutcome::Configured and RuntimeEvent::ConfigApplied to retain the existing durable completion and error reporting. Resolve the mode through HarnessKind::execution_enforcement and verify it through enforce_execution_mode. Preserve existing subprocess helpers and build configuration.

Initial plan records the diagnosed causes and the worker ownership decision before implementation.

Implementation update: both fixes are written and the first full suite passes. Added durable failure-delivery, idle-admission and replay checks because semantic mode restoration must remain correct across client disconnects and worker reopening.

Validation update: final test suites report success. Clippy cannot start until the independent mbx registrar/lease deadlock is released; approval was requested under the user's host-build rule after identifying the two exact wrappers and confirming neither has a child Cargo process.

Recovery update: the user authorized terminating both identified wrappers. They exited after SIGTERM, the final Cargo test command exited with status 0, and clippy started through the unchanged mbx configuration.

Final validation update: full Cargo tests and final worker/transcript tests exited with status 0. `cargo clippy --all-targets -- -D warnings` exited with status 0, as did rustfmt and diff checks. Logs are `/tmp/mj-guardian-plan-test.log`, `/tmp/mj-guardian-plan-final-test.log`, and `/tmp/mj-guardian-plan-clippy.log`. The code and this final validation record are ready for the current-branch commit and authorized upstream push.
