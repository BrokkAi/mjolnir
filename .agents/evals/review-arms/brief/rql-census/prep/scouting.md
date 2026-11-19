# Rust-repo scouting for the widened relation-brief census (stage 1)

Read-only scouting, 2026-11-18, for dbcece95. No experiment runs, no repo edits.
Data, scripts and per-PR traces are under `/mnt/optane/tmp/rust10/` (`data/`, `traces/`, `scripts/`).

## Bottom line

With the window starting on 2026-07-01, only 6–7 public Rust repos are
expected to give 5 or more clean multi-site bugs each, and about 11 give 3 or
more. Getting about 20 repos at 5–10 bugs each is not supported at this window.
The limit is the window, not the number of repos. A bug qualifies only if it
was introduced and fixed within the same 4.5 months. In the 198 sampled
bug-labelled fixes whose issue was opened in the window, only 19% blame clearly
to a change made in the window. Most fixes in the window repair older code
that was only recently reported.

Ranked list (estimated clean bugs: in the window, traceable and multi-site; method below):

| rank | repo | est. clean multi-site (T1w) | + unlabelled fixes (upper) | notes |
|---:|---|---:|---:|---|
| 1 | astral-sh/ruff (mostly ty) | 18 | 3.5 | Strongest. ty is changing fast; issues live in astral-sh/ty (cross-repo closing refs work). Hot areas repeat (2 of 10 sampled fixes share one intro), so the spread cap binds. salsa macros. |
| 2 | biomejs/biome | 14 | 2.6 | `S-Bug-confirmed` labels. 117 generated `.rs` files (codegen syntax crates) to exclude from indexing. Many fixes are single-rule local. |
| 3 | pola-rs/polars | 12 | 3.1 | Highest T1w (215), but only 2 of 10 had strong in-window blame. Many bugs are old code reported recently. Python-side tests; about 40% of the code is not Rust. |
| 4 | lance-format/lance | 9 | 3.4 | 8 of 10 had some in-window blame, and the intros are moderate (97–1,201 lines). Note: the repo moved from lancedb/lance. |
| 5 | oxc-project/oxc | 7 | 9.2 | Very high volume (3,764 merged PRs), but traces are weak (1 of 10). 58 generated files. Many issue labels are `bug: upstream`. |
| 6 | apache/datafusion | 6 | 2.7 | Clean intros at 129–1,160 lines; optimizer bugs are often caller/callee relations. |
| 7 | astral-sh/uv | 5 | 1.7 | 4 of 10 strong; intros 815–2,120 lines (several over the preferred 1,500). |
| 8 | swc-project/swc | 4 | 1.7 | Minifier and parser bugs are mostly old code; 173 generated/binding files. |
| 9 | GreptimeTeam/greptimedb | 4 | 3.3 | Strong concurrency and lifecycle bugs (flush cancellation, log-store backlog); intros 1.2–3.6k lines. |
| 10 | bytecodealliance/wasmtime | 4 | 1.5 | Good relation bugs (component concurrency, Cranelift lattice). Hazards: 114 ISLE `.isle` DSL files, where the logic is not in Rust, and one intro on 2026-07-01, right at the window edge. |
| 11 | rust-lang/rust-clippy | 4 | 0.2 | Bugs are lint-local (mostly single-site). Rustc subtree syncs show up in blame as 20k+-line "intros". One false attribution was seen. |
| 12 | zed-industries/zed | 1.3 | 1.2 | 72 T1w but 0 of 10 strong. Blame lands on huge PRs (one is 10.7k lines). 1.86M LOC. |
| 13 | rust-lang/cargo | 1.2 | 0.6 | Clean attribution when it works; low volume. |
| 14 | rust-lang/rust-analyzer | 1.2 | 0.7 | 0 of 10 had any in-window blame: its bugs are long-standing. |
| 15 | servo/servo | 1.1 | 1.8 | `mach` build, generated DOM bindings, large WPT tree. |
| 16 | typst/typst | 0.8 | 0.1 | Small and clean; low volume. |
| 17 | tikv/tikv | 0.6 | 0.1 | Good regression labels; low volume in the window. |
| 18 | nushell/nushell | 0.4 | 1.9 | Few bug labels; T2 has 147 unlabelled fixes. Intros are often huge (5–7k-line completer rewrites). |
| 19 | gfx-rs/wgpu | 0.5 | 0.5 | |
| 20 | denoland/deno | 0.2 | 2.0 | Regressions predate the window (for example, 2.8.3 shipped 2026-06-11); about 40% of the code is JS/TS. |

Each of these repos expects 0.5 or fewer clean bugs at T1, and none is worth
including: gitoxide, miri, tokio, jj, bevy, hyper, meilisearch, reth, vector,
helix, quinn, rustls (no bug labels), tauri and wasm-tools (no linked bug
issues).

Expected total if every repo is traced to exhaustion: about 86 clean
multi-site bugs from the top 11 (about 119 with unlabelled fixes), and about
93 from all 20 (about 105 with unlabelled fixes). The estimates for individual repos are rough (see
uncertainty).

## Window

- **Start 2026-07-01: provisional, set by the instruction.** No GPT-6.1 (Terra
  or Sol) training cutoff is recorded anywhere in `.agents/evals/review-arms/`
  (README, `brief/*.md`, corpus SPEC). The only recorded boundary is a
  review-exposure cutoff ("Neither model reviewed the repository's commits
  before about 2026-11-05"; corpus instants are strictly before
  2026-11-01T00:00:00Z). That boundary concerns our own repos, not training
  data. Claude Opus 5.5's cutoff is June 2026.
- The end is 2026-11-18 (today). The census covered PRs merged from
  2026-07-01 through 2026-11-18.
- Sensitivity: the window decides the yield. Moving the start to 2026-06-01
  would add about 22% more time, and the yield would rise faster than that,
  because both the intro and the fix must fall inside. Waiting adds about
  1/4.5 of the current yield per month.

## Method

1. **PR census, complete, not sampled** (`scripts/census_prs.py`). GraphQL
   searched `repo:X is:pr is:merged merged:A..B` in 14-day slices; no slice
   reached the 1,000-result cap. For every merged PR it recorded the closing
   issue references (labels, issue type, creation date), the changed file
   paths and line counts, the PR labels and the title. 33 repos and 36,881
   merged PRs in total; GraphQL cost was 3 points per page.
2. **Tiers** (`scripts/tiers.py`, the single classifier):
   - **T1**: the PR closes an issue whose label or issue type matches
     bug, regression, crash, panic, defect, miscompilation, unsound, incorrect
     or wrong as a whole word (so `area:debugger` does not count), and it
     changes a test path (`tests/`, `fixtures/`, snapshots, `*_test(s).rs`,
     mdtest, etc.) and at least one non-test `.rs` file.
   - **T1w**: T1, and every closing issue was created on or after
     2026-07-01. An issue opened before the window means the bug existed
     before it, so T1w is a necessary condition, not a sufficient one.
   - **T2w**: a fix-like PR (title says fix, the PR carries a bug label, or
     it closes an issue that is not bug-labelled) with a test and a non-test
     `.rs` change, with no pre-window issue.
   - Undercount: tests inline in the same `.rs` file (`#[cfg(test)]`) are not
     detected by path.
3. **Intro tracing, sampled** (`scripts/trace_local.py`). The sample is a
   seeded shuffle (seed 20261118) of each repo's T1w PRs. 5 PRs were traced
   per repo, and 10 for the 13 high-volume repos (a second seeded batch of 5).
   That makes 248 traced PRs: 198 T1, plus T2 fill-ins where T1w had fewer
   than 5. Each PR was traced as follows:
   - fix = the merge commit; base = its first parent;
   - in non-test `.rs` files, the old-side lines the fix deletes or changes
     (for add-only hunks, the two lines around the insertion) were blamed at
     base, in a shallow (since 2026-06-15) blobless clone under
     `clones/` (197 MB in total);
   - lines before the shallow boundary count as pre-window.
   - **Strong** means the top in-window blamed commit holds at least 30% of
     the blamed lines and changes 4,000 lines or fewer (SPEC rule 7).
   - Intro size is computed from `git show --numstat`, excluding goldens,
     fixtures, snapshots and lock files, the same rule as
     `select_diffs.py`.
   - The issue text was scanned for "regression", "bisect", "introduced
     by/in" and "caused by".
   - GraphQL blame through the API was tried first. It returned 502 errors
     and stalled on large files, so tracing used local clones.
4. **Calibration of blame against issue-text attributions.** Five sampled
   issues name their cause. In all five, blame agrees about whether the cause
   is inside the window:
   - tikv #19993 was caused by #18023 (2024): pre-window, and blame found 0
     lines in the window.
   - cargo #17665 was caused by #16745 (the 1.95 cycle): pre-window, 0
     in-window lines.
   - deno #36496 regressed in 2.8.3 (2026-06-11): pre-window, 0 in-window
     lines.
   - tikv #20379 is a recent regression; blame found in-window #20329.
   - hyper #4196 was bisected to an in-window change; blame found 36% of
     lines in the window.

   Blame misses bugs whose cause sits at a different site from the fix
   (the very multi-site cases we want). "Any in-window blame" (column below)
   is the loose upper bound. **Strong** is the conservative estimate used for
   the yield.
5. **Multi-site share.** I hand-read 8 fix diffs among the strong
   attributions:
   - multi-site (5):
     - ruff #29786 and #28289: salsa query cycles, a relation in the
       call/query graph;
     - cargo #17509: a new `FeatureDefinition::Metadata` variant that the
       publish normalizer flattens (producer and consumer);
     - wasmtime #15325: a readiness predicate that misses a queued-work state
       (writer and reader);
     - datafusion #25511: `is_volatile` covers only scalar functions, while
       callers rely on it for aggregates (callee and caller contract);
   - local (3): uv #22496, greptimedb #10272 and meilisearch #6731.

   That is 5/8, consistent with the past censuses: mjolnir had 18/30
   relation diffs, and bifrost 17/30 by the Sol census. The yield uses
   M = 0.58. The path-based proxy "fix touches ≥2 functions or files" is
   reported per repo, but it overstates the share, since most non-trivial
   fixes touch several hunks.
6. **Yield formula** (`scripts/score.py`):
   `T1w × strong_rate × Q × M`. Each repo's strong rate is shrunk toward the
   pooled T1 rate (0.187 over 198 traced) with 5 pseudo-observations.
   - Q = 0.5 is the share of bifrost groups that, having passed cutoff, size
     and attribution, also passed SPEC 3/4/5 (findable, real behavior, not
     preflagged): 31/67 (`corpus-build/yield-report.md`).
   - The T2 upper column is `T2w × (2/22 strong) × 0.5 (bug precision) × Q × M`.

## Full table

| repo | est. Rust kLOC | merged PRs (Jul 1–Nov 18) | T1w | T2w | traced (T1) | strong in-window intro | any in-window blame | fix touches ≥2 fns/files | shrunk strong rate | est. clean multi-site (T1w) | + T2w upper |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ruff | 828 | 2263 | 189 | 269 | 10 (10) | 4 | 9 | 7 | 0.33 | 18.0 | 3.5 |
| biome | 941 | 1409 | 183 | 198 | 10 (10) | 3 | 5 | 5 | 0.26 | 13.9 | 2.6 |
| polars | 612 | 1467 | 215 | 232 | 10 (10) | 2 | 6 | 9 | 0.20 | 12.2 | 3.1 |
| lance | 1368 | 2072 | 124 | 258 | 10 (10) | 3 | 8 | 10 | 0.26 | 9.4 | 3.4 |
| oxc | 1101 | 3764 | 186 | 701 | 10 (10) | 1 | 5 | 8 | 0.13 | 7.0 | 9.2 |
| datafusion | 1111 | 1945 | 74 | 206 | 10 (10) | 3 | 8 | 8 | 0.26 | 5.6 | 2.7 |
| uv | 599 | 1967 | 48 | 132 | 10 (10) | 4 | 7 | 7 | 0.33 | 4.6 | 1.7 |
| swc | 779 | 407 | 107 | 126 | 10 (10) | 1 | 3 | 6 | 0.13 | 4.0 | 1.7 |
| greptimedb | 1393 | 1376 | 48 | 252 | 10 (10) | 3 | 7 | 9 | 0.26 | 3.7 | 3.3 |
| rust-clippy | 307 | 315 | 48 | 15 | 10 (10) | 3 | 5 | 7 | 0.26 | 3.7 | 0.2 |
| wasmtime | 719 | 1045 | 49 | 114 | 10 (10) | 3 | 5 | 6 | 0.26 | 3.7 | 1.5 |
| zed | 1856 | 2403 | 72 | 93 | 10 (10) | 0 | 5 | 9 | 0.06 | 1.3 | 1.2 |
| cargo | 300 | 345 | 22 | 44 | 5 (5) | 1 | 2 | 1 | 0.19 | 1.2 | 0.6 |
| rust-analyzer | 493 | 478 | 68 | 51 | 10 (10) | 0 | 0 | 5 | 0.06 | 1.2 | 0.7 |
| servo | 621 | 2424 | 42 | 137 | 5 (5) | 0 | 3 | 1 | 0.09 | 1.1 | 1.8 |
| typst | 151 | 165 | 15 | 6 | 5 (5) | 1 | 2 | 4 | 0.19 | 0.8 | 0.1 |
| tikv | 751 | 189 | 10 | 11 | 5 (5) | 1 | 2 | 5 | 0.19 | 0.6 | 0.1 |
| wgpu | 377 | 523 | 9 | 40 | 5 (5) | 1 | 4 | 4 | 0.19 | 0.5 | 0.5 |
| nushell | 491 | 652 | 7 | 147 | 5 (5) | 1 | 2 | 5 | 0.19 | 0.4 | 1.9 |
| gitoxide | 522 | 310 | 10 | 111 | 5 (5) | 0 | 5 | 5 | 0.09 | 0.3 | 1.5 |
| miri | 139 | 353 | 6 | 28 | 5 (5) | 1 | 2 | 4 | 0.19 | 0.3 | 0.4 |
| tokio | 167 | 268 | 10 | 27 | 5 (5) | 0 | 1 | 4 | 0.09 | 0.3 | 0.4 |
| deno | 566 | 529 | 9 | 153 | 5 (5) | 0 | 3 | 5 | 0.09 | 0.2 | 2.0 |
| jj | 284 | 519 | 9 | 31 | 5 (5) | 0 | 1 | 4 | 0.09 | 0.2 | 0.4 |
| bevy | 617 | 930 | 2 | 22 | 5 (2) | 1 | 5 | 5 | 0.19 | 0.1 | 0.3 |
| hyper | 31 | 69 | 3 | 8 | 4 (3) | 0 | 2 | 2 | 0.10 | 0.1 | 0.1 |
| meilisearch | 277 | 132 | 1 | 18 | 5 (1) | 1 | 1 | 4 | 0.19 | 0.1 | 0.2 |
| reth | 534 | 1322 | 2 | 93 | 5 (2) | 0 | 4 | 5 | 0.09 | 0.1 | 1.2 |
| vector | 478 | 962 | 3 | 44 | 5 (3) | 0 | 4 | 5 | 0.09 | 0.1 | 0.6 |
| helix | 102 | 72 | 1 | 0 | 1 (1) | 0 | 0 | 1 | 0.16 | 0.0 | 0.0 |
| quinn | 48 | 192 | 1 | 10 | 5 (1) | 0 | 1 | 1 | 0.09 | 0.0 | 0.1 |
| rustls | 79 | 238 | 0 | 12 | 5 (0) | 1 | 3 | 4 | 0.19 | 0.0 | 0.2 |
| tauri | 95 | 421 | 0 | 8 | 0 | – | – | – | – | 0.0 | 0.1 |
| wasm-tools | 201 | 232 | 0 | 49 | 0 | – | – | – | – | 0.0 | 0.6 |

How the size and hazard columns were measured:
- **kLOC** is GitHub Rust bytes ÷ 36 bytes per line, tests included, so it
  is approximate.
- **Generated `.rs` files** are path matches for generated, `/gen/`, `_gen.rs`
  or bindings: biome 117, swc 173, servo 74, oxc 58, wasmtime 14, datafusion
  11, others under 10.
- **Non-Rust logic**: wasmtime has 114 `.isle` files; servo builds through
  `mach`. Every other repo is a plain Cargo workspace.
- **Heavy macros**: salsa in ruff/ty, rust-analyzer and biome; gpui in zed;
  bevy ECS; rustc-internal APIs in clippy and miri.

## Sampled intros (strong attributions)

All files under `traces/<repo>[.off5].jsonl`. Representative strong cases (fix ← intro, intro size in changed lines excluding fixtures and locks):

- ruff (ty) #29786 and #29759 ← #29706 (315 lines; one intro behind two fixes); #28289 ← #26746 (31); #27873 ← #27619 (1,336)
- biome #12599 ← #11330 (1,816); #12819 ← 44626baf6 (60); #13273 ← #12262 (3,113)
- polars #28768 ← #28506 (1,689); #28621 ← #28444 (783)
- lance #10591 ← #9747 (97); #10191 ← #8189 (408); #9628 ← #6236 (1,201)
- datafusion #26830 ← #25134 (129); #25511 ← #25385 (1,160); #27060 ← #26118 (970)
- uv #22055 ← #20685 (1,640); #22516 ← #19979 (1,174); #22990 ← #22252 (815); #22496 ← #22188 (2,120)
- greptimedb #9898 ← #8685 (1,177); #9558 ← 03823a9a0 (1,211); #10272 ← #9070 (3,618)
- wasmtime #15325 ← #13683 (212; the intro is dated 2026-07-01); #14632 ← #14230 (1,220); #14368 ← #14345 (1,980)
- rust-clippy #17866 ← 1b5517299 (355); #17881 ← 744b8fd5b (591); #17360 is a probable false positive: the intro has the same title, so it is a merge artefact.
- oxc #28101 ← #25417 (86)

Many intros are themselves fix PRs ("fix-on-fix" chains). That is fine under
the SPEC, but they cluster in the same areas, so the spread rule (at most 2
per feature) will bind in ruff/ty, polars and lance.

## Per-bug cost of the census (from past records, read-only)

Sources: `/mnt/optane/review-arms-bifrost-20261116/accounting/{source-preparation-summary,final-cost-summary}.json`,
`census-sol-driver/accounting.json`, `corpus-build/yield-report.md`,
`arms-preflight/usage-estimate.md`. I found no comparable cost records for the
mjolnir corpus build in `/mnt/optane/review-arms-20261113` (it has
`corpus/` and `label-facts/` but no usage summaries).

Bifrost corpus tracing and attribution (12 Sol/medium helper sessions) cost
**$41.88** in API-equivalent dollars ($48.96 including later orchestration).
That covered about 263 trace slots: 127 final-prefix groups, 41 beyond the
prefix, 72 issue fallbacks and 22 comment markers. The result was 30 selected
bugs.
- **About $0.16 per trace attempt, and about $1.40 per accepted bug**, with
  about 4.2 groups traced per qualified bug (24% yield).
- On the public repos I expect a lower qualification rate per raw candidate.
  In my sample, about 19% of T1w PRs had a strong in-window blame, and then
  Q = 0.5 applies. That gives about 10 candidates traced per clean bug, or
  about 18 per clean *multi-site* bug.
- Pre-screening with the blame script used here (no model, seconds per PR)
  lets an agent trace only blame-strong PRs, at about 2 per qualified bug.
  Then **tracing costs about $0.35–0.50 per qualified bug, and about
  $0.60–0.90 per qualified multi-site bug**. Without the pre-screen, the cost
  is about $1.60–2.80 per qualified bug.

Census labelling (`label/`, the decisive fact and its kind):
- Sol/high, 6 sessions × 5 diffs = 30 diffs: $1.845 and 5.29M tokens,
  so **about $0.06 per diff per labeller** and about $0.12 with an
  independent second labeller.
- The Carbon replication used 4 points of the gemini2 week for 5 diffs.
  That quota, not dollars, is the limit for a Carbon labeller.

**Per-bug census cost: about $0.5–1.0 API-equivalent per clean multi-site bug
with a blame pre-screen and two labellers ($1.7–3.0 without the
pre-screen).** For a corpus of 100 bugs, that is about $50–100, before parent
orchestration and audits. Those added about 17% on bifrost ($48.96 vs
$41.88).

## Main risks

1. **Unknown GPT-6.1 cutoff.** The 2026-07-01 start is provisional. If Terra
   or Sol trained on data after July, the in-window bugs may be contaminated;
   if before, the window could start earlier and the yield rises sharply.
   This needs a recorded source before freezing.
2. **The window is short (4.5 months).** Yield is concentrated: 6 repos
   provide about 70% of it. The estimates for individual repos rest on 5–10
   traces each, with intervals of roughly ±50–100%. For example, polars
   (215 T1w) got only 2 of 10 strong traces; oxc and zed are very uncertain.
3. **Blame misses multi-site causes.** When the cause sits away from the fix
   site, blame can't see it, so the strong rate likely undercounts exactly
   the relation bugs we want. "Any in-window blame" is 2–3× higher. Real
   tracing per SPEC (about 15 minutes per group) is still needed.
4. **Pre-screening on blame is itself a selection bias.** It favors bugs
   whose fix site equals the intro site, which tend to be local. Use it to
   order the work, not to exclude: trace weak-blame T1w PRs too, at a lower
   rate, and report both.
5. **Label discipline varies.** rustls, wasm-tools and tauri have essentially
   no linked bug issues; for those, T2 fixes need human bug-or-feature
   judgment. Cross-repo issues (ruff→astral-sh/ty) need repo-qualified
   lookups (my issue-text scan fetched the wrong repo for ruff; the census
   dates are right).
6. **Big intros.** uv, greptimedb, biome and nushell often trace to
   1.5–7k-line PRs, and zed to 10k+. SPEC 7 (preferably 1,500 or fewer, at
   most 4,000) will drop many.
7. **Subtree syncs.** In clippy and miri, blame attributes lines to the large
   rustc sync commits that merge the repo with rust-lang/rust, so those
   commits look like intros. Use blame's ignore-revs list or reject them.
8. **Spread.** The fix-on-fix clusters (one ty intro behind two sampled
   fixes) mean the spread cap of at most 2 per feature will cut the hot
   repos.
9. **Agent-written PRs.** Several repos (swc, tikv, greptimedb, tokio) now
   have PR bodies written by agents, some with AI-use disclosures. This
   doesn't affect contamination, but the bug population may differ from
   human-introduced bugs.
10. **Indexing hazards** for the query-language question: ISLE in wasmtime;
    generated syntax and AST code in biome, oxc and swc; servo's generated DOM
    bindings and `mach` build; salsa and gpui macros. In ruff/ty, biome and
    rust-analyzer, salsa query cycles are a relation that a call-graph query
    must model through macro-generated code.

## Reproduce

```
cd /mnt/optane/tmp/rust10
python3 scripts/census_prs.py owner/repo data/<repo>.jsonl          # census (GraphQL)
python3 scripts/summarize.py data > data/summary.md                 # tiers
python3 scripts/trace_local.py owner/repo --n 5 [--offset 5]        # blame tracing
python3 scripts/score.py > data/score.md                            # yield
```
