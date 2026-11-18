# Does the decisive fact lift reviewers on bifrost-dev?

`PLAN-bifrost-bugs.md` read "Does not generalize". On bifrost-dev's 17 relation diffs, Terra/high reviewers caught 17.6% with no brief and 7.8% with a blind Sol-written brief. That brief stated the decisive fact in only 3 of the 17. The proposed lever is better facts, from Bifrost's value-flow queries, but that rests on mjolnir alone. There, the stage A oracle brief, which states the decisive fact, took Terra from 20.4% to 59.3%. This arm tests the premise on bifrost-dev before more engine work is justified for this purpose. Jonathan asked for it on 2026-11-17. Registered before any brief is built.

## Diffs

The 17 frozen bifrost-dev relation diffs (`corpus-bifrost/relations.jsonl`, Sol primary census), in their frozen order.

## Arm

**OT (oracle brief):** stage A's oracle construction (`brief/PLAN.md`, `brief_oracle.py`), applied to bifrost-dev. The editor starts from the diff's frozen blind Sol brief and the census record: fact, evidence, other facts and kind, never the census query. It makes the brief carry the decisive fact, either by matching an existing item or inserting one. The rest of stage A's construction applies unchanged:
- the lint;
- the fact check, with a decision bound to the brief hash and the carrying item quoted verbatim;
- the paired leak check: `judge/SPEC.md` on the brief text must answer `missed`, with one wording-only rewrite and a recheck. As in stage B, a relation that stays caught or partial is listed, not deleted.

Each brief is read by 3 Terra/high reviewers through mj review's normal selection, with the same carrier, pins and blinding as the stored bifrost-dev arms. That's 51 reviewer sessions.

## Judging

Round L judges OT together with the stored bifrost-dev no-brief (A) and Sol-brief (BT) relation outputs, in blind mixed batches. It uses two GPT-6.1 Sol/high judges and a hand check of every caught or partial verdict and every disagreement. All three arms use round L's labels. The stored readout's labels for A and BT are reported beside round L's as a consistency check; more than 10 points apart is a caveat.

## Reading

Terra/high, mean catch rate over the 17 diffs, with paired 95% bootstrap intervals over diffs.

Outcomes, judged on OT − A and its interval:

- **Facts carry:** the estimate is at least 17 points and the interval's lower bound is above 0.
- **Partly:** the lower bound is above 0 but the estimate is below 17.
- **Facts don't carry:** the interval's upper bound is below +10, so a useful lift is ruled out.
- **Inconclusive:** anything else, which includes any interval that spans 0 while reaching +10 or more. With 17 diffs this is a real possibility; it's reported with the estimate and doesn't count as evidence against the premise.

What a result does and doesn't establish: "Facts carry" shows only that reviewers benefit when the decisive fact is supplied, a necessary condition for milestone 5. It doesn't show that Bifrost's value-flow queries can find that fact for a blind writer. Query reach is unmeasured, and milestone 5 measures it separately. "Facts don't carry" would mean that supplying the fact isn't enough on bifrost-dev, so milestone 5 would not help there through this path.

Also reported: OT − BT; per-diff rates; the diffs where the leak check left the fact caught or partial; and builder, leak and reviewer cost.

## Registration notes, 2026-11-17, before any oracle brief

The lead confirmed that the outcomes apply in their listed priority: Facts carry, Partly, Facts don't carry, then Inconclusive. A confidence interval entirely between zero and +10 points is a proven small lift and reads Partly. Classification uses unrounded estimates and interval bounds; display rounding does not decide an outcome.

The source brief is each final published BT brief, including the four neutralized texts. Editors receive only census fact, evidence, other_facts and kind, never query. Candidates remain unpublished until the final hash-bound fact check and paired leak check/recheck are complete. Round L uses fresh labels for every arm and compares its A/BT labels with the stored final labels for the same reviewer sessions.
