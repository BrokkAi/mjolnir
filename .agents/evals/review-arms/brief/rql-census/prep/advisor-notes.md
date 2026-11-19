# Advisor (91233b0e) input for the 10/20-repo census + mocked-RQL pilot, 2026-11-18

Adopted in full by dbcece95. Plan to be written by dbcece95 (parent writes ExecPlans).

Mocked-RQL arm:
- Simulator sees only post-change tree (no diff, no .git, no network); fresh session per query; query text only.
- Frozen grammar, no free text; added-step semantics written before any run (constructs crossed, depth).
- Grep writer gets same step list in prose; RQL writer keeps grep.
- Added steps chosen on dev data (mjolnir, bifrost-dev, declared dev half of new repos); test on the rest.
- Recall on census-decisive rows vs other rows (post-unblinding) = prescience measure.
- Positive result = "information helps when delivered this way", not "engine needed"; simulator cost/latency = price real RQL must beat.
- Candidates = Bifrost ∪ rust-analyzer ∪ identifier grep (fields: `.name`, `name:` in literals/patterns); tag rows by source (gives today's Bifrost recall vs union).
- Classification local only: one candidate site + few lines, no diff, no other candidates; value expressions verbatim, mechanically checked on that line.
- Value flow: repeated single hops (assignment, arg->param, return), mechanical candidates per hop; largest gold budget.
- Mechanical steps evaluated lazily with cache (deterministic); precompute only value flow + gold sample (drawn from potential table, not asked queries).
- Exclude symbols defined outside workspace by rule; never cut by fan-out.
- Before freezing "touched symbol" (def changed or referenced in changed line), report on dev data share of bugs whose decisive row the table contains (2-hop risk).

Fidelity (per added step, >=40 gold queries, decided before unblinding):
- Gold by pooling: simulator rows + second-family simulator (enumerate) + mechanical superset; adjudicate each candidate.
- Precision >=95%, recall >=90%, lower 95% bounds (bootstrap over queries) 90%/80%.
- Re-ask without cache: Jaccard >=0.9.
- Decisive-row recall within 10 pts of other-row recall vs gold (pilot can only bound ~±20; say so).
- Failing step removed, or report "RQL at measured recall r". Ask engine owner realistic recall per step; degraded arm in pilot if ~70%.

Sample size/analysis:
- Per-diff paired SD ~0.42 (from ±20 on 17). Repos buy more than diffs: 10x10 ±10/±14 (between-repo SD 5/15), 10x20 ±8/±13, 20x5 ±9/±11, 20x10 ±7/±9.
- Relation bugs ~60% of census. Prefer 20 repos, or 10 then add repos under a pre-declared interval-width stopping rule.
- Primary: per-diff paired diff, mean within repo, t interval over repo means (df = repos-1, equal weights). Sensitivity: cluster bootstrap, mixed logistic (repo intercept + arm slope, diff intercept). Report per-repo effects, between-repo SD, prediction interval for a new repo.
- Pre-declare minimum effect justifying engine work + decision bands (user asked; dbcece95 proposed +10 pts on relation bugs, LB > 0).
- 2 briefs per diff per arm x 2 reviewers; pilot estimates repo/diff/brief/reviewer variance components.

Census (stage 1) columns: anchored on diff symbol? hops; canonical query row count; baseline no-brief catch. Rank features by bugs decided among baseline misses. Fixed recipe "step S on every changed symbol": share of bugs whose decisive row emitted vs total rows. Minimal sufficient fact sets; inter-model agreement per category. Bias: post-cutoff bugs found quickly -> skew to visible regressions.

Arms to add/cut:
- Compute-matched arm: grep writer + helper answering same number of free-form questions (same model/inputs as simulator).
- Writer-only gate: "brief contains decisive fact" per arm before reviewers; skip reviewers if mocked RQL doesn't beat grep.
- Harm controls: non-relation census bugs + ordinary diffs; record brief size per arm.
- Cut 3rd reviewer; replace 2nd Opus judge with hand check of random sample of misses per arm.
- Pilot effects on mjolnir/bifrost-dev = development data only.
