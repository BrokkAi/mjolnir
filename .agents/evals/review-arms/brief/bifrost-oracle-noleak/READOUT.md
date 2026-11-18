# Bifrost-dev non-leaking oracle arm (ON): readout

Registration: `brief/PLAN-bifrost-oracle-noleak.md`, with its edit-scope clarification, the AF amendment and the judge-model diagnostic amendment, all recorded before the step they govern. Execution record: `.agents/plans/bifrost-oracle-noleak-arm.md`. Data root: `/mnt/optane/review-arms-bifrost-20261116`. Artifacts are in this directory.

## Reading: Facts carry

Terra/high reviewers, 3 per arm per diff, on all 17 bifrost-dev relation diffs; every diff was retained. Catch rate is the mean over diffs of the share of a diff's sessions that caught its defect. Intervals are paired 95% bootstraps over diffs. The classification uses unrounded bounds.

| Arm | Catch rate | Partial rate |
|---|---:|---:|
| ON, non-leaking oracle brief (this arm) | 45.1% | 5.9% |
| AF, fresh no-brief, same window and image as ON | 7.8% | 2.0% |
| A, stored no-brief | 17.6% | 3.9% |
| OT, round L's leaking oracle brief | 45.1% | 3.9% |

- **Primary, ON − AF: +37.3 points (+17.6 to +58.8). Facts carry:** the estimate is at least 17 and the lower bound is above 0.
- ON − A (stored): +27.5 (+9.8 to +47.1).
- ON − OT: 0.0 (−11.8 to +11.8). The point estimates match, but this does not establish that the leak had no effect. The interval allows a change of about 12 points either way, and the contrast spans two review windows and two images, so it measures wording plus any window, cohort or image difference.
- AF − A, the image and cohort effect: −9.8 (−19.6 to −2.0). The fresh no-brief cohort on the current image caught less than the stored no-brief cohort. Measured against the same-window control, ON's lift is larger than it is against stored A.

What the primary contrast compares: ON reviewers received a whole relation brief (affected callers, paths, value flow) that carries the decisive fact, while AF reviewers received no brief. So ON − AF measures the effect of the complete neutral brief, not of the decisive fact in isolation. Round L gives indirect evidence about the rest of the brief: the blind Sol brief (BT), which carried the decisive fact in only 3 of the 17 diffs, caught 7.8% against stored A's 17.6%. That suggests the surrounding relations alone do not produce the lift, but this arm doesn't test it directly.

What this shows and what it doesn't: when the decisive fact is stated neutrally, so that leak judges of both models read the brief as `missed`, Terra/high reviewers on bifrost-dev catch far more of these relation bugs than a same-window, same-image no-brief control. It does not show that Bifrost's value-flow queries can find that fact for a blind writer. Query reach is unmeasured and is milestone 5's question.

## No-leak construction

- **Round 1:** one Opus/high editor per diff on claude7 reworded the carrying or cited items of the final OT brief. The edits were wording only: same lines, same anchors, lint clean, and each declared change reviewed by the parent (`round-1/scope-review.json`). One blind Opus/high fact checker accepted all 17 as still carrying the census fact, each quoting its carrying line verbatim and bound to the brief's hash. Both Opus/high leak judges said `missed` on all 17, so no second or third round was needed, and none were excluded.
- **What the rewording removed.** The plan registers wording-only edits that keep every relation. In practice, 13 of the 17 edits removed explicit absence, order or contrast clauses ("without", "not", "before", "after", "only", "even when", "instead") and replaced them with literal descriptions of what the code does. That is how the leaks were removed, and it also makes some relations less explicit. Examples:
  - java-static-getter: the item no longer says that owner, static and namespace matching precede `found_alias`.
  - retained-fill: the item describes the memo's insert and hit paths, but no longer says they skip a completion check and do not replay build gaps.
  - go-receiver: the item gives the inventory key tuple, but no longer names the omitted `has_receiver`.
  - unproven-override: the item no longer says that both empty-vector routes enter the same arm.
  The blind fact checker judged that every carrying item still states the decisive census fact, and the parent accepted the scope. In hindsight, the parent's scope review was wrong to accept java-static-getter as relation-preserving: its edit dropped two reader-side relations the OT brief supplied, the comparison against the import demand namespace and the ordering of matching before `found_alias`. That is a deviation from the registered wording-only scope (`round-1/scope-review.json` is left as recorded). Even so, ON measures a fact stated literally and less explicitly than in OT, not one restated word for word (`published/<diff>/edit.json` holds every before/after pair).
- **Diagnostic 1:** the same two Opus judges flag 15 of the 17 unedited OT briefs (11 caught, 4 partial). These are the same 15 the Sol judges flagged, and Opus is stricter on three of them. So the clean round 1 result reflects the rewording, not lenient judges.
- **Inferred carrying items.** In two diffs, the fact checker accepted the item by inference rather than from an explicit statement of the census fact. For retained-fill, it inferred "caches without a completion check and replays no gaps" from the memo's hit and miss paths. For kotlin-value-class, it inferred "the dispatch receiver has no carrier binding" from separate descriptions of receiver creation and formal-slot binding. The run and its labels are kept as recorded. Excluding those two diffs, ON − AF over the remaining 15 is +40.0 points (+17.8 to +62.2), still **Facts carry**.
- **Order of the diagnostics.** The diagnostic amendment held reviewers until the supervisor had seen both diagnostics. After diagnostic 1, the supervisor lifted the hold for diagnostic 2 and directed that the reviewers and diagnostic 2 start together when `codex` reset. The first ON review therefore started at 05:28:31Z, eleven seconds before the Sol verdicts were written at 05:28:42Z. Diagnostic 2 is a retrospective check, not a gate.
- **Diagnostic 2:** two Sol/high judges, round L's leak-judge model, say `missed` on all 17 published ON briefs. The secondary reading that excludes Sol-flagged diffs is therefore empty.

## Per diff (catches of 3; partials in parentheses)

| Diff | Round L OT leak label | A | AF | ON | OT | ON − AF |
|---|---|---:|---:|---:|---:|---:|
| python-filescope-enclosing-tie | partial | 1 | 0 | 3 | 3 | +100.0 |
| cpp-template-inherited-base-signature | caught | 0 | 0 | 3 | 3 | +100.0 |
| incompatible-semantic-bundle-late-gate | partial | 0 | 0 | 0 | 0 | 0 |
| retained-fill-java-memo-loses-gap | caught | 1 (2) | 0 (1) | 0 (3) | 2 (1) | 0 |
| kotlin-source-constructor-steals-class-header-comment | partial | 0 | 0 | 0 | 0 (1) | 0 |
| mcp-default-advisory-reported-clean | caught | 0 | 0 | 0 | 0 | 0 |
| go-receiver-poisons-package-result-contract-discovery | caught | 0 | 0 | 0 | 0 | 0 |
| python-module-binding-representative-after-event | caught | 2 | 0 | 3 | 3 | +100.0 |
| jvm-protected-unindexed-hierarchy-denied | partial | 0 | 0 | 2 | 1 | +66.7 |
| python-module-mutation-repeated-scan-budget | missed | 0 | 0 | 0 | 0 | 0 |
| dependency-failure-drops-independent-model-evidence | missed | 0 | 0 | 0 | 0 | 0 |
| php-indexed-write-reenters-current-rhs | caught | 2 | 1 | 3 | 3 | +66.7 |
| kotlin-value-class-this-loses-underlying-projection | partial | 0 | 0 | 1 | 2 | +33.3 |
| go-nilness-ignores-modeled-nonreturn | partial | 0 | 0 | 2 | 0 | +66.7 |
| java-static-getter-drops-incomplete-route | partial | 0 | 0 | 0 | 0 | 0 |
| unproven-override-family-certifies-empty | caught | 3 | 3 | 3 | 3 | 0 |
| js-synthetic-owner-leaks-file-summary | caught | 0 | 0 | 3 | 3 | +100.0 |

The lift is concentrated in 8 diffs; 9 do not move. Seven of those nine catch nothing in any arm, and unproven-override is caught 3 of 3 everywhere. All three ON reviewers on retained-fill reached the right memo but were judged partial under the stored transient-failure standard.

## Round M judging and consistency

The round has 204 blind outputs: ON, AF, stored A and stored OT, 51 each. The A and OT outputs match round L's frozen hashes. There were three mixed batches, each read by two Opus/high judges on claude7, who agreed on all 204 (137 missed, 66 caught, 1 partial). One Opus/high assessor hand-checked the 67 caught or partial rows and confirmed every judge label. Before joining arms, the parent applied the same stored-round precedent as round L to seven retained-fill rows, changing them from caught to partial. The rule: a finding that confines the cached failed index build to a transient or store failure is partial. Identical texts keep their earlier labels. The seven turned out to be 3 ON, 2 A, 1 AF and 1 OT (`round-m-stage/handcheck-parent-decisions.json`).

- Round M against round L on the same sessions: A and OT each differ by 0.0 points, with 0 changed labels. As in round L, part of this agreement comes from applying the precedent.
- One sealed decision is arguable: AF output 0b81aa10 names "an incomplete read or store error", and the parent labeled it partial under the lower-label rule. Counted as caught instead, AF would be 9.8% and ON − AF +35.3, still Facts carry. The sealed label stands.
- Sensitivity, using the assessor's labels unchanged: ON 51.0%, AF 9.8%, A 21.6%, OT 47.1%. ON − AF is +41.2 (+21.6 to +60.8), still **Facts carry**. ON − OT is +3.9 (−3.9 to +13.7), and AF − A is −11.8 (−23.5 to −2.0).

## Image, window and profiles

- All 34 ON and AF carriers and all 102 reviewers ran on one pinned image, `36578a96…`. Each reviewer was observed live inside its carrier's container and has a runtime record (`runtime.json`) with its process list, so no reviewer went unobserved. There were no probe errors and no set-aside carriers.
- Review windows overlap. ON ran from its first review start to its last status poll over about 36 minutes, and AF over about 32 minutes, both in the same span. ON and AF were interleaved per diff, four jobs at a time.
- OT (round L) ran on `89128665…` and A on the older stored images, which is why ON − OT and ON − A are secondary.
- 91 reviewers ran on the `codex` profile and 11 on codex4 (6 ON, 5 AF). The supervisor accepted this, because mj review's quota-weighted draw cannot exclude a profile that is low but not empty without a config edit. codex4 went from 8% to 4% during the run.

## Cost

These are recorded API-equivalent estimates at 10^10 ticks per dollar, not subscription spend. The total is a recorded subtotal, because carrier usage has 20 unfinished turns.

| Stage | Sessions | Estimate |
|---|---:|---:|
| Round 1 editors (Opus/high) | 17 | $4.15 |
| Round 1 blind fact check | 1 | $0.75 |
| Round 1 paired leak check | 2 | $1.48 |
| Diagnostic 1 (Opus on the OT briefs) | 2 | $1.84 |
| Diagnostic 2 (Sol on the ON briefs) | 2 | $0.41 |
| ON/AF reviewers (Terra/high) | 102 | $4.62 |
| ON/AF carriers; coverage incomplete | 34 | $1.58+ |
| Round M judges | 6 | $3.25 |
| Round M hand check | 1 | $0.72 |
| **Recorded subtotal** | | **$18.81+** |

## Limits

- 17 diffs give wide intervals. The lower bound of the primary contrast is +17.6 points.
- The leak rule depends on the judges. The diagnostics show that both judge models pass the ON briefs and flag the OT briefs, but a stronger or differently prompted judge could still find hints.
- AF is a single fresh cohort. Its 7.8% is below stored A's 17.6%, a cohort and image effect that this design measures but cannot explain.
- Some edits made relations less explicit (see "What the rewording removed"). The registered wording-only scope was judged per edit by the fact checker and the parent, not verified mechanically.
- Exact agreement with round L's labels is partly by construction, through the parent precedent; the sensitivity run shows the outcome does not depend on it.
- Reviewer tool audits and latency are unavailable, because reviewer sessions are destroyed at role end.
