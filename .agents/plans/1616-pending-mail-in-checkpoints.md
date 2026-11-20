# Carry accepted, undelivered mail through checkpoints (#1616)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root. It implements the "Pending mailbox events (#1616)" slice of `.agents/plans/origin-delta-session-snapshots.md` (Milestone 2), and records here where the implementation refined that paragraph.


## Purpose / Big Picture

Sessions receive messages: a parent's instructions, another session's note, a review verdict, a GitHub event. The daemon (the control process, `mj daemon-run`) keeps each message in a durable outbox and submits it to the session's worker (the per-session process on the target that runs the agent). Once the worker accepts it, the daemon marks the outbox row accepted and never sends it again. The worker holds the message until the agent can see it: at the agent's next tool boundary, attached to the next prompt, or by starting a turn of its own (a "wake").

Suspend, Move and restart replace the worker with a new one restored from a checkpoint archive. Today the archive carries no mail. A message the worker accepted but had not yet handed to the agent is lost: the sender was told it was accepted, the recipient never sees it, and nothing retries.

After this change the checkpoint copies the worker's mail state at the cut, and restore reinstates it. A message pending when a session is suspended arrives after it is resumed, in its original order, exactly once. To see it: in an isolated instance, send a non-waking message to an idle session, suspend it, resume it, and prompt it; the agent receives the message with that prompt.


## Progress

- [x] (2026-11-20) Read the mail paths (worker relay snapshot, daemon outbox, checkpoint latch, restore seed, Move admission, cross-harness resume) and settled the design below.
- [x] (2026-11-20) Archive shape: `CanonicalMailbox` on the canonical session and the restore seed, archive schema 13, validation, and `content_matches` so an unchanged-archive reuse cannot drop mail.
- [x] (2026-11-20) Relay protocol 75: `ReadCheckpointMailbox`, answered only at the exact ready cut (one predicate, `at_checkpoint_cut`, shared with Close's refusal); the controller reads it under the checkpoint latch and reports what an older worker cannot save.
- [x] (2026-11-20) Restore: `restore_queue_policy` decides queue and mail together; the seed carries mail accordingly; the worker opens with it; Move admission and fresh-native resume submit deferred mail under `mailbox_command_id`.
- [x] (2026-11-20) Store revision 142, breaking; the push migration test now checks its compatible revisions below that fence, as the usage-report test already does for revision 136.
- [x] (2026-11-20) Tests: a worker round trip (relay at the cut, archive, restore under each policy, retries) and extensions of three controller tests (restart recapture at protocol 75, the close latch against a protocol-58 worker, Move queue replay with crash and retries). Clippy clean on the affected crates.
- [ ] Commit and push.


## Surprises & Discoveries

- Observation: delivered-key deduplication already covers producer retries, so mailbox command receipts need not travel.
  Evidence: `submit_command` in `mj-worker/src/relay/commands.rs` acknowledges a `DeliverMailboxEvent` whose key is pending, leased to a hook, or in `delivered_mailbox_event_keys`, under any command ID, before it consults the receipt ledger's absence.

- Observation: deferring the queue is not enough to keep restored mail from running early. A restored worker with a pending waking event starts a `MailboxWake` turn on its own as soon as it can submit.
  Evidence: `promote_next_queued_command` in `mj-worker/src/relay/commands.rs` calls `enqueue_mailbox_wake` whenever the queue is empty and `mailbox_wake_is_allowed`. On a cross-harness resume that wake can start before the controller installs the handoff context, and `InstallPromptContext` refuses while a prompt is active, which would fail the resume.


## Decision Log

- Decision: the controller reads the worker's mail state with a dedicated relay request, `ReadCheckpointMailbox`, while it holds the checkpoint latch, and the worker answers only when its barrier is ready at exactly the cut the controller names and nothing has been journaled since.
  Rationale: the worker owns accepted mail, so the checkpoint asks it rather than reconstructing mail from the daemon's projection, which holds only lossy presentation rows. Putting payloads in the operational state would send up to a megabyte on every sync. Reading the relay's state file during export is unsafe because the latch may be released and the published file can lag the journal. A mail delivery that lands after the cut moves the frontier; Close is then refused for the same reason, so the exact-cut check introduces no new failure.
  Date/Author: 2026-11-20, implementer.

- Decision: the archive carries pending events (hook-leased events first, as a lease return puts them), the delivered-key ledger, and the mail already claimed for queued recovery prompts (`mailbox_prompt_contexts`). It does not carry mailbox command receipts.
  Rationale: see the first Surprise; the key ledger is the single deduplication fact for mail, so a second copy keyed by command ID would be a second predicate for the same question.
  Date/Author: 2026-11-20, implementer.

- Decision: pending mail follows the dispatch decision of the queue it would join. `QueueRestorePolicy` gains a fourth case, so the one policy value decides both: `Restore` seeds the queue, claimed mail and pending mail; `Discard` drops the queue and its claimed mail and seeds pending mail; `Defer` seeds only the claimed mail, bound to its prompt's command ID, and the controller admits queue and pending mail after readiness; `DiscardDeferringMail` drops the queue and its claimed mail and the controller admits pending mail after readiness. A Move destination uses `Defer` or `DiscardDeferringMail` until Move commits ownership, and so does a resume into a fresh native session, which must install its handoff first.
  Rationale: plan review required restored wakes to sit behind the Move ownership commit, and deferred recovery prompts to find their claimed mail bound to their original command IDs before they start. Seeding the context under the original ID satisfies the second; deferring pending mail to the same admission that submits the queue satisfies the first. A separate flag beside the queue policy would allow the invalid pair "deferred queue, seeded mail".
  Date/Author: 2026-11-20, implementer.

- Decision: a worker older than relay protocol 75 cannot report mail payloads. The checkpoint proceeds, logs the undelivered keys at WARN, and tells the user how many accepted messages could not be saved.
  Rationale: refusing would strand a suspend on a worker the user cannot upgrade on demand. This is the pre-fix behaviour, now reported instead of silent; it is not described as preserved.
  Date/Author: 2026-11-20, implementer.

- Decision: archives that carry mail declare archive schema 13, and store revision 142 is breaking: it raises the compatibility floor to 142 in the same transaction.
  Rationale: the shipped `CanonicalSessionSnapshot` reader denies unknown fields, so an older daemon could not resume a session whose archive carries mail. The parent plan assigned this slice a breaking revision for that reason.
  Date/Author: 2026-11-20, implementer.


## Outcomes & Retrospective

Implemented as designed. A message the worker accepted before a Suspend, Move or Restart cut now reaches the restored session once, in order; a daemon retry of any accepted or delivered key is acknowledged without a second copy. Restored mail never starts a turn before a Move commits ownership or before a fresh native session has its handoff, and a deferred recovery prompt finds its claimed mail under its own command ID.

Remaining limits. A worker older than relay protocol 75 still cannot hand over its mail; the checkpoint now says how many messages were not saved instead of dropping them silently. A queued `MailboxWake` still makes checkpoint export fail (#1594); this slice does not change that. Native-use evidence, which the parent plan paired with this schema bump, still needs its own archive change.


## Context and Orientation

The worker's durable state is a relay journal (an append-only event log on the target) plus a snapshot of state folded from it, `RelaySnapshot` in `mj-core/src/relay/snapshot.rs`. Its mail fields are `pending_mailbox_events` (accepted, waiting), `mailbox_hook_lease` (events a Claude tool hook has taken but not yet confirmed writing; on expiry they return to the front of the pending list), `delivered_mailbox_event_keys` (recently delivered keys with the journal ordinal of delivery, at most `RELAY_RETRY_ID_RETENTION`), and `mailbox_prompt_contexts` (mail already rendered into the hidden context of a specific prompt, keyed by its command ID; a worker restart moves an interrupted prompt's context to its queued recovery prompt). Transitions are in `mj-core/src/relay/snapshot/apply.rs`.

A checkpoint is taken under a barrier: the controller submits `BeginCheckpoint`, waits until the worker reports `checkpoint_ready` at a cursor (journal ordinal and digest), and holds a latch on the relay connection while it builds the canonical session from the daemon's projection at that cursor (`mj-controller/src/controller/checkpoint/latched.rs`). Worker-owned state not in the projection (assessment state, notification stack) is copied from the operational state into `CanonicalSessionSnapshot` (`mj-core/src/archive.rs`). The archive writer chooses its schema version in `mj-checkpoint/src/archive.rs`.

Restore runs on the target: `relay_seed_for_restore` in `mj-checkpoint/src/checkpoint/restore.rs` writes a `RestoredRelaySeed` (`mj-core/src/relay.rs`) that the new worker reads once in `DurableRelay::open` (`mj-worker/src/relay.rs`). `QueueRestorePolicy` decides whether the archived queue is seeded, dropped, or left for the controller to admit. The controller chooses it in `restore_into_target` (`mj-controller/src/controller/resume.rs`). Move admits its queue in `admit_move_queue` (`mj-controller/src/controller/move_session.rs`) after persisting `queue_admission_started`, which is the point after which the Move cannot roll back to its source. A resume into a fresh native session installs a handoff context and then replays the queue in `restore_into_target`.

The daemon submits outbox mail with command ID `mailbox_command_id(event_key)` (`mj-store/src/mailbox_outbox.rs`). The worker deduplicates a `DeliverMailboxEvent` by event key.


## Plan of Work

In `mj-core/src/archive.rs`, add `CanonicalMailbox { pending_events, delivered_event_keys, prompt_contexts }` with a `validate` method (non-empty and unique keys, renderable content, no key both pending and delivered, non-empty contexts) and `is_empty`. Add `mailbox: Option<CanonicalMailbox>` to `CanonicalSessionSnapshot`, written only when present; include it in `content_matches` so an unchanged-archive reuse cannot carry stale mail; in canonical validation, require each context to name a queued prompt. Add `ARCHIVE_SCHEMA_VERSION_MAILBOX = 13`. In `mj-checkpoint/src/archive.rs`, declare 13 first in the writer's chain when mail is present, accept 13 in the reader, and refuse mail under an older declared schema.

In `mj-core/src/relay.rs` and `mj-core/src/relay/protocol.rs`, add protocol 75 (`RELAY_CHECKPOINT_MAILBOX_PROTOCOL`), `RelayRequest::ReadCheckpointMailbox { barrier_command_id, cut }` and `RelayResponsePayload::CheckpointMailbox { mailbox }`. In `mj-worker/src/relay/requests.rs`, answer it from the snapshot after checking barrier, ready cursor and frontier. Add the client call in `mj-session-runtime/src/worker_client/relay.rs` and `StandaloneSession`. In `latched.rs`, read it right after building the canonical session, before the latch can end; for an older worker, warn and notify.

Add `mailbox: Option<CanonicalMailbox>` to `RestoredRelaySeed`; extend `QueueRestorePolicy` with `DiscardDeferringMail`; make `relay_seed_for_restore` select the mail as decided; in `DurableRelay::open`, install it. In `restore_into_target`, compute the policy in one function from the Move operation, native continuity and the queue choice; after the handoff, a non-Move fresh-native resume submits the deferred mail. `admit_move_queue` submits deferred mail after the queued prompts for both dispositions, under `mailbox_command_id`, pinned with the queue's command IDs.

In `mj-store/src/database/schema.rs` add breaking revision 142; bump `SCHEMA_VERSION` and the test floor.


## Concrete Steps

From the repository root:

    cargo test -p mj-core archive
    cargo test -p mj-checkpoint
    cargo test -p mj-worker relay::
    cargo test -p mj-controller checkpoint move_session resume
    cargo test -p mj-store schema
    cargo clippy -p mj-core -p mj-checkpoint -p mj-worker -p mj-session-runtime -p mj-controller -p mj-store --all-targets -- -D warnings


## Validation and Acceptance

A worker-level regression accepts a non-waking and a waking message while a prompt runs, stops at a checkpoint barrier, reads the mailbox at the cut, builds a seed from a canonical session carrying it, opens a new relay from that seed, and shows the events pending in their order with the delivered keys retained; a retry of the same event under a new command ID is acknowledged without a second pending copy. A Defer seed opens with no pending mail and the claimed context bound to the recovery prompt's command ID. A protocol test shows the worker refuses the read once the frontier has moved past the cut.


## Idempotence and Recovery

Mail submission during admission is idempotent: the worker acknowledges a known key without adding it again, and the command IDs are derived from the keys. Revision 142 only raises the floor; rerunning migration is a no-op.


## Artifacts and Notes

The worker regression fails on the old behaviour (seed without mail):

    panicked at mj-worker/src/checkpoint_tests.rs:460:5:
    assertion `left == right` failed
      left: []
     right: ["first", "second"]

The Move admission regression fails when admission skips the archived mail:

    panicked at mj-controller/src/controller/move_session/tests.rs:1438:5:
    assertion failed: observed.contains(&mail_id)

The close-latch test's stand-in worker negotiates relay protocol 58 by default, which is how the older-worker path is exercised:

    protocol=58 undelivered=["waiting-message"]


## Interfaces and Dependencies

    // mj-core/src/archive.rs
    pub struct CanonicalMailbox {
        pub pending_events: Vec<crate::mailbox::MailboxEvent>,
        pub delivered_event_keys: BTreeMap<String, u64>,
        pub prompt_contexts: BTreeMap<String, String>,
    }

    // mj-core/src/relay/protocol.rs
    RelayRequest::ReadCheckpointMailbox { barrier_command_id: String, cut: RelayCursor }
    RelayResponsePayload::CheckpointMailbox { mailbox: CanonicalMailbox }

    // mj-checkpoint/src/checkpoint/restore.rs
    pub enum QueueRestorePolicy { Restore, Discard, Defer, DiscardDeferringMail }


Revision note (2026-11-20): recorded the implementation, its tests and the remaining limits (older workers, #1594, native-use evidence) after the slice was built.
