# Recover sessions after subscription quota resets

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

When a harness reports subscription exhaustion, Jev identifies the condition and deterministic code resumes already-authorized work one minute after the exhausted quota windows reset. The existing automatic-continuation setting controls recovery. Quota retries have a separate allowance, survive daemon restarts, and never override new user activity. Conversation notices explain the deadline or the absence of reliable reset data.

## Progress

- [x] (2026-09-21) Inspected continuation, relay admission, quota adapters, proxy contracts, and database compatibility.
- [x] (2026-09-21) Implemented versioned quota classification and deterministic reset selection/cache.
- [x] (2026-09-21) Implemented durable relay recovery and supervised controller scheduling.
- [x] (2026-09-21) Exposed recovery in conversation notices and control surfaces; updated setting explanation.
- [x] (2026-09-21) Validated isolated behavior, full default Cargo suite, Clippy, formatting, proxy tests/type checks, and deployment dry run; prepared the task-only commit on master.

## Surprises & Discoveries

The current reset parser drops explicit named timezones. The existing relay continuation state suppresses every non-success completion and caps ordinary continuations at three. Both must be accounted for without weakening ordinary continuation admission. Quota collection currently replaces successful reports with errors, so recovery needs its own durable last-success cache.

## Decision Log

On 2026-09-21 the user chose Jev classification for any harness, deterministic scheduling, the existing setting, separate retry allowance, waiting for all exhausted windows, durable restart recovery, cached reset data on refresh failure, and a terminal notice when no data exists. These supersede the original request's deterministic detection and earliest-reset wording.

The implementation retains existing exclusions for subagents, active goals, planning mode, and lifecycle operations. Hosted proxy publication is outside this implementation request; preserve v1 and prepare v2 locally.

## Outcomes & Retrospective

Implementation and validation are complete. The full default Cargo suite passed with no failures (26 ignored opt-in/platform tests across its result blocks). `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, proxy tests, TypeScript checking, and the Wrangler deployment dry run passed. All runtime tests used isolated storage under `/mnt/optane/mj-quota-recovery-validation/`; no production instance or live store was upgraded. Hosted v2 publication remains a separate step before distributing this build to hosted users.

The initial test runs exposed short command IDs in new fixtures and existing migration fixtures tied to revision 43. The corrected fixtures now exercise protocol 20, snapshot 11, and the breaking database revision 44. Explicit-turn wait behavior remains unchanged during ordinary classification. Cached-window merging is shared between persistence and recovery so fresh usage values always win while missing reset timestamps survive provider failures.

## Context and Orientation

`mj-controller/src/daemon/continuation.rs` supervises completed-turn classification and guarded prompt submission. `mj-core/src/continuation.rs` defines evidence and ordinary allowances. `mj-worker/src/relay/commands.rs` admits commands atomically against a relay cursor (the journal's ordinal and digest). `mj-core/src/relay/snapshot/apply.rs` folds durable journal events into state. `mj-controller/src/quota.rs` obtains profile quota windows. `services/jev-proxy/src/continuation.ts` validates hosted classifier requests and responses. Controller SQLite schema changes live in `mj-controller/src/database/schema.rs`.

## Plan of Work

First add the v2 continuation assessment, which keeps bounded current quota evidence independent of optional ordinary-continuation evidence. Jev returns a quota probability, never a reset timestamp. Code uses a 0.90 cutoff and gives quota recovery precedence. Preserve the v1 hosted contract.

Next implement reset parsing with named timezone support, last-success caching at the shared quota collection boundary, and selection of the latest reset among exhausted windows plus 60 seconds. If no exhausted window is identified, use the explicit current-message reset or earliest future reported reset. Failed refreshes retain cache data. No known deadline means one notice and no polling or speculative prompts.

Then add durable relay recovery state and guarded schedule/resume commands. The daemon restores timers after reconnect, checks the setting before submission, and uses idempotent command IDs. Cancellation, new input, context/profile/mode changes and closure invalidate recovery. Retries retain the original authorization and ordinary attempt count. Advance protocol/snapshot versions and add a breaking database revision for new stored command values and quota cache.

Finally expose deadlines in TUI/web/API and add conversation notices. Expand the existing setting explanation. Tests must prove deadlines, stale guards, duplicate prevention, cache preservation, restart recovery, and independent task progress.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Run `cargo fmt --all`, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings` in the dev profile. In `services/jev-proxy`, run `npm test` and `npm run check`. Use `--instance quota-recovery` for executable invocations and isolated `MJ_CONFIG_DIR`/`MJ_DATA_DIR`; never open the live store with this build. Stage only changed task files and commit on the current branch.

## Validation and Acceptance

Use fake classifiers/providers and controlled clocks. A quota-positive completed turn must show its reset deadline, issue no prompt before deadline plus one minute, then submit exactly one authorized continuation. Test weekly/five-hour/both, cached failures, timezone/DST/date boundaries, unknown resets, repeated quota outcomes, three exhausted ordinary attempts, restart, cancellation, changed state, setting disabled, and old workers. The existing ordinary continuation tests must continue to pass. Database migration tests must operate on temporary stores and verify older readers cannot accept new stored variants.

## Idempotence and Recovery

Stable command IDs and exact relay cursor checks prevent duplicate prompts. Retain pending recovery across daemon restart but revalidate current state. Never migrate the host's default store. Do not publish, push, change branches, or include unrelated untracked files.

## Artifacts and Notes

Initial working tree contains unrelated untracked `.agents/plans/fix-subagent-tool-results.md`, `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, and `mj.sqlite3`; leave them untouched.

## Interfaces and Dependencies

Add a versioned completion assessment carrying independent quota evidence, optional ordinary evidence, and a quota probability. Add a serializable `QuotaRecovery` record, relay schedule/resume commands guarded by completion and cursor, and an optional recovery field in operational/session projections. Reuse `QuotaManager`, `RecordNotice`, shared subprocess helpers, and the existing continuation prompt. Time parsing must use IANA timezone rules rather than controller-local assumptions.

Revision note (2026-09-21): Created from the accepted conversation plan before implementation.

Revision note (2026-09-21): Recorded implemented milestones and initial validation. The optional desktop crate requires unavailable GTK development libraries; required validation uses the repository default members.

Revision note (2026-09-21): Completed validation, recorded the fixture fixes and shared cache merge, and prepared the required commit. Logs are in `/mnt/optane/mj-quota-recovery-validation/cargo-test.log`, `clippy.log`, and `wrangler.log`. The deployment dry run used an explicit writable log path and did not publish.
