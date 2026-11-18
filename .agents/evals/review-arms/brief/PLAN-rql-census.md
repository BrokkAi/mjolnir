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
- Test: 12 Rust repos (fewer if fewer qualify), fixed before any test reviewer runs, from the local corpus (`~/Projects/brokkbench/sft-tools-commits/rust`, clones in `/nfs/morannon/nvme/repo-clones`, read only). Jonathan chose fewer repos with more bugs each, because Bifrost indexing is expensive per repo.
- Selection rule: rank by realistic 2025+ yield from the screen (`rql-census/prep/scan.py`, then `yield_rank.py`, which turns the capped samples into per-repo yields: upper bound × bug precision 0.85 × admission rate 31/67; results in `yield.json` and `corpus-yield-summary.md`). Incompletely traced fixes never count as strong. Drop repos with indexing hazards: huge generated code, a non-Cargo build, heavy proc-macro output.
- Rank and measure at least 16 repos: one cold Bifrost index and one incremental re-index each, at a representative revision. Take the 12 that qualify with the most expected admitted bugs per indexing hour. Reaching ±10 needs about 12 repos if the between-repo SD is 10 points, and about 16 if it is 15. More bugs per repo can't substitute for more repos. If fewer than 12 qualify, use all that do; a wider interval then reads as the bands below say.
- A repo counts only if it has at least 8 admitted relation diffs, decided before any reviewer runs on it.

**Bugs.**
- Candidates come from each selected repo's full git history: fix commits that change non-test code and add or change a test. The brokkbench SFT lists only seeded the screen, and they are a model-screened subset capped at 1,000 commits per repo.
- The corpus SPEC rules (`corpus/SPEC.md`) apply, adapted: the regression test in the fix commit replaces the Hard-won marker, and the cutoff rule becomes the window below. Attribution, findable-from-the-diff, real behavior, not pre-flagged, ordinary commits, size and spread all apply unchanged.
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
- **Leak check:** report whether the menu changes when one test repo's development bugs are left out, and when all of them are left out. An unchanged menu means nothing leaked.
- **Exclusion:** a test bug in the same family as a development bug from that repo is excluded. Same family means the same issue, or introduced by the development bug's fix.

## Stage 1: census (no reviewers)

Two labellers from different families (Sol/high primary, Opus/high replication) see the bug record, fix and introducing diff. For each bug they record:
- **Decisive facts,** as minimal sufficient sets: several sets when more than one fact decides the bug.
- **Relation kind and RQL expression:** a documented step, a missing step (with its semantics), or not expressible.
- **Anchoring:** whether the canonical query anchors on a symbol the diff touches, and the hops from it.
- **Row count:** how many rows the canonical query returns at the introducing commit, counted mechanically.
- **Bifrost today:** whether today's Bifrost returns the decisive row (run it), and whether it's complete.
- **Primary membership:** a bug is a relation diff when the primary labeller (Sol) records a decisive fact relating two or more code sites. That includes relation bugs the menu can't express; the expressible subset is secondary. This is fixed before any reviewer runs, and labeller agreement is reported.
- **Baseline catch:** whether no-brief Terra reviewers catch it, from stage 2's no-brief arm.

It reports:
- features ranked by bugs decided among baseline misses;
- for each fixed recipe ("run step S on every touched symbol"), the share of bugs whose decisive row it emits, against total rows emitted. That evaluates a pushed-facts brief without reviewers;
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
- **When answers are computed:** mechanical steps are evaluated on demand and cached. Value flow and the fidelity sample are precomputed for every touched symbol, before any writer runs.
- **Scope:** a touched symbol is one whose definition the diff changes, or one referenced in a changed line. Symbols defined outside the workspace are excluded by rule; nothing is cut by fan-out.
- **Freezing the definition:** before freezing it, report on development data the share of bugs whose decisive row the table contains.

**Fidelity gate** (per added step, on at least 40 gold queries drawn from the potential table, settled before any reviewer outcome is unblinded):
- **Gold set:** built by pooling the engine's rows, a second-family model asked to enumerate, and the mechanical superset, with every candidate adjudicated.
- **Accuracy:** row precision at least 95% and recall at least 90%, with lower 95% bounds (bootstrap over queries) of 90% and 80%.
- **Stability:** re-asking a sample without the cache returns the same rows at Jaccard 0.9 or better.
- **Prescience:** decisive-row recall within 10 points of other-row recall. The pilot can only bound this gap to about ±20, which the readout says.
- **On failure:** a failing step is removed, or the result is reported as RQL at its measured recall.
- **Test repos:** measure fidelity on each test repo too, with about 10 gold queries per step per repo, before any reviewer outcome. A failing repo is reported at its measured recall, not dropped.
- **Realistic recall:** ask Bifrost's query-surface owner what recall each step could really reach. If that is about 75% or less, add arm D below.

**Arms.** All writers are Sol/high; all arms run in the same window on the same image (`driver --same-image`), interleaved per diff.
- **N:** no brief.
- **G:** grep writer. It gets the same step list in prose, so the contrast is the answers, not the checklist.
- **R:** mocked-RQL writer, which keeps grep and the list.
- **H:** compute-matched. A grep writer that may ask a helper free-form questions. The helper has the same model and inputs as the answer engine, and a token budget equal to the engine tokens spent answering R's queries on that diff. Matching on tokens, not call counts, matters because one RQL call can be a cached lookup or many classification and hop calls. Calls and tokens are reported for both arms.
- **D (conditional):** R with answers degraded at random to the owner's realistic recall.

**Leak audit.** Each brief's exact text, bound to its hash, gets the blind `judge/SPEC.md` check (one Opus/high) as if it were a review output. Briefs are not rewritten: a blind writer that states the defect from accurate rows is the product working. The readout reports, per arm, the share of briefs that state the defect, and a secondary R − G that excludes diffs where any arm's brief states it. That separates writer-found defects from facts the reviewer used.

**Writer gates.** One blind Opus judge decides per brief whether it contains the decisive fact (any minimal set). Every diff where R's and G's containment differ is hand-checked. A gate is read only if R actually queried the engine (Terra never called the tool in the Oz run). Prompt fixes are allowed in the pilot and frozen before test.
- **Pilot (futility bar):** if R's containment doesn't exceed G's by at least 10 points, stop before pilot reviewers and report. The menu was chosen on these bugs, so a pass here is weak evidence and a fail is strong.
- **Test (binding):** run every test writer first. If the upper bound of R − G containment (the same repo-level t interval) is below +10, skip the test reviewers and report "stopped for futility". On mjolnir, a brief with the decisive fact was worth about +40 to +50 catch points, so a +10 catch effect needs about +20 containment.
- **Arms N and G always run their reviewers,** whatever the gates say: the census ranking among baseline misses, the contamination measure and G − N all need them. The gates skip only R, H and D reviewers.

**Reviewers and judging.**
- Terra/high reviewers. Each brief arm gets 2 briefs per diff with 2 reviewers per brief. N gets 4 reviewers per diff.
- One Opus/high judge per output, plus a hand check of every caught or partial verdict and of a random 20% of misses per arm.
- Harm controls: the non-relation census bugs and the existing ordinary-diff controls, for R against G (retention of confirmed findings), plus brief size per arm.

## Pilot, then test

**Pilot (development only):** mjolnir and bifrost-dev. Its outputs:
- simulator fidelity per step;
- touched-symbol coverage;
- cost and latency per query;
- variance components (repo, diff, brief, reviewer).

These set the test-phase sample. Pilot effects are development data and are never pooled into the reading.

**Test:** the test repos, with arms N, G, R and H (and D if triggered).

## Reading

- **Primary:** R − G catch rate on relation diffs. Take the paired difference per diff, average within each repo, and put a t interval over repo means (repos − 1 degrees of freedom, equal weights).
- **Sensitivity checks:**
  - a cluster bootstrap;
  - a mixed logistic model with a repo intercept, a repo arm slope and a diff intercept;
  - the after-cutoff subset.
- **Also reported:** per-repo effects, the between-repo SD and a prediction interval for a new repo.

Decision bands (minimum effect set by Jonathan, 2026-11-18):
- **Don't build for review:** upper bound below +10. This takes precedence.
- **Build the features:** estimate at least +10 and lower bound above 0.
- **Partial:** lower bound above 0, estimate below +10, and upper bound at least +10.
- **Inconclusive:** anything else, including too few qualifying repos.

Also reported:
- **H against R:** if H comes within 5 points of R, the useful product today is a writer with model helpers, and Bifrost's case rests on cost, latency and completeness.
- **G − N:** whether the shipped writer helps on these repos.
- **The census rankings.**
- **Cost.**

**Fixed sample.** The repo set is fixed before any test reviewer outcome exists, with no sequential looks, so the t interval keeps its nominal coverage. A data-dependent rule that adds repos while the interval is wide would not. If the interval comes out wider than ±10, the bands decide, often Inconclusive. Adding repos afterwards is a new registered study.

**Contamination.** Most 2025+ bugs predate Sol's cutoff, so the after-cutoff subset will be small. The usable measure is the no-brief catch rate by fix date.

**Freeze.** Before the first test writer runs, commit a freeze manifest with hashes: menu, grammar, step semantics, prompts, the touched-symbol rule and the analysis script. Tag the pinned image.

## Cost and quota (estimates, refined by the pilot)

- **Census:** about $1 per traced candidate, including both labellers, so about $200–300 for 8 repos plus development.
- **Pilot:** about $300–400 in API-equivalent terms. That covers 35 relation diffs:
  - about 560 Terra reviewer sessions;
  - about 210 Sol writer sessions;
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
