# Separate user input from ongoing work

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

A parent asking for approval must become awaiting-input even while an independent child works. Child traffic must not prevent the parent classifier from checking after one minute of parent silence. Existing history selection, continuation policy, child rendering, database and relay contracts remain unchanged.

## Progress

- [x] (2026-09-21) Inspected classifier, proxy, runtime clocks and evidence generation.
- [x] (2026-09-21) Implemented independent assessments and additive v3 proxy contract.
- [x] (2026-09-21) Implemented parent-specific scheduling, freshness, and preservation of background controls on an input handoff.
- [x] (2026-09-21) Added decision, diagnostics, inventory, continuous traffic and child-preservation coverage; evaluated nine synthetic scenarios twice.
- [x] (2026-09-21) Full dev-profile Rust suite, strict Clippy, formatting, proxy tests/type checks, dry-run and diff review passed; implementation prepared for the required current-branch commit.

## Surprises & Discoveries

The original runtime marked overall activity before routing native child notifications. That overall clock remains unchanged for liveness; the new parent clock is marked only after routing. The transcript context already tracks evidence generations and background inventory identities, so these checks can be retained independently of a new parent clock.

## Decision Log

- Decision: Keep legacy v2 questions in a dedicated JSON file and use the existing shared questions file for v3.
  Rationale: Direct and hosted clients share one source while deployed older clients retain their contract.
  Date/Author: 2026-09-21, Codex.
- Decision: Store the process-local parent clock in the shared TurnContext.
  Rationale: Delivered prompts and parent transcript observations already pass through this shared context; child inventory changes only invalidate evidence.
  Date/Author: 2026-09-21, Codex.

- Decision: Apply the existing process-local idle inference when an awaiting-input prompt completion reaches the relay.
  Rationale: Ending the foreground prompt alone still exposes Background when child work exists. This preserves task inventory and controls while making the confirmed parent handoff visible immediately.
  Date/Author: 2026-09-21, Codex.
- Decision: Phrase the Noul question as a binary predicate with explicit concurrent approval semantics.
  Rationale: The first synthetic approval-plus-heap score missed the action threshold (0.67); the clarified wording scored 0.93 without changing the 85% threshold or history window.
  Date/Author: 2026-09-21, Codex.

## Outcomes & Retrospective

Implementation and required validation are complete. The parent can request input while children continue, continuous child traffic does not reset the parent countdown, and legacy proxy clients retain their contracts. All required checks passed. Publication is explicitly outside this task; deploy the additive endpoint before distributing the client.

## Context and Orientation

`mj-core/src/activity/verdict.rs` defines evidence, parsing and decisions. Its questions JSON is also imported by `services/jev-proxy/src/index.ts`, the hosted TypeSafe gateway. `mj-worker/src/acp/verdict_client.rs` sends requests, records rotating diagnostics and schedules running checks. `mj-worker/src/acp/drive.rs` routes harness notifications; `mj-transcript/src/turn_context.rs` accumulates parent evidence and invalidates answers when prompts or task inventory change. Overall ACP activity remains responsible for liveness and stalls.

## Plan of Work

### Milestone 1: Independent contract

First add independent needs_user_input probability and work_state choice/confidence. At >=85% input need, return AwaitingInput while running or InferIdle after reply regardless of work state. Otherwise running preserves state. After reply only <=15% input need and >=85% work confidence allow background_work to ExpectContinuation or finished to InferIdle. Invalid responses fail closed. Preserve v1/v2 proxy parsing and add /v3/turn-verdict using the same evidence shape as v2.

### Milestone 2: Parent scheduling and handoff

Then track delivered prompts, parent conversation and parent tool activity in a process-local clock. Route native child events before marking it. Use this clock for silence and in-flight freshness, retaining evidence-generation checks and the existing bounded retry schedule. Preserve children when applying input decisions and log assessments, thresholds, proposed decision and actual outcome.

### Milestone 3: Validation and evaluation

Finally test approval plus an independent heap task, conservative decisions, malformed answers, legacy proxy behavior and request parity, child traffic and freshness. Evaluate only synthetic scenarios through Jev and record observed scores separately from deterministic tests.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Run `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. In `services/jev-proxy`, run `npm test`, `npm run check`, and `npm run deploy:dry-run`. Use existing isolated test directories and `--instance input-work-v3` for any live build invocation. Review `git diff --check` and stage only task-owned files before committing on the existing branch.

## Validation and Acceptance

Deterministic tests must show simultaneous high input need and background work leading to input state, retaining running children and controls. Child traffic must allow a silent parent check to run and apply; new parent activity or changed inventory must invalidate pending answers. Uncertainty and endpoint failure must preserve activity. Proxy tests must show v1/v2 unchanged and v3 sharing Rust's question file. Synthetic evaluation records observed probabilities without treating model responses as deterministic assertions.

## Idempotence and Recovery

No migration, remote publication, branch changes or live-store operations are needed. Tests and dry runs can be repeated. Leave unrelated untracked files untouched. Endpoint failures keep runtime activity rather than using an older contract.

## Artifacts and Notes

Validation completed successfully with `cargo test --quiet` (entire workspace, elevated, dev profile), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check`, and proxy `npm test`, `npm run check`, `npm run deploy:dry-run`. The worker library passed 539 tests with 9 preexisting ignored tests. The production-cadence child-traffic test completed successfully in the focused run at 60.18 seconds and again in the full run. The proxy suite passed 19 tests. V2 questions were also byte-compared against the original committed resource.

The first full run exposed a fixture setup omission: the new Claude bridge needed to advertise and acknowledge its required `auto` execution mode. Fixing the fixture made the one-minute routing test pass. A diagnostic test accounts for JSON's floating-point round-trip representation while checking the exact stored f32 scores and fields.

The synthetic evaluation's readable report is `.agents/docs/jev-turn-verdict-v3-evaluation.md`; both raw runs and the repeatable synthetic-only script are retained. No live daemon or default instance was exercised; automated tests retained isolated directories. No migration, relay protocol change, push or deployment occurred.

## Interfaces and Dependencies

Use the existing reqwest client, TypeSafe systemone endpoint, Jev model and proxy tooling. TurnVerdict exposes needs_user_input, work_state and work_state_confidence. WorkState contains BackgroundWork, StillWorking, Finished and Unclear. TurnContext exposes parent activity marking and snapshot access without serialization or relay changes.

Revision: Initial implementation plan records the user's scope and inspected integration points.

Revision: Implemented v3 and runtime integration. Initial synthetic approval-plus-heap input probability was 0.67. Rephrasing Noul as a binary question with explicit concurrent approval semantics produced 0.93 input need and 0.98 background-work confidence. Both question versions and their responses are retained in `.agents/docs/jev-turn-verdict-v3-synthetic-initial.json` and `.agents/docs/jev-turn-verdict-v3-synthetic.json`. No private transcripts were submitted. Proxy tests (19), type checking and deployment dry-run passed; full Rust validation remains in progress.

Revision: Final validation passed, including the corrected Claude bridge fixture and full workspace suite. Prepared the reviewed implementation for commit on master; rollout remains separate.
