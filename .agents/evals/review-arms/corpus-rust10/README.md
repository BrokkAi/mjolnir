# Rust RQL census corpus

Test bugs for the registered RQL census (`../brief/PLAN-rql-census.md`): 12 Rust repos, chosen by `../brief/rql-census/prep/select_cohort.py`, traced in seeded order (seed 20261118) to 25 distinct relation diffs or until the pool ran out. Every repo clears the 8-diff floor.

## Counts

A relation diff is one introducing commit with at least one confirmed relation defect. Disputed and rejected rows are kept in `test.jsonl` but don't count.

| Repo | Window | Stop / pool | Relation diffs | Relation bugs | Local controls | Dev bugs | Disputed | Rejected |
|---|---|---|---|---|---|---|---|---|
| 1jehuang/jcode | 2025 | 110 / 483 | 25 | 25 | 9 | 0 | 2 | 1 |
| carthage-software/mago | 2025 | 136 / 771 | 25 | 25 | 11 | 1 | 3 | 0 |
| Helvesec/rmux | 2025 | 79 / 80 | 16 | 16 | 4 | 0 | 5 | 2 |
| j178/prek | 2024-extended | 68 / 69 | 15 | 18 | 5 | 0 | 5 | 5 |
| kreuzberg-dev/kreuzberg | 2025 | 504 / 505 | 18 | 19 | 8 | 0 | 7 | 4 |
| max-sixty/worktrunk | 2025 | 171 / 366 | 25 | 25 | 10 | 0 | 4 | 2 |
| nushell/nushell | 2025 | 194 / 282 | 25 | 25 | 9 | 10 | 5 | 2 |
| rust-lang/rust-analyzer | 2025 | 287 / 336 | 25 | 27 | 6 | 10 | 1 | 0 |
| rustfs/rustfs | 2024-extended | 494 / 495 | 23 | 23 | 16 | 0 | 17 | 9 |
| sinelaw/fresh | 2025 | 84 / 1085 | 25 | 25 | 2 | 0 | 3 | 4 |
| uutils/coreutils | 2024-extended | 416 / 625 | 25 | 25 | 28 | 10 | 5 | 2 |
| web-infra-dev/rspack | 2025 | 197 / 666 | 25 | 27 | 5 | 10 | 5 | 3 |
| Total (12 repos) | | | 272 | 280 | 113 | 41 | 62 | 34 |

## Files per repo

- `test.jsonl`: admitted rows in the corpus/SPEC.md format, plus `relation`, `fix_test`, `window`, `second_check` and `order_index`.
- `second-check.jsonl`: the second-family verdict for each row, with evidence. A `resolution` field marks a row whose final status differs from the checker's verdict, and says why.
- `rejected.jsonl`: traced candidates the tracer didn't admit, with the rule that failed.
- `dev.jsonl`: development bugs fixed before the repo's test window (census only).
- `trace-order.json`: the seeded order and stop index. Extended repos also record `order_2025_len`. The rmux and prek files predate the `repo` and `pool_size` fields; their pool size is the length of `order`.
- `beyond-stop.jsonl` (coreutils only): rows admitted before the extended-window re-walk moved the stop earlier. They are set aside and not counted, and their verdicts are in `second-check-beyond-stop.jsonl`.

## Rulings applied to every repo

- A tracer from one model family admitted rows as pending. A Claude Sonnet check, run on separate clones at the same pins, confirmed, rejected or disputed each one. Only confirmed rows count.
- Any relation-call disagreement makes the row disputed, even when the checker confirmed the attribution.
- An intro that only moved, refactored or reverted already-faulty code, or only made it reachable, doesn't introduce the defect. Those rows are rejected or disputed.
- The fix commit's test must exercise the defect: a fix_test that passes before the fix fails the row.
- A panic counts only if it happens in the shipped build. Integer-overflow panics in repos whose release profile has no overflow checks are disputed.
- Spread allows at most two admitted diffs per feature or commit series. Where a third appeared, the latest in seeded order was rejected.
- A short repo resumed from its recorded stop index in the same seeded order. When a window extended to 2024, rows rejected only for a 2024 introduction were re-evaluated at their original positions (the plan's Window section).

The tracer brief is `tracer-brief.md`. The pending-then-check flow and the later repos were sent as messages.
