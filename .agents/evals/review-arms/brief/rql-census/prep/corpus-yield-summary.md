# Local Rust corpus yield screen (2026-11-18, dbcece95; v2 after review)

Candidates: brokkbench sft-tools-commits/rust (993 repos), records with test_files>=1, non_test_files>=1 and a bug word in the oneline: 9,647. Clones /nfs/morannon/nvme/repo-clones, read only.
Method: `scan.py` dates every candidate and blames the fix's deleted non-test .rs lines at fix^ on a capped sample (10 per repo per window from 2024; 300 pre-2024 globally). A fix with any failed blame is an incomplete trace and never strong. `yield_rank.py` turns the samples into per-repo yields: upper bound x bug precision 0.85 x admission 31/67.
Strong = top blamed commit >=30% of blamed lines and <=4,000 changed lines. Multi-site proxy = fix touches >=2 non-test files or >=2 functions (crude, over-counts relation bugs).
Run: undated 0, blame errors 0, incomplete traces 0.

| Window (intro and fix on or after) | Upper bound | Realistic | Repos >=5 | >=10 | >=15 | >=25 |
|---|---:|---:|---:|---:|---:|---:|
| wide_2025 | 1576 | 620 | 24 | 8 | 4 | 3 |
| ext_2024 | 2100 | 826 | 30 | 13 | 8 | 4 |
| mid_2026 | 814 | 320 | 9 | 4 | 2 | 1 |
| strict_2026_07 | 14 | 5 | 0 | 0 | 0 | 0 |

Supply limit: the SFT lists are a model-screened subset capped at 1,000 accepted commits per repo, so this screen undercounts. Bug tracing for selected repos mines each repo's full git history.

Top realistic yields, window from 2024: web-infra-dev__rspack 43.9, 1jehuang__jcode 40.7, carthage-software__mago 38.1, sinelaw__fresh 29.4, uutils__coreutils 16.8, nushell__nushell 15.8, gitbutlerapp__gitbutler 15.6, foundry-rs__foundry 15.5, max-sixty__worktrunk 14.2, rust-lang__rust-analyzer 11.8, delta-io__delta-rs 11.0, prefix-dev__pixi 11.0, j178__prek 10.7, pola-rs__polars 9.9, Helvesec__rmux 9.6, rust-lang__cargo 9.6, PerryTS__perry 9.4, tracel-ai__burn 9.2, paradedb__paradedb 8.4, rustfs__rustfs 8.2.
