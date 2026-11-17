# Report a sub-agent's login failure only when the login is refused, and deliver renewed logins to every session

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

Issue: GitHub #1581 (related: #1508, #1547, #1564).


## Purpose / Big Picture

A sub-agent ("child") is a Mjolnir session started by another session (its "parent") through the `spawn` tool. When a child's turn fails because the model provider rejected its login, the parent is told the child failed with failure kind `login_invalid` and the text "the login is no longer valid; run `mj login --profile <p>` and spawn again". That instruction is correct only when the profile's current login really is refused. On 2026-11-17 parents were told this seconds after the daemon had renewed the profile's login; the profile was healthy and nobody needed to log in.

After this change three things hold. First, a parent is told `login_invalid` only when the profile's current login was refused, as decided by one owner. A child whose turn failed on a login that has since been renewed is shown as still running while its retry is coming, or as an ordinary failed turn that `send_message` resumes (a wake gives it the current login), never as `login_invalid`. Second, when the daemon renews or pulls a fresher profile login, every live session that holds an older copy is guaranteed to receive the new one: a session that cannot be reached now is retried until it reports holding the new login or ends, and one slow session cannot delay the others. Third, a child whose worker will resubmit its failed turn once the new login arrives is held as running across transient states such as a checkpoint, but never held forever.

To see it working: run the tests named in `Validation and Acceptance`. The headline test replays the 2026-11-17 incident (a turn fails on the old token, a checkpoint barrier begins, the new login is installed during the barrier, the barrier is released, the retry is submitted) and asserts that the parent is never told the child failed.


## Progress

- [x] (2026-11-17) Diagnosis of the 2026-11-17 occurrence (two read-only children, reports under `.mj/agents/83a84352…/diagnosis.md` and `.mj/agents/91d33c3f…/credential-diagnosis.md` in the planning clone; summarized in `Surprises & Discoveries`).
- [x] (2026-11-17) Design reviewed with the senior advisor session; decisions recorded in `Decision Log`.
- [x] (2026-11-17) Milestone 1: classification. The refusal owner decides each accepted turn failure and typed wake failure. Child reporting and park eligibility share that verdict.
- [ ] Milestone 2: delivery. INFO logging of every per-session delivery deferral and conclusion (first commit), then per-session delivery owed until the worker reports the current login.
- [ ] Milestone 3: the hold. Worker publishes retry intent separately from its submit gate; the daemon holds a child only while the retry is owed and the login is coming or already present.
- [ ] Integrated review, merge, push; close #1581.


## Surprises & Discoveries

- Observation: the park did not cause the false report; a checkpoint barrier released the hold first.
  Evidence: child d5a964ac241ced5bc1b77d3f3d782002, daemon 401548 (2.71.0+e1159749), log `mj-daemon-20261117T145409.534Z-401548.log`. claude6 renewed 15:22:57.5Z. Turn failed 15:24:13.212 with `Authentication required`, fingerprint `c8c76b74…ac06f`. Ten "a child's turn failed on a login that was renewed since; its worker is waiting…" lines through 15:24:34.57. Child recovery-copy prestage ended 15:24:37.42, after which checkpoint capture holds a barrier. Parent outbox received typed `login_invalid` at 15:24:39.60. Park finished 15:24:49.11. "superseded by suspend" in that revision is printed for any cancellation; there was no user suspend.

- Observation: the renewed login never reached the child, and nothing logged why.
  Evidence: the child was woken 14:56:01 on remote host ubuntu@54.166.242.198 and staged the login current then. The 15:22:57 renewal pushed claude6 to 29 sessions (log lines 22850–23170, last 15:23:41); d5a964ac was not among them, although sessions on both sides of it in ID order were. No "synchronized a harness login" line for it in the whole log. A running, staged, remote session that is waiting on its own review is eligible at e1159749 (`mj-controller/src/pollers/worker_targets.rs:96–157`, `mj-controller/src/daemon/owner.rs:18–24,123–143,347–374`). The remaining silent or DEBUG-only skips cannot be told apart from history: the whole prepared owner snapshot fails revalidation when any session's record, lifecycle or config changed (`daemon/owner.rs:373–453`, `worker_client/credential_sync.rs:1095–1122,2611–2631`); the session's actor is missing or reconnecting; its exclusive relay connection is leased, which still defers credential jobs after #1564 (`recovery_gate.rs:129–156`, `session_manager/actor.rs:1104–1125`).

- Observation: the per-profile credential reconcile walks sessions serially, so one slow session delays every later one.
  Evidence: a claude6 pass was still on session 419e3471 (sorted before d5a964ac) at 15:24:29.9 (`credential_state` waiting), 15:24:37.9 (`skills_state`), 15:24:44.6 (`github_token_state`). The serial slot is in `worker_client/credential_sync.rs:449–483,754–777,2373–2428`.

- Observation: the "16:50 resume and new-start refusals" in #1581 have no producer in the store or logs; most likely a conflation with the same parent's 2026-11-16 10:06Z note, made under a daemon that predates #1547.

- Observation: the wake staging path can read the identity of the exact temporary login copy it installs, including the file copied from the Keychain for Claude. Local bare Codex links to the profile login instead of installing a fixed copy, so a failed wake through that link has no staged identity to attest. The failure is retryable.
  Evidence: `controller/worker_binary/launch.rs` and `controller/worker_binary/install.rs` distinguish staged copies from live links.


## Decision Log

- Decision: one owner decides whether a child's failure is a refused login; the status projection stops matching text.
  Rationale: four places decided it with three methods (registry token comparison, projection text match plus a live-worker hold, wake-failure text match, unpark-error text match). The text match is what reported this incident. AGENTS.md: one owner per fact; two predicates for one question are a bug even while they agree.
  Date/Author: 2026-11-17, planning session with advisor 91233b0e.

- Decision: `login_invalid` comes only from the credential refusal registry (`mj_core::credentials::RejectedLogins`), after the credential sync has reconciled that failure, for every harness. The projection does not run `classify_authentication_failure` itself.
  Rationale: token equality at one instant is also true just before a late renewal lands; the registry answers after reconciliation. `settle_renewed_login_failures` reads the profile's login file on each projection, a second reader on its own snapshot. "Renewed" (tokens differ) is monotone, so the credential owner can answer it any time. Non-Claude harnesses, which carry no token fingerprint, get the same rule through the registry's file-fingerprint refusal.
  Date/Author: 2026-11-17, advisor.

- Decision: an empty registry (no record of this failure, e.g. after a daemon restart) means an ordinary retryable failed turn, never `login_invalid` and never a hold.
  Rationale: "unknown" is not "refused"; and a hold must have an owner that ends it.
  Date/Author: 2026-11-17, advisor.

- Decision: land in three commits in the order classification, delivery, hold.
  Rationale: classification alone removes the false `login_invalid`. Logging must come early because the exact skip cause is unknown. The hold is safe only once delivery is guaranteed to conclude; under the old delivery this incident's child would have been held forever.
  Date/Author: 2026-11-17, advisor.

- Decision: delivery becomes per session (prepare S, validate S, push to S), level-triggered on "S's held login differs from the profile's current login", re-armed on the event that ends a deferral where one exists and with backoff otherwise. One serialized decision per profile remains for "what is the current login" (including pulling a fresher session copy); deliveries fan out from it, and a delivery that finds a fresher copy hands it back to the profile owner instead of deciding.
  Rationale: a re-armed obligation that still runs through a whole-snapshot revalidation can be voided again by any other session's churn, so its retry has no state change that ends it; behind a serial walk a hold lasts as long as the queue. Per-session units remove both. No new concurrency cap (AGENTS.md: no speculative caps).
  Date/Author: 2026-11-17, advisor.

- Decision: the hold requires the retry in both terms: the worker owes a retry AND (a delivery to it is owed and pending OR it already holds a login other than the rejected one). No retry owed means no hold.
  Rationale: every branch then ends: delivery concludes, or the worker submits, or the retry is withdrawn.
  Date/Author: 2026-11-17, advisor.

- Decision: the worker's retry intent is split from its submit gate term by term. Transient terms that clear by themselves (checkpoint barrier, command or tool in flight, a present driver, `!can_submit` for in-flight work) only gate the submit. Terms that wait on a person (assessment questions, paused goal, budget-limited goal) or mean the session is ending (close barrier) withdraw the intent, so the child is not shown as running.
  Rationale: the incident's hold dropped on a checkpoint barrier; a person-blocked child must not look busy.
  Date/Author: 2026-11-17, advisor.

- Decision: new relay snapshot fields for new meanings; no second reader of the old `authentication_retry_waiting`.
  Rationale: widening the old field's meaning silently changes what old daemons read; keeping a reader of it keeps two predicates. A child on an old worker is a retryable failure until that idle worker upgrades.
  Date/Author: 2026-11-17, advisor.

- Decision: the daemon does not wake a parked child to run its pending retry.
  Rationale: the worker's retry intent is durable in its journal; any wake resumes it (the worker reads the staged login at dispatch start), and a parent prompt correctly supersedes it. After this change a parked child with a pending retry is a leftover case not worth a resumable daemon obligation.
  Date/Author: 2026-11-17, advisor.

- Decision: a failed wake (unpark) is stored as a typed failure carrying the fingerprint of the login the daemon staged for it, not as a `login_invalid:` string; it is classified inside the credential owner by comparing that fingerprint with the current login. Old text-only rows read as ordinary wake failures.
  Rationale: the daemon staged that token itself, so equality with the current token is sound evidence of refusal there.
  Date/Author: 2026-11-17, advisor.


- Decision: keep wake detail in `last_error` and append typed evidence only when the failed boot staged an identifiable login. Current readers separate the evidence from the detail. Older readers still see the original failure at the start of the same text column. The write that releases the boot intent persists both together, with no schema migration.
  Rationale: helps distinguish old text from identity evidence without changing how old daemons update or clear session errors.
  Date/Author: 2026-11-17, Milestone 1 implementation.

- Decision: retain auth signals by session and event ordinal, with the failed fingerprint checked when one is present. Accept a trigger under the same registry lock used to conclude it, and retire old records when the daemon observes a newer completed turn or removes the session.
  Rationale: two turns can fail on the same token, and later warning events can follow a failed turn. Neither can use that turn's pending decision or conclusion.
  Date/Author: 2026-11-17, Milestone 1 implementation.

## Outcomes & Retrospective

Milestone 1: the parent receives `login_invalid` only for a refusal of the current login. A missing or inconclusive reconciliation leaves the child promptable, and a pending reconciliation keeps it unfinished. The live worker retry publication still holds a child whose login was renewed. The isolated incident test passes with Milestone 1 and fails on `15183b431`, where the durable mailbox contains `login_invalid` and an instruction to sign in again. Targeted controller and core tests passed (164 passed, one existing AWS CLI fixture ignored), as did clippy and formatting checks for both crates.


## Context and Orientation

Mjolnir runs coding agents ("harnesses" such as Claude Code or Codex) in sessions. The daemon (`mj daemon-run`, crate `mj-controller`) is the control plane: it owns the SQLite store, session records and lifecycle operations. Each session has a worker process (crate `mj-worker`), which runs the harness and keeps a durable relay journal of the session's state; the worker publishes an "operational state" snapshot (`mj_core::relay::snapshot`, struct field list near `mj-core/src/relay/snapshot.rs:762`) that the daemon reads through the session's actor (`mj-controller/src/session_manager/`). A "profile" is a configured login for a harness (e.g. `claude6`); its current login lives in a file on the daemon host (the "marker"). Each session holds its own copy of the login. The "credential sync" (`mj-controller/src/pollers/credential_sync.rs`, executor in `mj-controller/src/worker_client/credential_sync.rs`, target selection in `mj-controller/src/pollers/worker_targets.rs` and `mj-controller/src/daemon/owner.rs`) reconciles those copies: it pushes a fresher profile login to sessions ("Pushed") and pulls a fresher session copy into the profile ("Pulled"). A sync pass may carry a "cause" (`mj_core::credentials::CredentialSyncCause`): a session whose turn just failed authentication, with that turn's access-token hash (`login_fingerprint`).

A Claude login's "fingerprint" is a hash of its access token (`mj_core::credentials::claude_access_token_fingerprint`). A failed turn's diagnostic (`mj_core::diagnostic::TurnDiagnostic`) records the fingerprint of the token the turn used. `mj_core::credentials::classify_authentication_failure(failed, current)` returns `LoginRenewed` when they differ, `CurrentLoginRefused` when equal, `Unknown` when either is missing (`mj-core/src/credentials.rs:680–712`).

The "refusal registry" is `mj_core::credentials::RejectedLogins` (`mj-core/src/credentials.rs` around 1060–1170). It observes finished sync passes (`observe`) and records a profile as refused when a pass caused by an authentication failure reached the failing session cleanly, pushed and pulled nothing, and (for Claude) the failed token is still the current one. `refusal(profile_id)` re-checks against the current marker and returns the `login_invalid` text. It is used to refuse spawns (`mj-controller/src/server_runtime/api.rs:626,4033`).

How a child's status reaches its parent: `mj-controller/src/server_runtime/api.rs` reads each child's progress from the store into `ChildProgress`, then `project_child_failure` (~line 3000) sets `failed_turn`, `login_failure` and `provider_failure` from a `ChildFailureSource`: either `Wake(message)` (a parked child's `sessions.last_error`, `child_failure_source` ~2977) or `Turn(turn)` (the last materialized turn outcome). `login_failure` is set by text match: `mj_core::credentials::text_reports_auth_failure` for wakes, `mj_core::subagent::turn_failed_on_login` (`mj-core/src/subagent.rs:1378`) for turns. Then `settle_renewed_login_failures` (api.rs ~1760) clears `login_failure` and sets `turn_pending` only while the child's live worker publishes `operational.authentication_retry_waiting` equal to the failed fingerprint and the profile's current token differs. The state/output projection (~2760–2810) turns `login_failure` into state `failed` with the `login_invalid_reason` text, and the agent entry (~3105–3135) adds `"failure": {"kind": "login_invalid", "profile_id": …}`. `mj-core/src/mailbox.rs:612` renders that kind in the parent's mailbox. Parking (`mj-controller/src/daemon/delegation.rs` ~700–765) parks a finished child only after `reconcile_child_report` returns `Told`, i.e. after the parent was told.

A failed wake: `mj-controller/src/controller/subagent_park.rs` `unpark_subagent_worker` (~211) stages the profile's current login into the parked child's home (#1547, ~258–269), starts the worker, and on a proven-dead worker builds `"login_invalid: login refused for profile … run `mj login` …"` by text match (~335–343) and stores it with `crate::database::fail_subagent_unpark` (`mj-controller/src/database/worker_restart.rs:166`) into `sessions.last_error`.

The worker's retry: `mj-worker/src/relay/commands.rs` `authentication_retry_identity` (~1669) returns `(terminal ordinal, rejected fingerprint)` when the last completed turn failed authentication and recovery is still admissible; `submit_authentication_retry` (~1721) resubmits once the worker's known login differs. `mj-worker/src/worker_runtime/unix/dispatch.rs` `recover_authentication` (~577) runs the check; a successful credential install wakes it. The worker's known login is `ClaudeLoginIdentity`, updated in `mj-worker/src/worker_runtime/unix/requests.rs` `apply_install_credentials` (~305–340) only when the installed snapshot's fingerprint equals the pushed bytes'. The relay snapshot field `authentication_retry_waiting` is set in `mj-worker/src/relay.rs` ~958.


## Plan of Work

### Milestone 1: classification

At the end of this milestone the parent is told `login_invalid` only when the registry says the profile's current login is refused; the false report of the incident cannot occur. A child whose failure is still being reconciled is shown as running; one whose login was renewed, or whose failure the registry has no conclusion for, is shown as an ordinary failed turn with an honest reason and `promptable` true.

In `mj-core/src/credentials.rs`, extend the refusal owner so it answers per failure, not only per profile. It must know three things about a failure identified by (session id, failed fingerprint or, for harnesses without one, the failed turn's terminal ordinal): that a sync caused by it is pending (recorded when the trigger is accepted by the credential sync poller, so the poller in `mj-controller/src/pollers/credential_sync.rs` must tell the owner when it accepts an `AuthenticationFailure` cause), and, once the pass finishes, its conclusion: refused (exactly the existing `observe` rule), delivered (the pass pushed or pulled for that session), renewed (the failed token is no longer current), or not reached / failed (the pass finished without a clean check of that session; keep the reason). Keep the profile-level `refusal(profile_id)` for spawn refusal. Expose one function that the daemon's child projection calls, returning an enum such as:

    pub enum LoginFailureVerdict { Deciding, Refused, Renewed, Unrefused { reason: String } }

where `Unrefused` covers "concluded without a refusal" and "no record" (e.g. after a daemon restart). Whether the failed token was renewed is answered inside this owner (it may read the marker, as `refusal` already does); remove the marker read from `settle_renewed_login_failures`. Records must be bounded by session lifetime (drop a session's records when it is destroyed or when its failure is superseded by a newer turn); do not add a size cap.

In `mj-controller/src/server_runtime/api.rs`, compute one child state once per child and have every consumer read it: the state/output projection, the agent entry's `failure`, `reconcile_child_report` (and through it park eligibility). Suggested shape:

    enum ChildState { Deciding, Refused { profile_id }, RetryOwed, RetryableFailure { reason }, Failed { state, reason, provider_failure } , ... }

(plus the existing running/completed cases). `Deciding` and `RetryOwed` are not finished (shown as `running`, not reported, not parked). `Refused` is the only producer of failure kind `login_invalid`. `RetryableFailure` is state `failed` with a reason that names the profile and says the login was renewed after the failure (or that the failure was not shown to be a refused login) and that `send_message` continues the child with the current login; no `failure.kind` of `login_invalid`. In this milestone `RetryOwed` is the existing live-worker hold (worker publishes `authentication_retry_waiting` equal to the failed fingerprint AND the verdict is `Renewed`); Milestone 3 replaces its inputs. Replace the three passes that set `login_failure` and then clear it (`project_child_failure` login text matches, `settle_renewed_login_failures`) with this single computation. `turn_failed_on_login` may still decide that a turn failed on authentication (it is the trigger), but it no longer decides `login_invalid`.

Typed wake failure: in `mj-controller/src/controller/subagent_park.rs`, record the fingerprint of the login `unpark_subagent_worker` staged (Claude; for other harnesses the staged file fingerprint) and, when the dead worker's error reports an auth failure, store a typed failure (kind, staged fingerprint, detail) instead of the `login_invalid:` string. Choose storage that older daemons tolerate: either a new nullable column added by a compatible migration (classify it, with the reason, per AGENTS.md; bump the revision, not the minimum compatible revision) or a structured encoding in `last_error` that old readers still display sensibly. `child_failure_source` reads it; the credential owner classifies it (staged fingerprint equals current → `Refused`; else `Renewed`). Old text-only rows become ordinary wake failures (`RetryableFailure`/`Failed` with their text), never `login_invalid`.

Notice text: wherever the daemon reports an auth failure to a user (the "Auth failure on profile …" notice), say whether the profile is refused or the session held a stale login, using the same verdict.

Tests for Milestone 1 (Rust, colocated `#[cfg(test)]`, AGENTS.md test rules; mark with `// Hard-won: #1581: …` where they pin this defect):

- A Claude child whose last turn failed on fingerprint F, profile's current token differs, worker publishing nothing (or a checkpoint barrier making it publish nothing) projects `failed` without `failure.kind == "login_invalid"`, `promptable` true, and its reason names the renewed login. This is the incident's report path.
- A failure whose sync is pending projects `running` and is not reported/parkable; the same failure after the registry concludes refused projects `login_invalid`.
- After a daemon restart (empty registry) the same child projects a retryable failure, not `login_invalid`.

### Milestone 2: delivery

First commit: logging. In the credential sync, every per-session deferral and conclusion for a session whose sync carries an `AuthenticationFailure` cause, and every session skipped by a renewal or pull fan-out, is logged at INFO with the session id, profile, the reason (snapshot stale, actor missing/reconnecting, relay connection leased, push failed, already holds the current login, target not eligible and why), and the cause's ordinal when there is one. This commit lands on its own so a recurrence names its cause.

Then the delivery change. The worker publishes the fingerprint of the login it holds (Claude: its `ClaudeLoginIdentity`; other harnesses: the credential file fingerprint from its credential state) in a new operational-state field. In `mj-worker/src/worker_runtime/unix/requests.rs` `apply_install_credentials`, the published identity must be what the file holds after every install: record the read-back value, or report the mismatch as an install failure; never leave the identity unchanged after a write that differed from the pushed bytes (under level-triggered delivery that case would be pushed forever).

In the daemon, restructure the credential sync so the unit of delivery is the session. Keep one serialized decision per profile that determines the profile's current login, including pulling a fresher session copy into the profile. Fan deliveries out from that value: for each live session of the profile whose held fingerprint differs, the sync owes it the current login. Preparing, validating and pushing are per session, so one session's record, lifecycle or config change re-prepares only that session and never voids the others; sessions are reconciled independently, so a slow relay call on one does not delay another. Do not add a concurrency cap; if part of the serial slot exists for a documented reason (a single marker read, a keychain access), keep only that part serialized and say why in a comment. A delivery that finds the session holds a fresher login than the profile hands it back to the profile owner instead of deciding. An obligation ends when the session reports holding the current login, or the session ends (closed, destroyed, parked; a wake stages the current login per #1547), or a terminal failure (push error that is not a deferral, target gone). A deferral (actor missing or reconnecting, relay connection leased, record changed mid-delivery) keeps the obligation and re-arms on the event that ends it when the owner can signal it (lease released, actor reconnected, record saved), with backoff only where it cannot. Expose to the refusal owner whether a delivery to a session is pending and how it concluded; Milestone 3's hold reads it, and Milestone 1's `Deciding` should include "a delivery for this failure's session is pending".

Tests for Milestone 2: a renewal fan-out where one session's record changes mid-pass still delivers to every other session; a session whose relay connection is leased during the renewal receives the login when the lease is released; a slow session does not delay delivery to another session of the same profile (drive it with a fake worker that blocks; assert ordering, not timing); an install whose read-back differs from the pushed bytes publishes the read-back fingerprint and is not pushed again in a loop.

### Milestone 3: the hold

In `mj-worker/src/relay/commands.rs`, split `authentication_retry_identity` into the durable intent ("the last completed turn failed authentication, with fingerprint F, and still owns a retry": no newer prompt accepted, no later harness turn, continuation not withdrawn at or after the failed prompt's acceptance, no assessment questions, goal not paused or budget-limited, no close barrier) and the submit gate (checkpoint barrier, `can_submit`, driver present). `submit_authentication_retry` requires both. Publish in a new operational-state field the intent's rejected fingerprint and whether the worker already holds a different login. Stop setting `authentication_retry_waiting` (leave the field present and `None` for old daemons' decoding, and do not read it in the new daemon).

In the daemon, `RetryOwed` becomes: the worker publishes the intent with fingerprint F AND (a delivery to that session is owed and pending OR the worker holds a login other than F). If the intent is published but the delivery concluded without delivering and the worker still holds F, the child is a `RetryableFailure` whose reason is the delivery's conclusion. No intent (including an old worker that publishes only the old field, or no worker view after a daemon restart before reattach — treat "no view" as unknown: keep `Deciding` only while a delivery is pending, else `RetryableFailure`) means no hold.

Tests for Milestone 3, all `// Hard-won: #1581: …`: the incident sequence end to end at the worker/daemon boundary that the existing tests use (fail on old token; checkpoint barrier begins; new login installed during the barrier; barrier released; retry submitted; the parent is never told); a pending checkpoint barrier keeps the intent published and blocks the submit until it clears (extend `mj-worker/src/relay/authentication_tests.rs`); the stuck case (intent published, delivery concluded without delivering, worker still on F) projects a retryable failure, not running; assessment questions or a paused goal withdraw the intent (child not shown as running).


## Concrete Steps

Implementers work in detached worktrees so builds do not collide, for example:

    cd /home/jonathan/Projects/mjolnir/.mj/clones/57d56ff63cc5de32b0e6e0e25653e816
    git worktree add --detach ~/Projects/mjolnir-wt/1581-m1 HEAD

Commit on the detached HEAD; do not push; the planning session cherry-picks and requests review. Run `cargo test` outside the restricted sandbox. Run targeted tests only, e.g.:

    cargo test -p brokk-mj-core --lib credentials
    cargo test -p brokk-mj-controller --lib -- server_runtime::api subagent_park credential_sync
    cargo test -p brokk-mj-worker --lib -- relay::authentication_tests
    cargo clippy -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker --all-targets -- -D warnings

Relay protocol or schema revision numbers are assigned at merge time; if a milestone needs one, take the next free number and expect the planning session to renumber.


## Validation and Acceptance

Milestone 1 is accepted when the Milestone 1 tests pass and fail on the parent commit (`65db49967` or later master), and no code path outside the refusal owner produces failure kind `login_invalid` (`grep -rn login_invalid_reason mj-controller/src` shows only the child-state `Refused` arm and the spawn refusal). Milestone 2 is accepted when its tests pass and a run of an isolated instance (`--instance t1581`) with two Claude sessions on one profile shows, after replacing the profile's login file, an INFO "synchronized a harness login" (or the new per-session line) for both sessions. Milestone 3 is accepted when the incident test passes and the stuck-case test shows the child reported, not held. Stop the test instance afterwards (`mj --instance t1581 daemon stop`).


## Idempotence and Recovery

All steps are code changes in worktrees; rerunning tests is safe. A schema migration, if Milestone 1 adds one, is additive and compatible; never rewrite it after it lands. If a milestone's review stalls, earlier milestones remain independently correct and shippable.


## Artifacts and Notes

Diagnosis reports (planning clone): `.mj/agents/83a843526c1f475b63ad2f3d6e7c028b/diagnosis.md`, `daemon-401548-excerpts.txt`; `.mj/agents/91d33c3fd3057969f2280abcd3085672/credential-diagnosis.md`, `daemon-evidence.txt`.


## Interfaces and Dependencies

No new crates or libraries. New public surface: in `mj_core::credentials`, the per-failure verdict API on the refusal owner (Milestone 1) and delivery-state queries (Milestone 2); in `mj_core::relay::snapshot` operational state, a held-login fingerprint field (Milestone 2) and an authentication-retry-intent field (Milestone 3), both `Option`, both defaulting to `None` so older peers decode them. The daemon API `list_agents` / wait output keeps its JSON shape; only which children carry `failure.kind == "login_invalid"` changes.
