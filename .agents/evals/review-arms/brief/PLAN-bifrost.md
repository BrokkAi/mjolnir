# Relation briefs, stage C: a Bifrost-generated brief (BrokkAi/bifrost-dev#13150)

Stage C replaces the Sol/high brief writer of stage B with the Bifrost tool of
BrokkAi/bifrost-dev#13149 and asks whether its brief helps reviewers as much.
Jonathan approved the preparation on 2026-11-15; the paid reviewer runs start
only on his go-ahead. This file is committed before any Bifrost brief exists,
and its sets, arms and rules do not change after that.

## Sets and comparators

- The frozen sets of `brief/PLAN.md`: primary P (18), secondary S (4),
  control C (8) Hard-won diffs, and the 40 ordinary diffs of
  `corpus/diffs.jsonl`.
- Comparators, all stored: no-brief Terra/high and Sol/high sessions (arms A,
  B, D, D2), and blind-brief sessions (arms `BT`, `BS`, the Sol/high writer's
  briefs). The oracle arms are not rerun or rejudged.

## Briefs

- The tool is `relation_brief` from BrokkAi/bifrost-dev#13224 (branch
  `13149-relation-brief`, head `e5b7b52a11ef`), built with
  `cargo build --profile measure --bin bifrost`. Its first version states
  callers and paths from call and reference relations; Value flow reads "Not
  computed". So this run measures the location-level brief that #13150 asks
  to measure first. Every Rust brief opens with "Diff inventory incomplete"
  (#13184), constant across diffs.
- `bifrost_brief_driver.py` runs it once per diff, serially:
  `bifrost --root <checkout> --tool relation_brief --args
  '{"base":"<parent sha>","target":"<intro sha>"}'` with the tool's defaults
  (25 items, 7,000 characters, tests left out) and one `BIFROST_CACHE_ROOT`
  for all runs. It records the version and commit, the exact command, wall
  time, exit status and completeness, and stores the JSON output and
  `structuredContent.brief` under `briefs/bifrost/<diff>/`.
- The brief is used exactly as the tool emits it: no edits, rewrites or leak
  rewrites. The mechanical lint of `brief_driver.py` and a leak verdict
  (`judge/SPEC.md` on the brief text) are recorded per brief for analysis.
- A diff whose brief failed to generate gets an empty brief (the section
  heading and note with no items): a reviewer in use would get nothing. The
  primary analysis counts these diffs; a secondary analysis leaves them out.
- For P and S, an Opus judge with the census record labels whether the brief
  contains the decisive fact (yes, partial, no) or only its location, as the
  stage A editor did; this is reported, not used in the decision.

## Runs

`FT` (3 Terra/high) and `FS` (3 Sol/high) on the 30 Hard-won diffs; `FT` on
the 40 ordinary diffs: 300 sessions. Everything else as in stage B (carrier,
isolation, audit, inventories). `driver.py` injects the brief after the commit
message under the same heading and fixed note as stages A and B.

## Scoring

- Hard-won: one judging round (`brief_score.py build --arms BT,BS,FT,FS
  --name brief-judge-f`) judges no-brief, blind and Bifrost sessions together
  in blind batches, with two Opus/high judges and a hand check of every caught
  or partial verdict and every disagreement, as in stages A and B.
- Ordinary: findings of the stored no-brief Terra/high sessions (arm A) and
  `FT` are pooled and verified blind (`build_verify.py verify-bifrost --arms
  A,FT`), then scored with `brief_control2.py --arm FT`, with every
  broad-match rejected finding hand-marked.

## Decision rule

Catch rates are per session on P, mean over diffs. For each model: N is the
no-brief rate, B the blind-brief rate, F the Bifrost-brief rate.
E = F − N is the Bifrost brief's effect and G = B − F its gap to the blind
brief, each with a paired 95% bootstrap interval over the 18 diffs. Terra/high
is the primary model: it had the largest blind-brief effect (+35.2 points) at
the lowest cost. The first outcome that holds, in this order, is the decision:

1. **Location is enough:** E for Terra ≥ 30 points and G for Terra ≤ 10
   points, and control 1 passes. A brief from today's outputs already gives
   most of the blind brief's effect; ship #13149 and lower the review-driven
   priority of #13211 and #13213.
2. **Site context needed:** E < 15 points for both models, and the upper end
   of E's interval < 30 points for both. The brief does not help as built.
   The value lies in stating relations, which the blind (+35.2 Terra, +18.5
   Sol) and oracle (+38.9, +25.9) arms measured; raise #13211 and #13213 ahead
   of further #13149 work. This is not a null result on briefs.
3. **Relations add:** E for Terra ≥ 15 points and the lower end of G's
   interval for Terra > 0. The brief helps, and a brief that states the
   relations helps more; keep #13149 and raise #13211.
4. **Inconclusive:** otherwise. Report it; the remedy is more traced Hard-won
   diffs (`corpus/SPEC.md`) or a rerun after #13211.

Controls:

- **Control 1:** on C, F − N < 10 points for both models. Otherwise the brief
  helps as a second reviewer would; E is then taken net of the C gain, as in
  stage A, before the rule above.
- **Control 2:** on the 40 ordinary diffs, confirmed problems per session with
  the Bifrost brief ≥ 90% of no-brief, and rejected findings that rely on a
  brief item (hand-marked) ≤ 8. A failure does not change the decision; it is
  reported as the brief's cost, beside the blind brief's 88%.

## Report

The decision with E and G and their intervals for both models; rates on S and
C; expected catches of 30 for one reviewer; the share of P and S briefs that
contain the decisive fact or only its location; lint and leak results; brief
generation time, failures and completeness per diff; control 2; reviewer
tokens and latency by arm.
