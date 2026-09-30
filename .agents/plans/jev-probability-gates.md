# Measure probability-based Jev gates

This living plan follows `.agents/PLANS.md`.

## Purpose

Determine whether comparing the winning Jev probability with its runner-up improves useful input and finished classifications. The user requested a 2x comparison and then several variations. Produce measured tradeoffs and a recommendation using unchanged prompts and identical API responses. Do not change runtime gates during the experiment.

## Progress

- [x] Verify the official API returns distributions and confidence as distinct fields.
- [x] Preserve raw answers in the evaluator; test that probability and confidence remain distinct.
- [x] Replay all 102 credential-scanned fixtures three times under the current frozen prompt.
- [x] Compare the current gate, ratios 2/3/4, probability floors, combined odds, and separate input-only versus activity/all gates.
- [x] Sweep 113 settings across five gate families; measure input and finished states independently.
- [x] Compare five-fold session-grouped selection; freeze finalists, run a fresh three-repeat replay, and report post-confirmation exploratory ranking.
- [x] Document results and validate the analysis.
- [x] Commit evaluator/artifacts.

## Context

`scripts/jev-scenarios-eval.py` submits previously authorized credential-scanned evidence to api.typesafe.ai. It used to discard `answers.*.probabilities`; it now preserves complete typed answers alongside the old compact verdict. TypeSafe documents confidence as a statistic derived from the distribution, not the winner's probability. Current activity gates use confidence 0.85; automatic continuation/recovery use 0.90. The exact runtime policy is mirrored in that evaluator. Worker admission still applies after classification, so these are policy comparisons rather than a live TUI simulation.

The frozen replay is `/mnt/optane/mj-jev-gates-20260930/replay/`: 306 successful responses, 54 scored required-input requests, 237 controls and 15 ambiguous requests. Correct-finished cases account for 87 requests. `.agents/docs/jev-gates-20260930/compare.py` reproduces the first comparisons. Full API answers stay outside Git; compact artifacts and source scripts belong in that same repository documentation directory.

## Plan of Work

First retain distributions and use the existing evaluator to get an apples-to-apples replay. Then extend offline analysis across confidence cutoffs, winning-probability floors, probability gaps, and winner/runner ratios with optional floors. Separate input-alert gates from finished-state gates and leave automation unchanged in those comparisons. For finished states, use the existing input and failure gates to isolate what changing the work gate does; separately show changing both activity gates. Never call a running-turn finished inference an actual runtime action: report replied-turn harmful finishes separately.

Freeze a deterministic five-fold source-session split. Keep every repeat and every case from a session together, and keep synthetic related H cases together. Select parameters on four folds, evaluate on the fifth, and pool the held-out predictions. Compare explicitly stated training false-alert budgets rather than hiding the tradeoff in one arbitrary score. These folds are exploratory because earlier comparisons already inspected the corpus; they are not an untouched independent validation set.

## Validation and Acceptance

Run `python3 scripts/jev-scenarios-eval.test.py`; the new response-retention test must preserve confidence 0.42 and winner probability 0.8 separately. Assert every response has valid probability keys and approximate unit sum, every repeated case stays in its session fold, and complete-response counts match the frozen replay. Check numerical boundaries and that the baseline action equals the existing evaluator. Review `git diff --check` and commit only task files on the current branch. No Rust or dependency change is planned; no Cargo build or daemon is required.

## Surprises & Discoveries

A fresh replay produces one false input alert under the current gate despite zero in the previous run. Wording and sampling variability matter near the cutoff. The 2x gate detects all 54 required requests but produces 12 false input alerts. Applied to finished inference too, it raises correct finishes from 31/87 to 63/87, but replied-turn harmful finishes from 3 to 17. It is not a uniform replacement for every gate.

## Decision Log

2026-09-30: Preserve full answers going forward; the compact verdict cannot reconstruct runner-up probability. Reuse one response set across all candidate gates so sampling variability cannot masquerade as a policy gain. Keep prompts and labels fixed.

2026-09-30: User requested several variations. Broaden offline comparison, measure input and finished separately, and use grouped cross-validation with explicit error budgets. Report the selection bias remaining from this previously inspected corpus.

## Outcomes & Retrospective

Two 306-request replays completed successfully with identical prompts and evidence. The 2.5x input ratio plus probability >= 0.50 detects 102/108 required replies with 13/474 false alerts; baseline detects 71/108 with one false alert. Finished probability >= 0.80, preserving the other gates, detects 104/174 completed cases versus 63/174 baseline, with the same six existing A03 false finishes. The 0.70 confidence candidate introduced two extra false finishes on the second replay and is not the final recommendation. Fourteen evaluator tests and four analysis tests pass. Runtime gates are unchanged.

2026-09-30: After confirmation exposed additional false finishes under work confidence 0.70, compare the full original grid on both runs and recommend work probability 0.80. This final ranking is exploratory; the second run is not an independent holdout for it. Both raw runs and the narrower frozen finalist list are retained.

## Recovery and Artifacts

All analysis is offline and repeatable from frozen replay files. Do not overwrite API responses or labels. Save gate definitions, grouped splits, compact metrics, and the report under `.agents/docs/jev-gates-20260930/`. Commit after validation without pushing.

Updated after both replays: recorded final tradeoffs, the confirmation failure of the initial finished candidate, and the exploratory nature of the final recommendation.
