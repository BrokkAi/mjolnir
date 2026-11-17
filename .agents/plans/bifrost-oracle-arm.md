# Run the registered Bifrost oracle-fact arm and mixed round L

This ExecPlan follows `.agents/PLANS.md`. The parent maintains its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective. Registration is `brief/PLAN-bifrost-oracle.md` under `.agents/evals/review-arms/`, copied from lead commit c0057b53b before any new brief. The earlier blind-brief experiment remains immutable and complete at 8cb640946.

## Purpose / Big Picture

Test whether Terra reviewers benefit when their brief actually contains the decisive census fact on Bifrost's 17 relation diffs. This is the premise behind supplying better facts to a writer. It does not measure whether Bifrost queries can retrieve those facts. The result is a new oracle arm, OT, compared to the stored no-brief A and blind-brief BT outputs. An oracle brief is the frozen final BT brief with one neutral relation inserted or a partial item replaced, unless it already carries the fact.

The deliverable is a locally committed readout with mixed-round labels for all three arms, paired confidence intervals, stored-label consistency, fact and leak checks, cost/coverage, execution limits and cleanup. Nothing is pushed. No earlier corpus, brief, label, decision or readout is overwritten.

## Progress

- [x] Read original registration 1c67abb55 and its pre-brief correction c0057b53b. No oracle model role has started.
- [x] Record the 17 fixed relation IDs in existing order, final BT hashes, census hash and OT job file under the data root's oracle-arm directory. Current ordinary cached codex4 quota is 76%, valid/no error; retain the 25% per-launch floor.
- [x] Map existing builder/collector, oracle instructions, task/judge runner, paired leak/rewrite helper and scorer. Workflow tool is unavailable here; reuse the existing task runner without a copied runner.
- [x] Generalize oracle construction and scoped candidate leak handling; implement explicit round-L reading and same-session reference comparison. Preserve existing defaults.
- [x] Run focused offline serialized-format/hash/isolation regressions after both implementers finish; parent review and local commit before any editor.
- [x] Run 17 Sol/high editors and validate schema, minimal edits, lint and exact fact projection. Freeze raw edits and unpublished candidates.
- [x] Run and freeze paired leak round 0. Two Sol/high judges completed 34 verdicts over the 17 candidates.
- [ ] Review the one wording pass and paired recheck for the 15 flagged candidates. Parent fact-check every final brief with its exact hash and verbatim carrying item before OT admission.
- [ ] Freeze final 17 oracle briefs and run 17 OT jobs with three Terra/high reviewers each; freeze 51 original outputs and suspend own carriers.
- [ ] Round L: mixed stored A/BT and new OT, 153 opaque outputs and 306 paired verdicts. Hand-check all positives/partials/disagreements before arm attribution, then score all arms on round-L labels.
- [ ] Report the registered four-outcome reading, OT–BT, per-diff rates, stored-label consistency, retained neutral flags and cost; commit locally, validate frozen/index bytes and finish cleanup.

## Surprises & Discoveries

Integrated source review caught the new scorer reading referring to its report before initialization. The parent moved the explicit reading after the existing shared rate/effect/accounting report, so it reuses that calculation and only bypasses the old two-model stage-A decision. A serialized 17-diff build/collect/score/reference regression is required before admission; syntax parsing alone did not protect this path.

The original JavaScript workflow runs its own Opus agents and there is no Workflow tool in this harness. The existing verify_driver task mode already creates checked high-effort sessions, uploads only a unit's files, fetches structured output and suspends it. Adapt the oracle file contract to that runner, preserving the original editor instructions and output schema.

brief_oracle.build hardcodes mjolnir census paths and work directory. Its collector already withholds publication for unchecked or stale fact decisions. Its legacy stripped-text hash differs from the newline-normalized final-byte fact hash. Do not interchange those conventions.

brief_score.score unconditionally calls the original stage-A two-model/control decision when oracle is present. A Terra-only P17 round would fail or misapply that decision. Add an explicit persisted reading selection, with unchanged legacy defaults.

An initial sandboxed quota read could not open the ordinary startup lock/log housekeeping path. The authorized production wrapper succeeded outside the sandbox; both records are preserved. This was not an authentication or quota-capacity failure.

## Decision Log

Use the existing driver.py OT arm, brief_oracle.py, brief_writer_terra.py scoped paired checks, verify_driver.py task/judge modes and brief_score.py. This generalizes the mechanisms already checked rather than introducing another runner. No Rust builds, store changes or daemon changes are involved.

Use the final published BT briefs at ROOT/briefs/blind, including the four neutralized texts, not the original pre-leak writer archive. They are the bytes the stored BT reviewers received and the earlier contains diagnostic measured.

Keep the existing ROOT/arms/sets.json and corpus paths unchanged; filter construction and judging by the new exact OT job file. Work and candidates belong under ROOT/briefs/bifrost-oracle-work. Only the final accepted briefs publish to ROOT/briefs/oracle, the existing OT injection path. Original A/BT paths are read-only.

Editors and paired judges are codex4 GPT-6.1 Sol/high; OT reviewers are three GPT-6.1 Terra/high through normal selection. Use the production default instance via committed mjx, the owned Bifrost clone/local Podman target and the unchanged 25% per-launch floor. Quota snapshots are shared availability, not isolated consumption or a dollars conversion.

Registration c0057b53b has four outcomes. Implement the listed precedence: carry, partly, don't carry, inconclusive; the lead confirmed this priority before any brief is built. An interval entirely between zero and +10 is Partly, a proven small lift. This ruling is recorded in the registration notes before model admission. A positive result concerns supplied facts only; query reach remains unmeasured.

## Outcomes & Retrospective

Preparation and registration are recorded, with no new experimental outputs. The prior blind study reads Does not generalize and remains intact. Harness generalization and its validation are the next milestone.

## Context and Orientation

Work in `/home/jonathan/Projects/mjolnir/.mj/clones/d2fa840b25852a07c2e024357afec69d`. Scripts are `.agents/evals/review-arms`. ROOT means `/mnt/optane/review-arms-bifrost-20261116`. The owned history clone is `/mnt/optane/tmp/d2fa-bifrost/bifrost-dev`, pinned to 35933693612806a7ab6446bdcf52cfc579eebf9d. Never fetch or change the stale shared `~/Projects/bifrost` clone.

ROOT/census-results/missing-facts.jsonl is the frozen complete Sol census, hash cec120df43bc890efbfbd5fee79fc2236ff654e1249b3dcb1f9309c054536089. ROOT/census-results/relations.jsonl has flags for all 30 known diffs; select the 17 true relation entries in existing order. ROOT/oracle-arm/ids.json and jobs-ot.json record exactly that order, with one OT job per diff. Registration and the copied corrected plan are ROOT/oracle-arm/registration. Existing environment is ROOT/arms/environment.json; retain its default-instance, corpus/diffs/bundle/repo settings and codex4=25 gate. The preseeded verifier checkout/workspace already belongs to this study.

The editor may see only its item, ORACLE.md and WRITER.md plus historical source at intro/base as needed. The item contains id/group/intro/base/blind_brief and census fact/evidence/other_facts/kind, with allow_in_diff derived from group. Never upload the census query, full census, known-bug record, fix commit, another brief, previous diagnostic verdict or outcome. Leak judges receive the original known-bug decision inputs; reviewers see the oracle brief in the same historical context wrapper as stored arms. Private judging keys stay outside uploaded units.

## Plan of Work

The parent owns this plan, registration, integration, fact decisions, rewrite acceptance, stage admission, handchecks, commits and readout. Implementers own disjoint files. One owns brief_oracle.py, brief_writer_terra.py and focused oracle task/serialized-format tests; one owns brief_score.py and test_scoring_interfaces.py. They may syntax-check their own files during edits. Neither runs importing tests while the other writes shared modules. After both report source completion, one designated child runs the combined focused tests; fix and rerun failures only.

After the scorer handback, the parent took ownership of brief_score.py to correct the integrated report path. Its implementer now owns only the additional serialized scoring-interface regression. The construction owner keeps its original files. Neither child changes the parent's scorer while preparing tests.

For construction, generalize brief_oracle.py with explicit census, bugs-from and work-name options while retaining old paths/default behavior. Scope must persist and reject mismatched reuse. Add task-unit build/collect/sync steps rather than another runtime. Build standard immediate task directories under the work directory's edit subdirectory. Each contains only INSTRUCTIONS.md, item.json, ORACLE.md and WRITER.md. Instructions change only output delivery: write the original editor schema to edit.json instead of the unavailable StructuredOutput tool, with only that untracked output write allowed. The original editing rules still apply, and repository reads are limited to intro/base; no other history, network, tests, builds, tracked edits, memory or experiment data.

The schema is id, contains (yes/partial/no), matching_item, inserted_item, replaced_item, oracle_brief and rationale. Validate IDs/schema and lint, and preserve raw output. For yes, return the original brief unchanged; for partial/no, preserve every unaffected item and edit one item as the original rules specify. Any failure stays unpublished and is reported, never silently coerced or dropped. Candidate texts and immutable edit metadata go under work/candidates; no initial publication to the OT path.

Generalize the paired leak helper to kind oracle and an explicit brief-root override, defaulting to its current directory. Include an explicit override in saved scope and use the same setting for every build/collect/rewrite step. This lets the existing paired checks and one wording-only rewrite operate on candidates without publishing or modifying BT. Retain all raw A/B verdicts and exact text hashes. The sync step translates completed edit and paired-leak records into the existing workflow result schema; require complete hash-matching final checks and at most the registered rewrite. Never represent unperformed checks as missed.

Use the existing collector to emit final factcheck rows and publish only after the parent decision names the exact newline-normalized final-byte SHA and a carrying item literally present in the final brief. Enforce literal quote presence rather than accepting whitespace-only reconstruction. A rewrite invalidates stale decisions. The parent reads each item against fact/evidence/other facts, checks neutral wording and preserved anchors, and records carries or an explanatory failure. Any failure holds OT admission for the full fixed set; there is no case exclusion. A remaining neutral caught/partial leak is retained and reported after the final fact check, not stripped of the tested fact.

For scoring, add an explicit reading option and reference-round option to brief_score build, persist them in the round key, and restore them for collect/score. Missing options preserve legacy behavior. The bifrost-oracle reading requires Terra-only P17 and exactly three A, BT and OT sessions per diff. Generic rates and paired effects remain the existing calculation; bypass the unrelated original two-model/control decision only for this explicit registered reading. Use the same bootstrap implementation, retaining raw quantiles for strict decisions before display rounding if necessary; default outputs remain unchanged.

The reading reports OT–A and OT–BT means/paired intervals, exact per-case counts/rates, and the four registered outcomes. Facts carry means estimate at least17 points and lower bound strictly above zero; Partly means lower bound above zero and estimate below17; Facts don't carry means upper bound strictly below10; otherwise Inconclusive. Apply the listed-rule precedence, confirmed by the lead before admission. Load final labels through one common validated reader, including required handchecks; do not duplicate interpretation of missed/missing decisions.

The reference is ROOT/bifrost-judge-relation, with its private key under ROOT/private. Compare round-L A/BT final labels to reference final labels by the same session IDs, not opaque output IDs or raw pre-handcheck labels. Require complete same-session sets, preserve the old labels/files, report per-arm percentage-point differences and changed-label counts, and flag any absolute difference greater than10 points. Stored labels are a consistency comparison; every arm in the new effect uses round-L labels.

## Concrete Steps

Before editors, commit the copied corrected registration, this plan and fixed IDs/jobs/hashes locally. Generalize the shared source, run focused offline regressions and parent-review the diff. Freeze new runtime hashes separately from prior immutable be74 hashes; do not rewrite earlier freezes.

Construction command shapes are brief_oracle.py edit-build/edit-collect/task-sync and collect, with the same census/jobs/name options on every step. Run verify_driver.py on briefs/bifrost-oracle-work/edit with kind task, output edit.json, profiles codex4, model gpt-6.1-sol, target podman, bundle bifrost-dev, repo-commit the source pin, concurrency3. The unit instructions enforce historical source reads. All sessions check high before prompting and normally suspend on completion.

The paired helper uses kind oracle, brief-root ROOT/briefs/bifrost-oracle-work/candidates, bugs-from ROOT/oracle-arm/jobs-ot.json and name bifrost-oracle. Round0 covers17 briefs; only flagged briefs may get the one rewrite/recheck. Parent accepts raw rewrite deltas before publication. Sync and factcheck collect then freeze all17final published oracle hashes and decisions.

Run driver.py with ROOT/oracle-arm/jobs-ot.json, concurrency3jobs and stop-on-failure. This launches only17OT carriers/51Terra reviewers. Record actual roster, source/context/inventory/image hashes and normal role end; suspend own carriers, preserve stored handbacks/usage. Reviewer tool audits/latency remain unavailable unless actual captured evidence exists; no new capture mechanism.

Build RoundL with brief_score.py build, arms OT,BT, models gpt-6.1-terra, bugs-from the OT job file, name bifrost-judge-l, bugs-per-batch8 and the explicit oracle reading/reference. Expect153opaque outputs, three cells per arm/diff and306paired labels; every stored A/BT output hash must match its old archive. Run verify_driver judge with Sol/high/codex4 and the same production setup. Collect and perform the same blinded round-J handcheck procedure for every caught/partial/disagreement, fixing parent decisions before arm attribution. Score and report once.

## Validation and Acceptance

Tests protect external structured editor/round formats, query exclusion, exact scope/IDs, stale-hash/wrong-quote refusal, final carrying item after rewrite, candidate/publication isolation and old default compatibility. Scoring tests cover three-arm key roundtrip, one-model selection, strict zero/10/17 boundaries, same-SID reference comparison and missing/duplicate coverage failures. Use temporary Git repositories and recorded/serialized output fixtures; no live model or daemon tests, no Cargo. Do not add tests that merely restate an ordinary default/count. One child runs applicable existing and new offline test files after both writers stop, then only failing tests after corrections. Parent checks syntax/diff and final source/input hash consistency.

Success requires17fully validated fact-carrying briefs,51new exact original reviewer outputs,153complete mixed-round labels with all required handchecks, explicit registered interpretation, source/reference consistency and incomplete-cost qualifications. Freeze data and hashes under ROOT/oracle-arm and mirrored harness brief/bifrost-oracle. Commit locally without push. Retain old experiment outputs and its Does not generalize reading unchanged.

## Idempotence and Recovery

Every step records immutable input/output hashes and scope; never overwrite accepted role outputs or old study paths. Ordinary read-only API correlation errors may recover the same accepted IDs with original evidence retained. Drain accepted work before resuming and never repeat a review cell. The existing driver's review-start/done markers support collection-only recovery and completed-job skips. Quota null/error/below25 holds new launches while accepted work finishes; do not change profile, credits, daemon or configuration. Record snapshot age without adding a freshness-after-each-stage gate.

All own editors, wording editors, judges and carriers are suspended at role end; reviewers end normally. Account root usage once per SID and preserve null for missing cost. Do not infer attention from absent reviewer transcripts, clean truth from no findings, actual engine reach from supplied facts, or subscription spend from API-equivalent estimates.

## Artifacts and Notes

The original corrected registration is available read-only in the lead clone at c0057b53b. The first construction map is `.mj/agents/24873100ef4a5d06e031541f9a4c328d/oracle-map.md`; the parent decisions here supersede its generic distinct-mode suggestion. Prior final readout is `.agents/evals/review-arms/brief/bifrost-arms/README.md`. Do not amend it for the oracle experiment.

Revision note: initial plan records the corrected reading, existing-runner adaptation, isolated candidates and explicit mixed-round scoring before any editor or reviewer. The reason is to test supplied facts without leaking the census query or changing stored comparison arms.

Revision, 2026-11-17: the lead confirmed listed outcome priority before any brief. Record Partly for a proven positive small lift even when the interval is wholly below +10; use unrounded boundaries.

Revision, 2026-11-17: focused offline checks pass after recorded construction and scoring-fixture corrections. Parent reviewed integrated source and preserved all 6,862 baseline artifact hashes. Candidate/rewrite newlines are canonicalized before leak checking, so final-byte hash bindings agree. No experimental role has started.

Revision, 2026-11-17: all 17 original editor tasks completed; one container bootstrap failed before any prompt and normal same-SID resume refused for lack of a checkpoint. The failed zero-turn record/diagnostic was archived before the existing runner reprovisioned only that unprompted case. Eighteen admitted SIDs represent 17 successful editor roles plus one zero-turn failure; no accepted turn or completed case was rerun. A transient empty cached quota report held an earlier pre-admission attempt; normal cached reads recovered without source/config changes. The original registration hashes match; an operator compared the wrong plan path and its false mutation note is corrected in bootstrap-recovery-ruling.json. Standard CLI transcript defaults exclude tool roles, so the audit uses explicit role filters, not an events-stream workaround.

Revision, 2026-11-17: round 0 gave 10 caught, five partial and two missed. One build command pointed to the shared checkout and failed argument parsing before admission. The two actual judges ran from the frozen checkout and suspended normally. Their full usage reports total $0.3831644 at 10 billion ticks per dollar. The 17 editor turns total $1.5263112 and their transcript audit covers 114 tool requests. One registered wording pass is running for the 15 flagged cases.
