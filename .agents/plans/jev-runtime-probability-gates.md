# Apply the measured Jev probability gates

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Jev should flag a model's request for a user decision when its selected `required` answer has probability at least 0.50 and at least 2.5 times the next most probable input choice. Completed work should qualify at winning probability 0.80. Provider confidence is a different statistic; recovery, continuation, waiting, no-input, and background gates retain their existing confidence requirements. The user approved these gates after the experiment documented in `.agents/docs/jev-probability-gates-20260930.md`.

## Progress

- [x] (2026-09-30) Trace assessment parsing, activity decisions, evaluator, and hosted proxy.
- [x] (2026-09-30) Preserve and validate distributions; implement the shared policy and proxy transport.
- [x] (2026-09-30) Add shared boundary, running-turn, completed-turn, malformed-response, and compatibility checks; Python and proxy checks pass. Full `cargo test --quiet` and all-target Clippy passed.
- [x] (2026-09-30) Update evaluator and documentation; validate Python/TypeScript, deploy the proxy at the user's request, and verify live synthetic responses.
- [x] (2026-09-30) Clippy passed with `--all-targets -- -D warnings`.
- [x] (2026-09-30) Full `cargo test --quiet` passed, including isolated upgrade regressions and doctests; changes are prepared for the required commit on master.

## Surprises & Discoveries

The hosted proxy strips distributions today, and running-turn activity uses a separate confidence predicate. Both must change for the gates to reach the TUI. Durable assessments also contain older judgments without distributions; their saved action remains authoritative and must remain readable.

## Decision Log

- Decision: Retain full probability maps in judgments, with an absent map accepted only when reading old durable records. Require valid distributions in fresh API responses. Preserve the frozen v5 proxy route and add distribution forwarding to v6.
  Rationale: No invented conversion from confidence to probability, and older clients can ignore additive response fields. No SQL schema or protocol request changes are needed; saved actions remain authoritative.
  Date/Author: 2026-09-30, Codex.
- Decision: Freeze the old offline comparison policy explicitly and identify new evaluator runs by policy version.
  Rationale: Historical measurements must stay reproducible after the production policy changes.
  Date/Author: 2026-09-30, Codex.

## Outcomes & Retrospective

Implementation is complete. The updated evaluator reproduces all approved counts on the 612 saved responses. Nineteen Python evaluator tests, four historical sweep tests, and the proxy tests/typecheck pass. Full `cargo test --quiet` and all-target Clippy passed. The user subsequently requested proxy deployment during the Rust suite. Wrangler deployed version `14ca6393-f397-40c7-a1c2-615c9bb32b6e`; live synthetic verification passed for v6 decisions/completion, v5 compatibility, and malformed requests. No live daemon replacement, mj release, or push is part of this change.

## Context and Orientation

`mj-core/src/assessment.rs` owns the semantic action for a completed turn. Its judgment currently contains only a choice and confidence. `mj-core/src/activity/verdict.rs` parses running-turn responses and chooses a UI decision. `mj-worker/src/relay/verdict.rs` stores completed assessments and admits activity changes against current process facts. The hosted service in `services/jev-proxy/src/index.ts` forwards only approved fields from TypeSafe. `scripts/jev-scenarios-eval.py` mirrors the action policy for the recorded scenario suite. Existing comparison tools under `.agents/docs/jev-gates-20260930/` evaluate frozen historical responses.

## Plan of Work

First add validated probability maps to the assessment judgment and central required-input predicate. Use that predicate for both running and completed turns; running turns still cannot infer completion or initiate recovery. Finished actions use the winning work probability while failure-none and input-none confidence gates remain. Keep legacy v3/v4 activity decoding unchanged. Update v6 proxy forwarding and tests without changing frozen v5 responses. Update fake API responses with complete realistic distributions and exercise malformed inputs. Then update evaluator policy, frozen historical analysis, diagnostics, and architecture notes.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir`. Run `cargo fmt --all`, `python3 scripts/jev-scenarios-eval.test.py`, and `python3 .agents/docs/jev-gates-20260930/test_sweep.py`. In `services/jev-proxy`, run `npm test` and `npm run check`. Run `cargo test` outside the sandbox and `cargo clippy --all-targets -- -D warnings` on the normal dev profile with normal mbx storage. Inspect the diff and commit only changed task files on the current branch.

## Validation and Acceptance

A low-confidence but 2.5-times-leading required answer produces AwaitInput in both phases. A winner below 0.50 or ratio below 2.5 does not. A finished probability of 0.80 qualifies despite lower confidence, but uncertain failure or no-input confidence blocks completion. Waiting and automatic continuation retain their previous cutoffs. Invalid distributions fail visibly; old stored judgments deserialize and retain saved actions. Proxy tests prove the full distribution survives the HTTP response and provider diagnostics remain filtered. Existing isolated worker tests prove decisions are applied through normal admission.

## Idempotence and Recovery

Tests use existing isolated stores and local fake HTTP services. Do not start a new build against the default instance. Changes are additive to serialized judgments and do not migrate or touch live data. Tests and formatting are repeatable. Failed validation is fixed before commit; unrelated changes are excluded.

## Artifacts and Notes

The prior two-run replay contains 612 responses in `/mnt/optane/mj-jev-gates-20260930/`. The approved gates detected 102 of 108 required replies, with 13 of 474 control replies falsely flagged; finished detection reached 104 of 174 with the same six false completions as baseline. These are curated replay results, not production accuracy estimates.

## Interfaces and Dependencies

Use existing serde and BTreeMap support, without new dependencies. `Verdict::action` borrows its verdict; `Verdict::requires_input` owns the probability decision. Fresh response parsing validates complete named distributions, finite bounded values, approximate normalization (hundredth rounding), and a selected maximum. Missing distributions in old persisted judgments do not qualify for new probability gates.

Revision: created after tracing the runtime and proxy, before implementation.

Revision: implemented full distributions and shared gates, added 16 common cases, and confirmed the replay counts without new API requests.

Revision: the user explicitly authorized deploying the proxy while the Rust suite runs. The validated bundle has been published and all four live synthetic smoke checks passed.

Revision: full dev-profile workspace tests and Clippy completed successfully. The controller suite passed 2,074 tests (9 ignored), core passed 585, and worker passed 696 (10 ignored); all remaining workspace, integration, upgrade, and documentation tests passed. Validation was slow because the schema regression spent time synchronizing temporary SQLite journals, but it progressed and finished. No build storage or host configuration was changed.
