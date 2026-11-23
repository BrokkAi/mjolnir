# Compare Sol 6.2 medium with the saved Sol 6.1 medium evaluation

This living plan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Measure whether one GPT 6.2 Sol reviewer at medium effort improves on GPT 6.1 Sol at medium effort, both alone and together with two GPT 6.1 Terra reviewers at high effort. Reuse the reviewer evaluation's original 70 Mjolnir diffs and saved baseline sessions. The user requested one new reviewer per diff and concurrency 16. The deliverable is a reproducible comparison of confirmed findings, severity, latency, and recorded usage, with a clear account of uncertainty and coverage.

## Progress

- [x] (2026-11-23 23:50Z) Located the original study and its persistent run directory; delegated runner/scoring mapping and baseline/model checks.
- [x] (2026-11-23 23:52Z) Confirmed the runner accepts `gpt-6.2-sol:medium`; reserved unused arm O. Baseline metadata has 140 E Sol/medium sessions and 221 A/B Terra/high sessions across 70 diffs. Independent availability/count checks remain in progress.
- [ ] Register and run one new Sol 6.2 medium review on each original diff, at concurrency 16 (completed: registration, jobs/readiness audit, successful first review and independent input/capture audit; full campaign running, remaining: all 70 completion audits).
- [ ] Audit completed runs, verify findings using the established blinded method, and score old and new configurations consistently.
- [ ] Independently check the comparison, record results, commit, obtain review, and push to origin/master.

## Surprises & Discoveries

The old Sol 6.1 medium arm used two reviewers per diff; the requested Sol 6.2 arm uses one. The analysis must preserve this difference and must not present repeated combinations as independent new reviews. Existing study scripts form reviewer combinations afterward from independently run sessions.

The current comparison round is verify6, published in `results/stage4-hydrogen-medium/`. The established judge was changed after the original study: verify6 uses Neon/medium with the gemini and gemini2 profiles. Arm B exists for only 11 diffs, so readiness must enumerate the actual B jobs rather than assume 70. The persistent study now uses the installed default instance through its `mjx` wrapper, which clears inherited session environment; the retired private instance must not be started.

The original discovery worker's read-only assignment caused automatic approval review to reject its later attempt to register the arm after mailbox reassignment. No edit or new review started. A fresh worker received the complete implementation and campaign authorization in its initial assignment.

The installed 2.74.0 CLI removed `suspend --acknowledge-unpublished-work`. The captured first review was preserved and collected again without starting another review. Removing that obsolete flag at the two carrier cleanup sites in `driver.py` and the ordinary judge cleanup in `verify_driver.py` makes the study compatible with the installed command. Ordinary suspend's dry run reported no blockers, untracked work, Git operations, or Git delta.

## Decision Log

- Decision: Interpret “vs 6.1” as Sol 6.1 at medium effort, and “2xterra/high” as the saved Terra 6.1 high runs.
  Rationale: This holds effort and the companion reviewers constant while changing the requested Sol model version.
  Date/Author: 2026-11-23, parent agent.
- Decision: Reuse the original 70 diffs and baseline reviews, with a separate arm and additive result directory.
  Rationale: This isolates the model change and preserves all prior results. Do not overwrite the old verdicts or raw sessions.
  Date/Author: 2026-11-23, parent agent.
- Decision: Keep concurrency at 16 for reviewer launches and use the existing blinded scoring method.
  Rationale: The user explicitly selected concurrency; a model comparison requires the same scoring standard on both sides.
  Date/Author: 2026-11-23, parent agent.
- Decision: Use new arm O and a fresh verify7 round pooling A, B, E, and O, restricted to `gpt-6.1-terra:high`, `gpt-6.1-sol:medium`, and `gpt-6.2-sol:medium`.
  Rationale: These are precisely the requested configurations. Joint blinded judging gives all four comparisons one verdict set; consistency against verify6 will reveal changes in adjudication. Preserve verify6 unchanged.
  Date/Author: 2026-11-23, parent agent.

## Outcomes & Retrospective

The first review completed under installed Mjolnir 2.74.0 and passed independent audit. Its live configuration confirms `gpt-6.2-sol` at medium effort; historical context matches arms E and N byte for byte. The 57.862-second live-prompt clock matches usage, and inventory was unchanged. Four raw finding bullets await blinded verification. The full campaign is running at concurrency 16, PID 184435 at launch, with durable logs. Completion and the full comparison remain pending. Arm registration checkpoint `b2b743052` passed repository review.

## Context and Orientation

`.agents/evals/review-arms/README.md` describes the experiment. Stage 2 expanded it to 70 diffs and measured the latency/findings Pareto frontier. Stage 3 added Sol 6.1 medium as arm E, two sessions per diff. Stage 4 pooled the original and expanded model arms for blinded verification.

The repository holds scripts, corpus specifications, and published results under `.agents/evals/review-arms/`. Raw session metadata, the persistent evaluation instance, transcripts, and verdicts live under `/mnt/optane/review-arms-20261113`, selected by `REVIEW_ARMS_DIR`. A reviewer arm is one model/effort configuration, assigned a distinct identifier so its saved runs cannot collide with another configuration. A diff is the original code change under review. Blinded verification means a separate judge evaluates pooled findings without being told which model produced them.

`driver.py` launches saved job definitions; `build_verify.py` and `verify_driver.py` prepare and judge pooled findings; `score.py`, `pareto.py`, and `compare.py` produce per-session and combined results. The installed `mjx` 2.74.0 is the current runner executable. The new arm's job IDs come from the exact original 70 IDs in `arm-N-jobs.json`. `--same-image "$REVIEW_ARMS_DIR/arm-O-image.json"` fixes its image for the campaign. One validation run is retained as part of the 70. The detailed instance and scoring commands are being recorded by the campaign owner before execution. No Rust build or installation is required or authorized by this evaluation.

## Plan of Work

First confirm the study's current executable, persistent instance, available model identifier, and complete saved baselines. Register a new arm with one `gpt-6.2-sol` medium reviewer for each of the original 70 diffs, retaining the original prompt and review inputs. Make only the runner or collection changes necessary for this arm and current Mjolnir interfaces. Assign one worker ownership of campaign execution and shared runner edits; other workers may perform read-only checks or analysis on completed outputs.

Run the reviewer jobs at concurrency 16. Retain failures and retry only runs that did not produce an accepted completed review. Audit model and effort, input fidelity, and completion before counting a run. Verify new findings with the existing blinded method and preserve baseline verdicts; if consistent pooling requires a new joint round, explicitly record that choice and compare all requested configurations within that round. Exclude unrelated later experiments that share the raw data directory.

Compute the solo and two-Terra combinations with the established per-diff combination method. Report mean confirmed findings across the corpus, medium/high severity findings, median latency using the slowest member of a combination, and available usage/cost estimates. Use paired resampling of diffs for uncertainty, keeping all sessions belonging to one diff together. One new session per diff limits evidence about within-diff reviewer variability; say so in the result.

## Concrete Steps

All repository commands run from this checkout, with study scripts run from `.agents/evals/review-arms/`. The environment and reviewer commands are:

    export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
    export REVIEW_ARMS_INSTANCE=default
    export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/candidates-all.jsonl"
    export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
    unset REVIEW_ARMS_MJ REVIEW_ARMS_MJ_WORKER REVIEW_ARMS_SCRIPTS REVIEW_ARMS_REPO REVIEW_ARMS_BUNDLE REVIEW_ARMS_REPO_DIR
    R=$REVIEW_ARMS_DIR
    jq 'map(.arm = "O")' "$R/arm-N-jobs.json" > "$R/arm-O-jobs.json"
    jq '[.[] | select(.bug == "diff-05bc8c1e68")] | select(length == 1)' "$R/arm-O-jobs.json" > "$R/arm-O-canary.json"
    jq -s '([.[0][] | select(.arm == "A" or .arm == "B" or .arm == "E")] + .[1])' "$R/stage6-ready-jobs.json" "$R/arm-O-jobs.json" > "$R/stage7-ready-jobs.json"
    python3 -B driver.py --jobs "$R/arm-O-canary.json" --concurrency 1 --same-image "$R/arm-O-image.json"

After the validation run passes, execute the full job list with durable detached logging; the successful first review is skipped by its saved completion record:

    setsid nohup python3 -B driver.py --jobs "$R/arm-O-jobs.json" --concurrency 16 --same-image "$R/arm-O-image.json" > "$R/arm-O-driver.log" 2>&1 < /dev/null &

After 70 audited successful O records, joint verification and scoring use:

    mkdir -p "$R/verify7"
    cp "$R/verify6/SPEC.md" "$R/verify7/SPEC.md"
    python3 -B build_verify.py verify7 --arms A,B,E,O --configs gpt-6.1-terra:high,gpt-6.1-sol:medium,gpt-6.2-sol:medium --ready-jobs "$R/stage7-ready-jobs.json" --bugs-from "$R/arm-O-jobs.json"
    python3 -B verify_driver.py verify7 --kind verify --target localhost --model gemini-4-neon --effort medium --profiles gemini,gemini2 --quota-scope gemini --concurrency 16
    python3 -B score.py verify7
    python3 -B consistency.py verify6 verify7
    python3 -B compare.py verify7 Sol/medium gpt-6.2-sol/medium --severities high,medium,low
    python3 -B compare.py verify7 Sol/medium+Terra/high+Terra/high gpt-6.2-sol/medium+Terra/high+Terra/high Terra/high+Terra/high --severities high,medium,low
    python3 -B compare.py verify7 Sol/medium gpt-6.2-sol/medium --severities high,medium
    python3 -B compare.py verify7 Sol/medium+Terra/high+Terra/high gpt-6.2-sol/medium+Terra/high+Terra/high Terra/high+Terra/high --severities high,medium
    python3 -B pareto.py verify7 --sessions Sol/medium=2,gpt-6.2-sol/medium=1

Run long judge work with the same durable detached logging pattern. Record process IDs and inspect existing state before resuming so that a second driver cannot duplicate accepted work. Do not expose credentials or edit shared profiles unnecessarily.

## Validation and Acceptance

Acceptance requires 70 audited new reviews, one per original diff, and complete matched baseline coverage for all four requested setups. Verify collection completeness and model/effort from actual session metadata. Check that each scored finding maps to the correct review and judge verdict, that combinations union findings rather than double-counting them, and that comparisons use identical diff coverage. An independent worker checks the decisive aggregates and uncertainty calculation from saved outputs.

Validate any script changes with focused syntax checks and the relevant real campaign path. No new tests are needed for plain result documentation. New tricky collection or scoring behavior warrants a targeted regression at the external format boundary. Commit only files changed for this task, request repository review before pushing code/scripts/configuration, and merge upstream if origin/master has moved.

## Idempotence and Recovery

Use a new arm identifier, job file, verification round, and result directory. Keep all old raw runs and results intact. Resume from durable completed metadata after interruption, rather than launching duplicate accepted reviews. Diagnose failed runs from saved metadata and logs before retrying. Stop only daemons or sessions created for this task when cleanup is necessary; preserve the established study instance and baseline evidence.

## Artifacts and Notes

Original study: `.agents/evals/review-arms/README.md`. Original frontier: `.agents/evals/review-arms/results/pareto.md`; expanded frontier: `.agents/evals/review-arms/results/stage4/pareto.md`. Raw data: `/mnt/optane/review-arms-20261113`.

First-run evidence: `$REVIEW_ARMS_DIR/runs/diff-05bc8c1e68/O/`; driver PID 153560 at launch; log `$REVIEW_ARMS_DIR/arm-O-canary.log`. Captured image digest: `sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db`. The live model/effort was observed rather than inferred from arm configuration.

Full campaign log and process receipt: `$REVIEW_ARMS_DIR/arm-O-driver.log` and `$REVIEW_ARMS_DIR/arm-O-driver.pid`. First-run audit: `$REVIEW_ARMS_DIR/arm-O-canary-audit.json`. The changed scripts passed Python syntax parsing, and the repaired cleanup completed against the real installed CLI.

## Interfaces and Dependencies

Use the installed `mj` CLI and the existing evaluation scripts. User credentials remain managed by Mjolnir. Do not change Rust target directories, run Cargo builds, or install a replacement build for this measurement. Model selection must be explicit and checked against the reviewer session's actual model and effort.

Revision note (2026-11-23): Registered the requested model comparison before any new review outputs were collected. Added the reserved arm, baseline metadata counts, executable, joint verification decision, exact commands, and execution ownership after runner inspection. Recorded the rejected read-only-to-write reassignment and the fresh explicitly authorized execution worker.
