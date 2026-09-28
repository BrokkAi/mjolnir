# Remove the durability layer from the concurrency sweep and keep its single-owner fixes

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Commit 0a98bc4f ("Give daemon and worker work explicit durable owners", 2026-09-27) set out to prevent race conditions by giving each resource one mutator. It shipped, in releases 2.23.2 and 2.23.3, as a 140-file change that also added an exactly-once delivery layer: worker command receipts and cancellation tombstones (relay protocol 26), a serialized review state machine in the database, five breaking schema migrations (59 to 63), and a git-pinned unreleased Anvil revision. The owners of this repository decided that the single-mutator fixes are wanted, that startup prompts and sub-agent delegation deserve a small durable record because a program retries them, and that everything else should return to "the daemon restarted; the next turn covers it".

After this plan is complete, a reader can verify the following. A daemon restart during a turn review cancels the review and posts the notice "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes", and the next review covers both turns. A daemon restart between queuing a session's first prompt and its delivery still delivers that prompt exactly once. A sub-agent `spawn` tool call interrupted by a daemon restart still produces exactly one child. The database opens at schema revision 64 with every session, transcript and checkpoint intact and with the tables `session_incarnations` and column `turn_review_state.orchestration` gone. `cargo install brokk-mj-controller` from crates.io compiles, because the workspace depends on a published `brokk-anvil-client`. A worker still running the 2.23.3 build keeps working until the daemon replaces it at idle.

## Progress

- [x] (2026-09-28) Audit of 0a98bc4f completed in conversation; per-area findings recorded under Context and Orientation and Surprises & Discoveries.
- [x] (2026-09-28) Design decisions taken by Jonathan; see Decision Log.
- [ ] Milestone 1 (2026-09-28: superseded the vendor-service review-and-fix work, see Decision Log. Anvil branch `kimi-provider-hook` holds two reverts, the hook commit e11c890 and the 0.28.6 bump ba73dd1; format, Clippy and 394 client tests green; generated license files match CI. Mjolnir: adapter, restored quota refresher and lock tests, and lease-handoff removal are in the working tree and compile and pass focused tests against the local Anvil checkout. Remaining: full Mjolnir suite; commit Mjolnir source against the existing git pin, which also provides the hook; merge Anvil to master and push tag v0.28.6, which publishes to crates.io; then set `anvil-client` in `Cargo.toml` to registry version 0.28.6.)
- [ ] Milestone 2: worker relay keeps a bounded ring of terminal command IDs; remove protocol 26 receipts, tombstones, ledger export and seal; remove the reviewer generation file and on-disk lane dispatches.
- [ ] Milestone 3: controller stops using protocol 26; remove the durable submit path from `session_manager`, `worker_client`, and `mj-client`.
- [ ] Milestone 4: review host returns to clear-and-notify on daemon start; remove the durable checkpoint, outbox and receipts.
- [ ] Milestone 5: delegation keeps `delegation_effects` as an idempotency record only; remove the incarnation fence and receipt cleanup loop.
- [ ] Milestone 6: startup queue simplified in place.
- [ ] Milestone 7: migration 64 and compatibility with 2.23.3 workers and archives.
- [ ] Milestone 8: full validation on an isolated instance and a migrated copy of a real store.
- [ ] Follow-ups: GitHub ticket for moving the review driver into the worker (filed 2026-09-28); decide whether to reopen issue 1146; decide whether to yank the unbuildable 2.23.3 crate or supersede it.

## Surprises & Discoveries

- Observation: the crate `brokk-mj-controller` 2.23.3 on crates.io does not compile from the registry.
  Evidence: its `Cargo.toml` declares registry `brokk-anvil-client` 0.28.5; the published 0.28.5 crate contains neither `build_with_token_provider` nor `KimiService` (checked by downloading both crates on 2026-09-28).
- Observation: the store's schema compatibility floor was raised to 63 by the shipped releases, so an older binary refuses to open it. Migrations can only be added, never removed.
  Evidence: `mj-controller/src/database/schema.rs`, `minimum_compatible_version` updates in migrations 59 to 63, and the open-time check that refuses `state.revision > SCHEMA_VERSION`.
- Observation: the worker's journal garbage collection prunes a handled command's ID once its terminal ordinal is at or below the acknowledged frontier (`prunable_command_ids` in `mj-worker/src/relay/journal.rs` before the commit). This is the only reason the receipt protocol was invented. A duplicate submit whose ID is still present already returns the original accepted ordinal and rejects a changed payload (`mj-worker/src/relay/commands.rs`, `handled_commands.get`).
- Observation: the worker's sub-agent socket answers a lost daemon with the "still running" placeholder, not an error, and keeps the request pending for the next daemon (`SOCKET_WAIT_CEILING` in `mj-worker/src/worker_runtime/subagents.rs`). A daemon that executed `spawn` and died before recording the result is therefore re-asked to spawn. That is the double-spawn path an idempotency record must close.
- Observation: the upgrade handoff gate in `mj-controller/src/upgrade.rs` is documented to wait only for short control operations and to cancel minutes-long work. Holding it for a review would contradict its contract.
- Observation: three later commits build on 0a98bc4f in the same files: 0cffddc6 and 55527484 (Move workspace and environment fixes, which reference the ledger seal) and 6f89e55f (secrets). A plain `git revert` conflicts with them and would also discard the wanted fixes.
- Observation: tables `startup_steps`, `delegation_effects` and `session_incarnations` have no `DELETE` anywhere in the tree; cancellation tombstones and reviewer generation nonces are never evicted and refuse new entries at 4096.
- Observation (2026-09-28, pre-publication review of Anvil 585ebf2 and 8248f52): the service design is sound where it matters. Same-process callers share one owner per home, cross-process callers serialize on an exclusive flock in `server/anvil-service.lock`, the pre-exec code is async-signal-safe, stdout and stderr go to `/dev/null`, and `server.token` is never logged. Four permanent-failure states need fixing before publishing: a non-2xx from `/api/v1/meta` is treated as an error instead of "not our service" and the profile's token is sent to whatever now owns a reused port (service.rs:544-556); a corrupt `anvil-service.json` fails every call (378-379); a pending launch marker whose PID was reused waits 30 s and fails every call (596-603); a hung vendor marked stopping times out every call (493-500). Services are never stopped in normal operation and accumulate one per home until reboot. The release drops Windows OAuth and changes the `credentials_path` contract, which is not a patch-level change. Mjolnir passes its managed executable, a lease path and extra environment into the service fingerprint (`mj-controller/src/kimi_auth.rs:47-60`); both Mjolnir callers (quota and utility inference) take that same path, so the fingerprint-flap hazard the review raised only applies if standalone Anvil is pointed at a Mjolnir profile home, which nothing does today.

## Decision Log

- Decision: keep the single-mutator fixes from 0a98bc4f. Concretely: the identity-checked attempt guards and atomic close in `mj-controller/src/recovery_gate.rs`; the single Move ownership map in `mj-controller/src/controller/move_session.rs`; `save_resumed_session` and the `record_recovery_*_if_current` writes in `mj-controller/src/database/sessions.rs`; computing the close route inside lifecycle admission in `mj-controller/src/daemon/close.rs`; the reviewer lifecycle states with cooperative cancellation and a retained `Stopping` state in `mj-worker/src/worker_runtime/reviewer.rs`; reviewer admission joining the primary worker's idle reservation (`mj-worker/src/relay/reviewer_admission.rs`); process-group and pipe supervision in `mj-worker/src/user_shell.rs` and `mj-worker/src/worker_runtime/unix/supervisor.rs`; the process birth-time PID check in `mj-controller/src/targets/worker_daemon.rs`; cancellable SSH connect in `mj-controller/src/worker_client/connect.rs`; the bounded per-stream request supervisor in `mj-controller/src/session_manager/channels.rs`; the deferred-submit cap in `session_manager/actor.rs`; the desktop external-link launcher in `mj-desktop/src/external_links.rs`; and the Kimi credential fix via Anvil.
  Rationale: each replaces two mutators with one, or a check-then-act with an admitted action, and none needs schema or protocol changes.
  Date/Author: 2026-09-28, Jonathan with Claude.

- Decision: do not `git revert` 0a98bc4f. Remove the durability layer per area, using `git checkout 0a98bc4f~1 -- <file>` for files the layer dominates and hand edits for mixed files.
  Rationale: the schema floor cannot move backwards, later commits conflict, and a revert discards the kept fixes.
  Date/Author: 2026-09-28, Jonathan with Claude.

- Decision: keep `startup_steps` and simplify it in place; keep `delegation_effects` as an idempotency record; keep `worker_restart_intents` unchanged; drop `session_incarnations` and `turn_review_state.orchestration`.
  Rationale: startup prompts and delegation are retried by programs, so a retry can double-execute; reviews and cancels are covered by the next turn or a human retry.
  Date/Author: 2026-09-28, Jonathan.

- Decision: accept that a daemon upgrade or crash cancels an in-flight turn review. Do not hold the upgrade gate for reviews. File a follow-up ticket to move the review driver into the worker sidecar, which makes a daemon restart a non-event for reviews the same way it already is for sub-agents.
  Rationale: the daemon is meant to be a control plane and its upgrade should be blocked by as little as possible; holding the gate for minutes contradicts that. The pre-commit behavior is lossless at the coverage level because the baseline does not advance.
  Date/Author: 2026-09-28, Jonathan.

- Decision: replace relay protocol 26 receipts with a bounded ring of retained terminal command IDs in the worker journal.
  Rationale: the pre-commit duplicate-submit path already returns the original outcome; the only gap was garbage collection pruning the ID. The tombstone case (a stale buffered submit landing after a cancel) and carrying the ledger across a worker Move are accepted losses; the pre-commit code had neither.
  Date/Author: 2026-09-28, Jonathan with Claude.

- Decision: review the two Anvil commits before publishing 0.28.6, and fix what that review requires first.
  Rationale: crates.io publication is irreversible and nobody had read the Anvil side.
  Date/Author: 2026-09-28, Jonathan.

- Decision: drop the Anvil vendor-service approach to the Kimi credential race entirely, including the four review fixes, and use the small fix instead: one refresher per profile inside the daemon, shared by quota and inference through Anvil's token-provider hook.
  Rationale: before the sweep, Mjolnir's quota poller already refreshed correctly (take the vendor's lock, re-read, refresh only if still stale). The second writer was Anvil's inference provider, which guarded refresh with an in-process mutex only. Handing Anvil the poller's provider removes that writer with about twenty lines in Anvil and forty in Mjolnir, keeps Windows OAuth, and needs no subprocess, registry, or unsafe code. The service approach traded a copy of the vendor's lock protocol (failure mode: a forced re-login if stale-lock reclamation ever diverges from the CLI's) for a resident `kimi web` process per profile with 1,450 lines of lifetime management. Anvil branch `kimi-provider-hook` reverts the service commits, keeps `KimiBackendConfig::build_with_token_provider` and the once-only 401 retry through `BearerTokenProvider::rejected_bearer_token`, and bumps to 0.28.6; branch `kimi-service-0.29.0` is kept only as a record. In Mjolnir, `mj-controller/src/kimi_auth.rs` is now an adapter over `quota::ensure_fresh_kimi_token`; the pre-sweep `quota.rs` and its lock tests are restored; the harness lease handoff (`controller/worker_binary/harness.rs`, the worker's `--runtime-info`/`--runtime-ack` transfer, `PreparedHarnessInfo`) is removed because only the service needed a managed `kimi` binary in the daemon. Credentials are read from `credentials/kimi-code.json` as before the sweep; the vendor's hashed per-endpoint credential file names are not replicated.
  Date/Author: 2026-09-28, Jonathan.

- Decision: tolerate 2.23.3 workers and archives during the compatibility window rather than adding a version gate for every removed feature. The controller must ignore `command_ledger` in archives and must not send protocol 26 requests. A worker sealed mid-Move at upgrade time may need a manual worker restart; document that.
  Rationale: the daemon already replaces mismatched workers at idle, and there are two users.
  Date/Author: 2026-09-28, Jonathan.

## Outcomes & Retrospective

Not started. This section will record, per milestone, what was removed, what tests were deleted with it, and the final test counts.

## Context and Orientation

Mjolnir is a Rust workspace. The daemon (`mj-controller`, entry in `mj-controller/src/daemon/`) is the one process that owns the SQLite database and coordinates every session; it is meant to be a control plane. A worker (`mj-worker`) is a separate process, one per session, that runs the coding agent ("harness") and owns a durable relay journal: an append-only log of commands and events under the worker's directory, from which the daemon projects transcripts. The daemon talks to a worker over a Unix socket using the relay protocol defined in `mj-core/src/relay/protocol.rs`; each request carries a minimum protocol version and workers report the version they speak. A checkpoint is an archive of a session captured while the worker is idle; a Move restores a checkpoint into a replacement worker, possibly on another machine. A schema migration is one numbered step in `mj-controller/src/database/schema.rs::migrate_schema`; the store records its revision and the minimum executable revision that may open it, and shipped migrations are never rewritten. The upgrade gate in `mj-controller/src/upgrade.rs` decides when a replacement daemon may take over.

A turn review is a second-opinion review of a coding agent's turn. The reviewer roles are harness processes in the worker's reviewer sidecar (`mj-worker/src/worker_runtime/reviewer.rs`), each with its own journal. The decisions about what to do next are made by `TurnReviewDriver` in `mj-review/src/driver.rs`, a pure state machine hosted in daemon memory by `mj-controller/src/review_host/`. Before 0a98bc4f, `turn_review_state.active` marked a running review and the daemon cleared it at startup, releasing held prompts and posting a notice. 0a98bc4f instead serialized the whole driver, including each role's transcript, into the new `orchestration` column and re-ran it from an outbox after restart.

The sub-agent facility lets a parent agent call `spawn`, `wait`, `send_input`, `interrupt` and `close` through an MCP server in the worker (`mj-worker/src/subagent_mcp.rs`). The worker forwards each request over a socket to the daemon (`mj-worker/src/worker_runtime/subagents.rs`) and the daemon executes it (`mj-controller/src/server_runtime/api.rs`, `execute_subagent_tool_durable`, and `mj-controller/src/daemon/delegation.rs`). 0a98bc4f added `delegation_effects` (migration 60), which records a request's chosen target and result keyed by parent session and request ID, plus `session_incarnations` (migration 63), a random identity per session that SQL triggers rotate on resume so a delayed `close` cannot stop a resumed child.

Startup delivery is how a new session's first prompt, hand-off text and configuration steps reach the worker once it is ready. 0a98bc4f moved this from an in-memory queue to the `startup_steps` table (migration 59), drained by one supervised task per session in `mj-controller/src/daemon/state.rs`, with API-driven follow-ups queued through `mj-controller/src/daemon/startup_followup.rs`. Each row carries a stable `command_id` that is also the worker command ID.

Protocol 26 added to `RelayRequest` and `ReviewerRequest`: `SubmitDurable`, `CommandReceipt`, `ReleaseCommandReceipt`, `CancelCommandAdmission`, `CheckpointCommandLedger`, `ReadLaneDispatches` and `AckLaneDispatches`. The worker retains a receipt per durable command until released, keeps a set of cancellation tombstones, exports both as a ledger during a checkpoint, and seals the journal while a Move is in flight. Callers are `mj-controller/src/worker_client/relay.rs`, `session_manager/handle.rs`, `session_manager/actor.rs`, and `review_host/durable.rs`.

The Anvil dependency: `Cargo.toml` pins `brokk-anvil-client` to git revision 8248f521c0f99b456b4c48353b7e4993675e137e of https://github.com/BrokkAi/anvil.git, sibling checkout at `/home/jonathan/Projects/anvil`. The two relevant Anvil commits are 585ebf2 and 8248f52 on its master; its `crates/anvil-client/Cargo.toml` still says 0.28.5, which is also the last published version.

Per-area findings from the audit, for orientation: the review host slice was +2233/-499 with a new `durable.rs` (428 lines) and `observations.rs` (100 lines); the worker slice was +3379/-469 with about 1,900 lines of tests; the API and session manager slice rewrote `api.rs` (+850 net) and removed the in-memory start map; the daemon and database slice added `startup.rs` (238), `delegation.rs` (227) and `worker_restart.rs` (106) under `mj-controller/src/database/`.

## Plan of Work

Milestone 1, Anvil. Read commits 585ebf2 and 8248f52 in `/home/jonathan/Projects/anvil` with the review questions in the Concrete Steps. Fix whatever that review marks as required before publication, bump `crates/anvil-client/Cargo.toml` to 0.28.6, run Anvil's tests and Clippy under its own AGENTS.md rules, commit, tag, and publish. Then in this workspace change the `anvil-client` line in `Cargo.toml` to `{ package = "brokk-anvil-client", version = "0.28.6" }`, run `cargo update -p brokk-anvil-client`, and confirm `Cargo.lock` shows a registry source. Publication is the one irreversible step in this plan; do it only after the review is clean and Jonathan has said go.

Milestone 2, worker. In `mj-worker/src/relay/journal.rs`, change the pruning of `handled_commands` so the newest `RETAINED_TERMINAL_COMMANDS` terminal entries (a constant of 512) survive garbage collection regardless of the acknowledged frontier; older ones are pruned as before. Add a test that a terminal command's ID still answers a duplicate submit after the journal has been acknowledged past it, and that the 513th oldest is gone. Then remove from `mj-core/src/relay/protocol.rs`, `mj-core/src/relay/snapshot.rs`, `mj-core/src/relay/snapshot/apply.rs`, `mj-worker/src/relay/requests.rs`, `mj-worker/src/relay/commands.rs` and `mj-worker/src/relay.rs` the `SubmitDurable`, `CommandReceipt`, `ReleaseCommandReceipt`, `CancelCommandAdmission` and `CheckpointCommandLedger` requests and responses, the `retained_command_receipts` and `cancelled_command_admissions` snapshot fields, `command_ledger_seal`, `refresh_sealed_checkpoint`, and the `outcome` and `failure` fields on `HandledRelayCommand` if nothing else reads them. Keep `RELAY_PROTOCOL_VERSION` at 26 and bump to 27 so a 2.23.3 controller cannot mistake this worker for one that serves receipts; keep `RELAY_MIN_PROTOCOL_VERSION` unchanged. In `mj-core/src/archive.rs` and `mj-checkpoint/src/checkpoint/restore.rs`, make `CanonicalSessionSnapshot.command_ledger` an ignored optional field with `#[serde(default, skip_serializing)]` so archives written by 2.23.3 still restore; stop marking discarded prompts as cancellation tombstones. In `mj-worker/src/worker_runtime/reviewer/`, delete `generations.rs` and `dispatches.rs`, restore the in-memory lane dispatch vector and `TakeLaneDispatches`, and drop `ReadLaneDispatches` and `AckLaneDispatches` from the protocol; keep the `ReviewerLifecycle` states, the operation `JoinSet`, and `reviewer_admission.rs`. Keep the `--generation` argument to the review MCP server only if the in-memory generation check still uses it; otherwise remove `LaneDispatchEnvelope` from `mj-core/src/review/mcp.rs`. Remove the `InstallPromptContext` relay command variant added in this commit (the request form in `worker_client/relay.rs` remains the one path). Keep the `HandbackReminder` command and its deterministic ID; it is small and it is the reason reminders no longer duplicate. In `mj-worker/src/worker_runtime/subagents.rs`, keep the originating-turn stamp and the persist-then-publish order; the semaphores and deadlines may stay, they are harmless.

Milestone 3, controller. In `mj-controller/src/worker_client/relay.rs`, delete the protocol 26 client methods. In `mj-controller/src/session_manager/handle.rs`, `actor.rs`, `actor_types.rs`, `channels.rs`, `remote.rs`, `standalone.rs` and `client_backend.rs`, remove `submit_durable`, `command_receipt`, `release_command_receipt`, `cancel_command_admission`, the `durable` flag threaded through submits, `ActorCommand::CommandReceipt`, and `RemoteActorMode`; keep `supervise_requests`, the per-stream budget, `drain()`, and `DEFERRED_SUBMIT_CAPACITY`. In `mj-client/src/session.rs`, remove the six `ReviewerAction` variants and the four backend methods added by the commit. In `mj-controller/src/server_runtime/api.rs`, keep the durable startup status derived from `startup_steps` and keep `execute_subagent_tool_durable` for now (Milestone 5 trims it); change every `submit_durable` call to `submit` with the same command ID, and delete `release_delegation_receipt` and receipt checks in `api/subagent_input.rs`, restoring its `Notify` wakeup instead of 250 ms polling if the pre-commit version still applies cleanly.

Milestone 4, review host. Restore `mj-controller/src/review_host/host.rs`, `begin.rs`, `dispatch.rs`, `events.rs`, `notices.rs`, `persist.rs`, `resolve.rs` and `roles.rs` from `0a98bc4f~1`, then re-apply by hand: per-role generations allocated in `run` (a small in-memory change), and the `observations.rs` coalescing mailbox with `completed_while_coalescing` if it applies cleanly on the restored files; if it does not, drop it and record that in the Decision Log. Delete `durable.rs`. Restore `mj-controller/src/database/reviews.rs` from `0a98bc4f~1` so `clear_interrupted_turn_reviews` returns, and remove the `orchestration` field from `TurnReviewState` in `mj-core/src/storage.rs`. Remove the serde derives added to `mj-review/src/driver.rs`, `mj-review/src/lanes.rs`, `mj-core/src/review/driver.rs` and `mj-core/src/review/lanes.rs` only if nothing else now needs them; they are harmless if left. Verify the startup notice text "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes" is emitted again.

Milestone 5, delegation. In `mj-controller/src/database/delegation.rs`, keep `prepare_delegation`, `record_delegation_result`, `load_delegation` and `acknowledge_delegation`; make `acknowledge_delegation` delete the row; delete `cleanup_receipts` and the `receipt_pending` index usage (the column may stay in the schema). Remove `close_incarnation` from `PreparedSpawn` and the `Superseded` comparison in `mj-controller/src/daemon/close.rs::close_subagent_request`, and delete `database::session_incarnation`. In `mj-controller/src/daemon/delegation/dispatch.rs`, keep the retry-instead-of-fabricate change to `failed_task` and the per-child `Handback` ordering; the lane limits may stay.

Milestone 6, startup queue. In `mj-controller/src/daemon/state.rs` and `mj-controller/src/database/startup.rs`: replace the single `startup_enqueue` mutex with a per-session lock (a `BTreeMap<String, Arc<Mutex<()>>>` behind the existing state lock is enough); delete rows in phase `done` or `dismissed` at settlement and at daemon start; restore returning a failed prompt's text to the session draft in `fail_startup_queue` (its `failed` parameter is currently unused) and make `append_draft_input` live code again; bound the drain's retries at five attempts before failing the group with a notice; and submit each step with plain `submit` using the row's `command_id`. Keep `queue_api_followup` and the durable status in `api.rs`.

Milestone 7, migration 64. In `mj-controller/src/database/schema.rs`, after migration 63, add migration 64 inside one `BEGIN IMMEDIATE` block: `DROP TRIGGER IF EXISTS session_incarnation_insert; DROP TRIGGER IF EXISTS session_incarnation_resume; DROP TABLE IF EXISTS session_incarnations; ALTER TABLE turn_review_state DROP COLUMN orchestration;` guarded by `legacy_schema::table_has_column` the way migration 61 guards the add, then `UPDATE schema_compatibility SET minimum_compatible_version = 64`, insert into `schema_migrations`, `PRAGMA user_version = 64`, `COMMIT`. Update `SCHEMA_VERSION` and the reader test's `MINIMUM_COMPATIBLE_VERSION` to 64 with a comment "Migration 64 removes review orchestration and session incarnations". Add to `mj-controller/src/database/tests.rs` a test that migrates a fixture at revision 63 containing rows in all five additions and asserts sessions and `startup_steps` survive while the dropped objects are gone. Document in `.agents/docs/` (a short runbook) that a worker sealed mid-Move at upgrade time is recovered by restarting that session's worker.

Milestone 8, validation. See Concrete Steps and Validation and Acceptance. Remove tests that only exercised removed code; keep and adapt tests for kept behavior. Commit per milestone with messages that say what was removed and why.

## Concrete Steps

All Mjolnir commands run from the worktree `/home/jonathan/Projects/mjolnir3` on branch hel3. Never redirect Cargo's target directory; the machine uses the shared mbx build cache. Never install or run a build against the default instance. Use the isolated instance name `cleanup-sweep`:

    export MJ_INSTANCE=cleanup-sweep
    export MJ_CONFIG_DIR=/tmp/mj-cleanup-sweep/config
    export MJ_DATA_DIR=/tmp/mj-cleanup-sweep/data
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    node --check mj-controller/src/web/viewer.js
    git diff --check

Anvil review, run in `/home/jonathan/Projects/anvil`:

    git show 585ebf2 8248f52 -- crates/anvil-client/src/kimi_auth crates/anvil-client/src/kimi_auth.rs crates/anvil-client/src/llm_client.rs

Questions the review must answer, and the plan must record: whether two callers can start a service for one profile at once; who ever stops a service process and whether they accumulate; every `unsafe` block and its soundness; that the secret-bearing stdout is drained privately and never logged; that the non-Unix path fails with a clear message; which vendor endpoints and file layouts are relied on; and what the tests fake.

Migration rehearsal against a copy of a real store, never the live one (the live database path is printed by `mj --instance default status` or found under the default data directory):

    mkdir -p /tmp/mj-cleanup-sweep/data
    cp <live-data-dir>/mjolnir.sqlite3 /tmp/mj-cleanup-sweep/data/
    cargo run -p brokk-mj-cli -- --instance cleanup-sweep daemon --check-schema
    sqlite3 /tmp/mj-cleanup-sweep/data/mjolnir.sqlite3 "PRAGMA user_version; SELECT count(*) FROM sessions; SELECT name FROM sqlite_master WHERE name IN ('session_incarnations','startup_steps');"

Expected: `64`, the same session count as the live store, and only `startup_steps` listed. If the CLI has no schema-check subcommand, starting the daemon once against the isolated instance and stopping it performs the same migration.

## Validation and Acceptance

Run the full test commands above and expect zero failures. Then, on the isolated instance, verify these behaviors. Start a session, queue a first prompt while the worker is still preparing, stop the daemon, start it again, and observe exactly one prompt in the transcript. Start a turn review, restart the daemon during it, and observe the cancellation notice and that the next review's delta covers both turns. From a parent session, call `spawn`, restart the daemon within the tool call's wait, and observe one child. Run the migration rehearsal and observe the expected output. With a worker left running from a 2.23.3 build (start one before switching binaries), confirm the new daemon connects to it, projects its transcript, and replaces it at idle; a sealed worker is the documented exception.

## Idempotence and Recovery

Every milestone is its own commit and is safe to redo from the previous commit. Migration 64 uses `IF EXISTS` and a column guard, so running it twice is harmless. Back up the live store before the first run of the new build against it. If the Anvil publish must be abandoned after a version bump, the bump commit can stay; a later 0.28.7 can carry the fix. Test fixtures live only under `/tmp/mj-cleanup-sweep` and may be deleted at any time.

## Artifacts and Notes

The pre-commit review-cancellation notice, for reference, from `mj-controller/src/review_host/begin.rs` at `0a98bc4f~1`:

    "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes"

The pre-commit duplicate-submit behavior, from `mj-worker/src/relay/commands.rs` at `0a98bc4f~1`:

    if let Some(handled) = self.snapshot.handled_commands.get(command_id) {
        if handled.command != command {
            return Ok(Err(relay_protocol_error(InvalidRequest,
                "command ID was already used for a different command", false, None)));
        }
        handled.accepted_ordinal
    }

## Interfaces and Dependencies

In `mj-worker/src/relay/journal.rs`, define `pub(crate) const RETAINED_TERMINAL_COMMANDS: usize = 512;` and change `prunable_command_ids` to return only terminal entries beyond the newest 512 by terminal ordinal. In `mj-core/src/relay/protocol.rs`, `RelayRequest` and `ReviewerRequest` must not contain any variant introduced by protocol 26, and `RELAY_PROTOCOL_VERSION` is 27. In `mj-core/src/archive.rs`, `CanonicalSessionSnapshot` keeps `pub command_ledger: Option<serde_json::Value>` with `#[serde(default, skip_serializing)]`. In `mj-controller/src/database/schema.rs`, `SCHEMA_VERSION` is 64. In `mj-controller/src/database/delegation.rs`, the public surface is `prepare_delegation`, `load_delegation`, `record_delegation_result` and `acknowledge_delegation`, the last of which deletes the row. In `Cargo.toml`, `anvil-client = { package = "brokk-anvil-client", version = "0.28.6" }`.

Revision note (2026-09-28): created from the audit conversation and Jonathan's decisions before implementation. Author: Claude with Jonathan.
