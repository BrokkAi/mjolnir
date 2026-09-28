# One owner for daemon state and incremental client updates

This ExecPlan is maintained under `.agents/PLANS.md`. Progress, discoveries,
decisions, and outcomes must be updated at each implementation checkpoint.

## Purpose / Big Picture

Issue #1172 exposes repeated scans of all historical sessions. The daemon loads
the full controller every 500 ms, the web runtime reloads on worker-driven
revisions, and client snapshots scan move records and native-agent owners.
Replace these independent snapshots with one operational state owner, committed
record changes, maintained indexes, and incremental TUI/web feeds. Routine work
must depend on changed sessions or relevant active sessions, not session history.

The other agent owns extraction of delegation dispatch from the web runtime.
Preserve its worker-owned request queue, scheduler identity/order/retry rules,
and execution backend. Its lifecycle actions consume this implementation's state
interface; do not implement a second delegation scheduler.

## Progress

- [x] Inspected full-state readers, writer lane, lifecycle ownership, and feeds.
- [x] User approved incremental TUI and web feeds as part of this refactor.
- [x] Claimed #1172 and added `agent-in-progress`.
- [x] Introduce structurally shared session/relation maps and point-read support.
- [x] Add the operational owner and transition/index tests, with work-count coverage at 100, 10,000, and 100,000 historical sessions.
- [x] Integrate ordered persistence receipts, typed Move ownership, atomic draft restoration, and generation-checked worker observations.
- [x] Replace ordinary daemon reloads and poller scans with maintained projections; retain explicit one-shot history operations.
- [x] Implement cursor-based incremental TUI and web feeds (daemon cursor protocol, keyed TUI consumers, shared web rows, browser indexes, and bounded SSE deltas implemented; transport-size audit remains below).
- [x] Remove obsolete refresh loops and validate scaling, upgrade, recovery, and actor replacement behavior.
- [x] Complete required dev-profile tests, final TUI/CLI integration checks, Clippy, formatting, and JavaScript syntax validation. The validated tree is ready for the authorized commit and HEAD:master push.

## Surprises & Discoveries

The existing database writer is exclusive and serialized, but many submitted
operations open an additional SQLite connection rather than using the writer's
connection. Commit observation must cover those writes until they are converted;
observing only the writer connection would silently miss changes.

The web projection clones all session records on every publication, and the
daemon runtime snapshot loads every move and queries native children separately
for every historical session. Removing the timer alone is insufficient.

`State` currently uses mutable BTreeMaps. A shared outer Arc would copy the whole
map on mutation. Structural sharing must happen inside the collections.

## Decision Log

- Decision: One owner admits control operations and publishes committed state.
  Rationale: Independent readers cannot reliably reconstruct lifecycle ownership
  from stale records. Worker execution and relay admission remain worker-owned.
  Date: 2026-09-27.
- Decision: Include both TUI and web incremental consumers.
  Rationale: User selected this scope; leaving full snapshots would preserve
  history-sized work on the ordinary activity path. Date: 2026-09-27.
- Decision: Preserve historical formats; no legacy-harness redesign or deletion.
  Rationale: User prioritizes scan frequency and ownership. Existing bootstrap
  warnings are acceptable. Date: 2026-09-27.
- Decision: Use imbl persistent collections and Arc-owned large values.
  Rationale: Snapshot acquisition must be constant-time and point updates must
  avoid copying unrelated records. Date: 2026-09-27.

- Decision: Observe affected durable keys using connection-local SQLite triggers
  on every writable connection, then read committed values on the ordered writer
  lane before replying. These triggers are temporary and introduce no migration.
  Rationale: Existing writer jobs may use additional connections. Caller-maintained
  invalidation lists would miss nested writes and cascading deletes. Triggered
  keys alone are not receipts because transactions may roll back; committed
  readback and comparison supplies the actual receipt. Publication failure must
  poison the writer and its snapshot feed, never permit mutation replay.
  Date: 2026-09-27.

- Decision: Keep worker eligibility in `RuntimeStateOwner` with structurally shared
  record indexes. Polling enumerates this set and checks lifecycle ownership under
  the same lock. It does not query SQLite or inspect unrelated historical records.
  Rationale: SQL can supply committed changes and bootstrap data, but repeatedly
  reconstructing eligibility would leave frequent work proportional to history.
  Date: 2026-09-27.
- Decision: A lifecycle watch channel carries notifications only. Executing,
  Cancelling, and Completed phases determine ownership. Completion applies the
  phase and sends the result under the owner lock, checking operation identity.
  Rationale: Channel contents and separately locked records cannot independently
  determine whether the worker belongs to a lifecycle operation.
  Date: 2026-09-27.

## Outcomes & Retrospective

The refactor and audit are complete. Ordinary worker selection reads a maintained
in-memory index, committed database changes update structurally shared records,
and daemon/TUI/web feeds publish keyed changes. Worker producer identities and
typed lifecycle phases reject stale effects. Move retries reserve their matching
destination at admission. Recovered drafts append atomically to committed input.
Initial runtime snapshots cross bounded transport frames and apply atomically.
History scans remain for bootstrap, explicit history views, and maintenance.
Legacy-record warnings are startup diagnostics rather than timer-driven reads.

The full dev-profile workspace test suite passes, including 1,904 controller
and 639 worker tests. The final TUI-only audit changes additionally pass all 837
TUI tests, 235 CLI tests, all 11 daemon-startup tests, all 11 PTY tests, and all
four store-divergence tests. Clippy with warnings denied, Rust formatting,
JavaScript syntax, and diff whitespace checks pass. Work-count regressions cover
100, 10,000, and 100,000 historical sessions and native children. All runtime tests
use isolated stores and the named state-owner-1172 instance; the live installation
was neither launched, upgraded, nor restarted.

No implementation or validation work remains. Publication is the user's explicitly
authorized final operation: commit the task files on hel2 and push HEAD:master
without force. Remote acceptance is recorded in the completion report. The
checkpoint notes below are historical records, not remaining work.

The working tree was clean at the start, on commit 40922d77. The user's update to
bbdef44b and the requested master integration through a3d4f61f are preserved.
The design lesson is that shared snapshots alone are insufficient: every derived
membership, transport boundary, and presentation-only row needs an explicit owner
or incremental reconciliation to keep routine work independent of history.

## Context and Orientation

`mj-controller/src/daemon.rs` and its modules implement the background process.
`controller.rs` defines a mutable configuration/state snapshot and lifecycle
methods. `database/writer.rs` provides the sole serialized persistence lane;
`database/state_io.rs` supplies full bootstrap and bounded point reads; daemon
`load_state()` now acquires the writer’s immutable committed snapshot. `server_runtime`
owns the web projection and presently reloads its own controller. `pollers/remote`
and `pollers/runtime_feed` bridge daemon snapshots into the TUI. `mj-client` owns
the internal daemon wire types, and `server/handlers` plus `web/viewer.js` own the
browser's revision-triggered full-snapshot loop.

## Plan of Work

First establish a shared immutable record projection and one operational owner.
Use a serialized transition interface for admission, persistence completion,
external-effect completion, cancellation, and worker observations. Database,
network, filesystem, process, and expensive rendering work stay outside the
owner's transition loop in supervised tasks. An operation holds a unique identity
and a phase; old completions cannot affect another operation or worker incarnation.

Persist changes through the existing ordered writer and publish exact committed
changes before acknowledging durable success. Failed transactions publish no
changes. A post-commit publication failure stops mutation service and requires
normal startup reconstruction; it never grants permission to replay a mutation.
Replace stale whole-record saves with field-owned updates as operation paths
migrate. Admission checks and related multi-session reservations are atomic.

Maintain indexes for pollable workers, workspace membership, children and occupied
child slots, configuration dependencies, and background candidates. Update their
membership in the same transition as the source record. A consumer enumerates
the relevant index and does not repeat the eligibility predicate. Lifecycle
phases preserve graceful close relay access through sealing, parked-child
visibility, and irreversible teardown's cancellation boundary.

Remove full-controller refresh on timer ticks and activity revisions. Observe
configuration changes and store compatibility separately. Configuration changes
invalidate dependent records through reverse indexes. Bootstrap, explicit
history/export reads, and maintenance may traverse history; ordinary operations
must use owner queries or indexed record reads. Retain persisted formats and
existing upgrade safety.

Publish typed record, lifecycle, worker, config/workspace, review, notice, move,
and native-agent changes. Introduce an incarnation/sequence cursor and Snapshot,
Delta, and ResetRequired frames. Capture a snapshot and establish its subscription
at one publication boundary. Retain at most 4096 batches or 16 MiB; slow consumers
reset explicitly instead of blocking the owner or retaining unbounded history.
Serialize immutable snapshots off the owner loop. Apply batches atomically in
both clients; keep their records and view memberships keyed instead of rebuilding
complete vectors. Preserve explicit public snapshot responses and advance the
internal daemon protocol. Normal clients must use the incremental path.

Implement in coherent checkpoints: model and tests; persistence and lifecycle
integration; daemon consumers; client feeds; removal and complete validation.
Keep each intermediate state buildable. Never leave a compatibility adapter as
the final authoritative path.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir2` on the current branch. Use the current
mbx Cargo configuration without changing target directories or build profiles.
Run focused tests after each coherent change and the full required checks at the
end. Every cargo test command runs outside the restricted sandbox.

    mkdir -p /tmp/mj-state-owner-1172-tests/config /tmp/mj-state-owner-1172-tests/data
    MJ_INSTANCE=state-owner-1172 MJ_CONFIG_DIR=/tmp/mj-state-owner-1172-tests/config MJ_DATA_DIR=/tmp/mj-state-owner-1172-tests/data cargo test
    cargo clippy --all-targets -- -D warnings

Live experiments must use `mj --instance state-owner-1172` and isolated config
and data directories. Do not start the new daemon against the default instance.
Stage only this task's files and commit each validated checkpoint; do not push.

## Validation and Acceptance

Exercise transitions using deterministic fake effects and controlled completion
order. Different sessions progress concurrently; conflicting operations cannot
both acquire ownership; late completions cannot recreate deleted records. Test
commit failure, cancellation, recovery failure, deletion, and atomic publication
of final records with released lifecycle ownership. Slow effects and clients
must not obstruct unrelated state transitions or shutdown.

Test initial attachment, duplicate batches, cursor gaps, resynchronization, and
daemon replacement in both client reducers. Preserve existing isolated upgrade,
schema, active-worker, and terminal-draft handoff regressions. Accepted durable
work retains existing upgrade admission through publication; worker turns and
restartable background preparation do not delay daemon handoff.

With a fixed active workload, seed 100, 10000, and 100000 historical sessions.
After bootstrap, drive idle ticks, streamed output, metadata changes, child
registration, and lifecycle completion. Count full-state loads, unrelated row
decoding, record projection, and emitted payloads. Expect zero full loads and
zero unrelated historical-record work, with only logarithmic collection costs.
Keep legacy-harness fixtures: bootstrap may warn, routine activity must not
rediscover those rows. Timings support these work-count assertions rather than
replacing them.

## Idempotence and Recovery

All durable state remains in SQLite and workers. Restart reconstructs the owner
and indexes before opening admission. No new durable queue is introduced. Test
failures use disposable named instances. Preserve accepted requests and command
IDs; never infer replay authorization from a missing acknowledgement. No deletion
of user history is needed for this work.

## Artifacts and Notes

The original incident had 89 unsupported historical session rows. Their warning
volume revealed approximately 1300 full-state loads in five minutes. The two
known sources are the daemon's 500 ms target refresher and the web runtime's
revision-triggered reloads. This is investigation evidence, not a performance
measurement of the replacement.

## Interfaces and Dependencies

`RuntimeHandle` supplies immutable snapshots, maintained queries, typed control
commands, and subscriptions. `RuntimeStateOwner` owns records and operation
phases. Commit receipts contain ordered changes from the persistence lane.
Cursor-based feed frames contain a daemon incarnation and monotonic sequence.
Use existing Tokio supervision, upgrade admission, subprocess helpers, and
SQLite writer; add imbl for structurally shared collections, not a new workspace
crate. Worker request dispatch belongs to the independent scheduler refactor.

Revision note (2026-09-27): Record the validated shared-map foundation, the user’s
fast-forward pull, and the explicit prohibition on testing against the live instance.

Validation note (2026-09-27): Default workspace checking and Clippy pass.
The controller suite passed 1,869 tests, including the new point-read regression
and historical migrations. The full workspace run stopped at the existing
worker test `checkpoint_wake_records_already_queued_runtime_events_first`, whose
one-second wait timed out. The isolated rerun passed in 0.12 seconds; the remaining CLI tests subsequently passed, including isolated terminal upgrade
handoff. Foundation committed as c2617cce. The committed-state publisher is now
integrated with the owner; four focused publication tests passed. A subsequent
controller run passed 1,872 tests and failed two damaged-store regressions.
Connection observer installation had incorrectly depended on an unrelated optional
table, and worker recovery had lost its fresh storage check. Fixes preserve the
optional write failure and use a bounded single-session recovery read. Both repaired regressions passed in the next controller run. All five publication
regressions pass, including the fatal-publication case. That broader run passed
1,860 tests but exposed 15 older worker-launch tests reading the ambient store,
whose schema had advanced from 57 to 58. Compatibility checks refused those reads.
The whole Cargo test process now receives dedicated config/data directories and
MJ_INSTANCE, in addition to preserving each test’s existing isolation.

Revision note (2026-09-27): Pollable workers are maintained in memory, as clarified
with the user. Committed row readback updates that state; it is not the polling
query. Record diagnostics and metadata backfill now run during startup instead of
every Controller::load. Lifecycle completion sets its explicit phase and notifies
waiters under the owner lock. The full refactor and final validation remain open.

Revision note (2026-09-27): Isolate the entire Cargo test environment because
some existing launch helper tests consult the ambient database even when they
construct their session records in memory. Never rely solely on fixture-level
isolation to protect the live store. The existing schema refusal prevented the
ambient reads; no live daemon was started or upgraded.

Checkpoint note (2026-09-27): `cargo clippy --all-targets -- -D warnings` passes
for the owner integration. Full workspace tests are running with the dedicated
whole-process environment. No manual daemon/TUI invocation has been made yet.
The next implementation checkpoint must remove historical work from
`daemon/snapshot.rs::runtime_snapshot`, `server_runtime/run.rs::publish_snapshot`,
and the TUI’s vector replacement path. Worker views and close intent still need
the remaining single-owner transition audit. Do not describe this intermediate
checkpoint as the complete refactor.

Validation checkpoint (2026-09-27): With whole-process isolation, the controller
suite passes 1,875 tests (8 ignored). All five publication regressions and the
previously failing launch tests pass. Clippy passes. Remaining workspace suites
are still running; the current checkpoint covers ownership, publication, and
the primary daemon pollable-worker index, not the later consumer/feed work.

Checkpoint update (2026-09-27): 08984a09 commits the operational owner and primary
poller refactor. Its full isolated workspace test run passed, including 1,875
controller tests, 635 worker library tests, 235 CLI unit tests, all 11 terminal
PTY tests, and store-divergence integration tests. No live daemon was upgraded.

The next uncommitted checkpoint forwards the primary manager's prepared targets
to the web manager, eliminating web-side target reconstruction. CommittedState
now bootstraps sessions, moves, and native summaries in one WAL snapshot and
updates move/native keys incrementally. Native replay staging remains private;
ReplayBegin legitimately publishes loss of availability, and ReplayCommit adds
or replaces replayed children while retaining older disconnected children.
Worker preparation captures move records with the session inputs. Project-memory
identity receives the parent's checkout from the controller snapshot instead of
querying the database from a low-level launch helper. New tests cover metadata
bootstrap, staged replay, phase changes, cascading deletion, and held snapshots.
This checkpoint's focused validation is in progress.

Validation checkpoint (2026-09-27): The metadata and prepared-target consumer
changes pass the controller suite (1,877 passed, 8 ignored) and Clippy across
all default workspace targets. The existing isolated-parent project-memory
regression still passes after removing its implicit database lookup. The full
workspace suite passed for the preceding owner checkpoint; final whole-workspace
validation remains due after the client-feed and remaining state-machine work.

Checkpoint note (2026-09-27): 272d95e8 commits prepared-target sharing and the
committed move/native metadata projection. The next checkpoint puts worker views
and background policy observations inside RuntimeStateOwner alongside records and
lifecycles. Record deletion removes those observations under the same lock; late
views for missing records are rejected. Stage and notice callbacks carry their
lifecycle operation ID, so callbacks from an old operation cannot mutate its
replacement. Hidden completed lifecycle outcomes retain at most 256 entries;
visible deferred cleanup continues to hold its existing handoff semantics.

Validation note (2026-09-27): All 103 focused daemon tests pass, including the
new late-callback, deletion, bounded-retention, and historical scaling regressions.
Polling performs zero eligibility predicate evaluations after bootstrap at every
tested history size. A single changed record evaluates only its before and after
eligibility. Formatting and whitespace checks pass. Full isolated workspace tests
and Clippy are in progress. Incremental client feeds are still outstanding; this
checkpoint is not completion of the whole plan.

Validation checkpoint (2026-09-27): Full `cargo test` with the dedicated
state-owner-1172 environment passed, including 1,881 controller tests, 635 worker
library tests, 235 CLI unit tests, 11 daemon-startup tests, all 11 terminal PTY
tests, and all 4 store-divergence tests. `cargo clippy --all-targets -- -D warnings`
and formatting checks pass. No live instance was started or upgraded. The
remaining work is incremental client feeds and the remaining ownership audit.

Checkpoint note (2026-09-27): 98960791 commits worker observations under the
operational owner and bounded lifecycle retention. The next checkpoint introduces
protocol 40 with RuntimeChanges: an incarnation/sequence cursor, immutable keyed
snapshots, deltas, and explicit ResetRequired. The daemon retains at most 4,096
snapshots or 16 MiB of changes. An oversized change discards prior history and
keeps the current snapshot as the reset boundary. Capture subscribes before
reading owner state, so a transition during attachment leaves a pending wakeup.
Records, workers, and lifecycles cross the same owner boundary; serialization and
history bookkeeping happen after releasing that owner lock.

The normal TUI poller now consumes RuntimeChanges and keeps records, relations,
moves, native summaries, and native views in structurally shared keyed maps.
Transcript reads come from changed worker entries or outstanding retries.
Native projection loading maintains pending keys instead of scanning all desired
children. Existing explicit RuntimeSnapshot callers remain for one-shot actions.
Legacy revision ordering is preserved for management-response reconciliation;
the transport's incarnation cursor controls resynchronization across replacement.

Validation note (2026-09-27): The first cursor/feed validation passed 41 client
tests and 1,884 controller tests (8 ignored). New tests exercise atomic delta
application, duplicate delivery, gaps, replacement, bounded retention, oversized
changes, and attachment followed by owner mutation. Additional TUI keyed-consumer
validation and Clippy are running. Browser snapshots, browser SSE, and the web
runtime's project/native metadata refresh loops still require conversion.

Validation checkpoint (2026-09-27): The keyed TUI projection passes 835 TUI
tests (2 ignored), seven native-loader regressions, and ten runtime-feed
regressions. A worker observation with no projection is now published once and
waits for a changed observation rather than staying in the retry set. Protocol
40 passes all 11 isolated daemon-startup tests, all 11 terminal PTY/handoff tests,
and all four store-divergence tests. Clippy and formatting checks pass. The full
workspace suite passed at 98960791; final full validation remains due after the
remaining web/ownership work. No live daemon was launched or upgraded.

Checkpoint note (2026-09-27): bf0dce4c commits the daemon/TUI incremental feed.
The current web checkpoint is unvalidated. ViewerSessions retains a persistent
map internally and serializes the existing public array shape. ViewerPublication
projects dirty records plus genuine project-identity dependents, retains unchanged
rows, and observes the daemon's shared move/native projections directly. Repeated
native-owner/move reload jobs are removed. Project discovery now schedules changed
keys and due retries; API activity recording diffs shared public rows. Credential
and capacity input construction uses the daemon's worker/active indexes.

The browser opts into `/api/events?format=changes`, whose stream establishes a
snapshot cursor and sends keyed deltas. It holds one coalesced watch baseline,
uses a one-item outgoing channel, and resets explicitly for deltas above 16 MiB.
Encoding runs off the web control loop. The legacy revision-only stream remains
for existing clients. Browser state keeps keyed rows, workspace/live/resumable
membership, and a cached sorted history list; individual lookups no longer scan
history. New tests cover shared rows and browser duplicate/gap/replacement handling
at 100, 10,000, and 100,000 rows.

Review items for this uncommitted checkpoint: keep dependency refresh pending
until the web loop schedules it (observe_runtime must not erase an earlier dirty
signal); refresh a completed background Controller reload from current owner
records before pruning live observations; run and repair existing sliced-JS test
fixtures that now need the keyed lookup helpers. Full controller validation and
Clippy are running. Worker-incarnation ownership, string-driven move target
ownership, remaining TUI record-change scans, and the existing 8 MiB internal
snapshot frame limit still need the final audit. Do not describe the plan as done.

Integration note (2026-09-27): At the user's request, fetched and merged master
through a3d4f61f into hel2 as 13f0a95c before further testing. Interrupted the
previous isolated full-suite run; it is not a completed validation result.
Preserved the in-progress web checkpoint in a named stash and restored it after
the merge. The new daemon-owned delegation service remains the only scheduler.
Its former controller-mutex reads now use the state owner, point session reads,
and the maintained worker projection for credential targets. Web credential,
quota, and delegation ownership stays removed as on master. Merge integration
checks are in progress; no live instance has been started or upgraded.

Validation note after master integration (2026-09-27): `cargo check --all-targets`
and `cargo clippy --all-targets -- -D warnings` pass. The new full isolated run
has passed 1,895 controller tests and 835 TUI tests, including the web scaling,
cursor, and corrected JavaScript fixture tests. The scheduler's state-owner
adaptation required one Clippy formatting simplification; no behavioral change.
The remaining workspace and isolated integration tests are still running.
The pending web dependency latch, fresh-owner reload, and consistent bootstrap
publication review items listed above are resolved. Remaining ownership and
historical-work audits are still open; this is a checkpoint, not plan completion.

Validated web/integration checkpoint (2026-09-27): The full isolated `cargo test`
run completed successfully after merging master, including worker and CLI tests,
all daemon startup, terminal PTY, and store-divergence regressions. Clippy,
formatting, JavaScript syntax, and diff whitespace checks pass. Runtime tests
used MJ_INSTANCE=state-owner-1172 with separate config/data directories throughout.
No live daemon was launched or upgraded. The saved pre-merge stash can be removed
once this checkpoint is committed; all its changes were restored and reconciled.

Audit implementation (2026-09-27): The user authorized completion and pushing to
origin/master after validation. The current audit closes producer identity for
session-manager observations: replacement invalidates queued old views and late
sends at the same registry lock, and producer registrations disappear on drop.
Delegation replacement also revokes an already queued observation. Move target
ownership now uses MovingDestination/CancellingMoveDestination phases and an
operation-ID-checked typed executor callback; recovering a durable destination
reserves it at lifecycle admission, before the worker manager can observe it.
Display notice text no longer decides ownership.

RuntimeChanges responses are encoded on a blocking task and sent as bounded
64 KiB transport fragments under the existing per-frame limit. A client exposes
only a fully decoded RuntimeFrame with a consistent request/protocol identity.
Connection loss drops partial publications. Initial snapshots may traverse
history once without entering an oversized-response retry loop. Explicit legacy
one-shot responses retain their existing size limit. The protocol revision is
still 40, introduced by this as-yet-unpublished refactor.

The TUI maintains live/stopped membership and child indexes from shared-map
differences; normal render and attention queries enumerate candidates. Record
publication patches durable rows and details instead of scanning all records.
Its polling/capacity controller contains active records and required ancestors.
Remote handles carry no locally reconstructed worker recovery plan. Checkpoint
size refreshes request only changed paths and validate completion identity per
session, so independent requests can finish concurrently without discarding one
another. Web cache pruning follows changed/deleted records. Stopped historical
checkouts are probed at bootstrap or identity changes but do not retry indefinitely.
Restored startup drafts append atomically on the writer lane; failure cannot
publish a successful in-memory edit. Startup prompt fixtures now use real,
process-isolated durable stores.

Focused controller/client/TUI/CLI tests are compiling. Remaining work is to fix
any regressions, finish the render call-site review, validate the full suite and
Clippy, update final outcomes, commit, and push HEAD to origin/master. Do not
claim completion or push until these checks pass. No live instance is involved.


## Final audit milestone

The audit closes remaining history-dependent work in native-child presentation
and attention counts. Separate durable and presented snapshot roots let the TUI
reconcile local launch rows without comparing every native presentation row to a
store snapshot that intentionally omits them. Native running-child membership
and managed active-child membership make badges depend on active children; total
child counts use indexed cardinality. Explicit child/history views still enumerate
their requested records. Removed child rows also drop their retained detail.

A replaced delegation observer suspends dispatch until its replacement reports
the durable queue. Already-executed results remain in delivery state, preventing
actor replacement from executing a request again. A matching retained Move
operation reserves its ready destination in the same admission decision as the
lifecycle entry; an unrelated old Move cannot claim a new operation's target.
The pending-queue check now also happens under the admission owner. Draft write
failure reports unsaved input instead of falsely saying it was restored.

Validation evidence: the complete isolated workspace `cargo test` passed after
the ownership, transport, draft, and rendering fixes. The subsequent TUI suite
also passed the 100,000-native-row reconciliation regression. Clippy passed.
The final active-child badge index changes only TUI code; the complete TUI/CLI
suites (including startup, PTY, and store-divergence integration tests) and Clippy
are being rerun for that final change. Formatting, JavaScript syntax, and diff
whitespace checks pass. No schema migration or live installation change was made.

All audit findings have implementations and focused behavioral coverage. Finish
by recording the final UI check results, committing the listed task files on hel2,
and pushing HEAD:master without force. Preserve concurrent upstream commits if
master advances. The pre-merge stash was already removed after its restoration
was validated and committed in e5f2e6df.


Final validation (2026-09-27): All final TUI/CLI checks completed successfully,
including startup, store-divergence, and PTY integration tests. The full workspace
suite and final Clippy, formatting, syntax, and whitespace checks pass. The audit
has no open findings. Publish the validated tree as requested; no further test
runs are needed unless a new change or upstream merge requires them.
