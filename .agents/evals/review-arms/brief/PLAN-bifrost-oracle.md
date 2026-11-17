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

- **Facts carry:** OT − A ≥ 17 points with the interval above 0. Better facts are the lever on bifrost-dev too, and milestone 5's engine path is justified for the writer.
- **Facts don't carry:** OT − A ≤ +10 points, or its interval includes 0. Supplying the fact doesn't lift reviewers there, and milestone 5's premise fails on bifrost-dev.
- **Partly:** otherwise.

Also reported: OT − BT; per-diff rates; the diffs where the leak check left the fact caught or partial; and builder, leak and reviewer cost.
