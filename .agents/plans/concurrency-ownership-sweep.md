# Make concurrency ownership explicit

This ExecPlan is maintained under `.agents/PLANS.md`. It is the implementation record for the user-approved concurrency sweep, including the authorized changes to sibling repository `/home/jonathan/Projects/anvil`.

## Purpose / Big Picture

Concurrent work must not undo newer user actions, lose acknowledged requests during daemon replacement, or leave subprocesses writing into files that have been cleaned up. The daemon is the database-owning control process; workers own agent execution and journals and survive replacement of that daemon. Each fact has one owner, each asynchronous attempt has an identity, and accepted work survives disconnection or restarts through durable records. Independent sessions must still run concurrently. Frequent work remains proportional to active or changed sessions, not historical session count.

The audit found concrete failure sequences: Move checks two independently locked ownership sets; recovery publishes snapshots out of order and writes late errors against new targets; resume saves an old title over a concurrent rename; startup prompts live only in daemon memory; delegation retries can select a newer turn; active reviews are cleared at startup; reviewer cancellation abandons blocking mutations; subprocess cleanup stops supervising before inherited pipes close; bounded channels drain into unbounded task chains; browser saves and navigation discard newer work. Kimi additionally has competing OAuth credential writers in quota polling and Anvil inference.

## Progress

- [x] (2026-09-27) Completed read-only Astra audit across daemon, persistence, worker, resources, UI, desktop, proxy, and Anvil authentication.
- [x] (2026-09-27) User approved the plan, proactive Kimi refresh, and changes to sibling Anvil.
- [x] (2026-09-27) Fetched origin/master; current mjolnir hel2 at 33c2011d contains all fetched master commits. Both repositories initially clean; Anvil is on master, version 0.28.5.
- [x] (2026-09-27) Session admission, Move ownership, recovery settlement, and field-owned persistence.
- [x] (2026-09-27) Durable startup/delegation requests and guarded reminder delivery.
- [x] (2026-09-27) Durable review orchestration and worker reviewer lifetime.
- [x] (2026-09-27) Process-group and pipe supervision, cancellable SSH admission, bounded daemon handoff.
- [x] (2026-09-27) Bounded observation/command queues and supervised request dispatch.
- [x] (2026-09-27) Browser/TUI/SSE/desktop asynchronous ownership.
- [x] (2026-09-27) Shared vendor-owned Kimi authentication and Anvil integration.
- [x] (2026-09-27) Integrated isolated regressions, dev-profile tests, Clippy, formatting and reviewed changes; final commit/push follows.

Implementation checkpoint (2026-09-27): all milestone owners have changes in the shared tree; integration is in progress. Core executor tests (5) and SSH tests (37) pass, including inherited pipes and admission cancellation. Browser deferred-operation regressions and existing viewer unit files pass. Desktop exact-module tests (2) and Clippy pass; full native desktop compilation is unavailable because GTK/GLib development packages are absent. Worker lib/binary cargo check passed before subsequent integration edits. Dashboard Rust tests now pass (109), all viewer unit files pass, and core focused coverage totals 53 passing tests with core Clippy clean. Controller Kimi tests pass (24); the actual pinned Kimi 2.0.2 contract test passes against isolated fake OAuth endpoints. Anvil client tests pass (399), and Anvil root tests pass (953 unit plus 35 integration, with documented ignored tests); final Clippy remains pending. These are focused results, not a completed full-suite validation.

## Surprises & Discoveries

The database writer already serializes commit and publication correctly; preserve it. Worker idle reservation and checkpoint admission already share an authoritative relay owner. The recently implemented incremental feed and actor-generation fencing are foundations to retain.

Kimi quota freshness affects profile ranking and automatic quota recovery, not only display. Making quota polling read-only would permit an idle account to remain unavailable indefinitely, and Anvil would still independently rotate the same credentials. The user explicitly rejected that simplification.

The pinned Kimi 2.0.2 executable supports `kimi web --no-open --host 127.0.0.1 --port 0 --log-level silent`. Its authenticated `/api/v1/oauth/usage` calls the vendor freshness mechanism without a model turn. The actual pinned-binary contract test found quota at `quota.usages`, including `limit5h`, `limit7d`, `monthTotal`, and `monthCode` with `usedRatio`; earlier source research alone gave the wrong response shape. Process registration and authenticated HTTP metadata also use different service IDs, so both identities must be validated separately. Even silent startup prints a bearer-bearing URL: never log or persist raw stdout. The actual port is registered under the profile home at `server/instances`, and authentication uses `server.token`. No supported forced-refresh endpoint was found. Vendor ACP processes still coordinate through vendor locking; consolidating our clients does not prove all vendor processes race-free.

## Decision Log

2026-09-27, user and root: implement structural resource owners and explicit transitions rather than adding timing checks. Preserve independent-session concurrency, no global actor framework and no new workspace crate solely for organization.

2026-09-27, user and root: preserve proactive Kimi refresh and permit Anvil changes. Route quota and inference through one profile-scoped vendor-service interface; remove custom OAuth POST/credential-write paths. On rejected apparently fresh credentials, reread and retry only if changed; otherwise report rejection rather than invent undocumented refresh behavior.

2026-09-27, root: use Astra agents for disjoint implementation boundaries, authorized explicitly by the user. Root owns integration, migration revision coordination, final validation, plan maintenance, and commits. Agents must not stage or commit other agents' files.

2026-09-27, root: use `concurrency-sweep` as the isolated test instance with `/tmp/mj-concurrency-sweep-tests/config` and `/tmp/mj-concurrency-sweep-tests/data`. These are configuration/data fixtures, never Cargo build storage. Use existing mbx build layout. Never install or run the new build against the live/default instance.

2026-09-27, root: worker command-ID deduplication was pruned by journal acknowledgement, so stable IDs alone could not make durable outbox replay safe. Protocol 26 adds explicitly retained command receipts with outcome lookup and idempotent release. New receipts are bounded, with pre-acceptance refusal when full. Startup, review/config delivery, and delegation share this mechanism. Daemon protocol advances to 41; database migrations 59–63 cover startup, delegation, reviews, worker restart intents, and execution incarnation identities, each breaking because older actors would violate durable ownership semantics.

2026-09-27, root: Mjolnir initially consumed sibling Anvil through a local path dependency during coordinated tests. Final integration pins the tested and pushed Git revision 8248f521c0f99b456b4c48353b7e4993675e137e. A future crates.io release must publish the new Anvil API and replace the Git pin before packaging Mjolnir against registry dependencies. The user explicitly authorized pushing completed commits to origin/master; live upgrades and registry/tag publication are not part of that instruction.

2026-09-27, root: ordinary API followup startup and daemon startup are being unified into one durable ordered queue; no second task/status map survives. Persistent group IDs retain deduplication, and only pending rows are indexed and read for frequent draining.

2026-09-27, root: an absent receipt is not a cancellation barrier across separate network connections; an older buffered submission may arrive later. Cancellation therefore asks the worker owner to atomically return the accepted receipt or retain a cancellation tombstone. Receipt release cannot erase that tombstone. Checkpoint Move must carry this deduplication ledger into the replacement worker.

2026-09-27, root: a durable CloseAgent effect selects a database-owned execution incarnation in the same transaction that prepares its effect. Resume rotates this identity; close preserves it. Lifecycle admission precedes checking that identity, so a delayed old close cannot stop a newly resumed child or write failure bookkeeping against it. Worker delegation requests also stamp their originating turn under relay ownership; daemon delay cannot retarget an old handback.

2026-09-27, root: reviewer processes must participate in the primary worker's atomic idle reservation. Their admission leases are acquired under the relay owner and retained through preparation and stopping. Final worker exit must join retained cleanup owners, even when ordinary bounded pause has timed out.

2026-09-27, root: automatic vendor-service launch requires Unix, where launch registration and inherited lease ownership are implemented and tested. Unsupported platforms fail visibly instead of silently using a weaker detached-process lifetime. API keys and injected providers remain portable.

2026-09-27, root: the integration run exposed that an ownerless checkpoint recovery path cleared a Move seal. A sealed close now survives disconnect and process restart; the current durable Closing lifecycle adopts its existing barrier. Internal worker startup observations refresh the exact ready cursor under that same seal, and stale paged exports are refused. Ordinary background checkpoints cannot adopt this durable close owner.

2026-09-27, root: review persistence must itself have one in-flight owner. An acknowledgement of an older saved snapshot cannot dispatch a newer outbox, and retry timers cannot send captured stale snapshots directly to the database. Coalesce newer desired state, identify acknowledgements by review epoch and save revision, and send retries back through the review owner.

2026-09-27, root: merged origin/master through 3b604eed, retaining the user's native-goal Esc fix and newer delegation guidance. The three overlapping local files were temporarily preserved and restored cleanly; the task-specific stash was removed after restoration.

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

Implementation and validation are complete. Commit 138020d7 establishes subprocess/SSH ownership with focused tests. Anvil commits 585ebf2 and 8248f52 are pushed to origin/master; Mjolnir pins exact revision 8248f521c0f99b456b4c48353b7e4993675e137e, so the sibling checkout is not required. Anvil root tests (988), client tests (401), final queued-cancellation regressions (17), both Clippy checks, and the pinned vendor contract pass. The full Mjolnir integration run exercised all default workspace targets and all historical migrations through revision 63. It exposed lifecycle/test-fixture mismatches, now corrected, and prompted deterministic regressions for review save acknowledgement ordering, failed-save retries, and cancellation cleanup. The final full workspace run passed every target except two controller test fixtures; after correcting those fixtures, the entire controller library passed (1,950 passed, zero failed, eight ignored). Worker library passed 665 tests, and all 11 isolated daemon-upgrade regressions passed. Browser viewer tests passed all five files. An isolated `--instance concurrency-sweep` smoke test passed startup, readiness, and graceful shutdown with fresh temporary configuration/data. Final `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks pass. Clippy cleanup only boxed the lifecycle completion payload and removed unnecessary test borrows; the dashboard regression rerun passed all 111 tests. No live instance was upgraded or used.

Revision note (2026-09-27): created from the approved cross-repository plan and audit evidence before implementation; updated with receipt-retention discovery, coordinated schema/protocol revisions, startup unification, publication boundary, and focused validation evidence.

Validation note (2026-09-27): final logs are `/tmp/mj-concurrency-final-tests.log` (workspace), `/tmp/mj-concurrency-controller-final.log` (complete corrected controller rerun), `/tmp/mj-concurrency-clippy.log`, `/tmp/mj-concurrency-fmt.log`, and `/tmp/mj-concurrency-browser-tests.log`. The final two fixture corrections retain the preparation owner until settlement and use the existing checked-in fake-command dispatcher to avoid the documented fork/exec ETXTBSY race; production code was unchanged by those corrections.

Final publication note: the requested current-branch commits are being pushed to `origin/master`; no release tag, registry publication, installation, or live-instance upgrade is part of this change. The Anvil automatic vendor-service launcher remains explicitly Unix-only, and native desktop integration validation remains limited by missing GTK/GLib development packages.
