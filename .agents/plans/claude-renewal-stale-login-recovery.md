# Sessions recover from a Claude login renewal wherever they are

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir runs Claude Code sessions on shared Claude subscription logins ("profiles", such as `claude6`). Each profile's login lives in a native home such as `~/.claude6/.credentials.json`. Every session gets its own copy of that file, with the refresh token removed, so that only a daemon ever refreshes the login (that design came from #1490: when every copy held the single-use refresh token, the copies spent it against each other). About every four hours one daemon renews the login by running Claude Code's `claude auth login`, which writes a new access token into the profile's file, and the daemons then push the new copy to their sessions.

An Anthropic login renewal revokes the previous access token immediately. So from the moment of renewal until a session's copy is replaced, that session holds a dead token, and any API call it makes fails with an authentication error. Since v2.71.0 (#1515) a top-level session recovers by itself: the worker records the access-token hash of the failed turn, and when a copy with a different token is pushed, the relay resubmits the turn once. On 2026-11-16 that worked for every top-level session that received a push. It did not help five sessions, all sub-agents ("children"), because no push ever reached them, and a bookkeeping error marked the freshly renewed `claude6` login as refused, steering new sub-agents away from the profile until the daemon restarted.

After this change, a session that fails because its login was renewed under it recovers by itself regardless of whether it was running, starting, resuming or a sub-agent, and a login is only ever marked refused when the token that actually failed is still the profile's current token. To see it working: renew a profile's login while a sub-agent is about to make its first call after resuming; the sub-agent's turn fails once, receives the current login within seconds, and continues without the parent being told it failed.


## Progress

- [x] (2026-11-16) Diagnose the 2026-11-16 claude6 failures (`.mj/agents/059f87984f8f3a681191fe3d4955c8b0/DIAGNOSIS.md`, including the per-session follow-up table).
- [x] (2026-11-16) Map the credential staging, push, resume, retry and refusal code (`.mj/agents/6384c8ef03aabe83128b0b6af7b1b0d5/MAP.md`).
- [x] (2026-11-16) Write this plan.
- [x] (2026-11-16) Milestone 1: retained-environment repair (a dead worker) and sub-agent unpark restage the profile with the new `WorkerFilePreparation::RefreshProfileOnly` before the worker starts (`resume/repair.rs`, `subagent_park.rs`, `worker_binary/launch.rs`).
- [x] (2026-11-16) Milestone 2: `CredentialSyncSignal`/`CredentialSyncCause` carry the failed token hash; `classify_authentication_failure` is the one decision; `RejectedLogins` keys Claude refusals by access-token hash; authentication signals skip the cooldown.
- [x] (2026-11-16) Milestone 3: push targets are every pollable non-disconnected session; `settle_renewed_login_failures` keeps a child that failed on a renewed login unfinished.
- [x] (2026-11-16) Milestone 4 except review/commit: three tests added, core and controller suites and workspace clippy run (3 controller failures exist on unmodified master), isolated live check passed for a top-level session. Child live check covered the parked-then-woken child only.


## Surprises & Discoveries

- Observation: the five sessions that did not recover on 2026-11-16 were all children. Three were EC2 children resumed after a renewal (their retained environment restarted with the copy they had when suspended); one was suspended seconds after failing; one failed while its worker was restarting and was not a push target. Children report a `login_invalid` failure to their parent and are parked within seconds.
  Evidence: DIAGNOSIS.md follow-up table; push targets are only sessions in Running or Checkpointing (`mj-controller/src/pollers/worker_targets.rs:96-110`); `mj-controller/src/controller/resume/repair.rs` contains no credential handling.
- Observation: the false refusal came from a failure signal held by a five-minute per-session cooldown (`mj-controller/src/pollers/credential_sync.rs:6`, 34-84). When it was finally processed, the session already held the renewed copy, and `RejectedLogins::observe` (`mj-core/src/credentials.rs` around 988-1049) compared whole-file fingerprints and recorded the renewed login as refused. `RejectedLogins::refusal()` is consulted when listing sub-agent candidates (`mj-controller/src/server_runtime/api.rs:566`, :3538), so new sub-agents went to other profiles.
- Observation: linking session login files to one shared file (as local Codex sessions do) is unsafe for Claude: Claude Code writes per-session data (`mcpOAuth`, `trustedDeviceToken`) into the same file, and a server-controlled flag switches it to a reader that opens the file with `O_NOFOLLOW` and treats a symlink as no login. See MAP.md point 4.


## Decision Log

- Decision (implementation, 2026-11-16): non-Claude harnesses keep the whole-file refusal rule. Only Claude workers report an access-token hash, so for Codex and others a hash-less cause cannot be classified; their refusal is still recorded from the sync's own snapshot, which is now taken the moment the failure is reported (no cooldown). Claude with no hash records nothing.
- Decision (implementation, 2026-11-16): the push targets cover sessions the daemon polls (`session_target_is_pollable`) except `Disconnected`. A starting, resuming or parked session has no connection to push to; its start path stages the current login (Milestone 1) and an authentication failure on a renewed login is handled at once.
- Decision (implementation, 2026-11-16): `RefreshProfileOnly` is a mode of the existing staging function (profile restaged over a retained home, binary untouched), not a second staging path. Repair uses it only when it starts a worker itself (`Missing`, or a `Live` worker whose preparation failed) and not for `Installed` (just staged) or a reserved swap (the worker was receiving pushes). No persisted format changed: the signal is in-memory only.

- Decision: Keep copies plus push; do not link Claude login files.
  Rationale: see the third observation above. The push design plus #1515's retry already recovers running top-level sessions; the defects are in the paths that a push cannot reach and in refusal bookkeeping.
  Date/Author: 2026-11-16 / parent session.
- Decision: One question decides what an authentication failure means: "is the access token that failed still the profile's current access token?" The daemon answers it by reading the profile's current login at the moment it handles the failure (not from a cached copy and not after a delay), comparing access-token hashes in the same hashing domain the worker already uses for the failed turn (`diagnostic.login_fingerprint`; see `mj-worker/src/acp/session.rs:978,1143` and the existing correct use in `mj-controller/src/daemon/delegation/policy.rs:437-451`). If it is no longer current, the login was renewed: push the current copy to that session immediately. If it is still current, the provider refused the current login: record the refusal for exactly that token. Every consumer (push, refusal, child outcome) uses this one decision.
  Rationale: today two predicates answer the same question on different snapshots (a delayed whole-file fingerprint comparison versus the failed turn's token hash), which is how a renewed login was marked refused. AGENTS.md: one owner decides each fact.
  Date/Author: 2026-11-16 / parent session.
- Decision: Authentication-failure signals are handled at once, without the per-session cooldown. Repeated signals for the same token are idempotent under the decision above (a push only happens when the profile's token differs from the session's; a refusal is recorded once per token). If the cooldown also guards other signal kinds, keep it only for those, with a comment saying what it guards.
  Rationale: the cooldown delayed the decision until it ran on a newer snapshot; AGENTS.md forbids speculative caps.
  Date/Author: 2026-11-16 / parent session.
- Decision: Every path that starts a session's worker installs the profile's current login copy before the worker's first prompt: new sessions, restores, sub-agent starts, unparking, and repair of a retained environment (`mj-controller/src/controller/resume.rs:1611`, `mj-controller/src/controller/resume/repair.rs:26`, fix point near repair.rs:124). Use the same staging function the other start paths already use (find it; do not write a second one).
  Rationale: repair restarted EC2 workers with the copy they had at suspend; one staging path makes "starts with the current login" true by construction.
  Date/Author: 2026-11-16 / parent session.
- Decision: The push poller targets every session whose worker is connected, whatever its lifecycle state (including resuming and checkpointing), instead of only Running or Checkpointing. A parked session has no worker and is covered by the start path above.
  Rationale: a renewal between staging and the Running transition otherwise leaves a stale copy with nothing to replace it.
  Date/Author: 2026-11-16 / parent session.
- Decision: A child whose turn ends with an authentication failure on a token that is no longer the profile's current token is not reported to its parent as failed and is not parked; the daemon pushes the current copy and the existing #1515 resubmission continues the turn. Only a failure on the current token (a real refusal) is reported as `login_invalid`. This decision is made at the single place the daemon classifies a finished child turn's outcome (start from `mj-controller/src/server_runtime/api.rs:2330-2345`, 2660-2667) using the decision above, before the parent is notified.
  Rationale: children were the sessions that never recovered; the parent respawning a fresh child loses the child's work and costs a turn.
  Date/Author: 2026-11-16 / parent session.


## Outcomes & Retrospective

Implemented in the working tree, not committed. Live check (`.mj/agents/ea19a5a238a7f01b61afd272d799e343/live_check.py`, real pinned Claude Code against a loopback fake API in a private network namespace) showed: a renewal under a running session produced one push and a retried turn with the new token, no refusal; a failure on the current token logged exactly one refusal across two failures. A child that had parked holding the old login and was then prompted after a renewal made its first API call with the new token (restaged on unpark, no 401). Not verified live: a still-running child that fails on a stale token (unit-tested only) and refusal clearing on renewal.


## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-controller`) owns session records and pushes credentials; one worker process per session (crate `mj-worker`) runs Claude Code through its ACP bridge and keeps running across daemon restarts. Several daemons (different `--instance`s) may share one profile home; `mj-core/src/credentials/locking.rs` owns the native lock that serializes refresh and sync across them.

Renewal: `mj-controller/src/claude_usage.rs:841` runs `claude auth login` under the native lock; the lock drops when that child exits (~:885) and the daemon rereads the file (~:801). The `refreshed_by_this_daemon=false` branch (~:680) is a daemon that found another daemon already renewed. Push: `mj-controller/src/pollers/credential_sync.rs` (the 60-second sync pass, the auth-failure signal handling with its cooldown at :6 and 34-84, fingerprints at 2635-2668) and `mj-controller/src/worker_client/credential_sync.rs`. Push target selection: `mj-controller/src/pollers/worker_targets.rs:96-110` (and :144 skips linked Codex logins; keep that). Copies omit the refresh token via `HarnessKind::copies_omit_refresh_token` and `credential_copy_for_harness` (`mj-core/src/credentials.rs:279` onwards). The worker learns a new login only at startup (`mj-worker/src/.../dispatch.rs:145`) or from a daemon push (`requests.rs:304-340`); #1515's retry is `mj-worker/src/relay/commands.rs:1631-1699`. The failure signal type is `CredentialSyncSignal` (`mj-core/src/credentials.rs:661`), which today carries no token identity; `latest_credential_sync_signal` is in `mj-controller/src/.../state.rs:608`. Refusal bookkeeping: `RejectedLogins` in `mj-core/src/credentials.rs` (~988-1049). macOS keeps the host login in the Keychain (#1491); copies are files on every OS, and the daemon reads the current login through the profile's login store, so the decision must read through that same abstraction rather than the file directly.

Full detail with line references: `.mj/agents/6384c8ef03aabe83128b0b6af7b1b0d5/MAP.md` and `.mj/agents/059f87984f8f3a681191fe3d4955c8b0/DIAGNOSIS.md`. Never print token values in logs, tests or reports; use hashes.


## Plan of Work

Milestone 1. Find the function that stages a stripped login copy for new sessions, restores and sub-agent starts, and call it from retained-environment repair (resume.rs:1611 / repair.rs) and from unparking if unparking does not already go through it, before the worker's first prompt. Container, SSH and EC2 targets receive the copy through the same transport the other start paths use.

Milestone 2. Carry the failed turn's access-token hash in `CredentialSyncSignal` (and wherever the cause is recorded). Add one daemon-side decision function, for example `classify_authentication_failure(profile, failed_token_hash) -> StaleLogin | CurrentLoginRefused | Unknown`, that reads the profile's current login through the login store under the native lock and compares access-token hashes. `Unknown` (no hash, unreadable login) behaves as today's "nothing fresher" path but must never record a refusal. Handle authentication signals immediately: `StaleLogin` pushes the current copy to the session now; `CurrentLoginRefused` calls `RejectedLogins` with that token's identity, and `RejectedLogins` keys refusals by access-token hash so that a later, different token is never considered refused. Remove the whole-file fingerprint comparison from refusal decisions. Make the user-facing notice say what happened ("login was renewed; pushed the current login, the turn continues automatically" versus "the current login was refused; run `mj login --profile <p>`").

Milestone 3. Change push target selection to every session with a connected worker. At the place the daemon classifies a finished child turn, if the failure is an authentication failure and the decision says `StaleLogin`, push and leave the child running so #1515 resubmits; do not notify the parent or park. If the resubmitted turn fails again on the then-current token, it is a real refusal and is reported normally. Confirm in the worker that #1515's gate (`relay/commands.rs:1631`) fires for child turns; fix if not.

Milestone 4. Tests, per AGENTS.md "Which tests to add" (no unit tests of our own code on hand-built input). Required: (a) a tricky-condition test that a delayed authentication signal for an old token, processed after the session already holds a newer copy, records no refusal and leaves sub-agent candidates on that profile available (hard-won, mark `// Hard-won: <commit or #issue>: a delayed auth signal marked a renewed Claude login refused`); (b) a test that repair of a retained environment starts the worker with the profile's current login, not the retained copy (hard-won, the EC2 children); (c) a test that a child turn failing on a stale token is resubmitted after the push and its parent receives no failure. Prefer driving these through the existing controller test harness with a fake harness/worker, as the #1515 tests did (find them via `git show --stat ca2b5bf8b`). Run `tests/e2e/claude_stripped_credentials.py` only if Claude Code's handling of the copy changes (it should not).


## Concrete Steps

From the repository root, on the dev profile, outside the sandbox:

    cargo test -p brokk-mj-controller <filter>
    cargo test -p brokk-mj-core credentials
    cargo test -p brokk-mj-worker <filter>
    cargo clippy --all-targets -- -D warnings

Iterate with targeted filters; run the full suites of touched crates and workspace clippy once at the end. The controller test `review_host::tests::push_review_delivers_each_verdict_without_holding_or_prompting_the_primary` hangs on this host independently of this work; skip it with `-- --skip push_review_delivers_each_verdict_without_holding_or_prompting_the_primary` and say so.

Live check in an isolated instance only (`--instance <name>`, never the default instance, and never with a real profile's native home: use a fake or test harness login). Show a stale-token failure being pushed and resubmitted for a top-level session and for a child, and a refusal of the current token being recorded once.


## Validation and Acceptance

The three tests above fail on current master and pass after the change; all suites and clippy pass. In logs, an authentication failure after a renewal produces one push to that session within a second and a resubmitted turn, with no "refused" line; an authentication failure on the current token produces exactly one refusal for that token, and a later renewal clears it without a daemon restart.


## Idempotence and Recovery

All changes are in daemon/worker logic; no database migration is expected. If a schema or persisted-format change becomes necessary (for example, if `latest_credential_sync_signal` is persisted and gains the token hash), classify it under AGENTS.md's migration rules and record it in the Decision Log. Relay/worker protocol changes must stay compatible with older workers (an older worker sends no hash; treat that as `Unknown`).


## Artifacts and Notes

2026-11-16 false refusal, from the main daemon log:

    10:03:58 synchronized a harness login direction="pushed" profile_id=claude6 session_id=e1b593f5...
    10:08:25 the profile's login was refused and there is no fresher copy; sub-agent spawns on it are refused until it changes profile_id=claude6 session_id=e1b593f5...


## Interfaces and Dependencies

No new crates. `CredentialSyncSignal` gains an optional access-token hash. One daemon-side classification function as above is the only place that decides stale versus refused; `RejectedLogins` keys refusals by access-token hash.
