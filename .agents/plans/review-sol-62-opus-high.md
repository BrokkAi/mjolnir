# Expand Sol 6.2 samples and judge the frontier with Opus high

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The user explicitly requested Opus/high judging, two additional reviews per diff for both Sol 6.2 low and medium, and no further Neon judging. The completed experiments have one Sol 6.2 review per effort on each of 70 historical Mjolnir diffs. Add two independent samples per effort per diff, giving three low and three medium samples on every diff. Fresh Opus/high judgments will then support comparable one-, two-, and three-reviewer configurations and an updated Pareto frontier, which identifies configurations that improve findings, price, or latency without another configuration improving all the measured axes.

Concurrency remains 16 from the user's earlier instruction. Reuse accepted reviews and collect 280 new reviewer sessions, then judge the full historical frontier pool and all six Sol 6.2 samples per diff jointly. Deliver confirmed low/medium/high counts, price, latency, paired uncertainty estimates, and reproducible saved artifacts. The earlier Neon results remain historical records, and the new Opus results become the requested comparison.

## Progress

- [x] (2026-11-24) Recorded the new user mandate and delegated execution plus an independent Opus/profile/cohort preflight. No Neon judging is authorized.
- [x] (2026-11-24) Confirmed unused Q/R/S/T, the exact historical cohort, original/current Opus 5.5 route, Claude-only profiles, quota and judging commands. Registered slots passed review and were pushed in merge `9ef3bc9c0`.
- [ ] Add and audit 280 new reviews at concurrency 16, preserving all existing O/P samples and pins (completed: Q/R canaries passed, full manifest is active; remaining: all 280 accepted slots).
- [ ] Freshly judge the complete selected raw pool with Opus/high at concurrency 16; validate every accepted judge's identity and high effort.
- [ ] Compute and independently audit the one-/two-/three-reviewer results, severity tiers, frontier, price, latency, and paired intervals.
- [ ] Publish, commit, obtain review, and push to origin/master.

## Surprises & Discoveries

The original Stage 4 Opus/high round was `verify5`. A later user-requested round, `verify6`, used Neon/medium when Claude subscription profiles were exhausted. On 1,928 shared raw findings, Neon promoted 212 previously plausible/rejected findings to confirmed and demoted 10, with upward severity changes. The completed Sol 6.2 comparisons used fresh joint Neon rounds (`verify7` and `verify8`), not Opus. The user has now explicitly rejected further Neon judging. No existing verdict will be copied into the new Opus result.

The old runner's extra two-creation gate has already been removed. Each new job must have a single reviewer so the 16-worker executor does not silently admit 32 reviewer turns. All builds are outside this task: use installed Mjolnir and the existing pinned reviewer image.

At 2026-11-24 03:30:31 UTC, all 60 completed new slots had observed runtime/image and matched model, effort, handback, sole completed usage turn and prompt clock, including four no-findings reviews. A review request to change generic completion/retry semantics for hypothetical missing observations was declined: completion of a stochastic review and acceptance of its measurement evidence are distinct facts. The strict gate must hold a real gap for recovery from existing authoritative evidence or an explicit methodology ruling, rather than silently rerun or discard completed samples. The reproduction document records this recovery rule.

Review found that the first orchestration version assumed a prepared ready manifest and tried to exclusively recreate a reviewer hash manifest produced by the manual flow. The ready command now owns deriving and creating its manifest, or validates exact existing membership without rewriting it. The reviewer audit creates an absent hash manifest or verifies the exact existing digest/path multiset against the current audited sources, preserving existing bytes and refusing mismatches. Two focused filesystem tests cover cold preparation, reordered matching evidence, and missing/extra/changed membership or digests. The wrapper remains explicitly for a fresh round; after later-stage interruption, resume the diagnosed stage while preserving its private key and batches.

Preflight confirmed 1,306 historical sessions with 1,960 raw findings and 396 empty reports. The historical cohort plus existing O/P comprises 784 jobs and 1,446 sessions; adding 280 new jobs gives 1,064 jobs and 1,726 sessions. Only DSF/low is incomplete, with 35 sessions on 16 diffs. It remains in judging and is excluded from the primary 70-diff frontier. Historical missing token/API-price fields remain unavailable, not zero.

All 34 original Opus turns used bridge selector `opus`, high effort, fast and ultracode off, and provider `claude-opus-5-5`. Eleven also recorded known auxiliary `claude-haiku-5-5` usage. Recent completed turns on the installed runtime attest the same route on claude4. The 2026-11-24 03:01 UTC quota snapshot showed 90% standard weekly remaining on claude4; claude5 had 7% weekly and 86% five-hour remaining. The parent accepted claude4 initially, with claude5 considered only after an observed quota problem and a new parent ruling.

## Decision Log

- Decision: Add four distinct single-reviewer arms, provisionally Q=low, R=medium, S=low, T=medium after confirming those names are unused.
  Rationale: Existing O and P supply the first medium and low sample. Four single-reviewer jobs per diff add exactly two per effort while preserving a 16-job/16-reviewer maximum. Distinct slots prevent overwriting or counting a retry as another sample.
  Date/Author: 2026-11-24, parent agent.
- Decision: Use Opus/high exclusively for the new joint judging round, provisionally verify9.
  Rationale: This is the user's explicit correction. Pin the exact supported Opus identity and Claude profiles, attest every accepted turn, and fail visibly if unavailable; no other judge is an authorized fallback.
  Date/Author: 2026-11-24, parent agent.
- Decision: Use `--model opus --effort high --profiles claude4 --quota-scope claude`, with exact command-specific provider attestation and a first-real-batch canary.
  Rationale: The bridge normalizes model IDs to `opus`; an exact full-ID selector would conflict with the existing returned-value check. Durable selection plus provider usage must show the main `claude-opus-5-5`, with only the original known Haiku auxiliary allowed. A narrow repeatable `--batch` selector keeps the canary in verify9 with the same stable command and no copied judgments.
  Date/Author: 2026-11-24, parent agent.
- Decision: Include the original historical frontier's raw reviewer cohort plus all 420 Sol 6.2 reviews in one fresh blinded pool.
  Rationale: This makes the new sample range comparable with earlier frontier configurations under one Opus verdict set. Use historical round metadata only to enumerate raw inputs, never to import Neon judgments. Exact membership is pending the independent preflight; the expected cohort is 1,306 historical sessions plus 420 Sol 6.2 sessions.
  Date/Author: 2026-11-24, parent agent.
- Decision: Report x1, x2, and x3 for each Sol 6.2 effort, and mixed configurations with at most three reviewers in the primary frontier.
  Rationale: This extends the study's established frontier comparison. Reuse the same Terra sample pool and historical sample caps. Incomplete historical configurations cannot silently change the primary frontier's 70-diff denominator; report them separately on explicitly matched subsets if necessary.
  Date/Author: 2026-11-24, parent agent.

## Outcomes & Retrospective

The Q low and R medium canaries passed all input, runtime, handback and matching-turn checks. Their reviewer durations were 59.486 and 101.649 seconds respectively. The Q carrier completed its original context turn after 440 seconds; it was not restarted or duplicated. The full 280-slot interleaved manifest is running at executor concurrency 16. Earlier results remain preserved in `results/sol-62-medium/` and `results/sol-62-low/`. No Opus judging has started yet; it waits for all accepted reviews and the exact pool audit.

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
    C="$R/sol-62-repeats"
    S="$PWD/results/sol-62-opus-high"
    PREFLIGHT="$C/preflight"
    OPUS_JUDGE_STORE=/home/jonathan/.local/share/mjolnir/mj.sqlite3

After recording exact unused names and a saved interleaved manifest, the reviewer interfaces are:

    set -euo pipefail
    python3 -B driver.py --jobs "$R/sol-62-repeats/canaries.json" --concurrency 2 --same-image "$R/sol-62-repeats/image.json" --stop-on-failure
    python3 -B "$S/audit_reviewers.py" "$R" --canaries
    python3 -B driver.py --jobs "$R/sol-62-repeats/jobs.json" --concurrency 16 --same-image "$R/sol-62-repeats/image.json"

Run the full driver with durable detached logging and record its PID. Accepted canaries must skip on the full invocation. The canonical judging entrypoint is the checked-in campaign orchestrator:

    bash "$S/run_verification.sh"

Run it only after the reviewer driver exits. It enables shell error exit and pipeline error propagation, prepares or validates ready and reviewer hash manifests, checks new reviewer evidence, builds and audits the private pool, runs and attests one Opus canary, then runs and attests the rest before scoring. The ordered commands below document its gates for inspection and case-specific recovery, with shell error exit required throughout. Normal reproduction uses the canonical scripts; do not run both flows. A matching previously prepared manifest is reusable. Once verify9 exists, diagnose any interruption and resume that stage instead of rerunning fresh-round preparation or rebuilding its key and batches:

    set -euo pipefail
    python3 -B "$S/audit_round.py" "$R" ready > "$C/ready-audit.json"
    python3 -B "$S/audit_reviewers.py" "$R" --hash-file "$C/new-reviewers.sha256" > "$C/reviewer-audit.json"
    (cd "$R" && sha256sum --quiet -c "$PREFLIGHT/historical-and-op.sha256" && sha256sum --quiet -c "$PREFLIGHT/previous-rounds.sha256" && sha256sum --quiet -c "$C/new-reviewers.sha256")
    mkdir "$R/verify9"
    cp "$R/verify5/SPEC.md" "$R/verify9/SPEC.md"
    python3 -B build_verify.py verify9 --arms A,B,D,D2,E,G-base,G-plus,H,I,J,K,L,N,O,P,Q,R,S,T --configs gpt-6.1-terra:high,gpt-6.1-terra:xhigh,gpt-6.1-sol:high,gpt-6.1-sol:medium,gpt-6.1-sol:xhigh,greptile:base,greptile:plus,deepseek-flash:low,muse-ember-1.0:xhigh,gemini-4-hydrogen:high,gemini-4-hydrogen:medium,kimi-code/k3.1-pro:high,gpt-6.2-sol:low,gpt-6.2-sol:medium --ready-jobs "$C/ready-jobs.json" --bugs-from "$R/sol-62-repeats/jobs.json"
    python3 -B "$S/audit_round.py" "$R" pool "$PREFLIGHT/historical-and-op-sessions.jsonl" > "$C/pool-audit.json"
    (cd "$R" && sha256sum verify9/SPEC.md verify9/batch-*.json private/key-verify9.json) > "$C/verify9-pool.sha256"
    python3 -B verify_driver.py verify9 --kind verify --target localhost --repo-commit 3c3fa4b13bc32ba99fc349355081ee0d007831fe --model opus --effort high --profiles claude4 --quota-scope claude --concurrency 1 --batch 0
    python3 -B "$S/audit_round.py" "$R" judges "$OPUS_JUDGE_STORE" --batch 0 > "$C/judge-canary-audit.json"
    (cd "$R" && sha256sum --quiet -c "$C/verify9-pool.sha256")
    python3 -B verify_driver.py verify9 --kind verify --target localhost --repo-commit 3c3fa4b13bc32ba99fc349355081ee0d007831fe --model opus --effort high --profiles claude4 --quota-scope claude --concurrency 16
    python3 -B "$S/audit_round.py" "$R" judges "$OPUS_JUDGE_STORE" > "$C/judge-audit.json"
    (cd "$R" && sha256sum --quiet -c "$C/verify9-pool.sha256" && sha256sum --quiet -c "$PREFLIGHT/historical-and-op.sha256" && sha256sum --quiet -c "$PREFLIGHT/previous-rounds.sha256" && sha256sum --quiet -c "$C/new-reviewers.sha256")
    python3 -B score.py verify9 | tee "$C/score.json"
    bash "$S/run_analysis.sh"

The execution owner preserved the preflight manifests in `$C/preflight`. The `ready` preparation/audit derives exactly 1,064 jobs from the historical and new manifests, requires their exact completed paths and an unused verify9, and creates `$C/ready-jobs.json` only if absent. An existing file must have the exact expected membership and is preserved. `audit_reviewers.py --hash-file` similarly creates or verifies the frozen source manifest without refreshing changed evidence. The `pool` audit compares the generated private key against exactly the frozen 1,446 IDs plus 280 newly accepted reviewers, including all empty reports, and verifies source text/index/commit membership and the original specification before any judge is submitted. Merely passing `--ready-jobs` is insufficient.

Recovery writes the same audit and score receipt paths as the wrapper. In particular, `$C/judge-audit.json` and `$C/score.json` must contain the successful verified results before `run_analysis.sh` can proceed. Keep shell error exit and pipeline error propagation enabled so failed audits cannot advance to scoring or analysis.

Require the built first batch to be ID 0 before using the shown canary command; otherwise substitute the existing first ID in both the driver and audit. The selected `judges --batch 0` audit requires a completed accepted output and joins its stable command to durable usage and selection: opus/high on claude4, fast and ultracode off, main provider `claude-opus-5-5`, and no provider outside that model plus the original known `claude-haiku-5-5` auxiliary. Failure prevents full judging. The full invocation skips the completed batch. The unfiltered `judges` audit repeats these checks for every built batch and validates group coverage before scoring. Each audit exits nonzero on missing, unknown or mismatched evidence. The quota scope does not add a quota floor; it only interprets any explicitly configured existing floor using standard Claude windows rather than Fable windows.

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

Both canaries completed; `$R/sol-62-repeats/canary-audit.json` is the receipt. The full reviewer driver uses exec session 11483, with `$R/sol-62-repeats/reviewers.pid` and `reviewers.log`. Exact preflight manifests, checksum files, quota/model evidence, caps and accepted CLI arguments are in the preflight report directory's `preflight.md` and linked artifacts. The main driver audit must compare actual selected session IDs against those frozen manifests because `--ready-jobs` enforces readiness without whitelisting ingestion.

## Interfaces and Dependencies

Use installed Mjolnir, existing study wrappers, Python scripts, managed credentials, and the existing historical corpus. Do not install or rebuild Mjolnir, redirect Rust target storage, restart daemons, or change shared profiles. The parent will supply an explicit supported Opus/high route after read-only preflight; identity and effort checks must fail visibly when that route is not honored.

Revision note (2026-11-24): Registered the user's corrected judge mandate and additional sample counts before new outputs. The Opus/cohort preflight is complete, both repeat canaries passed, and the 280-slot reviewer driver is active. Added exact executable cohort, provider and hash gates plus the canonical verification script after publication review; an independent audit verified the actual gate ordering. Final live pool/provider validation and result reconstruction remain pending.
