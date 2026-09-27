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
- [ ] Add the operational owner and transition/index tests.
- [ ] Integrate ordered persistence receipts and lifecycle ownership.
- [ ] Replace daemon reloads and poller scans with maintained projections.
- [ ] Implement cursor-based incremental TUI and web feeds.
- [ ] Remove obsolete caches, validate scaling, upgrade, and recovery behavior.
- [ ] Complete required Cargo checks and commit validated checkpoints.

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

## Outcomes & Retrospective

The foundation compiles with `cargo check --all-targets`. The persistent-map
copy-count test passes at 100, 10,000, and 100,000 records, and the wire-format
test confirms ordinary JSON objects. Single-session durable reads now query only
the selected session and its target, mounts, and checkpoint; session-manager
outcome and repair reads use this path. The owner, indexes, and feed changes
remain pending, so the original reload loops have not yet been removed.

The working tree was clean at the start, on commit 40922d77. During implementation
the user pulled master forward to bbdef44b. Their changes are retained. All runtime
experiments must use the named instance below; never upgrade the live daemon.

## Context and Orientation

`mj-controller/src/daemon.rs` and its modules implement the background process.
`controller.rs` defines a mutable configuration/state snapshot and lifecycle
methods. `database/writer.rs` provides the sole serialized persistence lane;
`database/state_io.rs` currently reconstructs the entire store. `server_runtime`
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

    cargo test
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
one-second wait timed out. The isolated rerun passed in 0.12 seconds; the unreached CLI tests are
still running. `database/committed.rs` is a separate, not-yet-integrated prototype for
the next checkpoint; it is not part of the foundation validation.
