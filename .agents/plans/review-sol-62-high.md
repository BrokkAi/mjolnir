# Measure Sol 6.2 high effort with Opus high judges

This ExecPlan is maintained under `.agents/PLANS.md`. Keep Progress, discoveries, decisions and outcomes current.

## Purpose / Big Picture

Add one GPT 6.2 Sol/high review on each of the original 70 historical mjolnir diffs. Compare it with the three retained low and three retained medium samples per diff, especially confirmed high-impact findings. Report findings by severity, paired uncertainty, reviewer cost and latency, and update the combined solo cost chart. Remove the historical Terra point from that chart while retaining the historical baseline internally for Claude normalization.

## Progress

- [x] (2026-11-25 00:42Z) Confirmed Sol 6.2/high on codex3/codex4 and usable quota on those profiles plus claude4/claude7.
- [x] Prepared arm U and a 70-job manifest, alternating 35 jobs per Codex profile. Prepared two canaries included in those 70.
- [x] (2026-11-25 01:03Z) Two original canaries passed profile/model, exact native high-effort turn binding, original image/harness, unchanged checkout and output/clock audit; durations119.3/122.4s, four raw findings each.
- [ ] Full70 manifest launched with two executors of width16, total32; two accepted canaries skip. Receipt R/sol-62-high/reviews-launch.json and log reviews.log.
- [ ] Freeze verify9 cohort and add the 70 U sessions to a fresh blinded verify10 pool; verify exact session and finding membership.
- [ ] Run Opus/high canaries on claude4 and claude7, attest actual provider/model/effort, then finish all batches using only those judge profiles.
- [ ] Score all/low/medium/high and medium+high; compare high with low and medium on the same 70 diffs and publish costs/latencies and confidence intervals.
- [ ] Update and deliver the combined PNG, excluding historical Terra, then complete repository review and push.

## Surprises & Discoveries

The installed main runtime is Mjolnir 2.75.0, while the retained low/medium reviews used 2.74.0. The original image ID `9a1d16144cc1759206f95dc59942360e3bfa54264b0b5c5ea172fffb0469a470` remains installed. The native hidden-reviewer launcher has no per-job account pin: it load-balances model choices across enabled profiles. Altering shared profile availability would affect unrelated work.

The first two carriers omitted refs/mj/session-start because review is off. The old inventory pin step refused before either review prompt; read-only inventory plus the explicit tree-to-tree prompt is the matching audit for these direct sessions. Both original sessions are reused. Installed Codex exposes model, effort, collaboration_mode and fast-mode; the old mode/web_fetch fields are target-policy settings and were refused before any prompt. Keep normal collaboration with fast mode off and enforce the same explicit read-only/no-network review instructions plus post-review inventory. No shared permission settings are changed. Both accepted canaries completed on the original Codex0.170/ACP1.15.1 runtime. Usage omits effort, so capture the native rollout before cleanup and bind its high-effort turn_context to the exact command, prompt and completion interval using the existing driver helper. Preserve the raw final message and normalize only a leading bare [Pn] heading to a bullet for the established findings splitter.

## Decision Log

Use explicit profile-pinned top-level review sessions to honor 16 concurrent jobs each on codex3 and codex4 without shared configuration changes. Reuse the saved native review prompt template from `sol-62-repeats/prompt-Q.json`: replace only workspace path, captured trees, diffstat and chronological commit context. Replace the final handback-tool instruction with final-response delivery because these sessions are not hidden reviewer children. Preserve this protocol difference in the report; it is a cohort confound, not a controlled effort-only effect. The same pinned checkout, no later history, no memory, network/read-only constraints and original image are required before any reviewer prompt. The parent performs this work personally as requested; experiment model sessions are the measured workload.

Retain all verify9 sessions in fresh Opus judging so high, medium and low share adjudication and grouping. No Neon judges are permitted. Judge routes are claude4 and claude7, alias opus at high effort; completed turn usage must identify concrete `claude-opus-5-5`, allowing the known Haiku auxiliary only. No builds, installs or daemon/profile changes are needed.

## Outcomes & Retrospective

Two of70 high-effort reviews are accepted, one per requested profile. Each returned four raw findings; none is a confirmed finding until the fresh blinded Opus round. Native effort was proven from captured rollout prompt/turn_context despite the API usage effort field being null. The original Codex0.170/ACP1.15.1 and image were observed. Full cohort is running; setup/capture recoveries reused the same two sessions and did not sample either review again.

## Context and Orientation

Scripts are in `.agents/evals/review-arms`. Raw data is `/mnt/optane/review-arms-20261113` (R). New operational receipts are R/sol-62-high (C). Original jobs are R/arm-O-jobs.json; original low/medium arms are P/Q/S and O/R/T. Arm U is a single `gpt-6.2-sol:high` sample per diff. A diff is one original commit and its base; a reviewer session is one independent model attempt. Distinct confirmed issues are counted once per sampled combination, not once per duplicate report.

`driver.py` supplies corpus, installed CLI wrapper mjx, checkout inventory, runtime capture and usage helpers. `results/sol-62-high/run_reviews.py` runs the explicit profile split. Each job's durable data lives at R/runs/<diff>/U. It saves the exact sent prompt, session, profile, model/effort, live image/processes, prompt timestamp, completed usage, final response and unchanged checkout inventory. It emits the same done/reviewer/output shape consumed by build_verify.py, while explicitly marking final-response delivery instead of a native handback.

Fresh judges use `build_verify.py`, `verify_driver.py`, and the byte-identical verify9/SPEC.md, with the judge checkout pinned at `3c3fa4b13bc32ba99fc349355081ee0d007831fe` and no source metadata in uploaded batches. Private finding/session maps stay under R/private at mode0600. `score.py`, `compare.py` and `pareto.py` produce the matched readout. All 1,726 verify9 reviewer sessions plus 70 U sessions must be present, including zero-finding sessions.

## Plan of Work

Validate two U canaries before admitting the remaining jobs. Inspect actual final responses and captured transcripts rather than infer success from process exit alone. Then run the 70 manifest with two profile-specific executors of width16. Freeze source artifacts, reconstruct exact expected session membership from verify9 plus U, and prepare verify10. Audit every blinded finding against its source and original commit before any judge call. Run one real batch per judge profile; these are counted in the final round, not discarded tests. Require actual completed-command provider/effort/mode attestation before the remaining batches. Score only after all batches, profiles, enums and source hashes pass.

Produce solo effort contrasts with 20,000 paired diff-bootstrap resamples: low versus medium, medium versus high, low versus high, across all confirmed, low, medium, high and medium+high. Preserve old reports and raw verify9 unchanged. The single high sample has no repeat-level variance estimate; diff-level paired intervals still describe this observed cohort. Include launch protocol/runtime confounds in interpretation.

## Concrete Steps

Run from `.agents/evals/review-arms` with:

    export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
    export REVIEW_ARMS_INSTANCE=default
    export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/corpus.jsonl"
    export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
    C="$REVIEW_ARMS_DIR/sol-62-high"
    S=results/sol-62-high
    python3 -B "$S/run_reviews.py" --jobs "$C/canaries.json" --concurrency 1
    python3 -B "$S/run_reviews.py" --jobs "$C/jobs.json" --concurrency 16

The concurrency argument applies separately to each profile's executor. The saved manifests alternate original O job order between codex3 and codex4. Canary rows remain in jobs.json and skip after successful completion. All long-running work uses the installed main daemon through mjx, not the retired private instance. Record command, PID, script digest and log path before leaving a detached run.

After the recorded reviewer driver exits, the single-owner guarded entry point is:

    python3 -B "$S/audit_round.py" freeze
    python3 -B "$S/run_verification.py"

The entry point can be started while reviews run: it waits for the PID/command in C/reviews-launch.json, then requires all70 completed, captured and cleaned reviews. It builds verify10 from the exact frozen1,726 plus70 new sessions, audits raw membership and hashes, then submits batch0 to claude4 and batch1 to claude7. Both concrete Opus5.5/high completed commands must pass before remaining batches start. Even/odd batch IDs are disjoint profile shards, each at width16. Every batch's completed command, concrete provider, profile and modes are checked before score.py. compare.py runs20,000 paired resamples for low/medium/high in each severity tier; pareto.py preserves old caps plus one high sample and runs the all, medium+high and high frontiers. Receipts/logs are written in C; the final readout is prepared after these gates pass. No direct unguarded judging is an alternative.

## Validation and Acceptance

Require exactly70 U sessions on the original70 unique diffs, 35 on each requested profile, selected Sol6.2/high on the completed command, positive prompt-to-completion clocks, captured live original image and clean pre/post inventory. No ambiguous or failed attempt is silently replaced. Verify10 must cover exactly1,796 raw sessions with no missing or extra source findings, and retain all70 diffs via covered sessions. Batch entries must match exactly the nonempty commit set; a wholly empty diff needs no judge call. Every accepted batch must be completed Opus/high on claude4 or claude7 with the concrete provider identity and valid verdict/severity/confidence enums. The readout must state confirmed high-impact yield and its paired comparison, plus all/medium/low counts, reviewer cost and latency. The final PNG must omit the historical Terra point and label combined frontier and Greptile points by name.

## Idempotence and Recovery

A launch intent without a saved session acknowledgement requires reconciliation against the daemon before any new session creation. Reuse recorded sessions. Prompt command IDs are stable, so a repeated send after a lost receipt is deduplicated. A completed result is never sampled again. Preserve any failed attempt and diagnose whether any reviewer prompt was accepted before authorizing replacement. Runtime or clock evidence cannot be manufactured by rerunning a completed review. Save result/evidence before suspension; diagnose missing cleanup separately from measurement completion. Verification must preserve private keys and batch IDs on resume.

## Artifacts and Notes

C/jobs.json and C/canaries.json freeze assignment. The saved native prompt is R/sol-62-repeats/prompt-Q.json. Prior verify9 and original O/P images are immutable references. The new result directory contains audit scripts and guarded reproduction entry points; aggregate CSVs, paired comparisons and the report follow completion. The currently running reviewer process loaded the source frozen in C/run_reviews-launched.py; later source fixes do not alter that live interpreter. No full-run errors have been reported. Final audits include all70 accepted commands and per-profile overlap, not a subset of successful measurements. Detailed raw study data remains local; copy only the requested PNG to Minastirith Downloads.

## Interfaces and Dependencies

Use existing installed mjx and Python standard library for orchestration, and cached Matplotlib only for figure rendering. Do not change shared profiles, restart daemons, install tools or rebuild binaries. Review sessions are explicit named-profile top-level sessions with reviewer-only instructions; judge sessions use the existing verifier protocol. Repository changes are committed on the current branch, reviewed and pushed to origin/master after acceptance.

Revision note: Initial registration of the requested high-effort arm, strict profile split and fresh Opus-only comparison. The explicit-account requirement necessitates a recorded direct-session launch protocol difference from the earlier native hidden reviewers.
