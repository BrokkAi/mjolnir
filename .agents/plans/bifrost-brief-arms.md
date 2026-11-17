# Run the registered Bifrost brief arms through the shared harness

This ExecPlan is maintained under `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log and Outcomes & Retrospective current. The statistical registration is `.agents/evals/review-arms/brief/PLAN-bifrost-bugs.md`; this document specifies execution without changing its reading rules.

## Purpose / Big Picture

Measure whether blind relation briefs improve three Terra/high reviewers on Bifrost's confirmed bugs. A relation brief lists connections between changed code and existing repository code. Its writer must see only the historical introducing change. Compare the mean caught share for each diff in two arms: A has no brief and BT receives the Sol/high writer's brief. Start with the frozen17 relation diffs, judge both arms, and report that primary reading before spending on the remaining13 known bugs and40 ordinary controls. Ordinary controls have no known defect; they require verification of discovered findings rather than known-defect catch labels.

## Progress

- [x] M1 corpus and quota-amended census frozen locally in ec71375a4:30 confirmed bugs,40 controls,17 primary relation labels.
- [x] (2026-11-17) Review cutoff correction committed c0fe5f378 before any brief or review: explicit strict UTC boundary, same-seed control redraw, all30 bug/census records unchanged, original mjolnir corpus audited only.
- [x] Existing production-wrapper and stage B/round J mechanisms mapped; stage usage estimated from490 historical sessions and7 round J usage reports. Full-plan quota fit cannot be certified.
- [x] Registered Sol leak/judge amendment and relation-first details before any brief; job-order seed20261119 recorded before34job shuffle.
- [x] Shared harness source/scoring/leak integration reviewed and committedbe74a5cb9;6runtime and13scoring/default offline tests pass. Real frozen-input import check confirms70records,17/13sets and3Terra/highA/BT cells.
- [x] Changed harness interfaces validated offline; owned production workspace35b2367fbd89852d91b79743c552c3e1 and verifier workspace7dbd0e675f40b2d0792dec118a7ffba4 prepared against the owned full-history Bifrost clone.
- [ ] Freeze relation stage jobs, input/code hashes, launch quota and estimate; write17 blind Sol briefs, check leakage, run A/BT with3 Terra/high each, judge twice and hand-check required rows.
- [ ] Report the relation reading, cost/coverage/limitations and stage commit; suspend own carriers/writers/judges. Reestimate remaining13+40 and wait for lead direction before continuing if full plan remains below-floor risk.

## Surprises & Discoveries

The sampler previously passed a timezone-free cutoff to Git. One selected control was after UTC midnight; correcting the pool with the same seed changes39 of40 controls because history slices move. All30 bug introductions remain eligible; their census and relation set are byte-identical.

The local harness predates the production wrapper. Importing b4446dc5e alone is insufficient: the coherent source snapshots at lead commit5818c5380 include pinned writers, scoped quota checks, stop admission and lazy instance-specific workspaces. Drivers also hardcode mjolnir source paths and the original P18/S4/C8 sets. These must be parameters, not a second runner.

Reviewer sessions are destroyed when their role ends (#1554/#1559), leaving stored handbacks and usage but no transcript/session timing record. Lead explicitly directs reporting new reviewer tool audits and latency as unavailable. Do not build capture or infer an absent audit is clean.

The census81-to79percent quota pair is not a calibration: the after snapshot refresh predates the first prompt. No token or estimated-dollar conversion to weekly quota is known. Historical API-equivalent costs are scenario estimates, not subscription charges or quota guarantees.

## Decision Log

Decision: reuse driver.py, brief_driver.py, verify_driver.py, brief_oracle.sets, brief_score.py, brief_writer_terra.py leak/rewrite helpers, build_verify.py, score.py and brief_control2.py. Rationale: these already implement historical blinding, stored handbacks, paired judges and Control2; parameterizing them preserves one mechanism. Author/date: parent,2026-11-17.

Decision: use the production default instance only via the imported mjx wrapper; local Podman target and configured bifrost-dev bundle, owned Bifrost clone. Rationale: lead requests this host, no private daemon or global config changes; existing placement code is local Podman. New study sessions only are created, configured, and suspended. Author/date: parent,2026-11-17.

Decision: relation17 stage first and stop for a reading before13+40. Rationale: full central historical estimate$33.74 versus envelope$51.98API-equivalent does not certify remaining quota; relation estimate$8.98/$13.87 is a smaller gate, not a guaranteed bound. Existing paired leak helper uses two Sol/high checks; increase the provisional single-leak proxy accordingly before launch. Author/date: parent,2026-11-17.

Decision: writers/leak checks/both catch judges use codex4 Sol/high; every reviewer arm explicitly requests3 Terra/high through normal selection. Quota floor25 is checked by the existing gate with the approved profile scope; any gate stop is reported without changing profile/model. Source helper floor40 remains applicable to additional preparation waves. Author/date: supervisor instruction and parent,2026-11-17.

## Outcomes & Retrospective

M1 is complete and the UTC correction is committed. No study writer, brief, reviewer or judge has run yet. Own master is now7106e3fab, fast-forwarded per lead to the reviewed production baseline. The next milestone is shared-harness readiness, not an experimental result.

## Context and Orientation

Work in `/home/jonathan/Projects/mjolnir/.mj/clones/d2fa840b25852a07c2e024357afec69d`. All data belongs in `/mnt/optane/review-arms-bifrost-20261116` (ROOT). Harness scripts live in `.agents/evals/review-arms`. The selected records are ROOT/corpus/corpus.jsonl30 and ROOT/corpus/diffs.jsonl40. Primary relations are ROOT/census-results/relations.jsonl,17 true and13 false. The source snapshot is35933693612806a7ab6446bdcf52cfc579eebf9d; the owned full-history clone `/mnt/optane/tmp/d2fa-bifrost/bifrost-dev` is read-only and not another session's working clone. The lead clone is `/home/jonathan/Projects/mjolnir/.mj/clones/dbcece95f1cdc80785d2df3f995138f2`; import source files from its immutable5818c5380 object, never overwrite its working tree.

The existing driver exposes CORPUS records, ARMS reviewer lists, ROOT paths and save/mj helpers. Add source settings there: REVIEW_ARMS_CORPUS overrides known-bug JSONL, REVIEW_ARMS_DIFFS overrides ordinary JSONL, REVIEW_ARMS_BUNDLE and REVIEW_ARMS_REPO_DIR override bundle and container repository directory. Keep defaults identical for existing mjolnir studies. REVIEW_ARMS_REPO already selects the host Git checkout. Export resolved CORPUS_PATH and DIFFS_PATH for consumers. REVIEW_ARMS_SETS optionally names JSON with P/S/C arrays; parent freezes P=17 relation IDs,S=13 remaining known IDs,C=[] and keeps ordinary controls in a separate jobs list. Validate unknown/duplicate/overlapping IDs; never silently discard them.

## Plan of Work

The parent owns this plan, statistical amendments, integrated review, artifacts, commits and launch. Child045bf owns runtime imports and source parameters in mjx,driver.py,brief_driver.py,verify_driver.py,carrier_test.py and their existing workspace/instance regressions. Import production snapshots, retain local Podman execution, wire all launch bundle/cwd sites to resolved settings, and keep writer tools/prompt/blinding unchanged. No Rust builds or live campaign sessions are required for implementation checks.

A second child owns scoring and leak interfaces: brief_oracle.py,brief_score.py,brief_writer_terra.py,build_verify.py,score.py and applicable existing regressions. Use the same optional sets file and shared driver corpus. Add a persisted judge model selection so --models gpt-6.1-terra requires exactly3 Terra baseline sessions and the requested BT cells; defaults still include Terra and Sol as previously. Preserve exact coverage and private shuffled keys. Allow leak helpers to target kind blind and an explicit jobs subset, with existing two-judge hash binding, one rewrite/recheck and no automatic publication of leaking text. Read missing reviewer timing/transcripts as unavailable in score.py; never synthesize zero or clean audit. Leave legacy writer-model comparison/readout defaults intact.

After both slices pass focused offline interface tests, parent reviews their diffs and commits. No helper launches a campaign or edits global config. Parent imports the matching README production instructions without overwriting this study's own amendments, and records new source hashes separately from historical M1 census code snapshots.

Prepare a ROOT/arms/sets.json manifest from frozen relation flags, plus explicit relation A/BT jobs with deterministic order and17 writer IDs. Check selected IDs exactly equal the17 primary relation IDs and pair every diff in both arms. Use an owned production workspace stored in ROOT/default-instance/workspace.json, and the separate verifier record ROOT/default-instance/verify/workspace.json. Old ROOT/verify/workspace.json is not the new wrapper's path. Never start or stop the default daemon as campaign setup/cleanup, never start private review-arms, never edit ~/.config/mjolnir/config.toml, never add credit flags.

Run writers using brief_driver.py with kind blind, profilecodex4 and stop-on-failure. Each writer is pinned high, historical intro/base and --no-later-history, no GitHub credentials/events/project memory/delegation. Only commit intent/diff/historical repository reaches the writer, not census, defect, match rule or fix history. Capture existing inventories, output, transcript and usage, then suspend it. Run paired Sol leak checks through the existing builder and verify_driver judge mode; keys bind each decision to the exact brief hash. After the registered one wording-only rewrite and recheck, neutral relations still judged caught/partial are kept and listed, exactly as brief/PLAN.md selection step3 says; every factual relation and anchor must be preserved. Parent reviews all edited deltas before BT admission.

Run driver.py with the relation jobs and stop-on-failure, matching A/BT3Terra/high. Explicit reviewer args override the user's unrelated default roster without editing it. Preserve pre/post repository inventories and stored handbacks/usage. Reviewer tool audits/latency are unavailable under normal destruction; README blinding limitations must state this.

Build a Terra-only brief_score round from the exact jobs, run two Sol/high judges through verify_driver judge mode, collect and inspect every caught/partial verdict and every disagreement. Use the same existing hand-check method as round J, with a blinded task containing only output text, bug criteria and judge reasons, no arm/model/private key. Record decisions by output ID and score. Partial does not count as caught. Report mean per-diff caught shares and paired95percent bootstrap intervals; apply the registered17point uplift criterion only as a relation-stage reading. Overall generalization also needs the later ordinary retention reading and cannot be claimed yet.

## Concrete Steps

All commands are from the own repository root. Set REVIEW_ARMS_DIR to ROOT, REVIEW_ARMS_INSTANCE=default, REVIEW_ARMS_REPO to the owned clone, REVIEW_ARMS_CORPUS=$ROOT/corpus/corpus.jsonl, REVIEW_ARMS_DIFFS=$ROOT/corpus/diffs.jsonl, REVIEW_ARMS_BUNDLE=bifrost-dev, REVIEW_ARMS_REPO_DIR=bifrost-dev and REVIEW_ARMS_SETS=$ROOT/arms/sets.json. Use the existing quota gate's supported syntax to express the lead's25percent floor; read current quota and resolved gate scope before launching. Do not execute the old private wrapper before importing its production version.

Run focused existing Python regression files for the runtime and scoring interfaces after each slice. Check default archived mjolnir behavior and explicit Bifrost manifests, missing-data behavior, failed/partial coverage and hash mismatch rejection with subprocess fakes or persisted fixtures; do not launch live reviewers as tests. Run git diff --check. No Cargo changes mean Cargo tests/clippy are unnecessary.

Then use the imported shared commands documented by the two slice reports: brief_driver.py IDs --kind blind --profile codex4 --stop-on-failure; paired leak build, verify_driver.py NAME --kind judge --profiles codex4 --model gpt-6.1-sol with explicit bifrost-dev bundle/local Podman and snapshot; driver.py --jobs relation-jobs.json --stop-on-failure; brief_score.py build --arms BT --models gpt-6.1-terra --bugs-per-batch 8 --bugs-from relation-jobs.json --name bifrost-judge-relation; collect, handcheck and score for the same round. Freeze the final exact command vectors before launch rather than copying illustrative commands blindly. Quota stops report the unstarted cells and preserve accepted work.

## Validation and Acceptance

Acceptance requires17 completed brief texts with completed hash-bound leak checks and, where flagged, one wording-only rewrite/recheck plus documented neutral-relation exceptions,17A+17BT jobs each with exactly3 Terra/high stored outputs,204 valid catch decisions (102outputs twice), and required hand checks complete. Any missing cell means no complete primary readout. Bug/census/relations hashes stay those of the UTC-corrected M1 freeze; no outcome changes membership. Record code/input hashes, actual model/effort checks, profile selection, quota snapshot freshness, usage coverage and missing cost. Preserve absent tool audits/timing as unknown. A complete result includes primary reading and limitations plus next-stage estimates and a local commit, followed by no live own carrier/writer/judge sessions.

## Idempotence and Recovery

Respect existing launch-intent, launch, review-start, done and judge-key records. Resume accepted IDs through the existing drivers; do not create replacements merely because output is slow or a wait ended. Preserve hash-bound brief/judge keys and refuse stale rewrites. On quota floor, admit no new work, retain already accepted work and report the stop. Suspend only study-owned IDs after they finish. Do not suspend unrelated sessions or stop the production daemon. Keep M1 historical freezes and the corrected controls intact.

## Artifacts and Notes

Detailed read-only reuse map: ROOT/arms/preparation-reports/045bf52ed9425283f1ba1946c2b99eb2/stageB-reuse-map.md. Usage evidence: ROOT/arms-preflight/usage-estimate.{md,json}. UTC archive: ROOT/utc-cutoff-correction/pre-correction. Current M1 freeze SHA8859219e907208ce5e7a23e88fbc5188dca0e31a434b7be779efd103d4c32754. All commits stay local; supervisor receives SHAs and controls publication.

## Interfaces and Dependencies

Reuse installed mj through committed mjx and existing local Podman, Git and Python standard library. No new crate, scheduler, runner, credential path or transcript capture. The shared parameter contracts above are the only new source-selection interfaces. Shared files have one child owner; parent integrates after both handbacks and reruns only failures or checks invalidated by integration.

Initial plan written2026-11-17 after UTC correction; no study arms launched. Update this note when scope or evidence changes.

Pace update2026-11-17: lead requests concurrency3 for writers and reviewers with stop-on-failure and unchanged25percent floor. Apply through existing driver admission; preserve accepted writer work and durable command IDs.

Writer milestone2026-11-17: all17 Sol/high codex4 briefs complete with0lintrepairs, clean inventories and17/17 tool transcript audits. One quoted-heredoc scanner hit reviewed as prose. All17 sessions suspended; priced API-equivalent estimate$3.5996476. Fresh post-run cached weekly quota36percent is after finaldone, shared account activity prevents attributing its delta to writers alone. Original texts/manifests frozen in ROOT/arms/writer-stage; paired Sol leak round0 is next.

Leak protocol clarification2026-11-17, from pre-existing stageB PLAN.md:62–68: a neutral relation still judged caught/partial after one rewrite is retained and listed because deleting it removes the fact under test. This corrects the execution plan's earlier shorthand "only clean"; it does not change the statistical registration or stageB method. Round0 has13missed,3partial(Go receiver,MCPdefault,Pythonbinding) and1caught(overrideproof), bothSoljudgesagree. Four existingneutralizationtask units are authorized; parent will inspect factual/anchor preservation before publication/recheck. Editors are the existing post-leak neutralization role, distinct from the17 historically blinded original writers.

Lead clarification2026-11-17: keep review driver concurrency3 JOBS, up to9Terra reviewer turns, plus normal carrier orchestration; preserve25percent launch floor. Lead confirmed existing neutral-relation retention handling. A transient null quota report held rewrite admission; normal cached report recovered32percent at1794889972 without probes/config/daemon changes, and the4editor tasks proceeded. No review arms have launched.

Parent wording review2026-11-17: accepted all4 raw editor diffs after reading every change. They remove emphasis/consequence grammar while retaining the same facts and every anchor. Decisions and before/after hashes are ROOT/arms/rewrite-driver/parent-acceptance.json. Publication and paired round1 recheck authorized; no second rewrite. Reviewer admission remains pending completed hash-bound round1 and final17text freeze, under lead-confirmed3jobs/9Terra concurrency.

Neutralized publication checkpoint2026-11-17: parent published4accepted rawedits through existingrewrite-collect;13textsunchanged, originalwritertexts preserved. Existinground1builder prepared4hash-bound briefs/eightverdicts, no model calls. Finaltextsnapshot is ROOT/arms/neutralized-stage. Latest quota refresh has windows empty/error Codex quota request failed; modeladmission held. Lead requires usable >=25reading, no profileswitch/probe/configaction; if stillunavailable at05:17:40Z telllead fordecision. Operator d051 monitorsnormalcache and mayresume onlyprepared2judge recheck whenconditionmet. Reviewers remain unlaunched.

Round1/modeled execution update2026-11-17: ordinary quota recovered99percent at1794891172; lead reports the week reset and authorizes continuing13nonrelation+40controls immediately AFTER sending the judged17relation reading, without waiting for another reply.25percent floor/perlaunch checks stay. Actual cached report with valid windows/noerror/>=25 is admission evidence; snapshot freshness is recorded for accounting, not a new gate requiring refresh after every priorstage. Pairedround1completed2Sol/high sessions/eightverdicts, cost$0.2037592, bothsuspended. Final17hashes validated:13missed,3partial,1caught, same neutralflags retained perexistingrule. Freeze ROOT/arms/leak-round1-stage. Parent admitted34relationjobs to operator045, concurrency3jobs/9Terra; catchjudging follows completeoutputs. Later13+40 still follow stageorder, with no extra permission required after primary reading.
