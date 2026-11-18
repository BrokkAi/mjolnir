# Delegation startup routing comparison

Base `903cac6f6da1158bd1442007b5ac75f08f7e61fb`, normal dev CLI/worker, unchanged default `tests/e2e/delegation.py`, seed 1171, isolated instance `instructions-base-review3-55d6204e`: PASS. The held-model send_message returned in 0.194 seconds. Logs: `memory-instructions-review3-delegation-base.log`, `memory-instructions-review3-base-tools.log`. The daemon was stopped and the temporary detached worktree removed.

Current implementation before the routing fix, same seed and normal dev features, instance `instructions-review3-55d6204e`: FAIL before the changed prompt assertions. send_message took 60.373 seconds and failed with the startup model=tiny receipt timeout. Log: `memory-instructions-review3-delegation-e2e-normal.log`.

The child was a2f60c1c04b9bf6b3ae38451c59f7c89. Daemon trace: `/mnt/optane/tmp/hel-r-1992670-6hz3ndaa/data/logs/mj-daemon-20261118T183059.047Z-2022968.log`:

- 18:31:09.285410: parent send_message dispatch begins.
- 18:31:09.288723: child's relay hello succeeds.
- 18:31:09.306742: route chooses turn because the session manager reports “session ... is not managed”.
- 18:31:09.312573: first relay attachment succeeds.
- 18:31:09.336330: native startup phase completes, while the fixture still holds the model response.

The manager becomes available about 30 ms after the decision, without releasing the model gate. The fixed decision had already chosen the wrong route and awaited startup, while the fixture correctly waited for mailbox acknowledgement before releasing startup. Removing pre-harness memory I/O exposed this window; routing source was unchanged from base.

## Ownership correction

Before committing a new child relation, the daemon's blocking registration asks the session manager to acknowledge a Starting entry. Thus the database cannot expose an unowned launching child. A failed database registration removes the reservation. Both ordinary and automated-review creation use this path; the plain registration wrapper is gated to tests/test-support and the production API requires a manager. Target publication advances its lifecycle; target retirement removes it. Startup failure also releases it. The manager answers the one parent-message route query from its own registration and current published protocol. A registered child with no attachment queues a durable mailbox event; a published incompatible protocol keeps the historical turn route. No delay, route retry or timeout re-check is added.

The deterministic daemon regression `send_to_registered_child_before_attachment_queues_mailbox_without_a_turn` registers before publishing the child and never publishes a target or attaches an actor. Two identical sends produce one mailbox row and no accepted prompt.


## Acceptance result

After source commits 5cdda85fa and a0b529777, a normal dev rebuild and the full default seed-1171 scenario passed in isolated instance instructions-routing-55d6204e. The held-model send_message completed successfully in 0.180 seconds, and the unchanged startup gate and mailbox ordering/deduplication checks passed. Later handoff/recovery and cleanup checks passed too. Stdout: `memory-instructions-routing-delegation-e2e.log`; artifacts: `target/reliability-artifacts/delegation-seed-1171-903074`. The test daemon was confirmed stopped.
