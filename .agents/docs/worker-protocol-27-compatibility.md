# Workers from 2.23.2 and 2.23.3 after the durability-layer removal

Releases 2.23.2 and 2.23.3 shipped relay protocol 26, whose workers keep
durable command receipts, cancellation tombstones and a Move "seal" in their
journal snapshot and export a command ledger into checkpoint archives. The
daemon no longer uses any of that, and its workers speak protocol 27. This
note says what still needs care while a protocol 26 worker is alive.

## What the daemon tolerates

A protocol 26 worker's persisted snapshot may contain `retained_command_receipts`,
`cancelled_command_admissions` and `command_ledger_seal`. Those fields are kept
on `RelaySnapshot` as ignored, so a worker binary upgraded in place still opens
its own store. The same is true of `command_ledger` on `RestoredRelaySeed` and
`CanonicalSessionSnapshot`: archives written by 2.23.3 restore, and the ledger
inside them is simply not read.

The daemon replaces a mismatched worker with its own build the next time that
worker is idle, so the window during which a protocol 26 worker is running is
bounded by how long it stays busy.

## The one case that needs a hand

A worker that was sealed for a Move at the moment the daemon was upgraded
refuses every command except completing, releasing or closing its checkpoint
until a controller releases the seal. The new daemon does not know about seals.
If a session appears stuck after the upgrade with a Move that never finished,
restart that session's worker (stop it and let the daemon recreate it from its
last checkpoint). Nothing else is affected.
