# Recover refused Claude logins immediately

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

A provider can refuse a Claude access token hours before its stated expiry. The daemon must attempt renewal immediately through its existing single refresher, publish actionable failures, and deliver successful renewals to workers. Workers already resume authentication-failed work when a different access token arrives (#1515).

## Progress

- [x] (2026-11-13) Claimed #1529 and traced refusal detection, expiry-only renewal, and existing durable worker recovery.
- [x] (2026-11-13) Wake renewal from failed-turn access-token identity and reread under native locks.
- [x] (2026-11-13) Publish renewal failures to the dashboard, sessions output, and doctor, clearing them when the login changes.
- [x] (2026-11-13) Add regression coverage for immediate renewal, concurrent refresh, out-of-order refusals, failure reporting, and Keychain lock rereads. Existing worker recovery tests will also be run.
- [x] (2026-11-13) Ran full client/controller/CLI/worker suites; all new regressions passed. Fixed baseline fixture failures in f13b27a2d and confirmed every failed case on focused reruns; the worker and CLI unit suites passed 834 and 251 tests respectively.
- [ ] Finish the final Clippy pass, commit the recovery change, obtain review, push to origin/master, and dispatch platform CI.

## Surprises & Discoveries

The related #1515 is already fixed: `mj-worker/src/relay/commands.rs` admits a durable `auth-retry-*` command only after the access token changes. Refusal detection in `mj-core/src/credentials.rs` retains the exact host snapshot, but only uses it to block sub-agent selection. The renewal poller never receives that evidence.

## Decision Log

- Decision: Reuse the per-profile renewal task and the worker's existing retry state machine.
  Rationale: A second refresher or continuation owner would create competing decisions and token races.
  Date/Author: 2026-11-13, Codex.
- Decision: Carry the failed turn’s access-token fingerprint into renewal and reread under the shared native lock before spending a refresh token.
  Rationale: Another daemon can refresh between refusal and renewal; a delayed refusal must never rotate the replacement login.
  Date/Author: 2026-11-13, Codex.

## Outcomes & Retrospective

Refused access tokens now wake the existing renewal owner immediately, and its locked reread prevents duplicate exchanges across daemons. Pending refusals are collected by token identity so delayed reports cannot replace newer failures. Successful renewal requests credential delivery, feeding the worker's existing durable authentication retry. Failed renewal survives successful quota probes in the published status and is visible in the dashboard, session API/CLI, and doctor. Disabled profiles with live sessions receive the same renewal and status handling. Testing used temporary credential homes and isolated instances; no installation or real login exchange was performed.

The full validation uncovered pre-existing fixture assumptions from the catalog and account-environment changes. Those were repaired in a separate commit. Two additional timeout cases passed alone; no timeout was relaxed. All regression cases for this change passed. Final review and publication remain.

## Context and Orientation

The daemon owns credential refresh. `mj-core/src/credentials.rs::RejectedLogins` observes completed credential synchronization to block new spawns. Recovery instead uses the access-token fingerprint already stored in the failed turn diagnostic; it cannot confuse a later replacement with the token used by that turn. `mj-controller/src/daemon/delegation/policy.rs` receives those results and publishes quota reports to clients. `mj-controller/src/pollers/quota.rs` runs independent renewal tasks for enabled profiles and disabled profiles with live sessions. `mj-controller/src/claude_usage.rs` holds native locks and daemon handoff admission while invoking managed Claude Code. The worker owns accepted prompts and authentication retries, which survive daemon replacement.

## Plan of Work

First, publish the failed turn’s existing access-token fingerprint from `Policy::observe` in the existing poller batch, and wake the existing task. Extend the renewal decision to consider that fingerprint alongside expiry, preserving its locked reread and handoff behavior. A changed snapshot needs delivery, not another forced refresh. Successful renewal requests credential synchronization immediately.

Second, keep renewal failures alongside the profile's quota publication so ordinary usage results cannot conceal them. Disabled profiles with live session copies retain error publications as well. Project the error through existing profile views, the authenticated `/profiles/login-errors` API, and session API rows; doctor reads that small API response. CLI diagnostics do not fetch the complete runtime projection. Clear it only when renewal observes a replacement or succeeds. Use actionable text naming `mj login --profile <id>`.

Third, extend existing integration tests using temporary credential homes and a local executable that implements Claude's refresh contract. Cover a seven-hour unexpired refused login, multiple daemons serialized by the native lock, no repeat refresh for stale evidence, and failures visible despite a later successful quota probe. Retain worker retry coverage already proving cancellation and newer user input take precedence.

## Concrete Steps

Work at the repository root. Preserve its mbx-managed `target` symlink. Keep the package selection fixed while running related checks: `cargo test -p brokk-mj-controller -p brokk-mj-client -p brokk-mjolnir -p brokk-mj-worker -- <filters>`, outside the restricted sandbox. Adding the worker selection changes the dependency feature graph, so keep it selected in later focused and full invocations. For one integration binary, keep those same four selections and add `--test daemon_startup` or `--test termination_pty`. Run the touched crates' full dev-profile suites and `cargo clippy --all-targets -- -D warnings` after the last implementation round, plus `cargo fmt --all -- --check`. New executable invocations use an isolated named instance or existing isolated test configuration.

## Validation and Acceptance

A confirmed refusal with seven hours remaining starts refresh immediately without waiting for expiry or a usage API hold. Two callers rejecting the same snapshot spend its refresh token once. A replacement written while waiting on the lock is adopted. A failed renewal reports a login command in dashboard and diagnostics and does not repeatedly rotate a replacement. Existing worker tests demonstrate that successful delivery resumes interrupted work once and respects user intervention. Tests must use temporary login files and never invoke a real login exchange.

## Idempotence and Recovery

Repeated refusal evidence is coalesced by a set of access-token fingerprints per profile, so an older report cannot replace a newer refusal. Native locks serialize renewal across daemon instances. Started token exchanges retain their existing non-cancellable ownership and finish before daemon handoff. Failed checks retain the existing recheck backoff. Configuration changes discard obsolete results by request identity.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1529. Related worker recovery: #1515. Initial workspace was clean on master at 7d7757d51.

## Interfaces and Dependencies

Reuse `CredentialSyncResult`, `QuotaRefreshBatch`, `QuotaUpdate`, `ClaudeGrantCheck`, `LoginSource`, `CredentialWriteGuard`, and existing quota/profile publication channels. Add no dependencies, schema migration, harness pin, or worker protocol.

Revision: initial plan records the confirmed expiry-only trigger defect and reuse of #1515.

Revision: use the failed turn’s existing access-token identity directly, avoiding a delayed sync mistaking a replacement login for the refused one and removing changes to core credential tracking.

Revision: pending refusal evidence is a set per profile so older session reports cannot overwrite a newer rejected token. A successful replacement clears only the acknowledged set; obsolete completions cannot erase newer failures.

Validation discovery: the full controller suite passed 2,609 cases and exposed four failures. Two timeout cases passed individually. The capacity-admission and hung-launch fixtures deterministically panic at `daemon/create.rs:458` because commit 5e70f76a1 made session creation require a serving profile catalog but did not initialize it in those fixtures. Add the same catalog initialization used by that commit's other admission tests, then rerun just these failures. All new refusal and API regressions passed. Workspace Clippy and formatting passed before these test-only fixture repairs.

CLI validation: all 834 worker and 251 CLI unit cases passed. Two startup integration fixtures inherited the agent session identity; #1524 consequently reconstructed the account environment and discarded their fake HOME and SessionWiki hooks. Clear inherited session markers in the shared test-command builder; tests that explicitly exercise session launches can still set their own markers afterward. Rerun just the two failing startup cases. This is test isolation, not a daemon behavior change.

Validation evidence: the new renewal, failure/replacement, Keychain reread, authenticated API/session projection, and CLI/doctor golden cases all passed. Existing `authentication_recovery_survives_restart_and_never_retries_the_same_login`, `authentication_recovery_yields_to_new_input_cancellation_and_checkpoints`, and `authentication_recovery_handles_both_orders_of_install_and_failure` all passed. Full suites reported 56 client unit tests, 2,609 controller cases before four focused repairs/reruns, 834 worker unit tests, and 251 CLI unit tests. Repaired reruns passed two catalog fixtures, two startup cases, and all twelve affected PTY cases (the other two PTY cases had already passed). Workspace Clippy and formatting passed; the final Clippy rerun includes fixture repairs.
