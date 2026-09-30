# Improve required-input detection in five measured iterations

This living plan follows `.agents/PLANS.md` and extends the completed `.agents/plans/jev-user-decision-detection.md` experiment.

## Purpose / Big Picture

Jev decides whether the TUI should ask for user input after an assistant reply. The current prompt detects some decision handoffs but misses explicit approvals and external actions. Run five further prompt candidates, measuring all existing cases, then evaluate the best candidate on fresh sessions that were not used to tune it. Preserve correct finished states and avoid false input alerts.

## Progress

- [x] Read prior results, evaluator, prompt, and repository instructions.
- [x] Ask the previously authorized Luna agent to mine fresh session cases.
- [x] Freeze baseline and existing development corpus, credential scan, and run baseline.
- [x] Run and analyze five successive prompt candidates, three repeats per case.
- [x] Freeze round 4 before inspecting fresh-session holdout results.
- [x] Evaluate baseline and selection on eight reviewed, credential-scanned fresh fixtures (seven controls and one ambiguous).
- [x] Adopt round 4, validate 102 fixtures and proxy checks, and document results.
- [x] Commit final prompt, fresh fixtures, and experiment artifacts.

## Surprises & Discoveries

Existing labels include difficult distinctions between optional offers and approval gates. After the user explicitly requested correcting incorrect labels, audit labels from evidence and record each rationale. Preserve original runs; rescore their unchanged responses in new reviewed directories. No evidence or responses are changed. The earlier holdout is now development data because its results are known.

## Decision Log

2026-09-30: Use all 94 existing cases as development/regression data for each of five iterations, with three repeats each. This avoids selecting only known misses. Keep failure, work, background questions and policy thresholds fixed. The user has explicitly authorized credential-scanned fixture evidence and repository/session context to api.typesafe.ai; authorization persists for these comparisons.

2026-09-30: Select by required-input detection subject to no additional confident false input alerts or explicitly harmful actions, and compare finished and total correct actions. Prefer a candidate preserving baseline finished detections; report any tradeoff. Freeze selection before fresh holdout API results. Do not tune on that holdout.

2026-09-30: User corrected the initial decision to preserve known bad labels. Correct N01, N02, N06 (optional followup after answered advice) and S19 (explicit campaign decision); mark S09 ambiguous because the captured evidence lacks planning-mode/approval context. Freeze these reviewed labels before round 3 and rescore baseline/rounds 1–2 with identical response files.

## Outcomes & Retrospective

Five development iterations are complete. Round 4 is frozen for holdout evaluation: 36/54 required detections versus 29/54 baseline, 0/216 false alerts for both, 27/69 finished detections versus 22/69, and the same three listed harmful actions. Round 5 reaches 46/54 but adds three false alerts, so was rejected. Known round-4 losses are one D01 detection, three P13 continues, and one expected-uncertain S43 action. All misses and tradeoffs are in the report.

The initial round 3 encountered HTTP 402 on every request. The user replenished credits and the unchanged round was rerun in round-3-restored. The 283 billing errors (including one diagnostic) remain preserved outside accuracy counts. Label corrections and fail-fast evaluator behavior were committed in 4ff1ec84. Thirteen evaluator tests and three Rust fixture tests pass. Round 4 is adopted locally after the fresh controls passed: zero false alerts and harmful actions among 21 scored controls for both prompts, with one additional finished detection. No fresh required-input cases were found; recall generalization remains unmeasured. All 102 fixtures pass, and proxy tests/type checking pass. No live worker, daemon, or hosted proxy is changed.

## Context and Orientation

The runtime question bundle is `mj-core/src/activity/verdict_questions.json`. The input question chooses none, redundant_request, required, or unclear. The TUI detects required at confidence 0.85; automatic continuation requires 0.90 and independent work/failure conditions. `scripts/jev-scenarios-eval.py` submits evidence to TypeSafe, freezes manifests and snapshots, and reports input and resulting actions. `scripts/jev-scenarios-extract.py` scans credentials. Fixtures live under `mj-core/tests/jev-scenarios/`. New candidate prompts and compact results belong in `.agents/docs/jev-input-five-20260930/`; full raw results remain under `/mnt/optane/mj-jev-input-five-20260930/`.

## Plan of Work

First preserve the current candidate C as this experiment's baseline and freeze existing case IDs. Replay baseline, inspect failures, and write five successive alternative input questions. Each iteration must finish and be analyzed before deciding the next wording. No literal incident matching or case-specific paths should enter prompts. Meanwhile Luna mines new sessions and writes proposed fixtures outside the repository. Review provenance and labels without model results, then freeze them and the candidate selection. Replay baseline and selection on new cases. If evaluation supports adoption, change only the runtime input question and add the fixtures and report. Otherwise retain baseline and document what the five attempts taught us.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, use `python3 scripts/jev-scenarios-eval.py --questions .agents/docs/jev-input-five-20260930/baseline-questions.json --only <frozen IDs> --repeats 3 --output /mnt/optane/mj-jev-input-five-20260930/baseline`. Substitute candidate paths and fresh output directories per run. Use elevated permissions for approved network comparisons. Offline summaries read frozen fixtures and results, and count errors as unsuccessful requests.

## Validation and Acceptance

Compare required detections, false alerts, completed-state detections, total expected actions and explicitly wrong actions, at unchanged thresholds. Repeats measure variability rather than independent cases. Run `python3 scripts/jev-scenarios-eval.test.py`, elevated `cargo test -p brokk-mj-core --test jev_scenarios`, and proxy `npm test` and `npm run check` if adopting a bundle change. These fixture tests do not run a daemon or use a live store. Review JSON, credential scans, non-input axis equality, and `git diff --check`. No Rust source or dependencies are planned, so full Cargo/clippy validation is unnecessary. Commit only task files on the current branch, without pushing.

## Idempotence and Recovery

Each run has a unique output directory and frozen prompt/evidence hashes. Resume only identical runs; preserve errors and use a named retry directory if needed. No production mutations or credentials in artifacts. Leave unrelated work untouched.

## Artifacts and Notes

Prior experiment: `.agents/docs/jev-input-detection-20260930.md`. Current experiment report will be `.agents/docs/jev-input-five-20260930.md`. All five prompt candidates and a selection record will be retained, including rejected approaches.

## Account failure handling

The first blocked round exposed eager submission of every queued fixture despite HTTP 402. The evaluator now admits at most two requests and stops admission on HTTP 401/402, drains those already admitted, writes stopped.json, reports incomplete counts, and exits unsuccessfully. A stopped run cannot be resumed; preserve it and retry in a fresh directory after account recovery. Tests cover both account errors and complete successful repetitions.

## Interfaces and Dependencies

Use existing standard-library evaluator and `jev-latest` TypeSafe endpoint. No new dependency or runtime schema changes. Only the input question text and criteria are candidates for change.

Initial plan created 2026-09-30 for the user's explicit request for five further iterations.

Updated during round 3: documented the user-directed label audit and preserved original comparisons.

Updated after TypeSafe credit exhaustion: recorded the external blocker, preserved all errors, and added bounded submission with explicit incomplete-run reporting.

User replenished TypeSafe credits. Retrying unchanged round 3 in round-3-restored; failed round 3 remains preserved. Label/evaluator checkpoint committed separately.

Five iterations complete. Round 4 selected and frozen in selection.json: 36/54 required detections versus 29/54, zero false input alerts among 216 controls, 27/69 finished versus 22/69. Awaiting fresh-case label review and evaluation; no tuning from that evaluation.

Final evaluation complete: eight fresh cases across disjoint source sessions, seven strict controls and one ambiguous. Adopted round 4 under the frozen criteria; recorded that unseen required-input recall is not established. Validation complete; final prompt and artifacts are committed as the second coherent checkpoint.
