# Which RQL facts help reviewers? A Rust census and a mocked-RQL writer

Status: approved by Jonathan and registered on 2026-11-18, before any census label, simulator answer or reviewer run. Advisor input (91233b0e, two rounds) is folded in; its notes are in `rql-census/prep/advisor-notes.md`. Preparation artifacts (the public-repo scouting and the local corpus yield screen with its script) are in `rql-census/prep/`.

## Question

Milestone 5 of `.agents/plans/review-brief-writer.md` would give the brief writer RQL (Bifrost's code query language), not a hardcoded tool. Two questions decide whether to build the RQL features that would need:

1. **Census.** Across real Rust bugs, which relation decides each one, could an RQL query express it, and does today's Bifrost return it? This ranks candidate features by how many bugs they decide, needs no reviewers, and is reported as a ceiling.
2. **Mocked RQL.** If a writer could query a complete RQL, would its briefs lift Terra/high reviewers over a grep-only writer? Real RQL isn't complete yet, so a simulator answers. Its answers must be reasonable and complete, not prescient.

A positive result shows that the information helps when it's delivered this way. It doesn't show an engine is required: the compute-matched arm and the simulator's cost and latency set the bar real RQL must beat.

## Data

**Repositories.**
- Development: mjolnir and bifrost-dev, with their frozen corpora.
- Test: 12 Rust repos (fewer if fewer qualify), fixed before any test reviewer runs. Clones that share history (a common commit or root, such as forks, renamed repos or subtrees) count as one repo; the screen groups them by shared candidate commits and root commits, and keeps the member with the most candidates. Repos come from the local corpus (`~/Projects/brokkbench/sft-tools-commits/rust`, clones in `/nfs/morannon/nvme/repo-clones`, read only). Jonathan chose fewer repos with more bugs each, because Bifrost indexing is expensive per repo.
- Selection rule: rank by realistic 2025+ yield from the screen (`rql-census/prep/scan.py`, then `yield_rank.py`, which turns the capped samples into per-repo yields: upper bound × bug precision 0.85 × admission rate 31/67; results in `yield.json` and `corpus-yield-summary.md`). The screen applies the corpus size rule (goldens, fixtures and lock files don't count) and rejects merge commits as introductions. Incompletely traced fixes never count as strong, and the ranker refuses to rank while any planned draw lacks a trace record. File paths come from Git's NUL-separated name list, not patch headers. Functions are counted from the old file's `fn` signatures, so the screen doesn't depend on a repo's diff drivers. Drop repos with indexing hazards: huge generated code, a non-Cargo build, heavy proc-macro output. The screen only orders repos: its multi-site proxy over-counts (for example, inline `#[cfg(test)]` hunks in production files), so a repo enters the test set only after tracing admits at least 8 relation diffs, going down the ranked list. Inputs are reproducible: `stage_a.py` derives `stage_a_candidates.jsonl.gz` from the SFT lists, then `scan.py` and `yield_rank.py` run on it.
- Rank and measure at least 16 repos: one cold Bifrost index and one incremental re-index each, at a representative revision. Take the 12 that qualify with the most expected admitted bugs per indexing hour, using each repo's expected bugs capped at the tracing stop of 25 (`per_repo_realistic_capped`) and counting distinct introducing changes, at most one per shared-history group. Every member of a group stays ranked, so a related clone remains eligible if the yield leader is costly to index or disqualified. Reaching ±10 needs about 12 repos if the between-repo SD is 10 points, and about 16 if it is 15. More bugs per repo can't substitute for more repos. If fewer than 12 qualify, use all that do; a wider interval then reads as the bands below say.
- A repo counts only if it has at least 8 distinct admitted relation diffs, decided before any reviewer runs on it. Diffs are counted by introducing change: several fixes of one introduction form one diff, carrying the union of their decisive facts. When one introduction has several known defects, the relation defects define the diff's primary catch. A review catches the diff when it identifies any of its relation defects, and the judge gets every relation-defect record and names every one the review matched. The diff-level catch is derived from those matches (any match), and the census uses each defect's own matches. Its local, non-relation defects earn no primary credit; they count only in the non-relation controls. The corpus spread rule (at most two diffs per feature or commit series) still applies.

**Bugs.**
- Candidates come from each selected repo's full git history: fix commits that change non-test code and add or change a test. Each repo's eligible fixes are traced in a seeded random order (seed 20261118), stopping when the repo reaches 25 admitted bugs or the pool runs out. The order and the stopping point are recorded. The brokkbench SFT lists only seeded the screen, and they are a model-screened subset capped at 1,000 commits per repo.
- The corpus SPEC rules (`corpus/SPEC.md`) apply, adapted: the regression test in the fix commit replaces the Hard-won marker, and the cutoff rule becomes the window below. The ordinary-commit rule uses each repo's default branch (the clone's HEAD branch, `main` or otherwise) in place of `master`. Attribution, findable-from-the-diff, real behavior, not pre-flagged, size and spread all apply unchanged.
- Each candidate is admitted by a blind Sol/high tracer with a second-family check of the attribution, as in `PLAN-bifrost-bugs.md`.
- Target about 25 admitted bugs per repo.

**Window.**
- Test bugs have both the introducing change and the fix on or after 2025-01-01.
- If a selected repo falls short of the target, extend that repo's test window to 2024-01-01 and record it. This happens before its development sample is drawn.
- Contamination: a memorized bug tends to be caught in every arm, which shrinks the paired difference. An RQL row can also cue recall of a bug the reviewer missed without a brief, which inflates it, so the direction isn't guaranteed. Sensitivity checks are the after-cutoff subset (cutoffs: Sol 2026-04-30, Terra 2026-06-01, Opus June 2026) and the no-brief catch rate by fix date. The latter is descriptive only, because difficulty and repo mix confound it.

**Development and test split.**
- The RQL step menu is chosen only from development data: mjolnir, bifrost-dev and up to 10 bugs per test repo fixed before that repo's test-window start (before 2025, or before 2024 for an extended repo; census only). Development and test bugs therefore never overlap in time, and no introducing or fix commit appears in both sets.
- Development bugs are labelled for relation kind and expressibility only. That needs no indexing of old commits. They feed the menu counts and nothing else.
- Grammar, prompts and simulator tuning use mjolnir and bifrost-dev only.
- No 2025+ test bug informs the menu, the grammar or the prompts.
- **Menu rule, written before the development census:** the added steps are the top k = 3 missing step kinds, ranked by the repo-averaged share of relation bugs they decide.
- **Menu stability:** report whether the menu changes when one test repo's development bugs are left out, and when all of them are left out. This shows the menu is stable. It can't certify that no test information reached the frozen inputs: step semantics, query bounds or prompts could shift while the three step names stay the same. That protection is procedural. Nobody who shapes those inputs sees a test bug's census record, fix or decisive facts before the freeze.
- **Exclusion:** a test bug in the same family as a development bug from that repo is excluded. Same family means the same issue, or introduced by the development bug's fix.

## Stage 1: census (no reviewers)

Two labellers from different families (Sol/high primary, Opus/high replication) see the bug record, fix and introducing diff. For each bug they record:
- **Decisive facts,** as minimal sufficient sets: several sets when more than one fact decides the bug.
- **Relation kind and RQL expression:** a documented step, a missing step (with its semantics), or not expressible.
- **Anchoring:** whether the canonical query anchors on a symbol the diff touches, and the hops from it.
- **Row count:** how many rows the canonical query returns at the introducing commit. For documented steps it comes from running Bifrost. For a missing step it comes from the answer engine, once the menu is chosen, and so exists only for menu steps. Missing steps outside the menu are reported without row counts.
- **Bifrost today:** whether today's Bifrost returns the decisive row (run it), and whether it's complete.
- **Primary membership:** a bug is a relation diff when the primary labeller (Sol) records a decisive fact relating two or more code sites. That includes relation bugs the menu can't express; the expressible subset is secondary. This is fixed before any reviewer runs, and labeller agreement is reported.
- **Baseline catch:** the share of the bug's 4 no-brief Terra reviewers who catch it, from stage 2's no-brief arm, using miss-corrected labels. "Among baseline misses" weights each bug by its no-brief miss share. A bug every reviewer missed counts fully, one with 1 catch of 4 counts 0.75, and one every reviewer caught counts 0. This rule is frozen before any no-brief outcome is read.

It reports:
- features ranked by bugs decided among baseline misses;
- for each fixed recipe ("run step S on every touched symbol") over a documented or menu step, the share of bugs whose decisive row it emits, against total rows emitted. That evaluates a pushed-facts brief without reviewers;
- agreement between the two labellers per category;
- a bias statement: bugs fixed soon after they were introduced lean toward visible regressions.

## Stage 2: mocked RQL

**Language.**
- Documented RQL at a pinned Bifrost commit, plus the missing steps that the development census ranks highest (expected: value-flow endpoints, field writes with values, argument values at call sites).
- Each added step's semantics are written down before any answer is generated: which constructs it crosses and to what depth.
- Queries must parse against the frozen grammar, with no free text.

**Answer engine.** It sees the post-change tree of the introducing commit only: no diff, no `.git`, no network. Its model is Claude Opus/high on claude7, for both classification and value-flow hops. The gold set's second-family enumerator is Sol/high. Memorized fixes are a second route to prescience; the decisive-versus-other recall check covers that too.
- **Candidates:** Bifrost ∪ rust-analyzer ∪ identifier grep (for fields: `.name`, and `name:` in literals and patterns). Each row is tagged with its sources, which gives today's Bifrost recall against the union for free.
- **Classification:** a model classifies one candidate site at a time, from a few lines of context, with no diff and no other candidates. It copies value expressions verbatim, and a mechanical check confirms each is on that line. No paraphrase.
- **Value flow:** traced as repeated single hops (assignment, argument to parameter, return). Each hop draws candidates mechanically, so it can be checked hop by hop.
- **When answers are computed:** mechanical steps are evaluated on demand and cached. Value flow and the fidelity sample are precomputed for every touched symbol, before any writer runs. A value-flow query anchored on a symbol outside that table, for example a caller or binding the writer found with an earlier query, is answered on demand by the same single-hop procedure. Its answer is cached and flagged as on demand. Fidelity is sampled at two points, both before any reviewer outcome is unblinded. The first, before writers run, draws from the precomputed table. The second, after all writers in a phase have run, draws a seeded sample of at least 20 on-demand queries per step from the writers' actual query logs, with the same adjudication and bars. Both are reported separately, and a build recommendation needs both to pass.
- **Scope:** a touched symbol is one whose definition the diff changes, or one referenced in a changed line. Symbols defined outside the workspace are excluded by rule; nothing is cut by fan-out.
- **Freezing the definition:** before freezing it, report on development data the share of bugs whose decisive row the table contains.

**Fidelity gate** (per step the engine serves, documented and added alike, since R0 and R both serve simulated rows for documented steps; on at least 40 gold queries drawn from the potential table, settled before any reviewer outcome is unblinded). The sample is stratified. The decisive stratum holds every development query whose scope holds a census decisive row for that step. The random stratum holds at least 20 other queries drawn at random from the potential table, even when the decisive stratum alone reaches 40. Precision and recall are reported per stratum. The random stratum must always meet the bars, and so must the decisive stratum when it is non-empty. A documented step is never removed, whatever its fidelity: R0 and R serve it as is, and the readout reports it at its measured precision and recall. Only added steps can be removed. An added step needs at least 10 decisive queries in its sample, or the decisive-row recall gap is undefined and the step fails the gate. A documented step with fewer than 10 must still meet the precision, recall and stability bars; its gap is reported as not estimable. Documented steps also use real Bifrost as one more mechanical candidate source:
- **Gold set:** built by pooling the engine's rows, a second-family model asked to enumerate, and the mechanical superset, with every candidate adjudicated. For sampled queries whose scope includes a census decisive fact, those decisive rows are added to the gold set even when every enumerator missed them. They are never shown to the engine, so a step that misses the relations it exists to supply can't pass.
- **Accuracy:** row precision at least 95% and recall at least 90%, with lower 95% bounds (bootstrap over queries) of 90% and 80%.
- **Stability:** re-asking a sample without the cache returns the same rows at Jaccard 0.9 or better.
- **Prescience:** decisive-row recall within 10 points of other-row recall. The pilot can only bound this gap to about ±20, which the readout says.
- **On failure:** on development data (the pilot), a failing step is removed before the freeze. After the freeze the menu doesn't change: test-repo fidelity only qualifies the readout, which reports the step at its measured recall. Decisive-row recall on test bugs never feeds back into the menu.
- **Test repos:** measure fidelity on each test repo too, with about 10 gold queries per step per repo, before any reviewer outcome. A failing repo is reported at its measured recall, not dropped.
- **Realistic recall:** ask Bifrost's query-surface owner what recall each step could really reach, per mechanical candidate and per value-flow hop, the units D drops. If the owner can give only recall of returned rows, D's per-hop rate is calibrated so its mean returned-row recall on the pilot matches that figure. If returned-row recall is about 75% or less, add arm D below.

**Arms.** All writers are Sol/high; all arms run in the same window on the same image (`driver --same-image`), interleaved per diff. Every writer and the H helper get the reviewers' isolation, as in `PLAN.md`'s blind brief: a checkout at the introducing commit with no later history, a Podman target, no network, memory, GitHub, builds or helper agents beyond the arm's own, and no corpus records. So no writer can read the fix.
- **N:** no brief.
- **G:** grep writer. It gets the same step list in prose, so the contrast is the answers, not the checklist.
- **R:** mocked-RQL writer, which keeps grep and the list.
- **R0:** the same mocked engine with today's documented steps only (no added steps), keeping grep and the list. R − R0 is the added steps' own contribution, and R0 − G is what complete answers from today's steps add.
- **H:** compute-matched. A grep writer that may ask a helper free-form questions. The helper has the same model and inputs as the answer engine. Its token budget equals all the engine model tokens spent answering R's queries on that diff, counted once per diff. That includes precomputed hops and classifications behind those answers, and rejected candidates, rejected hops and other work that produced no rows. A costly empty answer is still information R received. Matching on tokens, not call counts, matters because one RQL call can be a cached lookup or many classification and hop calls. Calls and tokens are reported for both arms.
- **D (conditional):** R with recall degraded inside the engine, not on its final answers. Each mechanical candidate and each value-flow hop edge is dropped independently at the owner's per-step realistic recall, before composition. A path that needs two hops at 75% then survives about 56% of the time, as it would in a real engine. Dropped elements are drawn once per diff and seeded, so repeated queries agree.

**Leak audit.** Each brief's exact text, bound to its hash, gets the blind `judge/SPEC.md` check (one Opus/high) as if it were a review output. Briefs are not rewritten: a blind writer that states the defect from accurate rows is the product working. The readout reports, per arm, the share of briefs that state the defect. A secondary R − G excluding diffs where any arm's brief states it is descriptive only. That exclusion conditions on an outcome of the treatments, so it describes a selected subset and can't cleanly separate writer-found defects from reviewer reasoning.

**Containment.** One blind Opus judge decides per brief whether it contains the decisive fact (any minimal set). Every diff where R's and G's containment differ is hand-checked. Containment is read only if R actually queried the engine (Terra never called the tool in the Oz run). Prompt fixes are allowed in the pilot and frozen before test.
- **Pilot:** containment is reported but gates nothing, for the same reason as on test. The one pilot stop is mechanical: if R doesn't query the engine, fix the prompt in the pilot before going on.
- **Test:** containment is reported but gates nothing, and reviewers run for every arm on the fixed test sample. R can match G on containment yet still lift catch, for example by giving a concrete value or call path, so a containment bound can't rule out the +10 catch effect.
- **Arms N and G** need their reviewers for the census ranking among baseline misses, the contamination measure and G − N. Every arm's reviewers run in both phases.

**Reviewers and judging.**
- Terra/high reviewers. Each brief arm gets 2 briefs per diff with 2 reviewers per brief. N gets 4 reviewers per diff.
- One Opus/high judge per output, plus a hand check of every caught or partial verdict and of a seeded random 20% of misses, stratified by arm and repo: each arm-and-repo cell with misses gets ceil(20%) of them checked, and always at least one.
- **Missed catches in the estimate:** the checked misses give each arm's false-miss rate (the share of judge misses the hand check upgrades). Each unchecked miss counts as a catch with probability equal to its arm's rate, by inverse-probability weighting within arm and repo. Because rates are per arm and repo, the correction's uncertainty is already in the spread of repo means that the governing t interval uses (see Reading). Raw corrected labels without the weighting are a sensitivity check, and per-arm false-miss rates are reported.
- Harm controls: the non-relation census bugs and the existing ordinary-diff controls, for R against G (retention of confirmed findings), plus brief size per arm.

## Pilot, then test

**Pilot (development only):** mjolnir and bifrost-dev. Its outputs:
- simulator fidelity per step;
- touched-symbol coverage;
- cost and latency per query;
- variance components (repo, diff, brief, reviewer).

These set the test-phase sample. Pilot effects are development data and are never pooled into the reading.

**Test:** the test repos, with arms N, G, R0, R and H (and D if triggered).

## Reading

- **Primary:** R − G catch rate on relation diffs, using the miss-corrected labels below. Take the paired difference per diff, average within each repo, and put a t interval over repo means (repos − 1 degrees of freedom, equal weights). This one interval governs every decision band in the plan. The false-miss rates are estimated per arm and repo, so the audit's sampling noise is already part of the spread of the corrected repo means. The interval uses that spread alone and doesn't add the audit variance a second time. The same construction applies to R − R0, R0 − G and R − H.
- **Sensitivity checks:**
  - a cluster bootstrap (including the audit resampling);
  - a mixed logistic model with a repo intercept, a repo arm slope and a diff intercept;
  - the after-cutoff subset.
- **Also reported:** per-repo effects, the between-repo SD and a prediction interval for a new repo selected the same way. The repos were chosen for bug yield per indexing hour, with indexing hazards excluded, so neither the interval nor the prediction interval covers Rust projects the screen excludes.

Decision bands on R − G, which decide whether complete RQL is worth building for review (minimum effect set by Jonathan, 2026-11-18):
- **Don't build for review:** upper bound below +10. This takes precedence.
- **Build the features:** estimate at least +10 and lower bound above 0.
- **Partial:** lower bound above 0, estimate below +10, and upper bound at least +10.
- **Inconclusive:** anything else, including too few qualifying repos.

Fidelity on test governs credit: a Build or Partial reading of R − G, R − R0 or R0 − G recommends engine work only if every step R (or R0) serves passes the fidelity gate on the pooled test repos, decisive-versus-other recall gap included where it's estimable (at least 10 decisive queries pooled across test repos). A served step whose gap isn't estimable must still pass precision, recall and stability. It doesn't block a recommendation, but the readout flags it. Otherwise the contrast is reported as the effect of the answers actually served, at their measured fidelity, and recommends nothing.

Precedence for the engine decision: the helper comparison below can override R − G. If the R − H readout is "helper wins" or "equivalent", a Build or Partial R − G doesn't by itself justify engine work. The decision becomes a cost comparison: build only if real RQL can deliver the same rows at lower cost and latency than the helper's measured tokens and time per diff. Otherwise, R − G governs.

Which features to build, when R − G reads Build or Partial and the helper readout doesn't override it (same repo-level t interval):
- **Added steps:** if the lower bound of R − R0 is above 0, the added steps as a bundle are worth building. The study doesn't attribute that lift to individual steps. Build order comes from descriptive evidence: each step's share of the rows in briefs whose reviewers caught the bug, and the census rankings. As each step is built, it's measured on its own (milestone 5 adds and measures templates one at a time), and a step that adds nothing is dropped.
- **Today's steps:** if the lower bound of R0 − G is above 0 and every documented step served passed the fidelity gate, complete answers to today's steps carry lift, and their completeness and latency are worth engine work. This can hold alongside the added steps. If any served documented step failed the gate, R0 − G is reported as the effect of the answers actually served, at their measured fidelity, and supports no engine recommendation.
- **Unresolved:** a contrast whose interval includes 0 is reported as unresolved. It is never read as showing that part contributed nothing.

Also reported:
- **H against R** (paired R − H, same repo-level t interval):
  - **Upper bound below −5:** the helper writer beats RQL. The useful product today is that writer, and RQL's rows don't earn their cost for review.
  - **Interval within −5 to +5:** a writer with model helpers matches RQL to within 5 points. The useful product today is that writer, and Bifrost's case rests on cost, latency and completeness. If the lower bound is also above 0, the readout reports a real but small RQL edge, which counts as practically equivalent.
  - **Lower bound above 0 and upper bound at least +5:** RQL's structured rows add value over equal helper compute.
  - **Otherwise:** unresolved.
- **D − G** (when D runs; paired, same governing interval): the result at the recall the owner expects. If D runs, a build recommendation is stated at that recall. Build requires D − G to read Build as well. If R − G reads Build but D − G doesn't, the readout says the lift depends on recall above what the owner expects, and the recommendation becomes recall work first.
- **G − N:** the effect of the checklist-assisted grep writer over no brief. G gets the new step list in prose, which the shipped writer's prompt (`WRITER.md`) doesn't have, so this isn't a measure of the shipped writer itself.
- **The census rankings.**
- **Cost.**

**Fixed sample.** The repo set is fixed before any test reviewer outcome exists, with no sequential looks, so the t interval keeps its nominal coverage. A data-dependent rule that adds repos while the interval is wide would not. If the interval comes out wider than ±10, the bands decide, often Inconclusive. Adding repos afterwards is a new registered study.

**Contamination.** Most 2025+ bugs predate Sol's cutoff, so the after-cutoff subset will be small. The usable measure is the no-brief catch rate by fix date.

**Freeze.** Before the first test writer runs, commit a freeze manifest with hashes: menu, grammar, step semantics, prompts, the touched-symbol rule and the analysis script. Tag the pinned image.

## Cost and quota (estimates, refined by the pilot)

- **Census:** about $1 per traced candidate, including both labellers, so about $200–300 for 8 repos plus development.
- **Pilot:** about $350–500 in API-equivalent terms. That covers 35 relation diffs:
  - about 700 Terra reviewer sessions;
  - about 280 Sol writer sessions;
  - the simulator and the gold set.
- **Test:** about 7 times the pilot for 12 repos.
- **Indexing:** Bifrost and rust-analyzer compute for each bug's introducing commit, reusing incremental indexes within a repo. Runs on local clones copied from `/nfs/morannon` (never written).
- **Quota:** Sol and Terra on codex, Opus on claude7. Keep every profile above 25% of its week, checked before each launch, and don't use codex4.

## Work and ownership

dbcece95 owns the plan, rulings, integration and readout. Children implement each slice with mbx-only builds:
1. Repo index measurement and selection.
2. Bug tracing and admission.
3. Census labelling.
4. The grammar and step semantics.
5. The answer engine and gold set.
6. The arms in the existing driver.
7. Judging and analysis.

dbcece95 runs long scans itself in the background, because spawned children can't keep detached processes.
