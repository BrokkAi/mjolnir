# Compare Sol 6.2 medium with the saved Sol 6.1 medium evaluation

This living plan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Measure whether one GPT 6.2 Sol reviewer at medium effort improves on GPT 6.1 Sol at medium effort, both alone and together with two GPT 6.1 Terra reviewers at high effort. Reuse the reviewer evaluation's original 70 Mjolnir diffs and saved baseline sessions. The user requested one new reviewer per diff and concurrency 16. The deliverable is a reproducible comparison of confirmed findings, severity, latency, and recorded usage, with a clear account of uncertainty and coverage.

## Progress

- [x] (2026-11-23 23:50Z) Located the original study and its persistent run directory; delegated runner/scoring mapping and baseline/model checks.
- [ ] Confirm exact model identifiers, baseline coverage, and campaign commands.
- [ ] Register and run one new Sol 6.2 medium review on each original diff, at concurrency 16.
- [ ] Audit completed runs, verify findings using the established blinded method, and score old and new configurations consistently.
- [ ] Independently check the comparison, record results, commit, obtain review, and push to origin/master.

## Surprises & Discoveries

The old Sol 6.1 medium arm used two reviewers per diff; the requested Sol 6.2 arm uses one. The analysis must preserve this difference and must not present repeated combinations as independent new reviews. Existing study scripts form reviewer combinations afterward from independently run sessions.

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

## Outcomes & Retrospective

Pending. No new evaluation runs have started.

## Context and Orientation

`.agents/evals/review-arms/README.md` describes the experiment. Stage 2 expanded it to 70 diffs and measured the latency/findings Pareto frontier. Stage 3 added Sol 6.1 medium as arm E, two sessions per diff. Stage 4 pooled the original and expanded model arms for blinded verification.

The repository holds scripts, corpus specifications, and published results under `.agents/evals/review-arms/`. Raw session metadata, the persistent evaluation instance, transcripts, and verdicts live under `/mnt/optane/review-arms-20261113`, selected by `REVIEW_ARMS_DIR`. A reviewer arm is one model/effort configuration, assigned a distinct identifier so its saved runs cannot collide with another configuration. A diff is the original code change under review. Blinded verification means a separate judge evaluates pooled findings without being told which model produced them.

`driver.py` launches saved job definitions; `build_verify.py` and `verify_driver.py` prepare and judge pooled findings; `score.py`, `pareto.py`, and `compare.py` produce per-session and combined results. Their exact current interfaces and instance settings are being checked before execution. No Rust build or installation is required or authorized by this evaluation.

## Plan of Work

First confirm the study's current executable, persistent instance, available model identifier, and complete saved baselines. Register a new arm with one `gpt-6.2-sol` medium reviewer for each of the original 70 diffs, retaining the original prompt and review inputs. Make only the runner or collection changes necessary for this arm and current Mjolnir interfaces. Assign one worker ownership of campaign execution and shared runner edits; other workers may perform read-only checks or analysis on completed outputs.

Run the reviewer jobs at concurrency 16. Retain failures and retry only runs that did not produce an accepted completed review. Audit model and effort, input fidelity, and completion before counting a run. Verify new findings with the existing blinded method and preserve baseline verdicts; if consistent pooling requires a new joint round, explicitly record that choice and compare all requested configurations within that round. Exclude unrelated later experiments that share the raw data directory.

Compute the solo and two-Terra combinations with the established per-diff combination method. Report mean confirmed findings across the corpus, medium/high severity findings, median latency using the slowest member of a combination, and available usage/cost estimates. Use paired resampling of diffs for uncertainty, keeping all sessions belonging to one diff together. One new session per diff limits evidence about within-diff reviewer variability; say so in the result.

## Concrete Steps

All repository commands run from this checkout, with study scripts run from `.agents/evals/review-arms/` and `REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113`. The historical runner interface is:

    python3 driver.py --jobs <new-arm-jobs.json> --concurrency 16

The historical scoring interfaces are:

    python3 build_verify.py <new-verification-round> --arms <selected-arms> --ready-jobs <selected-jobs.json>
    python3 verify_driver.py <new-verification-round> --concurrency 16
    python3 score.py <new-verification-round>
    python3 compare.py <new-verification-round> <old-solo> <new-solo> <old-plus-two-terra> <new-plus-two-terra>

Replace these placeholders with the confirmed exact commands in this plan before launching. Long-running drivers must persist beyond a tool call; use the study's documented detached execution and save logs in the raw campaign directory. Do not expose credentials or edit shared profiles unnecessarily.

## Validation and Acceptance

Acceptance requires 70 audited new reviews, one per original diff, and complete matched baseline coverage for all four requested setups. Verify collection completeness and model/effort from actual session metadata. Check that each scored finding maps to the correct review and judge verdict, that combinations union findings rather than double-counting them, and that comparisons use identical diff coverage. An independent worker checks the decisive aggregates and uncertainty calculation from saved outputs.

Validate any script changes with focused syntax checks and the relevant real campaign path. No new tests are needed for plain result documentation. New tricky collection or scoring behavior warrants a targeted regression at the external format boundary. Commit only files changed for this task, request repository review before pushing code/scripts/configuration, and merge upstream if origin/master has moved.

## Idempotence and Recovery

Use a new arm identifier, job file, verification round, and result directory. Keep all old raw runs and results intact. Resume from durable completed metadata after interruption, rather than launching duplicate accepted reviews. Diagnose failed runs from saved metadata and logs before retrying. Stop only daemons or sessions created for this task when cleanup is necessary; preserve the established study instance and baseline evidence.

## Artifacts and Notes

Original study: `.agents/evals/review-arms/README.md`. Original frontier: `.agents/evals/review-arms/results/pareto.md`; expanded frontier: `.agents/evals/review-arms/results/stage4/pareto.md`. Raw data: `/mnt/optane/review-arms-20261113`.

## Interfaces and Dependencies

Use the installed `mj` CLI and the existing evaluation scripts. User credentials remain managed by Mjolnir. Do not change Rust target directories, run Cargo builds, or install a replacement build for this measurement. Model selection must be explicit and checked against the reviewer session's actual model and effort.

Revision note (2026-11-23): Registered the requested model comparison before any new review outputs were collected. Exact run commands remain to be filled from the delegated inspection.
