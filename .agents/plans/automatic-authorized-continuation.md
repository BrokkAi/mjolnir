# Automatically continue clearly authorized unfinished work

This living ExecPlan follows `.agents/PLANS.md`. Update Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation proceeds.

## Purpose / Big Picture

Mj should spare the operator redundant replies such as “yes, run the tests I already requested.” After a successful ordinary session turn, Jev checks whether explicit authorized work remains and no new input is necessary. Mj can submit at most three fixed continuation prompts between user messages. A Settings checkbox disables this default-on behavior. Review runs after the chain settles.

## Progress

- [x] Repository exploration and user decisions completed.
- [x] Created handoff-preservation issue https://github.com/BrokkAi/mjolnir/issues/1111.
- [x] Shared classifier, proxy endpoint, and configuration implemented; proxy checks, tests, deployment, and synthetic smoke checks pass.
- [x] Durable guarded relay admission and continuation state implemented; focused worker tests pass.
- [x] Supervised daemon decisions, review coordination, and visible status implemented; race and settlement tests pass.
- [x] Behavior tests, isolated acceptance, required checks, and documentation completed; committing validated changes on the current branch.

## Surprises & Discoveries

The existing activity classifier sees only a 1 KiB prompt tail; it cannot establish authorization. Review prompt holds live in the daemon, so the decision service must run there. The worker must still check current state atomically before accepting a nudge.

The controller projection is a bounded UI window, so authorization must be loaded separately from SQLite when history is omitted. The query reads only user/assistant text at the exact projection frontier on a blocking task; it does not load tool history. A changed frontier causes abstention.

2026-09-20: Pulled master af11979f at user request. Its current-user conversation policy remains in activity classification. Continuation deliberately retains earlier user messages: the user specifically noted authorization may be three exchanges back. Luna is mining local Codex sessions read-only; private raw excerpts remain outside the repository and hosted requests.

## Decision Log

2026-09-20: Model evaluation showed the original 0.95 cutoff abstained even on direct redundant-permission controls. Explicit true/false criteria plus a 0.90 cutoff on both answers recovered all four development positives without triggering evaluated negatives. Questions were frozen before Codex-derived evaluation; its ambiguous/blocked positive remains an abstention at the final cutoff. This changes the initial implementation parameter, not the user's authorization boundaries.

2026-09-20: Reused the existing per-session coalesced status feed rather than introducing an unbounded queue between the decision service and daemon publication.

2026-09-20: User selected default enabled with an unchecked setting to disable, three nudges per user message, and reviewing once after the chain settles. New input/cancellation wins over pending decisions. Native goals, child/reviewer sessions, errors, structured questions, and approvals are excluded.

## Outcomes & Retrospective

Implemented the default-on setting, bounded evidence collection, durable worker admission, cancellable daemon completion gate, UI/wait/review integration, and proxy route. Added worker/state-machine, concurrent-daemon, three-exchange history, SQLite-window/frontier, configuration, and wait behavior tests. The separate handoff ticket is #1111. Master af11979f was integrated without conflicts.

The proxy is deployed as abf42692-4729-42f5-98d1-26041fbe95c6 and all four routes passed synthetic checks. Named `jev-continuation` acceptance used fresh MJ_CONFIG_DIR/MJ_DATA_DIR, an ephemeral loopback port, and stopped its daemon afterward. The initial default-port attempt collided with an existing service; only the test configuration was adjusted.

Luna supplied twenty sanitized Codex-derived scenarios. The user explicitly approved their TypeSafe evaluation after automatic review initially blocked egress. Final scored results: zero false positives among thirteen mined negatives, but the one mined positive abstains. Eight synthetic development controls yield four/four continued positives and zero/four false positives when full bounded history is retained. See `.agents/docs/jev-continuation-evaluation-20260920.md`; this small curated set is not a calibrated accuracy estimate.

## Context and Orientation

`mj-core/src/activity/verdict.rs` and `services/jev-proxy/` implement existing bounded classification. `mj-controller/src/daemon/process.rs` forwards managed session views to the review host. `mj-controller/src/review_host/` owns automatic review and prompt holds. `mj-core/src/relay/snapshot.rs` defines worker commands and durable state; `mj-worker/src/relay/commands.rs` admits commands. `mj-tui/src/setup/schema.rs` supplies configuration labels and defaults.

## Plan of Work

First add a shared continuation contract, fixed questions, strict parsing, and the `/v1/continuation-verdict` proxy route while preserving old endpoints. Evidence contains complete chronological real user messages since reset (32 KiB maximum) and whole recent assistant replies (16 KiB maximum), with a 64 KiB serialized limit. Missing/truncated required evidence causes abstention. Both independent probabilities—unfinished explicit work and no new input needed—must reach 0.90. The fixed prompt grants no new authorization.

Next add a versioned guarded continuation command and durable allowance. Bind it to the completed turn and user-message boundary, admit only when genuinely quiet with no queued input, and use stable command IDs for duplicate suppression. Cancelling, resetting context, or explicit resume suppresses the feature until new user input. Generated input does not reset the limit.

Finally add supervised daemon background requests, integrate the completion gate with review and visible pending status, and add the default-on setting. Abort requests on changed evidence, new input, cancellation, disabling, lifecycle operations, or shutdown. Never revive historical idle sessions on daemon startup. Review and notification behavior resumes on abstention, failure, or timeout.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Implement and test each milestone, then run `cargo test` with escalated permissions and `cargo clippy --all-targets -- -D warnings` in dev mode. In `services/jev-proxy`, run `npm run check`, `npm test`, and `npm run deploy:dry-run`. Use only synthetic classification evidence in live checks. Use `--instance jev-continuation` for all CLI/daemon/TUI acceptance invocations. Never use the default live instance. Commit coherent validated changes on the current branch; do not push.

## Validation and Acceptance

Positive fixtures include redundant permission, explicitly omitted tests, and authorization from an earlier user message. Negative fixtures include optional work, revoked/superseded instructions, genuine decisions, plan/tool approvals, credentials, blockers, complete requests, quoted instructions, and incomplete context. Exercise exactly three nudges, fourth rejection, user reset, replay/restart deduplication, disable/cancel during HTTP, and races with queue, review, background work, checkpoint, move, and shutdown. Independent sessions must continue concurrently. Automatic review must run once after settlement, including HTTP failure. Verify direct/proxy parity, malformed replies, bounds, older-worker skipping, and configuration persistence. Run labeled synthetic live-model evaluation before enabling the feature in the shipped default.

## Idempotence and Recovery

Classification is advisory until an atomic guarded worker admission succeeds. Retries reuse command identity. Durable attempts survive restarts; in-flight HTTP never survives shutdown. Unsupported workers retain existing behavior. New protocol/state/config versions protect older readers; no controller database migration is planned.

## Artifacts and Notes

The separate GitHub ticket covers Jev verification of instruction preservation in cross-harness handoffs, starting in `mj-controller/src/compaction.rs`.

## Interfaces and Dependencies

Use existing reqwest, Tokio supervised tasks, TypeSafe credentials, shared subprocess helpers, configuration persistence, relay journal, and transcript projection. Add no workspace crate. Classifier questions live in mj-core and are imported by the hosted proxy. Expose `[continuation].enabled = true`, with the Settings label “Automatically continue unfinished requests.”

Final validation: `cargo test` passed in dev profile outside the sandbox; `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. Proxy TypeScript checks, all 18 tests, and deployment dry run passed. The named isolated daemon reported this build, returned an empty session list, and stopped cleanly. Test artifacts are under `/mnt/optane/mj-continuation-*`; no test build was pointed at the live default instance.
