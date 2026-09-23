# Compare Jev evidence on thirty turn-level cases

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

Determine whether limiting Jev to the latest delivered user request, removing tool calls, or selecting relevant calls improves activity classification. Produce a reproducible offline comparison and implement the user-selected policy after the results are reviewed. A case is a turn at a specific historical classification frontier, not a whole session.

## Progress

- [x] (2026-09-20) Agreed thirty real cases, one run per variant, and Luna mining.
- [x] (2026-09-20) Mined sixty candidates; strict evidence admits four evaluation points across two turns. Curated twenty additional unscored turns at user request.
- [x] (2026-09-20) Built shared-projection harness; eight Python behavior tests, 97 transcript tests and two example tests pass.
- [x] (2026-09-20) Ran four variants once on four strict points and six controls: fifty successful requests. No held-out claims are possible.
- [x] (2026-09-20) Final policy, documentation and validation complete; publishing the final commit to the authorized upstream.

## Surprises & Discoveries

The local store has 707 recorded turn starts across 458 sessions. Exact Jev evidence logs exist only for bifrost2; relay journals cover a small subset. Current materialized tool/message rows may contain later updates, so timestamps/frontiers must be validated before any historical reconstruction. Archives preserve checkpoints, not intermediate event streams.

## Decision Log

The user chose thirty curated turn cases and one request per variant, rather than repeated trials. The fourth control is current production evidence. Variant 0 uses the latest delivered user message onward; 0+1 removes all tool entries; 0+2 asks Jev for per-call relevance scores and keeps scores at least 0.5. Runtime facts and classifier questions remain unchanged. The user explicitly authorized Luna and TypeSafe evidence processing; previous push authorization persists. Raw corpus artifacts remain local. No deployment is included. The subsequent user choice of 0+1 authorizes the production evidence-selection change described in the final decision below.

## Context and Orientation

`mj-transcript/src/summary.rs` owns the shared deterministic projection and rendering. `mj-core/src/activity/verdict.rs` defines evidence, questions, thresholds and decisions. Extend offline tooling with `mj-transcript/examples/jev_evidence_probe.rs` using the existing projection, and `scripts/jev-evidence-experiment.py` for case validation, requests and reports. Store raw cases/results under `/mnt/optane/mj-jev-turn-experiment-20260920`, and findings under `.agents/docs/`.

## Plan of Work

First Luna mines roughly sixty turn candidates read-only, with session/command identity, user boundary, evaluation frontier/time, phase, source provenance, proposed class and reasoning. Review thirty reconstructable cases across completion, background work, user requests, intermediate progress and ambiguity. Never use a later-mutated row as historical evidence; reject unavailable conversation history and explicitly mark unknown runtime facts. Keep session groups together in ten development and twenty held-out cases. Include bifrost2 and six separately identified synthetic controls.

Second build an offline Rust example that applies production projection and structured selection before rendering. Python orchestrates baseline, scoped, no-tools, and selected variants. Selection asks one Noul relevance question per stable tool ID in one first-stage request; scores >=0.5 retain existing projected content. No rewritten summaries or scores enter the classifier. Bound requests to 64 KiB, record failures instead of changing variants, shuffle jobs, use concurrency two, and never silently retry completed experiments.

Finally run each variant once, independently, using fixed questions and threshold. Thirty real cases require 150 requests and six controls thirty more. Record raw answers, IDs/scores, request bytes, token usage, errors, both stages' latency, class accuracy, wrong high-confidence decisions, abstentions, and production actions. Separate running/replied and development/held-out results. Recommend the simplest supported strategy without selecting on confidence alone.

## Concrete Steps

From repository root, build the offline example with `cargo build -p brokk-mj-transcript --example jev_evidence_probe`. Run `python3 scripts/jev-evidence-experiment.py --help` for preparation/replay/report commands. Run Python behavior tests, `cargo test --no-fail-fast` outside the sandbox, `cargo clippy --all-targets -- -D warnings`, rustfmt check and diff check. Store Cargo logs in target. No daemon/TUI/live CLI invocation is needed; any such test must use `--instance jev-turn-experiment`.

## Validation and Acceptance

Prove delivered-user versus queued boundaries, filtering before budgeting, late-update exclusion, selector IDs/probabilities and failure recording, unchanged runtime evidence, Unicode and payload limits. Review all labels before querying Jev. Freeze prompts before held-out requests. Every included point must have auditable source provenance. Report any corpus shortfall or unsupported evidence honestly rather than inventing facts.

## Idempotence and Recovery

Open live SQLite read-only and never open the live worker for replay mutation. Work from copied artifacts for experiments. Persist per-job results and input hashes; resume only missing jobs with the identical frozen manifest. Keep unrelated untracked files untouched and commit only task changes on current master. Do not deploy.

## Artifacts and Notes

Previous experiments are in `/mnt/optane/mj-jev-experiments-20260920`; their 99% result used manually written factual summaries, not automatic name extraction. The new comparison must not assume that result generalizes.

## Interfaces and Dependencies

The Rust example accepts canonical snapshot JSON plus mode, optional selected tool IDs and byte limit, and returns rendered text with candidate IDs and prompt/assistant tails. Python uses the standard library, existing bundled classifier questions and the existing TypeSafe key resolution convention. Selector questions use the existing Noul response shape. No new crate, dependency, wire endpoint or database migration is required.

## Outcomes & Retrospective

The strict pilot favors 0+1: all four real points exceed the unchanged confidence threshold versus two for 0 and three for 0+2. Post-reply completion reaches 97%, 97%, and 98% respectively. All variants classify the six controls correctly. Fifty requests succeeded, with no production changes. The user subsequently authorized scoring the twenty next-best turns with explicitly unknown runtime facts. In that separate exploratory set, 0+1 matches all eighteen unambiguous labels versus sixteen for each other variant; no variant produced a wrong high-confidence category. See `.agents/docs/jev-turn-evidence-comparison-20260920.md` for provenance, limitations, timing and reproducible commands.

2026-09-20 corpus feasibility update: API history does not persist exact Jev runtime counters, and retained relay journals mostly begin after checkpoint trimming. Offered the user a smaller strict corpus versus thirty turns with explicitly unknown runtime facts; proceeding with the recommended latter assumption while preserving the option to revise if steered. These are conversation-evidence comparisons, not exact production replays; no unknown count is replaced with zero. The original bifrost2 evidence retains its captured runtime values.

2026-09-20 user correction: include ONLY cases with complete runtime evidence. The proposed unknown-facts alternative is rejected. Strictly exclude all incomplete candidates, use a smaller corpus if necessary, and do not claim the thirty-case target was met.

Final validation: dev-profile `cargo test --no-fail-fast --quiet` passed all workspace targets; `cargo clippy --all-targets -- -D warnings` passed. The focused transcript/example run passed 97 + 2 tests, and eight Python offline behavior tests passed. Rustfmt and diff checks passed. Logs are `target/jev-turn-final-tests.log`, `target/jev-turn-final-clippy.log`, and `target/jev-turn-focused-tests.log`. The shared renderer now exposes retained entry IDs with identical text to avoid relevance questions about budget-omitted calls.

2026-09-20 additional user instruction: score the next-best twenty turns too. This explicitly expands the experiment beyond the strict subset. Each additional payload marks silent_for_s, tools_in_flight, background_commands and queued_commands as null/unknown, never zero. Keep results separate from the strict subset. Reconstruct timestamp-checked transcript rows and review final-message labels; ranks 1 and 18 have ambiguous expected categories and are excluded from primary accuracy counts. The additional twenty run once per variant (100 HTTP requests), without a held-out claim. Python validation now also tests explicit unknown preservation.

Additional comparison complete: 100 requests succeeded, bringing the total to 150. Two premarked ambiguous labels are excluded from accuracy. 0+1 matched 18/18 labels with 17 above threshold, versus 16/18 and 15 above threshold for 0 and 0+2. One completion case favors retaining tools (0 at 89% versus 0+1 at 58%); documented rather than hidden. No real user-input cases or held-out split are available. Eight Python tests pass, including missing-fact and ambiguity handling.

2026-09-20 policy decision: the user selected 0+1 after the strict and exploratory results. Implemented latest-delivered-user conversation with transcript tool entries filtered before rendering; live runtime fields stay unchanged. The production collector and offline no-tools adapter share `TranscriptSummary::latest_user_messages`. Inspection also found confirmed steering did not previously update TurnContext; a shared delivered-prompt identifier now handles started prompts and confirmed steering in both durable collection and startup replay. Added behavior tests for preserved live tools, large noise removal, queued-versus-delivered steering and reopen. Final validation is being repeated for this implementation. No proxy deploy or live worker replacement is authorized by this policy choice.

Final adopted-policy validation: 98 transcript unit tests and two offline example tests passed; the corrected complete worker library passed 531 tests with 9 ignored. The concurrently running workspace binary retained the earlier invalid test-ID fixture and failed only that test; all its other targets passed, and the rebuilt worker suite supplies validation of the corrected fixture. Final all-target clippy passed. The new shared selection produces byte-for-byte identical payloads to all 30 evaluated 0+1 cases (four strict points, six controls, twenty exploratory turns), checked offline without further API calls. Eight Python tests and formatting/diff checks pass. Logs: `target/jev-policy-tests.log`, `target/jev-policy-worker-final-tests.log`, `target/jev-policy-focused-tests.log`, `target/jev-policy-final-clippy.log`.
