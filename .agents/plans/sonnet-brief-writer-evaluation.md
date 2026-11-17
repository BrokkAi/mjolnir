# Evaluate Sonnet as a relation-brief writer

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as the work proceeds.

## Purpose / Big Picture


Determine whether Sonnet/high is an acceptable cheaper writer for the relation brief that Terra/high reviewers read. A relation brief is a Markdown list connecting changed code to other repository locations; it is written without knowing the defect being measured. The completed experiment will provide 22 Sonnet briefs, 66 Terra reviewer outputs, blinded judgments, and a reproducible comparison against Sol-written briefs and no brief. The registered decision is Sonnet acceptable when its primary catch rate is at least Sol's minus ten percentage points and the paired confidence interval for its improvement over no brief is wholly positive. Otherwise the reading remains "Sol writes". This experiment changes no production configuration.

## Progress


- [x] (2026-11-17 17:43 UTC) Read the revised registered study plan from supervisor commit `702aab028bf9bad94245b875f66803772acaf431` before any Sonnet output. Round K compares all three conditions together; the earlier five-diff drift sample is removed.
- [x] Checked production quota and model discovery. Claude6 has 32% of its week; claude5's five-hour window has 23% and is ineligible under the 25% floor. Codex4 has 78%. Model ID `sonnet` advertises Sonnet 5.5 and high effort.
- [x] Read the existing writer, review, judging and readout design through a delegated source map; no sessions for the experiment have started.
- [x] Added Sonnet writer kind and TS review-arm mappings; the writer CLI exposes the new kind and both quota windows were rechecked.
- [ ] Generalize the existing helper with isolated regression coverage.
- [ ] Obtain direct user approval for the complete Sonnet study after local preparation; the initial launch attempt was rejected before execution.
- [ ] Commit the frozen plan and tested harness, then run one writer canary followed by the remaining writers.
- [ ] Complete paired leak judgments, any allowed rewrite/recheck, and report the result before any Sonnet-brief reviewers start.
- [ ] Run one reviewer-job canary, then complete all 22 jobs and 66 reviewers.
- [ ] Build and judge round K, handcheck required outputs, compute paired results and round-J consistency, and run the blinded containment diagnostic.
- [ ] Independently verify the readout, document cost/tokens/time and limits, commit locally, report SHA, and stop all owned work. Never push.

## Surprises & Discoveries


The initial assignment used five seeded BT control diffs and round-J baseline labels. Before any output, review changed the design to fresh mixed judging of TS, BT and no-brief on all 22 diffs. The primary decision therefore uses only round-K labels. Round J is a consistency reference, not a substitute baseline.

Current quota is materially below the earlier estimates. Of the subscription Claude profiles, only claude6 currently clears every applicable quota window. Claude5's larger weekly balance does not override its five-hour floor violation. Do not use the usage-billed Claude AWS profile or the separate Fable quota pool as substitutes.

Automatic approval review rejected the first attempted Sonnet canary before any command executed: the new external-model arm was requested only through an agent relay, which is not trusted user authorization for repository disclosure and quota use. No Sonnet session or output exists. Complete local code/tests and obtain direct approval before launching any study session; do not route the rejected action through another tool or child.

## Decision Log


- Decision: Keep the historical filename `brief_writer_terra.py` and generalize it with `--writer {terra,sonnet}`, defaulting to terra. Rationale: one implementation can serve both studies without copied helpers or compatibility wrappers; existing commands and regressions remain valid. Date/author: 2026-11-17, parent c8c109b6.
- Decision: Use `blind-sonnet`, arm TS, and native model ID `sonnet` with effort high, initially pinned to claude6. Rationale: the ID and effort were discovered from production `mj models --profile claude6`. Date/author: 2026-11-17, parent.
- Decision: Reuse the established bootstrap seed 20261116, 10,000 common-index resamples over diffs, and exact rational contrasts. Rationale: the new study changes writer identity, not the estimation method. The dropped seed 20261117 belonged only to the abandoned five-diff control. Date/author: 2026-11-17, parent.
- Decision: Keep the existing scorer's full baseline selection rather than adding a new model-filter feature. Round K will contain TS66, BT66, no-brief Terra72 and no-brief Sol66: 270 outputs, three batches of eight/eight/six diffs. Rationale: this matches round J and avoids changing sampling/completeness logic; the reading excludes Sol reviewer outputs. Date/author: 2026-11-17, parent.
- Decision: Use Sol/high on codex4 for paired leak checks, round K, handcheck and containment. Rationale: Carbon's pool is closed; Sol/high is the registered judging and containment model. Record all grader provenance before runs. Date/author: 2026-11-17, parent.
- Decision: Compare K with J on identical BT and no-brief Terra session IDs across all 22 diffs; report overall, primary and secondary changes with an absolute greater-than-ten-point flag. Rationale: it makes the plan's consistency caveat explicit without affecting the primary decision. Date/author: 2026-11-17, parent.

## Outcomes & Retrospective


Local preparation and implementation are in progress. No Sonnet writing, review or grading output exists yet. Execution additionally awaits direct user approval after an automatic-review rejection. Existing V1 artifacts must remain unchanged and independently readable.

## Context and Orientation


Work in `/home/jonathan/Projects/mjolnir/.mj/clones/c8c109b6c1c9a2378fe25ca5c76c9016`. Harness files are under `.agents/evals/review-arms/`. Data lives at `/mnt/optane/review-arms-20261113`, selected by REVIEW_ARMS_DIR. The committed `mjx` calls the installed mj on the production/default instance with a clean environment. Never use the stale data-root `mjx`, start the retired private daemon, or run a test binary against production.

The registered study plan is `brief/PLAN-writer-sonnet.md`, copied unchanged from supervisor checkout dbcece95 after the revision. The original `brief/PLAN.md` defines the frozen primary P18 and secondary S4 diffs, writer prompt and checks. `brief_driver.py` writes briefs using `brief/WRITER.md`; `driver.py` injects published briefs into historical review context and launches three Terra/high reviewers through normal mj review selection. TS means Sonnet-written brief; BT means stored Sol-written brief; TT means stored Terra-written brief. All reviewers in the reading are Terra/high.

`brief_writer_terra.py` already builds paired leak tasks, one allowed neutral rewrite, hash-bound judgments, blinded contains tasks, usage comparisons and exact paired readouts. `brief_score.py` builds mixed blinded batches and checks completeness. `verify_driver.py` runs task/judge sessions on the default daemon, uploads only each task's inputs, checks coverage, and suspends the session after collection. Its workspace record is `default-instance/verify/workspace.json`; no reviewer workspace is needed for scoring or quota checks.

Old artifacts remain `briefs/blind`, `briefs/blind-terra`, `runs/<bug>/BT`, `runs/<bug>/TT`, and `brief-judge-j`. New artifacts use `briefs/blind-sonnet`, `runs/<bug>/TS`, `brief-judge-k`, a distinct Sonnet containment task directory, and repository `results/brief-writer-sonnet/`. A blinding key maps random input IDs to writer identity; it always stays outside uploaded task directories.

## Plan of Work


### Milestone 1: Generalize and validate the existing harness

Add Sonnet/high to `brief_driver.KINDS` and TS to `driver.ARMS`/`BRIEF_ARMS`. Do not change the stage-B writer prompt, lint rules, tool isolation or fix-round policy. Generalize the existing helper with one selected-writer configuration supplying subject arm, kind, model, output paths, judging round and consistency reference. Every workflow uses this configuration rather than copied functions. The default Terra configuration must preserve all existing commands, paths and tests.

The Sonnet readout uses K labels for TS, BT and no-brief, and exact paired contrasts TS-minus-BT and TS-minus-no-brief on P18, with S4 reported separately. TT remains a historical beside-the-reading result from J, explicitly marked as such. The Sonnet success wording is "acceptable writer", not a claim that production defaults were changed. Preserve the old Terra `default_writer` report field for its existing study. Record writer identity, grader identity, source rounds and immutable raw-artifact hashes.

Use the same paired leak and rewrite pipeline under Sonnet-specific directories. Each brief requires two valid, hash-bound leak labels; perform at most the existing one neutral rewrite when needed and rejudge it. Do not silently drop or repair missing IDs. The containment task uses the same stage-A rule on 22 Sonnet and matching 22 Sol briefs, with randomized IDs and no uploaded writer key. Usage for the Sonnet study compares all three writer models; missing cost or token measurements remain null rather than becoming zero.

### Milestone 2: Writers and reviewers

Run one Sonnet writer canary, verify actual model/effort/profile, artifact output, lint, inventory, image identity, transcript isolation and usage coverage. Only then launch the remaining writers with stop-on-failure. Check both claude6 and codex4 floors before new work. Report refusals or below-floor conditions immediately; do not switch profiles silently, consume API credit, or modify configuration.

After all 22 briefs pass checks, run the paired Sol/high leak judgments and any single rewrite/recheck. Report the result to the supervisor before starting reviewers. Then run a single TS review job, verify three nonempty Terra/high handbacks, exact brief injection and clean inventory, and complete the remaining 21 jobs. Record per-job image IDs and wall times. Individual reviewer latency may be unknown when retained handbacks have no prompt timestamp; preserve that distinction.

### Milestone 3: Judging, diagnostic and delivery

Build K with TS and BT plus stored no-brief outputs; never rerun BT/no-brief reviewers. Run two independent Sol/high judges per batch with quota checks before each new session, then `collect`. Handcheck every caught/partial and every disagreement using the prior HANDCHECK.md task shape. Accept only complete validated decisions, inspect any changes, and compute the registered readout. Check identical-session label consistency against J without pooling their labels into the primary estimate.

Run the separate blinded contains task with one Sol/high session, validate all 44 IDs, hashes, labels and exact quotes, and report it as diagnostic only. Independently recompute key rates/intervals from raw keys, merged judgments and decisions. Add a README section, structured results, usage/image records and input manifests. Commit validated checkpoints and final results locally on the current branch. Send the supervisor the reading and SHA; never push or request review unless explicitly instructed later. Stop drivers and helpers when finished.

## Concrete Steps


From `.agents/evals/review-arms/`, use the committed wrapper to check quota and models. The environment floor syntax is profile-specific:

    export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
    export REVIEW_ARMS_INSTANCE=default
    export REVIEW_ARMS_QUOTA_FLOOR=claude6=25,codex4=25
    ./mjx quota --profile claude6 --json
    ./mjx quota --profile codex4 --json
    ./mjx models --profile claude6 --json

The updated helper commands retain their existing steps and add writer selection:

    python3 -B brief_driver.py --kind blind-sonnet --profile claude6 --stop-on-failure archive-resume-hint-without-prompt
    python3 -B brief_writer_terra.py leak-build --writer sonnet
    python3 -B brief_writer_terra.py leak-collect --writer sonnet
    python3 -B brief_writer_terra.py contains-build --writer sonnet
    python3 -B brief_writer_terra.py usage --writer sonnet
    python3 -B brief_score.py build --arms TS,BT --bugs-from /mnt/optane/review-arms-20261113/writer-sonnet-jobs.json --bugs-per-batch 8 --name brief-judge-k
    python3 -B verify_driver.py brief-judge-k --kind judge --model gpt-6.1-sol --profiles codex4 --target podman --bundle mjolnir --concurrency 2
    python3 -B brief_score.py collect --name brief-judge-k
    python3 -B brief_writer_terra.py readout --writer sonnet --name brief-judge-k

The jobs file contains all 22 frozen bug IDs with arm TS. Long drivers must log to the data root and survive tool-call completion; get their real PID from host `ps`, not `$!`, and check host processes with elevated execution because sandbox `ps` has a separate PID namespace. Use elevated execution for installed mj/Podman orchestration. Never start a new job from an ambiguous launch-intent record.

## Validation and Acceptance


Run the existing nine scorer/readout regressions and four workspace/instance regressions plus only new tests for changed model-selection and round-reference boundaries. New tests must cover complete TS data, incorrect arm/kind or missing observations, same-index bootstrap with partial never caught, exact zero/ten-point thresholds, and mismatched consistency session sets. Test temporary persisted data through actual helpers/CLIs without live daemons. No Rust source is changing, so no Cargo build or test is needed.

Acceptance requires 22 valid briefs, 66 valid TS reviewer outputs, complete paired K judgments and required handchecks, a reproducible P18/S4 readout, 44 valid containment outputs, quota compliance, unchanged old V1 results, documented unknown measurements, local commits, and no owned drivers left running.

## Idempotence and Recovery


Use existing done/intent records and stable command IDs. Completed jobs are skipped; existing sessions are collected rather than duplicated. An intent without a launch record requires reconciliation with production session titles before retrying. Only archive a proven pre-session failure; never erase working files to stop a process. On quota refusal or a below-floor report, stop starting new jobs and report the exact profile/window, letting accepted work drain. Do not reinterpret unknown quota as available. Reuse validated tests unless a later edit or concrete failure invalidates them.

## Artifacts and Notes


The registered plan revision is supervisor commit `702aab028bf9bad94245b875f66803772acaf431`; record its copy/hash before any Sonnet output. Prior V1 reading is TT29.6%, BT55.6%, no-brief20.4% on P18, all round-J labels. These numbers are historical context only and must not be substituted for K's rejudged baselines. Raw V1 and new Sonnet data share the data root but use disjoint artifact names.

## Interfaces and Dependencies


Use the existing installed `mjx`, `brief_driver.py`, `driver.py`, `brief_score.py`, `verify_driver.py` and generalized `brief_writer_terra.py`. No new scheduler, runner, store, plugin, production setting or build layout is needed. Parent owns design, documentation, orchestration, integrated review and commits; children may implement disjoint harness slices and independently verify measurement integrity. No concurrent Cargo or other build is planned.

Revision note: Initial plan records the revised mixed-round design and actual quota/model discovery before implementation or experimental output. The progress and execution gate now record the automatic-review rejection before any Sonnet session started.
