# Recognize current user decisions in Jev assessments

This ExecPlan is maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

When an agent hands an unresolved decision to the user, Mjolnir should show that it needs input, even while independent background work continues. The input question must also distinguish such requests from optional offers, rhetorical questions, decisions already answered, and permission for already authorized work. Improve the classifier instruction using diverse real local turns rather than matching one sentence.

The motivating turn is session `2ddbd4ceaa5916f6c58138d97e62d8c1`, ordinal 253087, at 2026-09-30 11:50:00Z (06:50 CDT). The completed reply handed the user a Go-work ownership decision while a benchmark ran. Jev selected required input at confidence 0.41, below the 0.85 attention threshold, and authorized unfinished work at 0.75. Mjolnir kept the background-busy state. Both the current question and the full reply were available; this is a classification problem.

## Progress

- [x] (2026-09-30) Diagnose the motivating turn from read-only worker logs and verify its completed reply was present in the evidence.
- [x] (2026-09-30) Delegate independent local-log mining and fixture labeling to Luna at the user's explicit request.
- [x] (2026-09-30) Add 17 diverse real cases from 11 sessions: seven required-input cases, seven nonrequests, and three explicitly unscored ambiguous cases. Remove a duplicate of existing S35 before evaluation. Freeze ten tuning and seven held-out cases by source session in `.agents/docs/jev-input-detection-20260930/split.json`.
- [x] (2026-09-30) Add reproducible prompt selection, frozen prompt/fixture snapshots and evidence hashes, and separate input-axis metrics to the live evaluator; ten offline behavior tests pass.
- [x] (2026-09-30) Prepare two general candidate prompts and the exact historical incident bundle. Freeze baseline and tuning comparison payloads under `/mnt/optane/mj-jev-input-20260930/` without sending requests.
- [x] (2026-09-30) Validate the final 94-case corpus with all three Rust scenario tests, rescan new fixtures for credentials, and exercise the extractor against the real incident using its repaired Mjolnir worker-directory default.
- [ ] Compare the current instructions with candidate instructions on tuning cases; select one candidate before evaluating held-out cases.
- [ ] Compare the selected candidate and baseline on held-out cases and the existing suite, keeping attention and automation thresholds fixed.
- [ ] Adopt a candidate only after live acceptance, validate the shared proxy bundle if it changes, and commit the measured prompt result. The validated corpus/evaluator preparation is a coherent checkpoint ready to commit.

## Surprises & Discoveries

The current input instruction asks whether already requested work can continue before the user responds, then separately says background work can coexist with required input. That wording may make an explicit user decision appear optional when another operation can still continue. The none criterion also includes plans, while the required instruction says unapproved plans need approval; the revision must make precedence and context clear.

The live evaluator currently reports agreement across three axes together, which obscures improvements to input recognition. It also resumes results by fixture ID and repetition alone; without verifying the prompt and evidence, a comparison could silently mix different experiments. The extraction script defaults to an older `hel` worker directory, so Luna imports it and points its existing WORKERS constant at the actual Mjolnir directory.

The installed incident worker used an older three-axis question bundle, not the current four-axis baseline. Preserve both so a later report does not attribute its recorded 0.41 confidence to the current prompt. One mined artifact turn was already fixture S35 and initially acquired a contradictory label; remove the duplicate before live evaluation. Full-reply review also corrected a false unfinished-work label when the reply explicitly reported the requested samples ready. Three child/scope cases remain unscored because the request's intended recipient or authorization is ambiguous.

## Decision Log

Use independent labels based on current reply and chronological user instructions, never the recorded Jev choice. Preserve exact recorded evidence when available and identify transcript reconstructions and unknown runtime facts explicitly. Keep credentials out of fixtures with the extractor's scanner. Ambiguous input labels do not enter the strict new comparison.

Separate new tuning and held-out cases by source session, keeping adjacent turns together. Reuse the existing suite as a broader regression set. Do not tune again after seeing held-out candidate results; report any remaining failures. Do not change the 0.85 attention or 0.90 automation thresholds, add phrase-matching activity rules, or change the evidence schema.

The user explicitly authorized Luna mining and prompt iteration. Live TypeSafe requests use the existing evaluator and key resolution without logging credentials. Read the default-instance database only with SQLite `mode=ro`; do not launch, replace, or stop its daemon or workers. Repository edits remain on the current branch. No release or remote publication was requested.

Automatic approval review rejected the first live-evaluation command because it would disclose local-log conversation evidence to TypeSafe without explicit disclosure consent. Do not retry or use an alternate execution path until consent arrives. An asynchronous request is pending, covering credential-scanned user instructions and assistant replies from existing and newly mined fixtures, including repository/session context. Local fixture and tooling validation is complete.

## Outcomes & Retrospective

Local preparation is complete: 17 new cases from 11 sessions extend the suite from 77 to 94, with session-separated tuning and held-out groups; ten evaluator tests and three Rust fixture tests pass, and the incident extraction smoke check succeeds. Live model comparison and selection remain pending disclosure consent. No improvement has been measured or claimed. The eventual report will record correct required-input detections above 0.85, false alerts on controls, action regressions, request errors, and remaining misses for both prompts. Improved wording is accepted only with measured benefit and no new confident control failures attributable to it.

## Context and Orientation

`mj-core/src/activity/verdict_questions.json` is the current shared question bundle: Rust direct clients embed it and `services/jev-proxy/src/index.ts` forwards it on `/v6/turn-verdict`. Older v1-v5 files remain frozen. This work changes the wording of the input question, not its choice names or wire format.

`mj-core/tests/jev-scenarios/*.json` contains recorded and reconstructed real turns. `mj-core/tests/jev_scenarios.rs` validates fixture structure, checks that recorded verdicts do not produce listed harmful actions, and checks quietness when process facts were recorded. It does not invoke a live model. New fixtures use IDs D01 onward, and carry evidence, source session and time, independent expected axes, outcomes, and follow-up context.

`scripts/jev-scenarios-extract.py` reads worker decisions and the controller transcript with SQLite in read-only mode, builds bounded authorization history, and refuses credential-shaped text. `scripts/jev-scenarios-eval.py` sends evidence and selected questions to Jev, derives the deterministic action, and writes results and a report. Offline tests are in `scripts/jev-scenarios-eval.test.py`.

## Plan of Work

First have Luna inspect local worker decisions and controller transcripts for current unanswered choices, missing facts, approvals, and reviews, plus counterexamples. Review its labels against the original conversation and choose a session-separated split. The motivating incident is a tuning case. Preserve full evidence and provenance; do not rewrite examples into the desired wording.

Extend the existing evaluator with `--questions` so every candidate is a normal shared question JSON file. Save and check the chosen prompt and serialized evidence identities before resuming a run. Report input choice agreement and required-input detection at the actual 0.85 threshold separately from whole-verdict agreement and final actions. Include controls classified as required above threshold, and action regressions. Add offline tests that exercise reporting and refuse mixed-prompt or mixed-evidence resumes.

Copy the baseline questions to `/mnt/optane/mj-jev-input-20260930/baseline-questions.json`. Candidate prompts also live under that artifact directory while being compared. Evaluate baseline and candidates on tuning cases only. Rewrite the input question around a current unanswered request directed at the user, independent of other ongoing work; treat declarative choices and approval gates as requests, but use chronological authorization to separate redundant permission, already answered questions, autonomous choices, and optional new scope. Prefer a short general distinction over incident-specific vocabulary. Select the candidate from tuning results and freeze it, then compare baseline and selected candidate on held-out cases and the existing suite in three repetitions. Report all errors and uncertain answers rather than treating them as successes.

After acceptance, copy the selected input instructions and criteria into the shared current bundle. Preserve the other axes. Update `.agents/docs/jev.md` with the general rule and evaluation evidence; save a compact experiment record and split under `.agents/docs/`. Raw results remain outside Git under `/mnt/optane/mj-jev-input-20260930/`.

## Concrete Steps

Run from `/home/jonathan/Projects/mjolnir`. Use the existing Cargo/mbx configuration without redirecting build output.

    python3 scripts/jev-scenarios-eval.test.py
    python3 scripts/jev-scenarios-eval.py --questions /mnt/optane/mj-jev-input-20260930/baseline-questions.json --only <tuning IDs> --repeats 3 --output /mnt/optane/mj-jev-input-20260930/baseline-tuning
    python3 scripts/jev-scenarios-eval.py --questions <candidate file> --only <tuning IDs> --repeats 3 --output /mnt/optane/mj-jev-input-20260930/candidate-tuning

The report should show separate input detections and false input detections. Once the candidate is selected, run the same commands using held-out IDs and new output directories, then use the existing suite IDs for the regression comparison. Live evaluation may require elevated network permissions; it reads the existing key without printing it.

Run fixture validation outside the restricted sandbox:

    cargo test -p brokk-mj-core --test jev_scenarios

For the shared proxy bundle run, from `services/jev-proxy`:

    npm test
    npm run check
    npm run deploy:dry-run

Finish with `git diff --check` and review the exact staged diff. Stage only this task's files and commit on the current branch; other people have ongoing edits in storage, daemon APIs, and delegation accounting.

## Validation and Acceptance

The new real cases must span multiple independent sessions and include explicit choices without question marks, choices alongside background work, missing information or review, and nonrequest controls. Required-input cases pass when the input choice is required at confidence at least 0.85; controls must not become required at that threshold. Redundant permission must remain distinct from genuine missing approval. Evaluate three repetitions per case and report both choice and action behavior. The selected prompt should improve required-input detection on tuning and held-out cases without adding confident required-input failures on controls or harmful automatic actions in the broader suite.

The deterministic fixture suite, offline evaluator tests, and proxy tests and checks must pass. A report must make remaining misses visible. A successful prompt does not retroactively update installed workers' embedded prompts or publish the hosted proxy; that rollout follows the repository's ordinary release/publication workflow.

## Idempotence and Recovery

Read-only mining cannot affect live work. Fixture creation uses unique IDs and credential checks. Each experiment uses a separate output directory, and resume checks prevent mixing changed questions or evidence. Raw experiment data is outside the repository. The prompt edit is reversible; no schema migration, daemon restart, or process teardown is required.

## Artifacts and Notes

The original incident is in `/home/jonathan/.local/share/mjolnir/workers/2ddbd4ceaa5916f6c58138d97e62d8c1/worker.log` lines 31-33 and `jev-decisions/decisions.0.jsonl` record 114. It selected required input at 0.41 and unfinished work at 0.75; the completion had no foreground tools and one background command. Store only credential-scanned source evidence in fixtures.

## Interfaces and Dependencies

No new runtime dependency, crate, wire field, or deterministic policy is needed. The Python evaluator uses standard-library JSON, SHA-256, HTTP, and unittest facilities. Preserve the existing CLI defaults; `--questions PATH` selects an alternative bundle for a reproducible comparison. Existing action-policy tests continue to pin the Python action port to Rust behavior.

Revision note (2026-09-30): created for the explicitly requested Luna mining and measured prompt iteration; updated with the finalized corpus, source-session split, local validation, exact historical/current question distinction, and the disclosure-approval blocker. Scope remains user-decision detection and its test corpus, independent of the earlier quiet/lifecycle plan.
