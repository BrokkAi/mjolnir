# Expand Sol 6.2 samples and judge the frontier with Opus high

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The user explicitly requested Opus/high judging, two additional reviews per diff for both Sol 6.2 low and medium, and no further Neon judging. The completed experiments have one Sol 6.2 review per effort on each of 70 historical Mjolnir diffs. Add two independent samples per effort per diff, giving three low and three medium samples on every diff. Fresh Opus/high judgments will then support comparable one-, two-, and three-reviewer configurations and an updated Pareto frontier, which identifies configurations that improve findings, price, or latency without another configuration improving all the measured axes.

Concurrency remains 16 from the user's earlier instruction. Reuse accepted reviews and collect 280 new reviewer sessions, then judge the full historical frontier pool and all six Sol 6.2 samples per diff jointly. Deliver confirmed low/medium/high counts, price, latency, paired uncertainty estimates, and reproducible saved artifacts. The earlier Neon results remain historical records, and the new Opus results become the requested comparison.

## Progress

- [x] (2026-11-24) Recorded the new user mandate and delegated execution plus an independent Opus/profile/cohort preflight. No Neon judging is authorized.
- [ ] Confirm exact unused arm names, complete historical raw cohort, original and currently available Opus model identity, Claude-only profiles, quota, and judge commands (completed: unused Q/R/S/T registered; remaining: Opus/cohort preflight).
- [ ] Add and audit 280 new reviews at concurrency 16, preserving all existing O/P samples and pins.
- [ ] Freshly judge the complete selected raw pool with Opus/high at concurrency 16; validate every accepted judge's identity and high effort.
- [ ] Compute and independently audit the one-/two-/three-reviewer results, severity tiers, frontier, price, latency, and paired intervals.
- [ ] Publish, commit, obtain review, and push to origin/master.

## Surprises & Discoveries

The original Stage 4 Opus/high round was `verify5`. A later user-requested round, `verify6`, used Neon/medium when Claude subscription profiles were exhausted. On 1,928 shared raw findings, Neon promoted 212 previously plausible/rejected findings to confirmed and demoted 10, with upward severity changes. The completed Sol 6.2 comparisons used fresh joint Neon rounds (`verify7` and `verify8`), not Opus. The user has now explicitly rejected further Neon judging. No existing verdict will be copied into the new Opus result.

The old runner's extra two-creation gate has already been removed. Each new job must have a single reviewer so the 16-worker executor does not silently admit 32 reviewer turns. All builds are outside this task: use installed Mjolnir and the existing pinned reviewer image.

## Decision Log

- Decision: Add four distinct single-reviewer arms, provisionally Q=low, R=medium, S=low, T=medium after confirming those names are unused.
  Rationale: Existing O and P supply the first medium and low sample. Four single-reviewer jobs per diff add exactly two per effort while preserving a 16-job/16-reviewer maximum. Distinct slots prevent overwriting or counting a retry as another sample.
  Date/Author: 2026-11-24, parent agent.
- Decision: Use Opus/high exclusively for the new joint judging round, provisionally verify9.
  Rationale: This is the user's explicit correction. Pin the exact supported Opus identity and Claude profiles, attest every accepted turn, and fail visibly if unavailable; no other judge is an authorized fallback.
  Date/Author: 2026-11-24, parent agent.
- Decision: Include the original historical frontier's raw reviewer cohort plus all 420 Sol 6.2 reviews in one fresh blinded pool.
  Rationale: This makes the new sample range comparable with earlier frontier configurations under one Opus verdict set. Use historical round metadata only to enumerate raw inputs, never to import Neon judgments. Exact membership is pending the independent preflight; the expected cohort is 1,306 historical sessions plus 420 Sol 6.2 sessions.
  Date/Author: 2026-11-24, parent agent.
- Decision: Report x1, x2, and x3 for each Sol 6.2 effort, and mixed configurations with at most three reviewers in the primary frontier.
  Rationale: This extends the study's established frontier comparison. Reuse the same Terra sample pool and historical sample caps. Incomplete historical configurations cannot silently change the primary frontier's 70-diff denominator; report them separately on explicitly matched subsets if necessary.
  Date/Author: 2026-11-24, parent agent.

## Outcomes & Retrospective

Pending. The 280 additional reviews and new Opus judgments have not yet completed. Earlier results are preserved in `results/sol-62-medium/` and `results/sol-62-low/` under `.agents/evals/review-arms/`.

## Context and Orientation

The study scripts, specifications, corpus and published data are under `.agents/evals/review-arms/`. The persistent raw directory is `/mnt/optane/review-arms-20261113`, selected through `REVIEW_ARMS_DIR`. A diff is an original historical code change. An arm identifies an independent reviewer slot and its model/effort; several arms can later contribute samples to the same configuration. A configuration is a combination of reviewer models and efforts. Scoring counts the distinct confirmed problems found by the combination rather than counting duplicate reports twice.

Arm O has 70 `gpt-6.2-sol:medium` reviews and P has 70 `gpt-6.2-sol:low` reviews. All have actual model/effort, captured original inputs, fixed image, matching completed usage turn and positive prompt clock. They used installed Mjolnir 2.74.0, Codex 0.170.0, Codex ACP 1.15.1, and image manifest digest `sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db`. Their image/config pin is recorded in `arm-O-image.json` and `arm-P-image.json`. The main execution owner must check current availability rather than alter shared profiles or install a replacement runtime.

`driver.py` defines ARMS and runs durable review jobs. `build_verify.py` combines raw findings, hides provenance using random identifiers and a private source map, and prepares per-diff batches. `verify_driver.py` runs the separate judges and retrieves their output. `score.py`, `compare.py`, and `pareto.py` derive per-session groups, union counts, paired comparisons, and the frontier. Ordinary judge cleanup already saves output before `suspend --discard-untracked`; the old unsupported acknowledge flag is gone.

The installed `mjx` wrapper addresses the default study instance and clears inherited session environment. The retired private instance must not be restarted. Long-running campaigns use the existing installed daemon and existing study workspaces. The verifier checkout is separately preserved at commit `3c3fa4b13` with no study history reachable from refs. Prior workspace pointers once went stale; validate them before launching judges.

## Plan of Work

One execution worker owns registration, saved job definitions, review execution, raw audit helpers, judge execution, and the new result directory. A separate read-only worker checks the exact historical cohort and the original/current Opus identity, explicit Claude profiles, efforts, quota and judge specification. The parent owns decisions, this plan, integrated review, commits, and delivery. No one runs a Rust build, and no competing driver may operate on the same job slots.

First confirm unused slots and create a deterministic job order interleaving low/medium and repeats across the exact 70 original IDs. Validate one new low and one new medium canary; keep both as accepted samples in the 280 jobs. Prefer the existing image/harness, verify the full captured inputs and completed handbacks, and then execute the remaining jobs at concurrency 16. Preserve every failed attempt and diagnose it before retrying. Never repeat an already accepted review or assume an ambiguous launch was rejected.

The parent will approve the exact Opus model/profile/effort route and selected historical cohort from preflight evidence before any judge starts. The user already authorizes judging; this is an internal correctness check, not a new user approval step. Build a fresh round with all selected raw findings, randomized provenance-free IDs and a private key kept outside judge payloads. Use the original Opus specification and require exact coverage, valid enums, correct commit membership, model/profile attestation and high effort. Preserve all previous raw rounds and their checksum manifests. Stop and report an unavailable Opus route rather than changing judge.

After judging, compute averages over valid assignments of distinct samples within each diff, with equal weight per diff. A one-reviewer 6.2 result averages its three samples; a two-reviewer result unions every distinct pair; three uses all three. Mixed efforts and Terra combinations follow the same distinct-sample rule. Include zero-finding reviews and the exact matched diff set. Bootstrap paired diff-level changes 20,000 times for the decisive comparisons. Keep all reviewer samples for a diff together when resampling.

Publish total confirmed findings and each impact tier, with medium+high also available. Report recorded API-price estimates separately from subscription charges. Measure each turn from its captured prompt to completion, excluding startup; combination latency models simultaneous starts using the slowest member and must be identified as simulated. State runtime/profile differences and judging uncertainty. Independently reconstruct the decisive results from saved raw inputs before acceptance.

## Concrete Steps

Commands run from this checkout, with study scripts run from `.agents/evals/review-arms/` and the same environment as the prior reproduction files:

    export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
    export REVIEW_ARMS_INSTANCE=default
    export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/candidates-all.jsonl"
    export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
    unset REVIEW_ARMS_MJ REVIEW_ARMS_MJ_WORKER REVIEW_ARMS_SCRIPTS REVIEW_ARMS_REPO REVIEW_ARMS_BUNDLE REVIEW_ARMS_REPO_DIR
    R=$REVIEW_ARMS_DIR

After recording exact unused names and a saved interleaved manifest, the reviewer interfaces are:

    python3 -B driver.py --jobs "$R/sol-62-repeats/canaries.json" --concurrency 2 --same-image "$R/sol-62-repeats/image.json" --stop-on-failure
    python3 -B driver.py --jobs <confirmed-280-job-manifest> --concurrency 16 --same-image "$R/sol-62-repeats/image.json"

Run the full driver with durable detached logging and record its PID. Accepted canaries must skip on the full invocation. Before judging, fill in and record the exact preflight-confirmed Opus and pool arguments; no placeholders may reach a live command:

    python3 -B build_verify.py verify9 --arms <exact-selected-arms> --configs <exact-selected-configs> --ready-jobs <complete-actual-ready-jobs> --bugs-from "$R/arm-QRST-jobs.json"
    python3 -B verify_driver.py verify9 --kind verify --target localhost --model <exact-opus-id> --effort high --profiles <claude-only-profiles> --quota-scope <confirmed-claude-scope> --concurrency 16
    python3 -B score.py verify9
    python3 -B pareto.py verify9 --sessions gpt-6.2-sol/low=3,gpt-6.2-sol/medium=3,<historical-caps>

Existing previous publications have complete environment and timing/scoring interfaces. Record all final commands, cohort definitions, audits and logs in the new result's reproduction document. Never execute the old Neon judge commands from those historical documents.

## Validation and Acceptance

Require 280 unique additional accepted reviewer turns and 420 total Sol 6.2 turns: exactly three low and three medium per original diff. Confirm IDs, inputs, effort, image/runtime, handback, inventory and matching usage/clock evidence. The complete selected raw historical pool must match the preflight manifest exactly and include zero outputs. No later static-analysis or brief experiments sharing the directory may enter accidentally.

Each raw finding must map one-to-one to a correct blinded input and one valid output group on its own diff. All accepted judges must be Opus/high on the intended Claude route. Coverage, producer and effort are acceptance conditions, not inferred from requested configuration alone. Independently verify the x1/x2/x3 and mixed assignment math, severity totals, key intervals, usage, latency and frontier eligibility. Verify frozen baseline checksums after completion.

Use Python syntax checks and the existing two recovery tests if relevant runner logic changes; a registry-only addition needs syntax plus the real canary path. Do not add tests that restate the arm list. Do not run Cargo or broad unrelated suites. The final report and data must pass `git diff --check`, be committed on the current branch, receive required review, and be pushed to origin/master; merge upstream if needed.

## Idempotence and Recovery

New arms, round and result directories must be additive and confirmed unused. Retain all O/P samples and every prior result. Run one driver per job manifest and never overlap retries with a driver that still owns accepted intents. A completed record skips, a recorded started review collects, and an intent without a receipt requires checking daemon state before any new launch. Preserve failed attempts. Opus quota or authentication trouble pauses the affected judging work for diagnosis and user-visible reporting; it never authorizes Neon or another judge.

## Artifacts and Notes

Original method and frontier: `.agents/evals/review-arms/README.md`. Completed current-model samples and tables: `results/sol-62-medium/` and `results/sol-62-low/` there. Raw verify5 is the original Opus round; verify6 is a historical cohort source; verify7/8 are archived Neon rounds. New expected raw round is verify9 and publication is `.agents/evals/review-arms/results/sol-62-opus-high/`.

Prior execution: `.mj/agents/108e236adc4f5d9bc5710ccb830ababa/`. Prior final independent audit: `.mj/agents/777987907c1b745b746e446b07cb0bd7/verify8-final-audit.md`. New execution reports: `.mj/agents/e716b3bf9cbf5440f17f95105b1245e3/`; Opus/cohort preflight: `.mj/agents/f75b54121d7b1bc32ae30aa6c5981737/`. The first detached canary command exited before launching or recording a session. The active canary driver uses exec session 5015; durable PID/log receipts are `$R/sol-62-repeats/canaries-exec.pid` and `canaries-exec.log`.

## Interfaces and Dependencies

Use installed Mjolnir, existing study wrappers, Python scripts, managed credentials, and the existing historical corpus. Do not install or rebuild Mjolnir, redirect Rust target storage, restart daemons, or change shared profiles. The parent will supply an explicit supported Opus/high route after read-only preflight; identity and effort checks must fail visibly when that route is not honored.

Revision note (2026-11-24): Registered the user's corrected judge mandate and additional sample counts before new outputs. Exact Opus/cohort commands are awaiting the delegated preflight; reviewer execution can proceed independently.
