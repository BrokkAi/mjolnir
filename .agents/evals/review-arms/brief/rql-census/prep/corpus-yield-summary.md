# Local Rust corpus yield screen (2026-11-18, dbcece95; v6 after six review rounds)

Candidates: brokkbench sft-tools-commits/rust (993 repos), records with test_files>=1, non_test_files>=1 and a bug word in the oneline: 9,647. Clones /nfs/morannon/nvme/repo-clones, read only.
Method: `scan.py` dates every candidate and blames the fix's deleted non-test .rs lines at fix^ on a capped sample (10 per repo per window from 2024; 300 pre-2024 globally). A fix with any failed blame is an incomplete trace and never strong. `yield_rank.py` turns the samples into per-repo yields: upper bound x bug precision 0.85 x admission 31/67.
Strong = top blamed commit >=30% of blamed lines, not a merge, and <=4,000 changed lines excluding goldens, fixtures and lock files (corpus SPEC rules 6 and 7). Clones sharing a candidate commit or a root commit (forks, renames, subtrees) are one repo; the member with the most candidates is kept. Diffs, blame and sizes ignore diff drivers and textconv. Multi-site proxy = fix touches >=2 non-test files or >=2 enclosing functions, found from the old file's `fn` signatures (crude, over-counts relation bugs). Paths come from `git diff --name-status -z -M`; renamed files keep their old path for blame.
Run: undated 0, blame errors 0, incomplete traces 0, shared-history groups 15; the ranker refuses to rank while any planned draw lacks a record.

| Window (intro and fix on or after) | Upper bound | Realistic | Repos >=5 | >=10 | >=15 | >=25 |
|---|---:|---:|---:|---:|---:|---:|
| wide_2025 | 1630 | 641 | 25 | 8 | 6 | 3 |
| ext_2024 | 2168 | 853 | 33 | 14 | 9 | 4 |
| mid_2026 | 847 | 333 | 10 | 4 | 2 | 2 |
| strict_2026_07 | 13 | 5 | 0 | 0 | 0 | 0 |

Supply limit: the SFT lists are a model-screened subset capped at 1,000 accepted commits per repo, so this screen undercounts. Bug tracing for selected repos mines each repo's full git history.

Top realistic yields, window from 2024: web-infra-dev__rspack 42.6, sinelaw__fresh 41.1, 1jehuang__jcode 40.7, carthage-software__mago 38.1, uutils__coreutils 17.4, gitbutlerapp__gitbutler 17.2, max-sixty__worktrunk 16.0, nushell__nushell 15.8, foundry-rs__foundry 15.5, delta-io__delta-rs 12.9, rust-lang__rust-analyzer 11.8, j178__prek 10.7, prefix-dev__pixi 10.3, paradedb__paradedb 10.0, pola-rs__polars 9.9, Helvesec__rmux 9.6, rust-lang__cargo 9.6, PerryTS__perry 9.4, modelcontextprotocol__rust-sdk 8.7, tracel-ai__burn 8.5, ironcalc__IronCalc 7.7, BigPizzaV3__CodexPlusPlus 7.0, kreuzberg-dev__kreuzberg 6.7, tokio-rs__toasty 6.7.
