# P2 RQL Lean Pilot — Interim Fidelity Gate Report (Paused at 42/72, 2026-11-23)

## Outcome and Pause State

Following the user's **"Stop now"** decision (relayed via the Native-engine coordinator on 2026-11-23), the Stage 2 lean-pilot fidelity gate stopped cooperatively at **`2026-11-23T03:48:43Z`** (`exit 3`, `{"status": "stop_requested"}`) immediately after query 42 completed.

- **Active state:** `rql-gate-full-2ef35d9b4-1795396704.service` is `inactive`, private instance `rql-census-pilot` has **0 live sessions**, `/mnt/optane/review-arms-pilot/fidelity/stop-requested` remains in place, and `/mnt/optane/review-arms-pilot/engine.json` still lists `claude5` only (`claude4` untouched).
- **Frozen inputs preserved:** `manifest_sha256` `88095b963e2819c6157f03b23320568d661ad8eb992b8f1b300f958e716b65b6`, `engine_inputs_sha256` `896aec04ff2c55ca86d343293ba5fcf607c5ba946d96e7704ca4b9041a9dbd9d`, frozen binary `/mnt/optane/review-arms-pilot/bin/rql-verify-7672df81c`, and `audit.py` were not modified. No `gate-report.json` was written.
- **Checkpoints preserved:** **42 of 72** sampled queries have completed `measurement.json` records on disk (`contract-cases` 19/19, `value-transfers` 16/16, `guarded-paths` 7/10 random and 0/7 decisive, `documented` 0/20).
- **Remaining quotas at pause (03:48Z):** `claude5` 5H 42% / Week 12% left (0% floor); `codex3` Week 87% left (25% floor).
- **Headline finding:** **All three added menu steps have already failed the preregistered fidelity bars** (`precision >= 0.95`, `precision_lower95 >= 0.90`, `recall >= 0.90`, `recall_lower95 >= 0.80`, `stability_jaccard >= 0.90`). Even the unfinished `guarded-paths` step cannot pass: all three of its random stability re-asks have already finished at mean Jaccard `0.3576 < 0.90`, and partial random precision is `0.8332`. Three independent issues explain the result:
  1. **Verifier/semantics mismatch (`rql-verify` vs `menu-steps.md`):** `rql-verify` forced `.unwrap()`, `.expect()`, and `assert_eq!`/`assert_ne!` as required `panic`/`raise` candidates, driving **84.4%–86.5%** of `contract-cases` invalid rows (`550–564/652`) and **77.3%** of `q42`'s `guarded-paths` invalid rows (`75/97`). Fixed in source (`rql_mock/verifier/src/syntax.rs`); fixing it alone does **not** make any added step pass.
  2. **Sonnet/high run-to-run instability:** mean stability Jaccard across the 14 completed fresh re-asks is `0.23–0.68`, far below `0.90`, due to whole-query granularity, depth-exploration, and guard-chain variance.
  3. **Terra gold-enumerator bailout:** Terra returned `complete=False` in **41 of 42** runs, with `~32–33/42` (`~78%`) explicit effort/size bailouts returning 0–5 rows and non-semantic `"not enumerated"` open frontiers. Only **19 of 2,246 gold rows (0.85%)** came from outside the engine, so **the gate's `0.95–1.00` recall is not meaningfully measuring true recall**.

---

## 1. Interim Per-Step Numbers (42/72 Measured)

Metrics below are computed directly by `rql_mock/fidelity.py`'s `stratum_metrics()` and `pooled_prescience()` (10,000 seeded bootstrap replicates over queries, seed `20261120`) on the 42 completed `measurement.json` files. `engine_failure_count = 0` (no query double-stalled).

| Step / Stratum | Measured | Precision (lo95) | Recall (lo95) | TP / Returned | Gold | Stability Jaccard (n) | Gate Status |
|---|---:|---:|---:|---:|---:|---:|---|
| `contract-cases` / random | 10 / 10 | **0.3786** (0.2172) | 0.9889 (0.9522) | 357 / 943 | 361 | **0.6240** (3) | **FAIL** (prec, lo95, stab) |
| `contract-cases` / decisive | 9 / 9 | **0.6000** (0.5426) | 0.9519 (**0.7609**) | 99 / 165 | 104 | **0.6809** (2) | **FAIL** (prec, lo95, rec_lo95, stab) |
| `value-transfers` / random | 10 / 10 | **0.9021** (0.8435) | 1.0000 (1.0000) | 350 / 388 | 350 | **0.2294** (3) | **FAIL** (prec, lo95, stab) |
| `value-transfers` / decisive | 6 / 6 | **0.7861** (0.6888) | 0.9680 (0.8750) | 272 / 346 | 281 | **0.3421** (3) | **FAIL** (prec, lo95, stab) |
| `guarded-paths` / random | 7 / 10 | **0.8332** (0.6170) | 0.9991 (0.9974) | 1,149 / 1,379 | 1,150 | **0.3576** (3/3) | **CANNOT PASS** (all 3 random re-asks done; stab &lt; 0.90) |
| `guarded-paths` / decisive | 0 / 7 | — | — | — | — | — (0/3) | Not reached |
| `documented` / random | 0 / 20 | — | — | — | — | — (0/3) | Not reached |

- **Interim `prescience_pooled` (15/22 decisive queries across `contract-cases` + `value-transfers`):**
  `gap = -0.0983`, `95% CI [-0.3661, +0.0599]`, **`pass = True`** (`<= +0.10`).
  - `contract-cases` decisive (9/9): `decisive_enumerated = 7`, `other_enumerated = 97`, `shared_blind_spots = 0`.
  - `value-transfers` decisive (6/6): `decisive_enumerated = 1`, `other_enumerated = 277`, `shared_blind_spots = 3`.
  - Note: because Terra bailed out on nearly every decisive query as well (see Section 4), `enumerated` is mostly the engine's own rows, limiting how much prescience signal the negative gap carries.

---

## 2. Root Cause 1: `rql-verify` vs `menu-steps.md` Panic/Raise Mismatch (and Fix)

### Evidence from the Live Gate

Across the 19 completed `contract-cases` queries, the engine emitted **652** invalid semantic keys (`586` random, `66` decisive). Classifying every single invalid key by its adjudicator reason yields zero uncategorized leftovers:

| Category | Random | Decisive | Total | Share | Queries |
|---|---:|---:|---:|---:|---|
| Unmodeled `.unwrap()` / `.expect()` panic | 383 | 1 | **384** | 58.9% | `q4` (119), `q5` (135), `q8` (3), `q10` (126: 123 unwrap + 3 expect), `q17` (1 expect) |
| Unmodeled `assert_eq!` / `assert_ne!` panic | 166 | 0 | **166** | 25.5% | `q4` (99), `q5` (12), `q10` (55: 54 `assert_eq!` + 1 `assert_ne!`) |
| Missing `#[cfg(test)]` on `.unwrap()` / `assert_eq!` panic | 14 | 0 | **14** | 2.1% | `q9` (12 `.unwrap()` + 2 `assert_eq!`) |
| Omitted earlier guards / early exits / folded disjunctions | 15 | 65 | **80** | 12.3% | `q2` (1), `q4` (13), `q7` (1), `q15` (5), `q17` (59), `q19` (1) |
| Partial `assert!` rows (missing predicate/payload/deps) | 7 | 0 | **7** | 1.1% | `q10` (7) |
| Missing `#[cfg(test)]` on `return ()` | 1 | 0 | **1** | 0.2% | `q9` (1) |
| **Total invalid `contract-cases` engine keys** | **586** | **66** | **652** | **100%** | |

The exact same bug hit `guarded-paths` (`Kind::Raise`) on `q42` (`d68913639f02`, `guarded-paths/random`):
- **75 of 97 (77.3%)** invalid engine keys in `q42` were `assert_eq!`/`assert_ne!` `raise` rows rejected by the adjudicator because `assert_eq!` is outside the modeled macro set.
- The other `22/97` in `q42` were `20` rows attributed to an outer `fn` instead of the nested closure that actually held the `call` seed (a smaller verifier/semantics discrepancy also seen on `q37` and `q41`, because `rql-verify` only models `fn` items as `Procedure`s, not closures) plus `2` omitted inner-loop guards.

**Why it happened:** The frozen step semantics in `brief/rql-census/menu-steps.md` explicitly define the modeled constructs:
- Line 47 (*Option and Result*) lists the modeled methods (`ok`, `map`, `and_then`, `or_else`, `is_some`, `is_none`, `is_some_and`, `is_none_or`, `unwrap_or`, `unwrap_or_default`, `then`, `then_some`) and deliberately omits `.unwrap()` and `.expect()`.
- Line 51 (*Written macros*) states: *"Model source-backed Rust `assert!`, `debug_assert!`, `panic!`, `unreachable!`, `anyhow::ensure!`, `anyhow::bail!`, and `vec!` with written operands... No procedural macros, implicit synthesized field or method bodies, arbitrary expansion, inferred logging-macro effects, implicit panics from indexing/arithmetic, or general destructor behavior."*
- Line 63 (*contract-cases*) states: *"A panic is a modeled explicit failure, not implicit runtime failure."*
- Line 105 (*guarded-paths*) states: *"A raise is a written explicit panic or throw."*

When `rql-verify` was added after freeze (`73f3134c3`, 2026-11-21), `rql_mock/verifier/src/syntax.rs` diverged from `menu-steps.md`:
- `Expr::MethodCall` (`syntax.rs:470-472`) added `Kind::Panic` for every `.unwrap()` and `.expect()` method call.
- `visit_macro` (`syntax.rs:492-500`) added `Kind::Panic` and `Kind::Raise` for `"panic" | "unreachable" | "todo" | "unimplemented"` plus any macro starting with `assert` or `debug_assert` (`assert_eq!`, `assert_ne!`, `debug_assert_eq!`, `debug_assert_ne!`, etc.).

Because `rql-verify` requires every depth-0 syntactic candidate to have a covering row or valid exclusion, it forced Sonnet to emit hundreds of `.unwrap()`, `.expect()`, and `assert_eq!`/`assert_ne!` rows on test-heavy random seeds (`q4`, `q5`, `q10`, `q42`), which the adjudicator then rejected under `menu-steps.md` (with minor adjudicator inconsistency on `q2` and `q9`, where 2 `.expect()` and 9 `.unwrap()`/`assert_eq!` rows were accepted).

**Counterfactual — fixing the verifier alone does not save `contract-cases`:**
Even removing all `564` verifier-forced invalid rows (`563` random, `1` decisive), `contract-cases` still fails:
- Random precision is at most `357 / (943 - 563) = 0.9395 < 0.95` (or `346 / 369 = 0.9377` if also excluding the 11 inconsistently accepted `.unwrap()`/`.expect()`/`assert_eq!` rows on `q2`/`q9`).
- Decisive precision is `99 / (165 - 1) = 0.6037 < 0.95` (driven by `65` omitted early-exit/guard/disjunction rows, including `59` on `q17` alone).
- Stability remains `0.6240` (random) and `0.6809` (decisive), both far below `0.90`.

### Authoritative Choice and Verifier Fix

**`menu-steps.md` is authoritative.** Per `.agents/plans/rql-census-lean-pilot.md:9`, *"Where they differ, the study plan wins"*, and `menu-steps.md` is the frozen study-plan step semantics (`inputs.steps_sha256`) shared by writers, engine, Terra, and adjudicator.

Updated in source (alongside ExecPlan `.agents/plans/rql-census-lean-pilot.md`, leaving the frozen binary `/mnt/optane/review-arms-pilot/bin/rql-verify-7672df81c`, `audit.py`, and frozen `manifest.json` untouched):
- `rql_mock/verifier/src/syntax.rs`: removed `Kind::Panic` generation on `.unwrap()`/`.expect()` method calls (they remain ordinary `Kind::Call` and `Kind::Argument` candidates, and claiming `kind: "panic"` on them now fails `anchor-kind`), and restricted `visit_macro`'s `Kind::Panic` / `Kind::Raise` generation to exact modeled explicit-panic macros (`assert!`, `debug_assert!`, `panic!`, `unreachable!`, unqualified or `std::`/`core::`). `anyhow::ensure!` and `anyhow::bail!` return `Result::Err` rather than panicking and stay covered conservatively by `syntax.opaque` for `anchor-kind`.
- `rql_mock/verifier/src/tests.rs`: updated existing test and added a hard-won regression test covering all modeled vs unmodeled panic/raise constructs and `anchor-kind` rejection of `.unwrap()`/`.expect()` panic claims (`38/38` tests pass under `mbx test`, `mbx clippy --all-targets -- -D warnings` clean).

---

## 3. Root Cause 2: Sonnet/high Run-to-Run Instability (`Jaccard 0.23–0.68`)

All 14 completed fresh re-asks across the three added steps show severe non-determinism on anything larger than a handful of rows:

| Query | Step / Stratum | Engine keys | Fresh keys | Overlap | Jaccard | Primary divergence |
|---|---|---:|---:|---:|---:|---|
| `q2` (`185468ff`) | `contract-cases` / random | 4 | 5 | 3 | 0.5000 | Fresh split depth-1 `Err` paths; base collapsed one case |
| `q8` (`a5eb3b6a`) | `contract-cases` / random | 11 | 11 | 11 | 1.0000 | Exact match |
| `q9` (`168836b0`) | `contract-cases` / random | 100 | 66 | 45 | 0.3719 | Different guard/cfg(test) and panic row sets |
| `q17` (`08881f2d`) | `contract-cases` / decisive | 127 | 129 | 68 | 0.3617 | Omitted early-exit guard chains differed across runs |
| `q19` (`788951ce`) | `contract-cases` / decisive | 5 | 5 | 5 | 1.0000 | Exact match (tiny 5-row seed) |
| `q21` (`873a26b9`) | `value-transfers` / random | 119 | 57 | 3 | **0.0173** | Base emitted 119 granular per-field/arm rows (96 valid); fresh bundled them into 57 invalid whole-aggregate rows |
| `q22` (`4f161deb`) | `value-transfers` / random | 64 | 66 | 43 | 0.4943 | Divergent branch/propagation paths in `handle_action` |
| `q23` (`114af0af`) | `value-transfers` / random | 17 | 3 | 3 | 0.1765 | Fresh omitted 14 transfer routes returned by base |
| `q31` (`5c2dacb8`) | `value-transfers` / decisive | 69 | 70 | 52 | 0.5977 | Partial route divergence across argument transfers |
| `q33` (`a2d89a47`) | `value-transfers` / decisive | 28 | 70 | 16 | **0.1951** | Depth-4 downstream search: fresh explored deeper depth 2–4 callers |
| `q35` (`7562f0ea`) | `value-transfers` / decisive | 21 | 53 | 14 | 0.2333 | Different upstream argument paths and depth coverage |
| `q38` (`22acc4da`) | `guarded-paths` / random | 21 | 17 | 11 | 0.4074 | Even small 21-row query differed on guard prefixes/events |
| `q39` (`9de342a2`) | `guarded-paths` / random | 47 | 57 | 24 | 0.3000 | Omitted early-exit `ensure!`/`?` exclusions varied by run |
| `q41` (`c639f990`) | `guarded-paths` / random | 575 | 486 | 284 | 0.3655 | Base 564/575 valid; fresh had 202 invalid rows (dropped early exits, cited loop-header fragments) |

This instability is independent of the verifier panic bug and independent of Terra: `value-transfers` had zero panic-macro involvement and still scored `0.2294` (random) / `0.3421` (decisive), and `guarded-paths` `q38`/`q39`/`q41` had no `.unwrap()`/`assert_eq!` blowup and still scored `0.3576`. Asking one Sonnet/high session to enumerate an entire multi-branch, multi-hop semantic relation whole does not reproduce the same row set across runs.

---

## 4. Root Cause 3: Terra Gold-Enumerator Bailout Investigation

### Verdict on Recall

**The gate's reported recall (`0.95–1.00`) is NOT measuring true recall.** Across all **42** measured queries (`2,246` gold rows total), only **`19` gold rows (`0.85%`)** came from outside the engine (`16` Terra-only across 5 queries + `3` census-only shared blind spots in `value-transfers`). Because Terra routinely stopped after `0–5` rows, the gold pool collapsed almost entirely to the engine's own valid rows, making recall trivially `~1.00` by construction.

*(Note: Precision and stability are unaffected by Terra's bailout — adjudication still rejects invalid engine rows against source, and stability compares `engine` vs `fresh` directly.)*

### Evidence Across All 42 Terra Runs

- **Completion flag:** **41 of 42** Terra runs returned `coverage.complete = false`. Only `q19` (`788951ce`, a 6-row query) returned `complete = true`.
- **Row volume:** `5` queries returned **0 rows** (`q4` vs 297 engine, `q21` vs 119, `q27` vs 34, `q31` vs 69, `q41` vs 575); `27` returned **1–5 rows** (`q5` 1 vs 273, `q10` 2 vs 210, `q17` 3 vs 127, `q32` 1 vs 203, `q37` 1 vs 189, `q40` 2 vs 111, `q42` 1 vs 392); only `10` returned `6+` rows.
- **Explicit effort/size bailouts:** Approximately **32–33 of 42 (`~78%`)** put explicit non-semantic partial-enumeration reasons in `coverage.open` (only `~8–9` cited genuine semantic boundaries like trait-object dispatch or unmodeled `BTreeSet::insert`/`find_map`/`dedup`). Representative `coverage.open` entries from accepted Terra outputs:
  - `q4` (`0` rows vs `297` engine): *"The matching oneshot calls occur in many enclosing procedures; their complete contract cases and depth-one call effects have not been enumerated."*
  - `q17` (`3` rows vs `127` engine): *"Contract cases and guard alternatives in this seed were not fully evaluated."*
  - `q21` (`0` rows vs `119` engine): *"The bounded transfer relation was not enumerated. The file contains many matching map calls..."*
  - `q42` (`1` row vs `392` engine): *"The bounded event relation, control-flow alternatives, and callee dispatch for this seed have not been fully resolved."*
- **Effort profile:** `38 of 42` Terra runs finished in a single turn (`4` took 1 repair turn for JSON/verbatim formatting), averaging only `0.22M` tokens/query (`9.22M` tokens total across all 42 queries). `41 of 42` Terra transcripts explicitly discussed writing `"partial"`, `"representative"`, or `"minimal"` outputs.

### Structural Mechanism

1. **`rql-verify` is never run on Terra.** `rql_mock/fidelity_live.py:314-318` validates Terra only with `validate_staged` (JSON + schema + verbatim checks). Because Terra sees no `unaccounted-candidate` errors, writing `rows: []` with a `coverage.open` entry passes validation on Turn 1.
2. **The `EXECUTION_LIMIT` regex misses bailout phrasing.** `rql_mock/engine/validation.py:11-14` rejects only `budget|exhaust(ion|ed)|cancel(led|ed|lation)|timeout|timed out|truncat(ion|ed)`. Terra avoided every word in that regex and wrote `"have not been enumerated"`, `"were not fully traced"`, `"were not fully evaluated"`, and `"have not been exhaustively analyzed"`, which passed `validate_answer()` cleanly.

### Implications for Any Re-Frozen Gate (Including Documented-Only)

- **Documented steps have no `rql-verify` candidate check either.** `rql-verify` returns empty candidates for documented steps (`rql-census-lean-pilot.md:161`), and `rql_mock/fidelity.py:456` currently omits real Bifrost from the pilot's documented gold pool.
- Looking at the exact 20 documented queries that `--served-steps ""` draws (Section 6), several are high-fanout queries (`uses` of `map` in `api/tests.rs`, `callers` of `new` in `hel_database.rs`, `callers` of `into` in `scope.rs`, `callers` of `and_then` in `call_conversion.rs`, `references-of` / `occurrences-in` of `environment` in `hel_login_environment.rs`) where an unmodified Terra enumerator can bail out identically.
- Therefore, **no re-frozen gate's recall number is valid** unless the gold pool includes a non-bailing second source (real Bifrost for documented steps, and/or fixing Terra's bailout gate before launch).

---

## 5. Measured Spend to Date and Option Cost Comparison

### Exact Provider-Reported Spend (42/72 Measured Queries)

Computed from `usage.json` (`provider_session_cost.amount` and `totals.total_tokens`) across all retired engine, adjudicator, and Terra sessions backing the 42 completed measurements:

| Step | Queries (Eng Ans) | Engine (Claude) | Adjudicator (Claude) | Total Claude | Claude $/q | Terra (Codex) |
|---|---:|---:|---:|---:|---:|---:|
| `contract-cases` | 19 (24) | 27s · 59.37M · $46.19 | 19s · 24.52M · $15.40 | 46s · 83.89M · **$61.59** | $3.24 | 19s · 4.63M |
| `value-transfers` | 16 (22) | 24s · 53.14M · $57.61 | 16s · 34.48M · $19.60 | 40s · 87.62M · **$77.21** | $4.83 | 16s · 3.38M |
| `guarded-paths` | 7 (10) | 13s · 99.81M · $63.21 | 45s · 36.59M · $32.69 | 58s · 136.40M · **$95.90** | $13.70 | 7s · 1.21M |
| **Measured Total (42/72)** | **42 (56)** | **64s · 212.32M · $167.00** | **80s · 95.59M · $67.69** | **144s · 307.91M · $234.70** | **$5.59** | **42s · 9.22M** |

Plus 4 retired failed adjudicator attempts prior to the batching fix (`13.62M` tokens, `$5.95`), for **`$240.65` total Claude list-price equivalent (`321.53M` tokens)** — consuming `~88%` of `claude5`'s weekly window (`~$2.73` / `~3.65M` tokens per 1% of a `$100` Claude Max week) — and **`9.22M` Codex tokens** (`~13%` of `codex3`'s weekly window; artificially low due to Terra's 1-turn bailouts).

### Cost Comparison Across Options

| Option | Additional Gate Queries / Calls | Est. Claude Spend (Quota) | Est. Codex Spend (Quota) | What It Yields |
|---|---|---|---|---|
| **A. Stop here (paused at 42/72)** | 0 | $0 | 0% | Preserves all 42 checkpoints; documents all 3 added-step failures cleanly. |
| **B. Finish remaining 30/72 of current run** *(rejected)* | 30q + 6 fresh (`36` eng, `30` Terra, `>=30` adj) | `~$100–$170` (`~37–62%` week; needs `claude4`) | `~9%` `codex3` week (if Terra unchanged) | Cannot rescue any added step; only adds 20 documented measurements under broken Terra recall. |
| **C. Documented-only re-freeze (`--served-steps ""`)** *(proposed below, NOT started)* | 20q + 20 fresh (`40` eng, `20` Terra, `<=20` adj) | **`~$45–$75` (`~17–28%` week)** | **`~10–18%` `codex3` week** (with Terra fixed; `~6%` if unchanged) | Qualifies documented-only R / R-loc pilot per `PLAN-rql-census.md:110` (tests documented RQL vs grep and vs locations; no added-step signal). |
| **D. Redesign + re-run 3-step gate** | 72q + 20 fresh (`92` eng, `72` Terra, `>=72` adj) | `~$250–$350` (`~90–130%` week) | `~25–40%` Codex week (with Terra fixed) | Requires changing the engine architecture/model (verifier fix alone does not pass precision or stability). |

---

## 6. Proposal: Documented-Only Re-Freeze (Per `PLAN-rql-census.md:110` — NOT Started)

Under `PLAN-rql-census.md:110`:
> *"If no added step survives the pilot fidelity gate, R and R0 offer the same language and answers. R0 is then dropped from the test, R serves the documented steps alone, and the readout records that no added step reached test, with no added-step recommendation. R − G then plays R0 − G's role in the documented-step rule below."*

### 1. Exact Manifest & Identity Change

```bash
python3 -B .agents/evals/review-arms/brief/rql-census/pilot/select_pilot.py \
  --served-steps "" \
  --plan-commit <current PLAN-rql-census.md commit>
```

- Sets `"served_steps": []` in `brief/rql-census/pilot/manifest.json` while keeping the exact same 35 development diffs (17 `mjolnir`, 18 `bifrost-dev`), 10 replicate diffs, 445 writer/reviewer jobs, and seed `20261120`.
- Produces a new `manifest_sha256` and a new `engine_inputs_sha256` (since `rql_mock/engine/__init__.py:92-94` hashes `semantics_only(menu, served_steps)` and `canonical(served_steps)`), creating a fresh fidelity checkpoint directory with zero cache reuse from the paused 3-step run. Because no failing `gate-report.json` was written, nothing blocks launching the fresh gate.
- `pooled_prescience(..., applicable=False)` returns `{"status": "inapplicable", "pass": True}` (no added steps served). Documented steps are reported-only (`PLAN-rql-census.md:69,97`).

### 2. Exact Fresh Gate Workload (Verified Read-Only via `fidelity.sample()`)

Running `fidelity.sample()` read-only with `served_steps = []` produces:
- **20 documented random queries** (identical to the 20 unrun documented queries of the 3-step sample): 12 `mjolnir`, 8 `bifrost-dev`, spanning 10 documented steps: `callers` (4), `procedure-of` (3), `occurrences-in` (2), `uses` (2), `call-sites-from` (2), `references-of` (2), `call-sites-to` (2), `bindings-in` (1), `callable-signature` (1), `enclosing-decl` (1).
- **20 fresh stability re-asks** (`STABILITY_N = 20`, so every one of the 20 queries gets a fresh re-ask!).
- **Nominal model turns:** **`40` Sonnet engine answers + `20` Terra enumerations + `<=20` Sonnet adjudications** (more adjudicator turns only if any single query's candidate bundle exceeds 128 KiB and batches).

### 3. Quota and Login Plan

- **Claude (`claude5` + `claude4`):** Estimated **`$45–$75` (`~17–28%` of one Claude Max week)** assuming `~$0.80–$1.40` per structural documented engine answer (`40` calls) and `~$0.60–$1.00` per adjudication (`~20` calls). Fits on `claude5`'s remaining `12%` week (0% floor; 5H `42%` resetting ~06:40Z) plus `~5–16%` of `claude4` (resets ~05:00Z, `75%` usable above 25% floor). `claude4` is added to `engine.json` only after explicit approval.
- **Codex (`codex3`):** Estimated **`~10–18%` of `codex3`'s weekly window** once Terra enumerates completely (comfortably within `codex3`'s `62%` usable above the 25% floor; `~6%` if Terra were left unchanged).
- **Downstream pilot cost (if launched after the documented-only gate):** unchanged `~117%` Codex week across `codex3` + `codex4` (125 Sol writers + 320 Terra reviewers) + documented-only R/R-loc engine calls + `~890` Sonnet judge/hand-check calls.

### 4. Preconditions Required for a Meaningful Documented-Only Gate Result

Given Section 4 (Terra bailouts), **running the documented-only gate unmodified will measure precision and stability, but will NOT measure recall**. Before starting a documented-only gate, one of the following must be chosen and put in place:

1. **Recommended — Add real Bifrost to the documented gold pool AND close Terra's bailout:**
   - Wire real Bifrost as a mechanical candidate source for the 20 documented queries as `PLAN-rql-census.md:97` specifies for the full gate (`fidelity.py:456` currently omits it in the pilot). Real Bifrost is deterministic, zero model quota, and provides a genuine independent recall baseline for `callers`, `uses`, `references-of`, `call-sites-*`, `occurrences-in`, `bindings-in`, and `procedure-of`.
   - Tighten Terra's validation in `fidelity_live.py` (`validate_staged`) and prompt so non-semantic partial-enumeration `coverage.open` strings (`not enumerated`, `not fully traced`, `not exhaustively analyzed`, `partial`, `too many`, etc.) are rejected back into the repair loop instead of accepted on Turn 1.
   - Because Bifrost's own known limits can miss some rows while Terra can still catch them (and vice versa), `engine ∪ Bifrost ∪ non-bailing Terra` makes documented-step recall genuinely informative.
2. **Minimum without real Bifrost — Fix Terra's bailout alone:**
   - Reject non-semantic partial-enumeration `coverage.open` reasons in Terra's `validate_staged` and instruct Terra explicitly that incomplete enumeration is a validation failure, not an open frontier. Cheaper to implement than Bifrost integration, though weaker than having a deterministic analyzer in the gold pool (and large queries like `uses` of `map` may still stress Terra's repair loop).
3. **Fallback (only if user explicitly waives recall):**
   - Run unmodified, knowing documented-step recall is unmeasured (only precision and stability are meaningfully measured), and record that explicitly in the pilot readout.
   - Note also the broader study decision for the user: because documented steps are reported-only and never gated out, a documented-only pilot (`served_steps: []`) spends `~117%` Codex week + `~890` Sonnet judge/hand-check calls purely to test whether *complete documented RQL* beats grep (`R − G`) and locations (`R − R-loc`), with no test of the three census menu steps.
