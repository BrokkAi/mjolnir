# Local Rust corpus yield screen (2026-11-18, dbcece95)

Input: brokkbench sft-tools-commits/rust (993 repos), candidates = records with test_files>=1, non_test_files>=1 and a bug word in the oneline: 9,647 (pre2025 4,423; 2025H1 553; 2025H2 1,030; 2026H1 3,390; post2026-06 251). Clones /nfs/morannon/nvme/repo-clones, read only; clone dates min 2020-04, median 2026-07-06, max 2026-10-02 (386 clones end after 2026-07-01).
Method: scan.py. Fix dates for all; blame of deleted non-test .rs lines at fix^ on a sample (all windows from 2025 capped at 10 per repo per window; 300 pre-2025 globally): 3,427 blamed, 0 errors.
"Strong" = top blamed commit >=30% of blamed lines and <=4,000 changed lines. "Multi-site proxy" = fix touches >=2 non-test files or >=2 functions (crude, over-counts relation bugs).

Rates (traceable / strong / strong+multi / strong+multi with intro >=2025-01):
- 2025H1: 0.77 / 0.64 / 0.45 / 0.22
- 2025H2: 0.75 / 0.59 / 0.44 / 0.29
- 2026H1: 0.67 / 0.52 / 0.39 / 0.32
- post2026-06: 0.58 / 0.40 / 0.33 / 0.31

Upper-bound yield (intro and fix both in window, strong, multi-site proxy):
- wide, >=2025-01: 1,516; repos with >=3: 126, >=5: 78, >=10: 39
- mid, >=2026-01: 863; repos >=3: 73, >=5: 45, >=10: 21
- strict, >=2026-07: 15 (clones mostly end ~July 2026; strict needs a fetch)
Deflators not yet applied: bug-word precision ~0.8-0.87 (eyeballed 30 onelines), SPEC admission ~0.5 (bifrost 31/67). Wide realistic: ~550-650 bugs, ~40 repos with >=5.
Top wide repos (upper bound): mago 97, jcode 64, fresh 58, rspack 58, worktrunk 32, coreutils 25, dbx 24, gitbutler 24, foundry 22, IronCalc 21, MCP rust-sdk 20, perry 18, rust-analyzer 18, CodexPlusPlus 18, goose 18, big-code-analysis 17, polars 17, prek 16, moltis 16, rmux 16, tsoracle 16, dbt-core 16, nushell 16, rustfs 16, cargo 15, burn 15, toasty 15, kreuzberg 14, patchloom 14, delta-rs 14. Full list: per_repo_wide.json.
No candidate carries test validation (validated_image) in the SFT records.
