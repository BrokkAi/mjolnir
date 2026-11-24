# Compare Sol 6.2 medium and low with the saved Sol 6.1 medium evaluation

This living plan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Measure whether one GPT 6.2 Sol reviewer at medium effort improves on GPT 6.1 Sol at medium effort, both alone and together with two GPT 6.1 Terra reviewers at high effort. Reuse the reviewer evaluation's original 70 Mjolnir diffs and saved baseline sessions. The user requested one new reviewer per diff and concurrency 16. The deliverable is a reproducible comparison of confirmed findings, severity, latency, and recorded usage, with a clear account of uncertainty and coverage.

The medium comparison is complete and published. The user then requested the same GPT 6.2 model at low effort. Extend the study with one low-effort review per original diff and compare it with both saved medium configurations, solo and with the same Terra pair, retaining concurrency 16. This measures the quality, price, and latency tradeoff of reducing effort.

## Progress

- [x] (2026-11-23 23:50Z) Located the original study and its persistent run directory; delegated runner/scoring mapping and baseline/model checks.
- [x] (2026-11-23 23:52Z) Confirmed the runner accepts `gpt-6.2-sol:medium`; reserved unused arm O. Baseline metadata has 140 E Sol/medium sessions and 221 A/B Terra/high sessions across 70 diffs. Independent availability/count checks remain in progress.
- [x] (2026-11-24 00:29Z) Completed exactly 70 audited Sol 6.2 medium reviews, one per original diff, with live runtime/image capture and matched positive prompt clocks for all 70. Two handbacks explicitly contain no findings.
- [x] (2026-11-24) Audited all 34 accepted Neon/medium judge outputs and durable model selections; scored all 70 diffs and 431 sessions with no missing timing or usage. Preserved verify6 checksum manifest.
- [x] (2026-11-24) Independently reconstructed the raw pool, groups, all four comparisons, paired bootstrap intervals, and model attribution. Published the complete readout and supporting data in the worktree.
- [x] (2026-11-24) Final result commit `07825127b` passed review and was pushed to origin/master in upstream merge `c6585cfed`.
- [x] (2026-11-24) Added independently calculated low/medium/high impact counts to the medium readout and pushed documentation commit `510d4f6e2`.
- [x] (2026-11-24 01:32Z) Registered and audited exactly 70 Sol 6.2 low reviews as arm P, one per original diff, at executor concurrency 16. No failed jobs or retries; 70 live runtime/prompt clocks and matching completed turns, three explicit no-findings handbacks, peak 12 overlapping reviewer turns.
- [x] (2026-11-24) Completed all 34 verify8 judges at concurrency 16 without failures or retries. Audited all 501 sessions, 881 raw findings, and 70 diffs; old rounds' checksum manifests remain intact. Scoring produced 333 groups, including 305 confirmed.
- [x] (2026-11-24) Independently checked exact source/group mappings, all 35 severity totals, all 20 paired bootstrap comparisons, model attribution, timing/price, and published exports. The severity/price/latency readout is complete in the worktree.
- [ ] Commit the low-effort results, obtain required review, and push.

## Surprises & Discoveries

The old Sol 6.1 medium arm used two reviewers per diff; the requested Sol 6.2 arm uses one. The analysis must preserve this difference and must not present repeated combinations as independent new reviews. Existing study scripts form reviewer combinations afterward from independently run sessions.

The current comparison round is verify6, published in `results/stage4-hydrogen-medium/`. The established judge was changed after the original study: verify6 uses Neon/medium with the gemini and gemini2 profiles. Arm B exists for only 11 diffs, so readiness must enumerate the actual B jobs rather than assume 70. The persistent study now uses the installed default instance through its `mjx` wrapper, which clears inherited session environment; the retired private instance must not be started.

The original discovery worker's read-only assignment caused automatic approval review to reject its later attempt to register the arm after mailbox reassignment. No edit or new review started. A fresh worker received the complete implementation and campaign authorization in its initial assignment.

The installed 2.74.0 CLI removed `suspend --acknowledge-unpublished-work`. The captured first review was preserved and collected again without starting another review. Removing that obsolete flag at the two carrier cleanup sites in `driver.py` and the ordinary judge cleanup in `verify_driver.py` makes the study compatible with the installed command. Ordinary suspend's dry run reported no blockers, untracked work, Git operations, or Git delta.

Review of the cleanup checkpoint found that ordinary judge/task sessions create untracked output files, unlike the clean carrier. The judge helper now supplies explicit `--discard-untracked` consent after a successful output is fetched and persisted; a quota-failed attempt has no accepted result and may discard partial output. Both call sites were checked, and syntax/diff checks pass. The first real verify7 judge will validate cleanup against the installed CLI. No judge result needs to be recomputed.

One carrier failed before any prompt or review acceptance during a daemon restart. The interrupted Create retired a worker environment and reprovisioned the same session against a retained relay projection, producing an event-3 digest mismatch and a correctly refused rewind to genesis. Independent trace/source inspection confirmed the pre-review failure. Filed live daemon bug #1675. The original driver finished with 69 accepted samples; after it exited, the failed attempt was archived and only that job was retried on a new carrier. The retry passed the same audit and supplies the seventieth sample. No accepted review was repeated.

The initial judge driver found a stale cached workspace ID: all 34 new-session commands were explicitly refused as unknown workspace before session creation or prompting. The old named workspace was absent from the live list. Under the existing workspace-setup lock the owner archived the stale pointer, recreated the same named workspace over the unchanged verifier checkout, and retried the 34 refused units with the identical blinded inputs. The first 16 replacement session receipts are persisted. This is setup recovery, not repeated judging.

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
- Decision: Remove the runner's hardcoded two-creation semaphore for future invocations, leaving the requested executor concurrency as the sole admission width. Let the already-running process finish with its existing gate.
  Rationale: An independent history audit traced the gate to the initial study commit without a documented or measured external limit. It is the observed launch bottleneck. The running Python process has no reload or drain interface and holds accepted launch intents; restarting or overlapping drivers could duplicate work or exceed the user's 16 jobs. Source edits will not change its in-memory semaphore. Record this timing limitation in the result.
  Date/Author: 2026-11-24, parent agent.
- Decision: Add arm P with one `gpt-6.2-sol:low` review per original diff and judge A/B/E/O/P jointly in fresh verify8.
  Rationale: The user's follow-up explicitly requests 6.2 at low effort. Compare it against 6.2 medium and 6.1 medium under one verdict set, including each with the same two-Terra sample pool. This gives 501 selected reviewer sessions while retaining original 70-diff coverage, and leaves verify6/7 immutable.
  Date/Author: 2026-11-24, parent agent.

The removal and the existing recovery tests' call signatures are implemented. Python syntax checks, `git diff --check`, and both existing driver recovery tests passed. The current driver was not restarted and retains the original in-memory gate.

## Outcomes & Retrospective

All 70 new reviews passed audit under installed Mjolnir 2.74.0; all have model/effort selection, fixed image, sole matching completed usage turn, positive prompt clock, and input/inventory fidelity evidence. The original process used executor concurrency 16 and the historical two-creation gate, with a peak of 11 overlapping reviewer turns. A single pre-review startup failure was retried after driver exit. Arm registration, cleanup, and removal of the redundant gate through `9c4d616e0` passed repository review and are pushed to origin/master.

Joint verify7 completed 34 blinded batches with Neon/medium on gemini and gemini2 at concurrency 16. Its audited pool contains exactly 431 selected sessions and 717 raw findings on 68 diffs; all 70 diffs remain covered, including two with no findings. The judge formed 310 groups: 291 confirmed, two plausible, and 17 rejected. The old verify6 inputs and specification are frozen. Across 443 common raw findings, verdict agreement is 98.0%; severity agreement is 84.6% when both rounds kept the finding.

Sol 6.2 medium raised confirmed findings from 50.00 to 259.00 solo and from 138.50 to 284.33 with two Terra/high reviewers. The paired 95% intervals for the gains are +177.00 to +242.50 solo and +121.17 to +171.58 with the Terra pair. High/medium findings rose from 47.50 to 189.00 solo and from 122.33 to 210.00 with the pair. Independent raw reconstruction exactly reproduced all totals, per-diff wins/ties, and 20,000-resample intervals, with no extraction or counting artifact found.

The quality improvement has higher recorded resource use: estimated median parallel turn time rises from 46.3 to 106.3 seconds solo and from 54.3 to 109.6 seconds with the pair. Estimated API cost per diff rises from $0.100 to $0.252 solo and from $0.187 to $0.339 with the pair. All counts use the same 70 diffs; costs exclude coordination/judging. The old and new cohorts ran on different dates and harness versions, and one new sample per diff does not measure repeat variance. These are the setups as run, assessed by the same fresh blinded judge round.

The result and reproduction commands are in `.agents/evals/review-arms/results/sol-62-medium/README.md` and `reproduction.md`, with scored CSVs, paired comparisons, Pareto CSVs, usage, coverage, judge attribution/audit, and prior-round consistency data. The main study README links the addition. Real carrier/judge cleanup succeeded after the current-CLI fixes; the two existing recovery tests and applicable script syntax checks passed. No Rust build or installation was needed. Final result commit `07825127b` passed review and reached origin/master in merge `c6585cfed`.

The follow-up low-effort campaign completed all 70 reviews without failures or retries. It used the same image digest, Codex 0.170.0, and ACP 1.15.1 as arm O, and the canary audit matched the captured prompt after normalizing the carrier checkout path. The executor width was 16 with no extra creation gate; 12 recorded reviewer turns overlapped at peak. Three handbacks explicitly contained no findings. All 70 have matching live runtime and positive prompt-to-completion clocks.

Verify8 completed all 34 judges at concurrency 16 with the same Neon/medium spec and profiles, split 17/17, and no failures or retries. Its audited pool contains exactly 501 sessions and 881 raw findings across 68 nonempty diffs; the two empty diffs remain covered. Scoring produced 333 groups, including 305 confirmed. Verify6/7 checksums remain intact. The 717 common raw findings have 97.9% verdict agreement and 90.0% retained severity agreement with verify7.

Final independently reconstructed verify8 totals are 50.00 for Sol 6.1 medium, 256.00 for Sol 6.2 medium, and 155.00 for Sol 6.2 low. With two Terra/high reviewers, the respective totals are 135.83, 278.67, and 197.67. Low has 125 high/medium findings solo and 163.17 with Terra; its high-only counts are 24.00 and 28.67. Recorded solo timing/cost is 46.0 seconds/$0.120 for low versus 106.3 seconds/$0.252 for medium. With Terra, low is 52.1 seconds/$0.207 versus medium 109.5 seconds/$0.339. These are fresh same-round comparisons; the earlier verify7 medium total of 259 must not be mixed into verify8 contrasts.

The independent audit exactly reproduced all 35 severity totals and all 20 paired comparison outputs from raw inputs, including the 70 P durable low-effort selections and 34 Neon judge selections. It found no integrity defect. All 501 session clocks and usage rows match, ten exports are byte-identical, and 382 prior-round checksums pass. Low versus 6.1 medium gains 105 confirmed solo and 61.83 with Terra, while trailing 6.2 medium by 101 solo and 81 with Terra. For high-impact findings alone with Terra, the gain over 6.1 medium is only 3.00, with a 95% interval of −1.00 to +7.83. The readout and supporting data are in `.agents/evals/review-arms/results/sol-62-low/`; final commit/review/push remains.

## Context and Orientation

`.agents/evals/review-arms/README.md` describes the experiment. Stage 2 expanded it to 70 diffs and measured the latency/findings Pareto frontier. Stage 3 added Sol 6.1 medium as arm E, two sessions per diff. Stage 4 pooled the original and expanded model arms for blinded verification.

The repository holds scripts, corpus specifications, and published results under `.agents/evals/review-arms/`. Raw session metadata, the persistent evaluation instance, transcripts, and verdicts live under `/mnt/optane/review-arms-20261113`, selected by `REVIEW_ARMS_DIR`. A reviewer arm is one model/effort configuration, assigned a distinct identifier so its saved runs cannot collide with another configuration. A diff is the original code change under review. Blinded verification means a separate judge evaluates pooled findings without being told which model produced them.

`driver.py` launches saved job definitions; `build_verify.py` and `verify_driver.py` prepare and judge pooled findings; `score.py`, `pareto.py`, and `compare.py` produce per-session and combined results. The installed `mjx` 2.74.0 is the current runner executable. The new arm's job IDs come from the exact original 70 IDs in `arm-N-jobs.json`. `--same-image "$REVIEW_ARMS_DIR/arm-O-image.json"` fixes its image for the campaign. One validation run is retained as part of the 70. The detailed instance and scoring commands are being recorded by the campaign owner before execution. No Rust build or installation is required or authorized by this evaluation.

## Plan of Work

First confirm the study's current executable, persistent instance, available model identifier, and complete saved baselines. Register a new arm with one `gpt-6.2-sol` medium reviewer for each of the original 70 diffs, retaining the original prompt and review inputs. Make only the runner or collection changes necessary for this arm and current Mjolnir interfaces. Assign one worker ownership of campaign execution and shared runner edits; other workers may perform read-only checks or analysis on completed outputs.

Run the reviewer jobs at concurrency 16. Retain failures and retry only runs that did not produce an accepted completed review. Audit model and effort, input fidelity, and completion before counting a run. Verify new findings with the existing blinded method and preserve baseline verdicts; if consistent pooling requires a new joint round, explicitly record that choice and compare all requested configurations within that round. Exclude unrelated later experiments that share the raw data directory.

Compute the solo and two-Terra combinations with the established per-diff combination method. Report mean confirmed findings across the corpus, medium/high severity findings, median latency using the slowest member of a combination, and available usage/cost estimates. Use paired resampling of diffs for uncertainty, keeping all sessions belonging to one diff together. One new session per diff limits evidence about within-diff reviewer variability; say so in the result.

### Follow-up milestone: Sol 6.2 low

Register unused arm P as `gpt-6.2-sol:low` and derive its 70 jobs by changing only the arm field in `arm-O-jobs.json`. The same execution worker owns runner registration, new raw jobs, review execution, verify8, and the new result readout. The parent owns this plan, design, independent acceptance, commits, and delivery. No other worker may mutate these files or run a competing driver. No Rust builds or installations are needed.

Use one canary, retained in the 70, to attest the actual low effort, model, input fidelity, image, handback, and prompt clock. Prefer the medium arm's image and harness if available without modifying global profiles; explicitly record any unavoidable cohort difference. Continue at executor concurrency 16 with the redundant creation gate now removed. Preserve accepted outputs and diagnose ambiguous starts before retrying. The previous failed pre-review slot remains archived and is not part of any new sample.

Create stage8 readiness by extending the exact stage7 A/B/E/O rows with P. Joint verify8 includes 221 Terra/high, 140 Sol 6.1/medium, 70 Sol 6.2/medium, and 70 Sol 6.2/low sessions. Copy the unchanged judge specification, use the same verifier checkout and Neon/medium profiles, and run at concurrency 16. Check current workspace validity before launch, since cached pointers were previously stale. Primary comparisons are low versus 6.2 medium, solo and with the shared Terra pair; also report the 6.1 medium baseline. Use separate direct contrasts so each confidence interval subtracts the intended reference.

Publish confirmed low/medium/high and total findings, high+medium totals, the paired diff confidence intervals, simulated median turn latency, and estimated API cost per diff. Preserve verify6/7 and their publications. An independent audit must check all 70 diff IDs, all 501 selected sessions, raw-to-group mappings, actual model/effort selection, shared Terra weights, and the decisive aggregates before acceptance. Store the new readout under `results/sol-62-low/` and link it from the study README.

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

For the new low-effort milestone, keep the same environment above and use these additive paths and commands after confirming arm P is unused:

    jq 'map(.arm = "P")' "$R/arm-O-jobs.json" > "$R/arm-P-jobs.json"
    jq '[.[] | select(.bug == "diff-05bc8c1e68")] | select(length == 1)' "$R/arm-P-jobs.json" > "$R/arm-P-canary.json"
    jq -s '.[0] + .[1]' "$R/stage7-ready-jobs.json" "$R/arm-P-jobs.json" > "$R/stage8-ready-jobs.json"
    python3 -B driver.py --jobs "$R/arm-P-canary.json" --concurrency 1 --same-image "$R/arm-P-image.json"
    python3 -B driver.py --jobs "$R/arm-P-jobs.json" --concurrency 16 --same-image "$R/arm-P-image.json"
    mkdir -p "$R/verify8"
    cp "$R/verify7/SPEC.md" "$R/verify8/SPEC.md"
    python3 -B build_verify.py verify8 --arms A,B,E,O,P --configs gpt-6.1-terra:high,gpt-6.1-sol:medium,gpt-6.2-sol:medium,gpt-6.2-sol:low --ready-jobs "$R/stage8-ready-jobs.json" --bugs-from "$R/arm-P-jobs.json"
    python3 -B verify_driver.py verify8 --kind verify --target localhost --model gemini-4-neon --effort medium --profiles gemini,gemini2 --quota-scope gemini --concurrency 16
    python3 -B score.py verify8
    python3 -B consistency.py verify7 verify8
    python3 -B compare.py verify8 gpt-6.2-sol/medium gpt-6.2-sol/low
    python3 -B compare.py verify8 gpt-6.2-sol/medium+Terra/high+Terra/high gpt-6.2-sol/low+Terra/high+Terra/high
    python3 -B compare.py verify8 Sol/medium gpt-6.2-sol/low
    python3 -B compare.py verify8 Sol/medium+Terra/high+Terra/high gpt-6.2-sol/low+Terra/high+Terra/high

Apply the same strict audits, durable detached logging, severity slices, and timing/usage methods as verify7. These commands describe distinct new work; do not relaunch earlier O reviews or verify7 judges.

## Validation and Acceptance

Acceptance requires 70 audited new reviews, one per original diff, and complete matched baseline coverage for all four requested setups. Verify collection completeness and model/effort from actual session metadata. Check that each scored finding maps to the correct review and judge verdict, that combinations union findings rather than double-counting them, and that comparisons use identical diff coverage. An independent worker checks the decisive aggregates and uncertainty calculation from saved outputs.

Validate any script changes with focused syntax checks and the relevant real campaign path. No new tests are needed for plain result documentation. New tricky collection or scoring behavior warrants a targeted regression at the external format boundary. Commit only files changed for this task, request repository review before pushing code/scripts/configuration, and merge upstream if origin/master has moved.

## Idempotence and Recovery

Use a new arm identifier, job file, verification round, and result directory. Keep all old raw runs and results intact. Resume from durable completed metadata after interruption, rather than launching duplicate accepted reviews. Diagnose failed runs from saved metadata and logs before retrying. Stop only daemons or sessions created for this task when cleanup is necessary; preserve the established study instance and baseline evidence.

## Artifacts and Notes

Original study: `.agents/evals/review-arms/README.md`. Original frontier: `.agents/evals/review-arms/results/pareto.md`; expanded frontier: `.agents/evals/review-arms/results/stage4/pareto.md`. Raw data: `/mnt/optane/review-arms-20261113`.

First-run evidence: `$REVIEW_ARMS_DIR/runs/diff-05bc8c1e68/O/`; driver PID 153560 at launch; log `$REVIEW_ARMS_DIR/arm-O-canary.log`. Captured image digest: `sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db`. The live model/effort was observed rather than inferred from arm configuration.

Full campaign log and process receipt: `$REVIEW_ARMS_DIR/arm-O-driver.log` and `$REVIEW_ARMS_DIR/arm-O-driver.pid`. First-run audit: `$REVIEW_ARMS_DIR/arm-O-canary-audit.json`. The changed scripts passed Python syntax parsing, and the repaired cleanup completed against the real installed CLI.

Final reviewer audit: `$REVIEW_ARMS_DIR/arm-O-audit.json`. Failed pre-review attempt: `$REVIEW_ARMS_DIR/runs/diff-660c78e96c/O-failed-bootstrap-dddc797142ac/`; retry log: `$REVIEW_ARMS_DIR/arm-O-retry-bootstrap.log`. Bug report: `https://github.com/BrokkAi/mjolnir/issues/1675`. Verify7 build/pool/driver receipts: `$REVIEW_ARMS_DIR/verify7/build.log`, `pool-audit.json`, `driver.log`, and `driver.pid`. The first judge driver PID 1627672 exited after explicit workspace refusals. Replacement driver PID 1642525 uses `$REVIEW_ARMS_DIR/verify7/retry-workspace.log`; stale pointer and live refusal evidence are `workspace-stale.json` and `workspaces-at-refusal.json`.

Low-effort reviewer receipts: `$REVIEW_ARMS_DIR/arm-P-canary.log`, `arm-P-driver.log`, and `arm-P-audit.json`; full driver PID 2403011 at launch. Verify8 receipts: `$REVIEW_ARMS_DIR/verify8/{build.log,pool-audit.json,driver.log,driver.pid}`; judge driver PID 3506812 at launch. Independent preflight: `.mj/agents/777987907c1b745b746e446b07cb0bd7/verify8-preflight.md`.

## Interfaces and Dependencies

Use the installed `mj` CLI and the existing evaluation scripts. User credentials remain managed by Mjolnir. Do not change Rust target directories, run Cargo builds, or install a replacement build for this measurement. Model selection must be explicit and checked against the reviewer session's actual model and effort.

Revision note (2026-11-23): Registered the requested model comparison before any new review outputs were collected. Added the reserved arm, baseline metadata counts, executable, joint verification decision, exact commands, and execution ownership after runner inspection. Recorded the rejected read-only-to-write reassignment and the fresh explicitly authorized execution worker.

Revision note (2026-11-24): Completed, independently audited, reviewed, and pushed the medium comparison and its impact breakdown. Extended the plan for the user's newly requested low-effort arm, preserving the completed results and the concurrency-16 preference.
