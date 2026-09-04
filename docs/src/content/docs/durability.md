---
title: Durability and recovery
description: Learn how journals, checkpoints, stop and resume, worker survival, orphan adoption, and upgrades protect a Mjolnir session.
---

Mjolnir separates a logical session from the target currently running it. The
target-side worker makes current activity durable; a verified recovery archive
lets the controller recreate the session after that target is stopped or lost.

These are complementary layers. A live worker has the newest journal and
queue. A recovery archive is the latest controller-held point from which a new
target can be built. Work newer than that archive is recoverable only while the
original target and its worker storage still exist.

## The target-side journal

Every session worker owns an append-only, ordered event journal and a compact
operational snapshot on its target. Commands and observations become durable
there before the worker acknowledges them. That includes queued prompts, so a
prompt accepted behind an active turn does not depend on an open terminal or
web connection.

Journal records carry an ordinal and content digest. On startup, the worker
loads its durable snapshot, validates and replays only the bounded crash tail
that follows it, and serves older history from journal segments when the
controller asks. Mjolnir 2's current records are self-describing: each record's
digest verifies that record, while ordered frontiers prove which exact prefix a
controller or checkpoint has accepted. Legacy chained records remain readable,
but new documentation should not treat the current journal as a single hash
chain.

The controller materializes journal events into its local session database and
acknowledges a verified frontier. Acknowledgement alone does not permit the
worker to discard history. Journal data is eligible for collection only after
it is both durably acknowledged by the controller and covered by a verified
checkpoint.

## Queued work continues in order

The worker promotes queued prompts one at a time, in the order they were
accepted. Closing the TUI with `Alt+Q`, closing a browser tab, or stopping the
controller daemon does not turn those accepted prompts back into client-side
drafts.

The queue waits behind an active prompt, configuration change, checkpoint
barrier, or close operation. Agent-started work and background commands also
count as live work; Mjolnir does not declare a session quiet merely because the
last user prompt appears complete.

## What a recovery checkpoint contains

A checkpoint barrier waits for earlier effectful commands and any harness-
started turn to finish. Once admitted, it briefly freezes new ACP dispatch at a
specific event frontier while Mjolnir captures the state needed for a coherent
archive.

The archive includes:

- the canonical transcript and session state at the captured frontier;
- queued prompts and queued configuration changes;
- each repository's committed delta, staged and unstaged changes, and untracked
  files;
- an allowlisted set of native session artifacts for the selected harness; and
- manifests describing the session, bundle, target provenance, versions, and
  payload hashes.

Harness credentials and configuration are excluded from native checkpoint
artifacts. They are supplied again from the selected controller-side profile
when a target is resumed. Repository origins containing embedded credentials
are also rejected or redacted at the archive boundary.

Checkpointing protects project workspaces. Installed packages, the rest of the
target user's home, `/tmp`, container layers, and other files outside the
declared project workspace are ephemeral. Directory attachments are not a
substitute for project workspace storage; copy durable results into a project
repository or push them to a remote.

## Verification before teardown

The target builds the archive and reports its SHA-256. After transfer, the
controller compares the received bytes with that digest, validates the archive
structure and each payload hash, and records the exact journal frontier and
frontier digest it covers. Restore performs the same structural and payload
verification before changing a destination.

Only a successfully installed, verified archive advances the worker's recovery
floor. If export, transfer, verification, or persistence fails, the previous
archive remains in place and the live target is left usable. Mjolnir does not
remove a target merely because checkpointing was attempted.

## Automatic and manual checkpoints

The recovery coordinator observes completed turns. When a session is idle and
its newest checkpoint is at least ten minutes old, it captures the newest
completed turn. The first eligible completed turn can be captured without
waiting for an older checkpoint to age.

A failed automatic checkpoint is reported and retried after a backoff that
starts at ten minutes and doubles to a two-hour ceiling. An automatic attempt
has a 15-minute deadline so a wedged transfer cannot own the lifecycle gate
forever.

Request a fresh checkpoint for an active session with:

```console
mj checkpoint --session <session-id>
```

The barrier still waits for a safe capture point. A session with an active
turn, harness-started work, terminal, background command, queued prompt, or
another checkpoint operation is not quiet and may defer the copy. See
[Troubleshooting](/troubleshooting/) if a checkpoint never becomes eligible.

## Detach, stop, destroy, and resume are different

**Detach** closes only the current client. The daemon, worker, harness, and
queue keep running. In the TUI use `Alt+Q`; running `mj` later reattaches.

**Stop** preserves the logical session. A normal stop records its intent,
creates or safely reuses a verified archive, closes and seals the relay at the
captured frontier, and only then removes the exact managed target. If a
checkpoint fails, stop fails non-destructively and can be retried. A controller
restart can continue a previously recorded closing transition.

**Resume** provisions a fresh selected target, verifies and restores the
archive, restages current credentials, and reconnects the logical session. You
may select a different compatible target and profile. The resume flow asks
whether archived queued work should be started or discarded.

When the same harness is selected, Mjolnir restores its allowlisted native
session state and verifies that the expected native session opened. When the
harness changes, Mjolnir starts a new native session and generates a
size-bounded, tool-free handoff from the canonical transcript. Queued work that
you choose to retain is replayed into the new relay.

**Destroy** is permanent. Destroying a stopped session removes its recovery
archive and record. Force-destroy can tear down an active target without a new
checkpoint and removes every Mjolnir-owned recovery artifact; it is the
explicit data-loss escape hatch.

A **force stop** is narrower: it requires an existing verified archive, skips
the fresh checkpoint, tears down the current target, and leaves the session
resumable from that older archive. Any work after that archive can be lost.

Session commands and confirmation screens are covered in
[Session lifecycle](/sessions/).

## What survives a failure

| Failure or action | What happens |
| --- | --- |
| TUI detaches or browser closes | Worker, harness, and durable queue continue |
| `mj daemon stop` or controller restart | Detached target workers continue; a new daemon reconnects and catches up from their journals |
| Controller host is unavailable | Remote workers can continue already queued work; local targets still depend on that host staying up |
| Worker process dies or becomes unresponsive | Mjolnir diagnoses it, restarts a confirmed dead or unresponsive worker, and recovers relay state from its on-target snapshot and journal |
| Target is lost | Resume can recover only through the newest verified controller-side archive |
| Controller state loses track of a still-running managed target | `mj recover scan` can find the orphan and `mj recover adopt` can add it back |

Mjolnir is conservative about live restarts: it does not kill a worker merely
because one connection failed. It distinguishes a dead worker, an unresponsive
handshake, a worker still recovering its journal, and a live process whose
transport is temporarily unavailable.

To find untracked managed resources:

```console
mj recover scan --json
mj recover adopt --session <session-id> --target <target-id>
```

Older resources without current ownership metadata may additionally require
`--profile` and `--bundle`. Adoption is preferable to destruction when the
resource may contain work not present in a recovery archive. See the recovery
flow in [Troubleshooting](/troubleshooting/).

## Upgrades wait for a quiet worker

After the Mjolnir binary changes, the daemon compares each connected worker
with the worker build it would install. An outdated worker is replaced in place
only at a quiet moment: no active prompt, autonomous harness turn, queued
prompt, user shell, agent terminal, background command, or checkpoint barrier.

Replacing a worker also ends its ACP bridge, so a session that never becomes
quiet keeps its current worker rather than sacrificing live work. Failed
upgrades retry with bounded backoff. The next stopped-session resume always
provisions a fresh target and installs the current worker.

Relay protocols and archive schemas are versioned. A build that cannot safely
understand a stored format reports an explicit compatibility error instead of
attempting a partial conversion. Keep the controller current before relying on
a long-lived archive for disaster recovery.
