# Bifrost-dev oracle arm: readout

Registration: `brief/PLAN-bifrost-oracle.md`, including the 2026-11-17 registration notes and the 2026-11-18 amendment that moved round L's judges and hand-check assessor to Claude Opus/high on claude7. Execution record: `.agents/plans/bifrost-oracle-arm.md`. Data root: `/mnt/optane/review-arms-bifrost-20261116`. Round L artifacts: `round-l-stage/` here, with the frozen inputs in `round-l-stage/input-freeze.json`.

## Reading: Facts carry (an upper bound)

Terra/high reviewers, 3 per diff on the 17 bifrost-dev relation diffs. Catch rate is the mean over diffs of the share of a diff's sessions that caught its defect. Intervals are paired 95% bootstraps over diffs (seed 1, 10,000 draws). The classification uses unrounded rational bounds.

| Arm | Catch rate | Partial rate |
|---|---:|---:|
| A, no brief | 17.6% | 3.9% |
| BT, blind Sol brief | 7.8% | 0.0% |
| OT, oracle brief | 45.1% | 3.9% |

- **OT − A: +27.5 points (+11.8 to +45.1).** The estimate is at least 17 and the lower bound is above 0, so the registered outcome is **Facts carry**.
- OT − BT: +37.3 points (+17.6 to +58.8).

**Leak caveat. Read this result as an upper bound.** After one wording-only rewrite and a recheck, the paired Sol/high leak judges still rate 15 of the 17 oracle briefs as pointing at the defect when the brief is read as a review output: 8 caught and 7 partial. The registered rule keeps them, so part of the lift may come from the brief naming the bug rather than from a supplied relation that the reviewer acted on. Only 2 briefs read as missed, and neither lifted the reviewers.

**Image caveat.** All 17 OT carriers ran on a different Podman image (`89128665…`) from the stored A and BT carriers (`0a614615…`; A on unproven-override-family-certifies-empty used `cd7e9788…`). The pins, intro/base commits and inventories match, but the brief and the reviewer image changed together across the whole OT cohort, so the paired bootstrap cannot separate their effects. The registered outcome stands as computed, and the lift is an observed cohort difference whose attribution to the brief rests on the image change being immaterial (`/mnt/optane/review-arms-bifrost-20261116/oracle-arm/review-driver/audit-report.md`).

What this shows and what it doesn't: reviewers on bifrost-dev benefit when the decisive fact is supplied. It does not show that Bifrost's value-flow queries can find that fact for a blind writer. Query reach is unmeasured and is milestone 5's question.

## Per diff (catches of 3)

| Diff | Final leak label | A | BT | OT | OT − A (points) |
|---|---|---:|---:|---:|---:|
| python-filescope-enclosing-tie | partial | 1 | 0 | 3 | +66.7 |
| cpp-template-inherited-base-signature | caught | 0 | 0 | 3 | +100.0 |
| incompatible-semantic-bundle-late-gate | partial | 0 | 0 | 0 | 0 |
| retained-fill-java-memo-loses-gap | caught | 1 | 0 | 2 (+1 partial) | +33.3 |
| kotlin-source-constructor-steals-class-header-comment | partial | 0 | 0 | 0 (+1 partial) | 0 |
| mcp-default-advisory-reported-clean | caught | 0 | 0 | 0 | 0 |
| go-receiver-poisons-package-result-contract-discovery | caught | 0 | 0 | 0 | 0 |
| python-module-binding-representative-after-event | caught | 2 | 1 | 3 | +33.3 |
| jvm-protected-unindexed-hierarchy-denied | partial | 0 | 0 | 1 | +33.3 |
| python-module-mutation-repeated-scan-budget | missed | 0 | 0 | 0 | 0 |
| dependency-failure-drops-independent-model-evidence | missed | 0 | 0 | 0 | 0 |
| php-indexed-write-reenters-current-rhs | caught | 2 | 0 | 3 | +33.3 |
| kotlin-value-class-this-loses-underlying-projection | partial | 0 | 0 | 2 | +66.7 |
| go-nilness-ignores-modeled-nonreturn | partial | 0 | 0 | 0 | 0 |
| java-static-getter-drops-incomplete-route | partial | 0 | 0 | 0 | 0 |
| unproven-override-family-certifies-empty | caught | 3 | 3 | 3 | 0 |
| js-synthetic-owner-leaks-file-summary | caught | 0 | 0 | 3 | +100.0 |

Descriptive means by final leak label. These are observed strata, not tested:

| Leak label | Diffs | A | BT | OT | OT − A |
|---|---:|---:|---:|---:|---:|
| caught | 8 | 33.3% | 16.7% | 70.8% | +37.5 |
| partial | 7 | 4.8% | 0.0% | 28.6% | +23.8 |

The 2 missed-leak diffs appear only as rows above, both 0 in every arm.

Nine diffs show no lift: 3 of the 8 caught-leak diffs, 4 of the 7 partial-leak diffs and both missed-leak diffs. One of the nine, unproven-override-family-certifies-empty, is already caught 3 of 3 without a brief. On the other eight, no OT reviewer caught the defect even with the fact in the brief. The lift is concentrated in the other 8 diffs, five of which reach 3 of 3.

## Leak rate compared with stage A's mjolnir oracle

On the 18 primary mjolnir diffs, stage A flagged 10 briefs (1 caught, 9 partial) and left 8 missed. Here 15 of 17 are flagged (8 caught, 7 partial) and 2 are missed. The bifrost-dev briefs leak more, and far more of them leak fully. The judging differs, though. Stage A used one Opus oracle leak judge, while bifrost-dev used two paired Sol/high judges with the either-judge rule. Here the two Sol judges gave the same final label on all 17 briefs, so either judge alone would have produced the same 15 flags, and the either-judge rule doesn't explain the gap. The difference in judge model and corpus still makes the comparison descriptive. Stage A's lift on its 8 missed-leak diffs (+29.2 points) has no counterpart here, because only 2 diffs are missed and neither moved.

## Round L judging and label consistency

The round has 153 blind outputs: 51 each for A, BT and OT. All 153 match their archived output hashes, and the A and BT sessions are exactly the 102 sessions of the stored `bifrost-judge-relation` round. There were three mixed batches of up to 8 diffs and two Opus/high judges per batch on claude7. The judges agreed on all 153 outputs: 113 missed, 39 caught, 1 partial. The 40 required rows (every caught or partial verdict; there were no disagreements) went to one Opus/high hand-check task, using the stored round's HANDCHECK.md, INSTRUCTIONS.md and SPEC.md, whose hashes match the contract. The task confirmed all 40 judge labels. I then read all 40 rows blind to arm and sealed the decisions before joining arms. Final hand-check labels are 36 caught and 4 partial.

The parent overrode the assessor on three rows, all in retained-fill-java-memo-loses-gap, changing caught to partial. The reason is the stored round's accepted hand-check standard: a finding that attributes the cached failed Java index build only to a transient store or read failure, rather than StructureUnavailable or equivalent stable incompleteness, is partial. Two of the three rows are byte-identical to stored outputs labeled partial under that rule. After the join, the three rows turned out to be 2 A and 1 OT. Details are in `round-l-stage/handcheck-parent-decisions.json`.

- Round L against the stored Sol/high labels on the same sessions: A differs by 0.0 points and BT by 0.0, with 0 changed labels in each. There is no >10-point caveat. This agreement is not fully independent. The two A rows the parent moved to partial were decided with the stored texts and their stored labels in view, so the stored standard was applied deliberately. The independent comparison is the assessor's unadjusted labels: A differs from the stored labels by +3.9 points (2 changed labels) and BT by 0.0. That is still under the 10-point caveat.
- Sensitivity, using the assessor's labels unchanged: A is 21.6%, BT 7.8% and OT 47.1%. OT − A is +25.5 (+9.8 to +43.1), still **Facts carry**. A would then differ from the stored labels by 3.9 points.

## Cost

These are recorded API-equivalent estimates at 10^10 ticks per dollar, not subscription spend. The total is a recorded subtotal, and the full cost is unknown. The carrier records hold 3 missing usage reports and 9 unfinished turns across 8 of the 17 carriers, so those carriers' costs are incomplete.

| Stage | Estimate |
|---|---:|
| Editors, two leak rounds and the wording pass (Sol/high, codex4) | $3.54 |
| OT reviewers, 51 Terra/high sessions | $2.28 |
| OT carriers, 17 sessions; recorded subtotal, coverage incomplete for 8 carriers | $1.13+ |
| Round L judges, 6 Opus/high sessions on claude7 | $2.91 |
| Round L hand check, 1 Opus/high session on claude7 | $0.69 |
| **Recorded subtotal** | **$10.56+** |

claude7 had 93% of its week left before round L. A and BT reused stored outputs, so they cost nothing new.

## Limits

- 15 of 17 briefs leak, so the result is an upper bound. With only 2 missed-leak diffs, the data can't separate supplied relations from named defects.
- With 17 diffs the interval is wide: the lower bound is +11.8 points.
- Round L used Opus judges, not the registered Sol, because of the amendment. The two Opus judges reproduced the stored judges' raw A and BT labels. After hand check, the Opus assessor alone differs from the stored final labels by 3.9 points on A, through two retained-fill rows, and the parent's precedent-based decisions remove that difference. Either way it is below the 10-point caveat, and the outcome is unchanged.
- The OT carriers ran on a different Podman image from the stored A and BT carriers (see the image caveat).
- Reviewer tool audits and latency are unavailable, because reviewer sessions are destroyed at role end (#1554/#1559).
