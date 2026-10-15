# Preserve child reports when a direct Codex wait is cancelled

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries,
Decision Log, and Outcomes & Retrospective current as the work proceeds.

## Purpose / Big Picture

Issue #1287 requires direct Codex MJ waits without model polling, with working
report delivery, cancellation and renewal. Direct routing is implemented and
pushed in `24a3f2e20`, with live proof that code mode cannot invoke MJ tools.
A closer review of cancellation evidence found that the cancelled wait still
consumed a later child report. Finish the cancellation behavior before the
requested local installation: an interrupted parent must be able to collect
the child's actual handback through a subsequent wait.

## Progress

- [x] (2026-10-15) Implement and live-verify direct-only `mcp__mj_agents` routing,
  110.95 seconds without inference, renewal at 240 seconds and executor exclusion.
- [x] (2026-10-15) Pass the merged 2.46.0 worker suite (700 passes, 11 ignores)
  and workspace Clippy; push `24a3f2e20` to master.
- [x] (2026-10-15) Identify the cancellation false positive in saved native tool
  results and stop `scripts/install.sh` before any binaries were replaced.
- [x] (2026-10-15) Serialize wait delivery against the worker's authoritative
  turn ownership, using its cancellation target and shared control-admission predicate.
- [x] (2026-10-15) Add regressions for cancellation before completion, including completion
  before the waiting task can run, and retain completion-first behavior.
- [x] (2026-10-15) Run affected regressions: the existing 23 cases passed,
  and the two new ordering/retirement cases passed after fixture ID corrections.
- [x] (2026-10-15) Validate all cancel forms through acknowledgement before prompt
  completion and delayed acknowledgement blocking promotion of a replacement turn.
- [x] (2026-10-15) Pass final core/worker suites: 1,205 tests passed, 12 ignored.
- [x] (2026-10-15) Serialize successive parent waits through delivery receipt and
  report-marker restoration; retain failed delivery across empty worker snapshots.
- [x] (2026-10-15) Pass all seven final dispatcher regressions and workspace Clippy.
- [ ] Finish the complete controller suite; one checkpoint subprocess exceeded
  its 60-second bound so far and needs an isolated rerun after its diagnostic is available.
- [ ] Live-test actual post-cancellation `reported`/`handback` payload delivery.
- [ ] Complete final checks, commit and push, then install without changing HEAD
  while the installer builds. Verify the installed version and daemon handoff.

## Surprises & Discoveries

The initial cancellation driver checked only that the parent became idle and
repeated a known report marker. Its final answer explicitly said no report
output was returned. `target/issue1287-live-2/cancel-native.jsonl` shows the
first direct wait aborted, then the next wait returned `nothing_to_wait_for`
with a completed child and null output. Slow and renewed waits do have real
`reported` payloads with `report_source: handback`.

The MCP server may retain its worker socket after Codex cancels the client
call. `SubagentEndpoint::complete` currently counts a live socket waiter as
delivery even when its originating parent turn was cancelled. Worker receipts
already support `delivered_to_waiter: false`, which makes the daemon restore
tentative child report markers. Use that existing mechanism.

A cancel acknowledgement can precede the prompt result. The existing durable
`cancelling_prompt_id` now covers generic Cancel/CancelTurn as well as targeted
cancellation. It remains attached to the original turn through acknowledgement,
clears when that turn ends, and clears when legacy Cancel actually steers.
Concurrent cancel admission uses the same owner predicate, so another control
cannot clear someone else's cancellation target.

## Decision Log

- Decision: Correct the ownership of wait delivery rather than add polling,
  sleeps, repeated handbacks or filesystem recovery to the parent's prompt.
  Rationale: only the worker relay knows whether the originating turn remains
  eligible to receive a result; socket liveness is insufficient.
  Date/Author: 2026-10-15, Codex.
- Decision: Stop installation before editing source, and repeat the affected
  live case with assertions on actual tool output.
  Rationale: builds must use one immutable revision, and echoing a known marker
  does not establish report delivery.
  Date/Author: 2026-10-15, Codex.

## Outcomes & Retrospective

Direct routing and executor exclusion are verified. Post-cancellation report
delivery is still incomplete; the original source-only fix is published, but
local installation was intentionally stopped. No default instance was used for
tests and no installed binaries were replaced by that stopped installer.

## Context and Orientation

The worker owns `DurableRelay` in `mj-worker/src/relay.rs` and its command state
in `mj-worker/src/relay/commands.rs`. It records the active prompt and explicit
cancellation target durably. `mj-worker/src/worker_runtime/subagents.rs` owns
the durable tool queue, socket waiters and completion receipts. Its existing
`interrupt_wait_at_turn_boundary` (formerly `yield_wait_for_steering`) retires
a wait without consuming child reports when its turn ends or a Claude
tool-boundary correction needs to run.

`mj-core/src/relay/snapshot/apply.rs` applies the durable cancellation target
for every cancel form. No journal, wire or database field is added.

The daemon consumes completion receipts in
`mj-controller/src/server_runtime/api.rs`; delivery false restores report
markers. Keep accepted child mutations and handbacks durable. Only completion
waits should end with their parent turn.

## Plan of Work

First establish the relay owner's answer to whether the original turn may
still receive a completion wait. Consult its active prompt and cancellation
state inside the relay; do not duplicate that interpretation in the socket
task or daemon. Acquire the relay owner before the queue lock, consistently
with durable tool admission and the existing steering boundary.

Use that decision both when waking the waiting socket and when acknowledging a
daemon completion. The latter must reject delivery even if cancellation has
already committed but the waiting task has not run. Preserve a completion that
won before cancellation and leave spawn, message and handback requests intact.
Retire interrupted waits durably with a false delivery receipt so later daemon
completion cannot consume a child report.

## Concrete Steps and Validation

Work in `/home/jonathan/Projects/mjolnir4` on branch `hel4`. Add focused colocated
ordering regressions near the existing steering and waiter tests, with
`// Hard-won: #1287: ...` comments. Run affected tests outside the sandbox with
`cargo test -p brokk-mj-worker <filter>`; run only failures during iteration.

Build matching CLI and worker binaries through normal Cargo/mbx. Adapt the
saved driver `target/issue1287-live-2/driver.py` for the cancellation case only.
Use a new named instance and copied credentials/profile homes. Cancel while a
real child waits at its gate, confirm the parent is idle and child remains
running, release the child, and collect its report in a fresh parent turn.
Assert the actual wait payload is `reported`, contains the child's handback,
and has the expected child session ID. Save native and API evidence.

After the last code change run the complete touched-crate core, worker and controller suites in the
dev profile and workspace `cargo clippy --all-targets -- -D warnings`. Commit
only owned files on the current branch and push upstream. Merge upstream on
push rejection; never rebase or change branches. Run `scripts/install.sh` only
after the final commit and keep HEAD fixed through completion. Check the
installed CLI version and default daemon metadata after ordinary startup has
had time to coordinate replacement.

## Idempotence and Recovery

No database or wire-format change is currently planned. Preserve the existing
queue and receipt formats. Tests must keep isolated config/data directories;
never exercise a target-directory binary against the default store. Destroy
only fixture sessions, stop their daemon and owning process groups before
removing files. Keep the existing successful routing evidence for comparison.

## Artifacts and Dependencies

Existing evidence is in `target/issue1287-live-2/` and
`target/issue1287-routing/`; the durable summary is
`.agents/docs/codex-direct-waits-1287.md`. The last successful merged checks are
`/tmp/mj1287-merged-tests.log` and `/tmp/mj1287-merged-clippy.log`. The stopped
installer log is `/tmp/mj1287-install.log`. Use the existing relay owner,
`SubagentEndpoint`, `LiveWaiter`, and delivery receipt protocol.

Plan created after verifying the cancelled native call lost its report; this
extends #1287 acceptance from cancellation responsiveness to real report
delivery after cancellation.

Updated after implementation: completion and interruption now both lock the
relay owner before the tool queue, and cancellation eligibility comes from the
same relay predicate used by mailbox wake admission. Focused regressions pass;
matching CLI and worker binaries are building for the next live run.

Updated after finding the acknowledgement boundary: cancellation now remains
latched to the original turn until its own result, and steering releases that
lease. The final checks must include both brokk-mj-core and brokk-mj-worker.

Updated after checking daemon delivery: the dispatcher now holds each parent wait
through receipt processing, so a successor cannot read report markers before a
cancelled predecessor restores them. Delivery retries survive worker snapshots
that already removed the request. Other parents and child mutations stay concurrent.
Final logs now live in `/home/jonathan/.cache/mjolnir/build/issue1287/`.
