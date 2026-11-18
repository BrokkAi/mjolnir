# Does a non-leaking decisive fact lift reviewers on bifrost-dev?

`PLAN-bifrost-oracle.md` read "Facts carry" (OT − A +27.5 points, 95% CI +11.8 to +45.1), but only as an upper bound. 15 of its 17 oracle briefs still leaked by the leak judges' rule, and the whole lift sat on leaking diffs. On 2026-11-18 Jonathan asked for it to be redone without leaks. This arm asks: when the decisive fact is stated in a way that doesn't give the defect away, do Terra/high reviewers catch more bifrost-dev relation bugs than with no brief? Registered before any brief is edited.

## Diffs

The 17 frozen bifrost-dev relation diffs, in their frozen order. A diff is in the reading set only if its brief passes the leak rule below. Diffs that don't pass are listed with the reason and their round L OT result, and are excluded from the reading.

## Arm ON (non-leaking oracle brief)

- Start: each diff's final OT brief (`oracle-arm/final-brief-stage`), and its census record (fact, evidence, other facts, kind; never the census query).
- Edit: one Claude Opus/high editor on claude7 rewrites only the item or items that carry the decisive fact. They state the relation neutrally: which code connects to which, what values or conditions flow, at which anchors. Nothing about consequences, failure, defects, expectations or what should happen, and no wording that singles the relation out as suspicious. Anchors stay.
- Fact check after every edit: the carrying item, quoted verbatim, must still state the decisive fact by the census. One blind Opus/high fact checker decides, bound to the brief's hash. An edit that removes the fact is rejected.
- Leak rule: two Opus/high judges on claude7 each run `judge/SPEC.md` on the brief text as if it were a review output. The brief passes only if both answer `missed`.
- Up to three rounds per diff of wording-only edits to the carrying items, followed by the fact check and the paired leak check. A diff that hasn't passed after round 3 is excluded as "fact not statable without leaking (by these judges)".
- Reviewers: 3 Terra/high per retained diff, through mj review's normal selection, with the same carrier, pins and blinding as the stored arms.

## Judging

Round M judges ON together with the stored no-brief (A) and oracle (OT) outputs on the retained diffs, in blind mixed batches. Two Claude Opus/high judges on claude7, and a hand check of every caught or partial verdict and every disagreement. Round L's labels for A and OT on the same diffs are reported beside round M's as a consistency check; more than 10 points apart for either arm is a caveat.

## Reading

Terra/high, the mean catch rate over the retained diffs, with paired 95% bootstrap intervals over diffs, on ON − A. Outcomes in priority order:

- **Facts carry:** the estimate is at least 17 points and the lower bound is above 0.
- **Partly:** the lower bound is above 0 and the estimate is below 17.
- **Facts don't carry:** the upper bound is below +10.
- **Inconclusive:** anything else, and in any case when fewer than 8 diffs are retained.

Also reported:
- ON − OT on the retained diffs, which is how much of round L's lift came from leaking wording;
- every excluded diff, with its exclusion round and its round L OT − A;
- per-diff rates, and the edit and leak-check history;
- cost.

A positive result shows that reviewers benefit when the decisive fact is supplied without giving the defect away. It doesn't show that Bifrost queries can find that fact.

## Models and quota

Editor, fact checker, leak judges and round M judges are Claude Opus/high on claude7. Reviewers are Terra/high on whichever Codex profile mj review selects. Every profile keeps the 25% weekly floor, checked before each launch.

Clarification, 2026-11-18, before any edit: wording-only edits may touch any item the leak judges cite as the source of the leak, not only the carrying items. No item is deleted, no relation or anchor changes, and the carrying items must still carry the decisive fact by the blind fact check.

Amendment, 2026-11-18, before any ON reviewer: round L's OT carriers ran on a newer Podman image than the stored A/BT carriers. To keep the comparison within one image, this arm adds AF, a fresh no-brief arm with 3 Terra/high reviewers per retained diff, run in the same window and on the same image as ON, interleaved with ON jobs. The primary reading is ON − AF on the retained diffs. ON − A (stored) and AF − A (stored), the image or cohort effect, are reported as secondary. Round M judges ON, AF, stored A and stored OT together.

Diagnostic amendment, 2026-11-18, after the round 1 leak check and before any ON or AF reviewer: every one of the 17 round 1 briefs passed, after one wording pass, under Opus/high leak judges, while round L's leak flags came from Sol/high judges. To tell the wording apart from the judge model, two diagnostics run outside the registered reading, each with `judge/SPEC.md` and round L's either-judge rule. (1) The same two Opus/high judges on claude7 read the 17 unedited OT briefs. (2) Once the `codex` profile is above its 25% floor, two Sol/high judges (round L's leak-judge model) read the 17 published ON briefs. The supervisor reviews both before any reviewer starts. If Opus passes most OT briefs while Sol still flags ON briefs, the sweep reflects the judge rather than the wording, and the supervisor decides whether to run more wording rounds judged by both models or to proceed. If Sol passes the ON briefs, the reviewers launch.
