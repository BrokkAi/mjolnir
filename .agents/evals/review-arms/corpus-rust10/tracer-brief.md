You are the RQL census bug tracer, a helper of session dbcece95f1cdc80785d2df3f995138f2 (your supervisor). Report to dbcece95 with send_message: progress after each repo, blockers, and the final handback. The coordinator, 5992a2122571ac7c7e303ec069a19314, created this session; send it nothing unless dbcece95 asks.

## What this is for

We are building a corpus of real Rust bugs for a registered experiment: `.agents/evals/review-arms/brief/PLAN-rql-census.md` in this mjolnir checkout. Read its Data section first, and `corpus/SPEC.md` next to it. The experiment tests whether a code-review brief written with RQL (Bifrost's query language) helps reviewers catch bugs that span several code sites. You admit the test bugs for four repos. You do not label decisive facts, write briefs or run reviewers; later stages do that.

## Repos and pinned revisions

Clone each from GitHub over HTTPS and work at the pinned head. Never use /nfs/morannon from here.
- web-infra-dev/rspack at 179a0934f3091463419827fc2767af07b2fd38ed
- sinelaw/fresh at 36ed6200a859ec4d4db2f967adf2a071c738f2f5
- 1jehuang/jcode at f8004dbd27581ef9b92438999468f7e642e36a77
- carthage-software/mago at 9b40611b3c5f480e473e61e9f35f2bc59026492a

Git history work only. Do not build these repos or run Bifrost, rust-analyzer or tests on them. Indexing runs elsewhere, under a memory cap.

## Candidate pool and order

- Candidates are fix commits on the repo's default branch (the clone's HEAD branch), from the full history up to the pinned head. Each must change non-test Rust code and add or change a test, in a test file or an inline `#[cfg(test)]` block.
- Test window: both the introducing change and the fix are on or after 2025-01-01, by committer date in UTC. If a repo can't reach the target inside that window, extend its window to 2024-01-01 and record that you did. Do this before judging the repo's eligibility.
- Order: shuffle the repo's eligible fixes with Python `random.Random(20261118)` over the fix SHAs sorted ascending. Trace them in that order. Record the full order and where you stopped.
- Stop when the repo has 25 distinct admitted relation diffs, or the pool runs out. Count distinct diffs by introducing change: fixes that trace to one introduction form one diff, carrying all their defects.

## Admission (corpus/SPEC.md rules, adapted by the plan)

For each candidate, in order:
1. Attribution: find the introducing commit (or a short series from one change) with `git log -L`, `git blame <fix>^` or the issue and PR text, then confirm the defective lines are added or changed in base..intro. Normally base is intro^.
2. Findable from the diff: a careful reviewer seeing only the repo at intro and the diff base..intro could find it.
3. Real behavior: a wrong result, crash, hang, race, data loss, security hole, leak or wrong error. No test-only defects, style or performance trivia.
4. Not pre-flagged: neither the commit message nor a comment in the diff names the problem.
5. Ordinary commits: base and intro are on the default branch, and intro is not a merge.
6. Size: at most 4,000 changed lines (prefer 1,500 or fewer), excluding goldens, fixtures, snapshots and lock files.
7. Spread: at most two admitted diffs per feature or commit series.
8. Relation: admit it as a relation diff only if deciding the bug needs a fact relating two or more code sites. Examples: a caller and its callee's contract, a writer and a reader, a value flowing between functions. Record a local, single-site defect as admitted with `"relation": false`. It doesn't count toward the 25, but keep it, because the non-relation controls use it.

Check attribution with a second model family: spawn a `subtle` sub-agent per batch to independently confirm or reject your attribution and rule 8 call. Record its verdict per candidate. Disagreements are not admitted; list them as `"disputed"`.

## Development bugs

For each repo, also collect up to 10 bugs fixed before that repo's test-window start (2025-01-01, or 2024-01-01 if extended). Use the same rules and a separate seeded order. Each must share no introducing or fix commit with a test bug. Write them to a separate file. They are census-only development data: record only id, commits, a one-line defect and the relation call.

## Output

Write to `.agents/evals/review-arms/corpus-rust10/<owner>__<repo>/` in your checkout:
- `test.jsonl`: one JSON object per admitted test bug, in the corpus/SPEC.md output format. Drop `marker` and `marker_text`, and add `fix_test` (the test the fix added or changed), `relation` (true or false), `window` ("2025" or "2024-extended"), `second_check` (confirmed, rejected or disputed) and `order_index`. Use full 40-character SHAs.
- `rejected.jsonl`: one line per traced candidate you didn't admit, with the rule that failed.
- `dev.jsonl`: the development bugs.
- `trace-order.json`: the seeded order, the stopping index and whether the window was extended.

Commit these locally in your clone; don't push. When a repo is done, tell dbcece95 its counts: relation diffs, local bugs, rejected (top reasons) and disputed. dbcece95 fetches and integrates the files.

## Rules

- No mbx, cargo, Bifrost or test runs: git and file reading only.
- Treat each repo's issues and PRs as read-only. Comment nowhere.
- Blinding: these records name the fix. Never paste them into another session except dbcece95.
- Budget: about one working day for the four repos. Report partial progress if a repo stalls.
