# Make concurrency ownership explicit

This ExecPlan is maintained under `.agents/PLANS.md`. It is the implementation record for the user-approved concurrency sweep, including the authorized changes to sibling repository `/home/jonathan/Projects/anvil`.

## Purpose / Big Picture

Concurrent work must not undo newer user actions, lose acknowledged requests during daemon replacement, or leave subprocesses writing into files that have been cleaned up. The daemon is the database-owning control process; workers own agent execution and journals and survive replacement of that daemon. Each fact has one owner, each asynchronous attempt has an identity, and accepted work survives disconnection or restarts through durable records. Independent sessions must still run concurrently. Frequent work remains proportional to active or changed sessions, not historical session count.

The audit found concrete failure sequences: Move checks two independently locked ownership sets; recovery publishes snapshots out of order and writes late errors against new targets; resume saves an old title over a concurrent rename; startup prompts live only in daemon memory; delegation retries can select a newer turn; active reviews are cleared at startup; reviewer cancellation abandons blocking mutations; subprocess cleanup stops supervising before inherited pipes close; bounded channels drain into unbounded task chains; browser saves and navigation discard newer work. Kimi additionally has competing OAuth credential writers in quota polling and Anvil inference.

## Progress

- [x] (2026-09-27) Completed read-only Astra audit across daemon, persistence, worker, resources, UI, desktop, proxy, and Anvil authentication.
- [x] (2026-09-27) User approved the plan, proactive Kimi refresh, and changes to sibling Anvil.
- [x] (2026-09-27) Fetched origin/master; current mjolnir hel2 at 33c2011d contains all fetched master commits. Both repositories initially clean; Anvil is on master, version 0.28.5.
- [ ] Session admission, Move ownership, recovery settlement, and field-owned persistence.
- [ ] Durable startup/delegation requests and guarded reminder delivery.
- [ ] Durable review orchestration and worker reviewer lifetime.
- [ ] Process-group and pipe supervision, cancellable SSH admission, bounded daemon handoff.
- [ ] Bounded observation/command queues and supervised request dispatch.
- [ ] Browser/TUI/SSE/desktop asynchronous ownership.
- [ ] Shared vendor-owned Kimi authentication and Anvil integration.
- [ ] Integrated isolated regressions, dev-profile tests, Clippy, reviewed commits.

## Surprises & Discoveries

The database writer already serializes commit and publication correctly; preserve it. Worker idle reservation and checkpoint admission already share an authoritative relay owner. The recently implemented incremental feed and actor-generation fencing are foundations to retain.

Kimi quota freshness affects profile ranking and automatic quota recovery, not only display. Making quota polling read-only would permit an idle account to remain unavailable indefinitely, and Anvil would still independently rotate the same credentials. The user explicitly rejected that simplification.

The pinned Kimi 2.0.2 executable supports `kimi web --no-open --host 127.0.0.1 --port 0 --log-level silent`. Its authenticated `/api/v1/oauth/usage` calls the vendor freshness mechanism without a model turn. Its response has `kind`, `summary`, `limits`, and `extra_usage`; current online documentation has a newer shape. Even silent startup prints a bearer-bearing URL: never log or persist raw stdout. The actual port is registered under the profile home at `server/instances`, and authentication uses `server.token`. No supported forced-refresh endpoint was found. Vendor ACP processes still coordinate through vendor locking; consolidating our clients does not prove all vendor processes race-free.

## Decision Log

2026-09-27, user and root: implement structural resource owners and explicit transitions rather than adding timing checks. Preserve independent-session concurrency, no global actor framework and no new workspace crate solely for organization.

2026-09-27, user and root: preserve proactive Kimi refresh and permit Anvil changes. Route quota and inference through one profile-scoped vendor-service interface; remove custom OAuth POST/credential-write paths. On rejected apparently fresh credentials, reread and retry only if changed; otherwise report rejection rather than invent undocumented refresh behavior.

2026-09-27, root: use Astra agents for disjoint implementation boundaries, authorized explicitly by the user. Root owns integration, migration revision coordination, final validation, plan maintenance, and commits. Agents must not stage or commit other agents' files.

2026-09-27, root: use `concurrency-sweep` as the isolated test instance with `/tmp/mj-concurrency-sweep-tests/config` and `/tmp/mj-concurrency-sweep-tests/data`. These are configuration/data fixtures, never Cargo build storage. Use existing mbx build layout. Never install or run the new build against the live/default instance.

## Context and Orientation

Mjolnir is a Rust workspace. `mj-controller/src/daemon/owner.rs` owns runtime projections and lifecycle admissions. `database/writer.rs` serializes database operations and publishes committed snapshots. `session_manager` owns per-worker connections and actor command admission. Workers in `mj-worker` own durable relay journals and harness process groups. The browser implementation is `mj-controller/src/web/viewer.js`; terminal background work is in `mj-cli/src/dashboard`; native shell work is in `mj-desktop`.

An operation identity is a unique identifier created when its owner admits work. A resource generation identifies the particular worker, reviewer, target, or UI request being used. Completion may mutate state only if both still match. A durable request is recorded before acknowledgement and retained until its receiving owner confirms completion. An uncertain delivery means a command may have been accepted even though its acknowledgement was lost; that is not permission to select a new target or issue a new command ID.

The Anvil client dependency is currently registry `brokk-anvil-client` 0.28.2; sibling source is 0.28.5. `crates/anvil-client/src/kimi_auth.rs` constructs OAuth providers and Kimi headers. `BearerTokenProvider` is public in `llm_client.rs`. Mjolnir builds Kimi inference in `mj-controller/src/utility_llm.rs`; quota polling is in `quota.rs`.

## Plan of Work

### Milestone 1: Resource admissions and durable field ownership

Replace Move's independently locked execution and queue sets with a single owner state and atomic handoff. Route the existing command admission predicate through that owner. In `recovery_gate.rs`, serialize admission, cancellation, closure, and watch publication. Return identity-bearing RAII attempt guards so completion/drop cannot release a newer attempt. Recovery and worker-upgrade coordinators must close admission before cancellation and supervise admitted work to settlement. Carry target/attempt identity into conditional durable recovery updates; do not release ownership before settlement.

Separate creation from field-owned session transitions. Ordinary resume and rollback must update lifecycle-owned columns only, preserving concurrently renamed titles and independently maintained drafts/read receipts. The owner observes committed data rather than optimistic clears after failed persistence. Tests pause operations between snapshot and commit and verify newer independent changes survive.

### Milestone 2: Durable startup and delegation delivery

Persist ordered startup prompt/handoff steps before returning success, including stable command identities. Rebuild pending queues at daemon startup; waiting for harness readiness is resumable and does not hold handoff admission. Preserve ordering between archived context installation and prompts. Keep in-flight steps in durable state until confirmed; ambiguous delivery is reconciled and never blindly restored as an unsent draft.

Extend the existing delegation scheduler rather than introducing another. Persist prepared effect identity and immutable child/turn selection before dispatch, record results before parent delivery, and retain deduplication evidence until parent acknowledgement. A retry after restart cannot cancel a later turn. Bound active execution while leaving durable pending requests queued. Handback reminders use worker-guarded admission tied to the completed turn and a deterministic command identity.

### Milestone 3: Review durability and worker-owned reviewer lifecycle

Persist daemon review orchestration phase, role identities, journal cursors, findings, and pending forwarding. Replace startup clearing with reconstruction and attachment to surviving worker roles. Retain pending lane dispatches durably with IDs until an explicit acknowledgement after controller acceptance; reading a dispatch must not consume it.

Reviewer roles become Preparing, Running, Stopping, and Stopped owners. A request disconnect cancels its wait, not an already-started blocking mutation. Keep role ownership until background preparation finishes, stage per generation, and publish the selected generation atomically. Pause retains task/process and lane capacity until actual termination is confirmed. Tests disconnect during preparation, timeout during stop, and restart during every review delivery phase.

### Milestone 4: Process lifetime and daemon replacement

In shared subprocess execution and worker user-shell execution, supervise process-group lifetime and stdin/stdout/stderr completion together. Direct-child exit does not end cancellation or deadline enforcement. Reap or terminate owned descendants before dropping guards or deleting their files. Teardown must validate process identity; startup JSON PID existence alone cannot authorize signaling.

SSH admission, master opening, and cross-process locking must honor the executor's cancellation/deadline. New automatic continuations use deferrable admission while accepted submissions retain their existing admission. Image downloads and other restartable preparation do not hold replacement open. Worker swaps still require the worker's atomic idle reservation, with durable restart intent allowing the next daemon to resume readiness observation.

### Milestone 5: Bounded delivery and supervised queues

Coalesce recovery, upgrade, and review observations by session, retaining monotonic completion frontiers so final-idle facts are not lost. Offload database settlement from coordinator decision loops. Use bounded per-session command queues with a supervised dispatcher instead of a task per queued predecessor; keep independent sessions concurrent and report every task failure. Leased actors must not drain bounded inputs into unlimited deferred submissions. Apply admission pressure before accepting new nondurable commands and reserve capacity for control actions.

### Milestone 6: UI, streams, and desktop jobs

Browser conversation and search operations use separate generation-owned request objects; a stale completion cannot clear new pending work. Drafts have one writer per session, a latest desired value/version, and one outstanding save; completion drains newer desired values, including clears and navigation-away captures.

The TUI runtime feed is the sole config/session authority. Background metadata loads cannot reinstall a stale Controller; retain pending refresh generations. Shutdown persistence shares the bounded cleanup deadline. SSE producers observe receiver closure and daemon shutdown while waiting or encoding. External desktop browser launching runs under a background owner and cannot block the event loop or closing the window.

### Milestone 7: Shared Kimi authentication and Anvil

Expose `KimiBackendConfig::build_with_token_provider(self, Arc<dyn BearerTokenProvider>) -> Result<Arc<dyn LlmBackend>>`, sharing existing header/request construction and never constructing a second provider on failure. Route default standalone OAuth provider construction through the vendor-service provider too; keep API keys supported.

Use a shared service per canonical profile identity. Bind only loopback with vendor authentication enabled, privately drain secret-bearing output, and validate actual port/server identity against the owned process registration. Service ownership must survive daemon handoff or caller cancellation while credential rotation can still commit. Expiry-driven refresh invokes vendor usage, then reads the resulting access token. Quota consumes the same service result. Multiple concurrent callers share in-flight refresh. Do not replicate vendor refresh-token writes or stale-directory reclamation. A rejected token permits one reread/retry only when changed; otherwise propagate authentication failure. Preserve profile-local environment, headers, device identity, and endpoint selection.

Validate Anvil changes against Mjolnir using a temporary local Cargo dependency override without altering target storage. Committed dependency integration must be distributable; publication requires the separately authorized release action, not an unrecorded sibling path dependency. Prepare all code, tests, and release-compatible version changes before any publication approval is needed.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir2`, on existing branch hel2. Anvil changes stay on its existing master. Read each repository's AGENTS.md, and coordinate schema migrations through root so every new migration has a unique revision and explicit compatibility classification. Do not rewrite shipped migrations.

Every Rust test command is executed with elevated permissions and existing build storage. The final Mjolnir validation commands are:

    MJ_INSTANCE=concurrency-sweep MJ_CONFIG_DIR=/tmp/mj-concurrency-sweep-tests/config MJ_DATA_DIR=/tmp/mj-concurrency-sweep-tests/data cargo test
    MJ_INSTANCE=concurrency-sweep MJ_CONFIG_DIR=/tmp/mj-concurrency-sweep-tests/config MJ_DATA_DIR=/tmp/mj-concurrency-sweep-tests/data cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    node --check mj-controller/src/web/viewer.js
    git diff --check

Run focused package/module tests for each milestone before the full suite. Run Anvil's applicable package tests and full required checks under its instructions, on the dev profile. Every manual or shell integration invocation of a new mj binary includes `--instance concurrency-sweep`; fixtures retain their isolated directories. Never run install, upgrade, or release commands against the host installation.

## Validation and Acceptance

Deterministic tests must demonstrate that a paused older effect cannot change replacement state; a concurrent rename survives both successful and failed resume; Move remains owned through queue handoff; shutdown cannot admit work after cancellation; and busy-state subscribers cannot regress. Restart tests exercise durable preparation, acceptance, result recording, and acknowledgement with stable command IDs and unchanged steering targets. Active reviews must preserve findings and worker role identity across replacement.

Process tests use more than 64 KB of traffic, early parent exit, children retaining pipes, TERM-resistant descendants, and cancellation while waiting for SSH admission. Verify cleanup occurs only after owned processes stop and stale identity cannot signal an unrelated process. Queue tests stall one session, exceed capacity, and verify bounded retained tasks while independent sessions and controls remain responsive.

Browser tests reproduce pending A then navigation B, saves during outstanding PUT including draft clear, reversed search completions, and stale errors. TUI tests deliver a newer feed while old metadata loads; idle SSE disconnect must release its producer without another update. Desktop tests keep a fake launcher pending while close events complete.

Kimi tests use only isolated homes and mock vendor/OAuth services, checking one shared refresh, no independent credential writes, cancellation and handoff during persistence, API-key behavior, and header compatibility. Add an isolated pinned-binary contract test with fake endpoints where feasible; never use host login credentials. Record exact test outputs and limitations below as implementation proceeds.

## Idempotence and Recovery

Stage only named files belonging to a completed checkpoint and commit on the current branch. Do not revert concurrent contributors' changes. Test fixtures are disposable and separate from user data. Migration compatibility includes older writes and replay semantics; uncertainty is breaking and raises the compatible revision floor transactionally. Worker protocol changes require capability/version handling that preserves existing live workers until safe idle replacement. Failed work remains visible and recoverable, never silently replayed with a new target.

## Artifacts and Notes

Initial audit base: mjolnir 33c2011d, fetched origin/master identical; sibling Anvil 0.28.5. Audit-only browser reproductions confirmed dropped draft saves and lost B navigation. No live instances or credentials were touched during planning.

## Interfaces and Dependencies

Keep resource-specific state machines local to their owning modules. Use existing Tokio watch, bounded mpsc, JoinSet, CancellationToken, shared subprocess helpers, and the database writer. A completion's admission guard includes attempt identity; durable delivery includes request/command identity and target generation; observational notifications either publish under the owner or carry a revision readers validate. Do not add a generic framework solely to share enum shapes.

## Outcomes & Retrospective

Implementation started; no milestone is yet claimed complete. This document will record validated commits, discovered constraints, and any remaining external publication dependency.

Revision note (2026-09-27): created from the approved cross-repository plan and audit evidence before implementation.
