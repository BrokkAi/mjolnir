```md
# Suspend settles a Move it supersedes, and failed lifecycle work says why

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root. Part of BrokkAi/mjolnir#1542 (cases 2 and 4).


## Purpose / Big Picture

On 2026-11-15 session f01b2eefc22385245286892277497036 was being moved to the EC2 Podman host s50-max-a. A Suspend was accepted while the Move was still cloning the destination checkout, before any worker was installed there. The Move was cancelled ("close superseded the operation"), and Suspend recovery then tried to lease and sync a worker that never existed (`mj-controller/src/controller/lifecycle.rs` ~625-648). Each failure only replaced the session's error text (`record_failed_close`, ~844), so the session stayed in `Closing` indefinitely while the relay actor kept retrying its hello with backoff. Separately, every failed lifecycle operation tells the user only "the daemon log records the reason under reference …" (built at `daemon/state.rs` ~693), so the user cannot see what went wrong or what to do.

After this change, a Suspend or Close that supersedes a Move ends in a terminal state the user can act on. That is Stopped when the session can be restored from a sealed source checkpoint, or a visible Error that Resume's repair can take. A session never stays in `Closing`. Every lifecycle failure the user sees states a cause, the next action, and the diagnostic reference.


## Progress

- [x] (2026-11-15) Milestone 1: settlement of a superseded Move under the existing lifecycle owner, and Closing → terminal transitions for every failure exit, including startup recovery of an interrupted close.
- [x] (2026-11-15) Milestone 2: the safe lifecycle-failure renderer (cause, next action, reference), reused by close, supervisor and startup failures.
- [x] (2026-11-15) Hard-won regression passed: live/restart settlement with full-checkpoint, retained-workspace and unproven-seal cuts, plus Running destination readiness surviving Closing (15.59 s).
- [x] (2026-11-15) Existing failure-publication regression passed (0.30 s); missing-worker startup suspension regression passed with Error and safe cause/action/reference assertions (8.03 s). rustfmt and diff whitespace checks passed.
- [ ] Commit and review; fix substantive findings with focused checks.
- [ ] Final full brokk-mj-controller suite and workspace clippy on the dev profile outside the sandbox.


## Surprises & Discoveries

- The first test command used the directory name instead of the Cargo package name; corrected `mj-controller` to `brokk-mj-controller` before any tests ran.
- Dev dependency compilation encountered many mbx shared C-object coordination waits; normal build storage and cargo wrappers were preserved.
- A worker boot intent protects the pre-Running launch window; `restore_into_target` removes it after publishing Running, before Move saves StartingQueue. The existing destination identity must survive the subsequent Closing transition.

- (child, 2026-11-15) A retained-source handoff Move has no portable full checkpoint, so settlement must preserve its source for Error and repair rather than delete it.


## Decision Log

- Decision: no proven source seal (`recovery_session` absent) means Error with every placement kept; Resume repairs in place before retrying Suspend. Do not ask a possibly sealed relay and then destroy from an older full checkpoint. Rationale: an early-cancelled Move requiring an extra step is preferable to losing newer source work. Approved by parent, 2026-11-15.

- Decision: preserve the already-Running destination identity in the same `adopt_move_destination` transaction that publishes Closing. A matching worker boot intent also prevents source settlement before Running publication. Rationale: destination startup can precede Move's StartingQueue write, so placement alone cannot be readiness. Approved by parent, 2026-11-15.
- Decision: startup's `recover_moves` declines Cancelled pre-queue Closing sessions; the interrupted-suspension owner receives them via the returned actual ownership set. Remove the unconditional all-Move-session extension that previously masked this route. Rationale: live supersession and restart must use the same settlement. Approved by parent, 2026-11-15.

- Decision: the daemon's existing `wait_before_close` stays the single serialized point of supersession. It cancels the Move, awaits the Move's actual completion, reloads its durable record, then settles under the existing lifecycle owner. Settlement lives in `controller/move_session.rs`, called by close recovery before any worker lease is requested. There is no independent recovery task and no worker-upgrade fallback. Date: 2026-11-15.
- Decision: the Move record is the authority for source seal, destination readiness, checkpoints, placements and queued admissions. `Cancelled` alone is not evidence of a cut, so classify from the durable boundaries the record retains. Date: 2026-11-15.
- Decision: source sealed and destination not ready means stop and remove each exact partial destination placement with the existing retirement and cleanup helpers, keep the record and the source checkpoint, and publish Stopped only after cleanup succeeds. Destination ready (queue admission started) is irrevocable: ordinary close checkpoints the destination, and the source is never restored over accepted destination work. Date: 2026-11-15.
- Decision: for a handoff-only Move that retained its source workspace, settlement must capture or preserve the full source workspace before it removes the source. If that cannot finish safely, the terminal result is Error with the source retained, never Stopped from a non-portable artifact. Date: 2026-11-15.
- Decision: every failed close completion transitions `Closing` to Error while keeping recoverable data and placements, instead of only replacing `last_error`. Startup recovery of an interrupted close uses the same publication. Date: 2026-11-15.
- Decision: there is one safe lifecycle-failure rendering path: a trusted cause sentence, a concrete next action, and the reference. Typed refusals keep their trusted public reason. Other failures use a handful of stage names (worker contact, checkpoint, cleanup), not a taxonomy. Raw paths, hosts and error chains stay in the log. Example: "Suspension could not contact the worker. Resume the session to repair its runtime, then retry suspension. Reference …". Date: 2026-11-15.
- Decision: `mj suspend` keeps returning on acceptance. The issue needs the failure reason to be visible, and the stored safe message shown by `mj sessions` and the dashboard provides it. Making the CLI wait for the outcome is out of scope. Date: 2026-11-15.
- Decision: no schema, relay protocol or persisted format change is expected. If one turns out to be needed, classify any migration per AGENTS.md. Date: 2026-11-15.


## Outcomes & Retrospective

Implementation and targeted regressions are complete. Review and final validation remain. No CLI wait behavior, persisted format, schema, or relay protocol changed.


## Validation and Acceptance

One Hard-won test of the Tricky-condition kind, at the real Move/close ownership boundary. It holds destination clone preparation until a Suspend cancels it, then asserts the terminal state, the preserved checkpoint and record, and the correct cleanup of placements. Mark it `// Hard-won: #1542: Suspend superseding a Move mid-clone left the session in Closing indefinitely`. Add boundary coverage for "destination ready" versus "unsealed source" only where no existing test protects it. Extend an existing lifecycle error golden if one exists. Run targeted tests per round, then the full suites of the touched crates and workspace clippy once, on the dev profile, outside the sandbox. Isolate the known hook flakes.
```

Plan update (2026-11-15): recorded approved readiness/startup/no-seal decisions and successful focused regressions; deferred full validation until the final review round.
