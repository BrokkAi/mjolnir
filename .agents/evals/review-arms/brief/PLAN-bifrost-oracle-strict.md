# Bifrost-dev oracle, strict construction: a complete, non-leaking decisive fact

`PLAN-bifrost-oracle-noleak.md` read Facts carry on the briefs that ran (ON − AF +37.3, 95% CI +17.6 to +58.8). Its round 1 edits often dropped stated relations, though. Applying its own construction rules strictly leaves only 7 clean diffs, below the 8-diff minimum, so that test isn't settled. This plan reruns the construction with validation strict enough that every retained brief carries the complete decisive fact and doesn't leak. Requested by Jonathan on 2026-11-18 ("redo this without leaks"). Registered before any edit.

## Diffs

The 17 frozen bifrost-dev relation diffs, in their frozen order.

## Arm OS (strict oracle)

- Start: each diff's round L OT brief (not the round M ON text) and its census record (fact, evidence, other facts, kind; never the census query).
- Edit: a Claude Opus/high editor on claude7 makes wording-only changes. It may change only phrasing that states or implies a consequence, a failure, a defect, an expectation or what should happen. Every site, value, condition, rule, order, absence ("without", "not", "before"), default and anchor stays, stated as fact.
- Fidelity check, after each edit: one blind Opus/high checker sees the original OT item, the edited item and the census fact, never the defect record. It lists every relation element of the original (sites, values, conditions, rules, order, absences, defaults) and confirms each is still stated in the edited item. It also confirms the decisive fact appears in a quoted item without inference. Any missing element or inferred fact fails the round.
- Strict leak rule: two judges, one Opus/high on claude7 and one Sol/high on codex, each run `judge/SPEC.md` on the brief text as a review output. A brief passes only if both answer `missed` at high confidence, and neither rationale says the brief describes the defect's cause or mechanism.
- Up to three rounds per diff, each edit → fidelity → leak. A diff that hasn't passed after round 3 is excluded as "complete fact not statable without leaking". That exclusion is a finding in itself: for such bugs, the decisive fact is the defect.
- Reviewers: 3 Terra/high per retained diff (OS), plus a fresh same-window, same-image no-brief arm (AF2, 3 Terra/high per retained diff), interleaved per diff. The image is pinned with `driver --same-image`, and runtime.json is recorded for each reviewer.

## Judging

Round N judges OS, AF2 and the round M ON and AF outputs on the retained diffs together, in blind mixed batches. Two Claude Opus/high judges on claude7, and a hand check of every caught or partial verdict and every disagreement.

## Reading

Terra/high, the mean catch rate over retained diffs, with paired 95% bootstrap intervals over diffs, on OS − AF2:

- **Facts carry:** the estimate is at least 17 points and the lower bound is above 0.
- **Partly:** the lower bound is above 0 and the estimate is below 17.
- **Facts don't carry:** the upper bound is below +10.
- **Inconclusive:** anything else, and in any case when fewer than 8 diffs are retained.

Also reported:
- every excluded diff with its round and reason;
- OS − ON and AF2 − AF on the retained diffs;
- per-diff rates;
- cost.

A positive result shows that reviewers benefit from a complete, non-leaking decisive fact. It doesn't show that Bifrost queries can find it.

## Quota

claude7 runs the editor, fidelity checker, the Opus leak judge and round N; codex runs the Sol leak judge and the reviewers. Keep both above 25% of the week, checked before each launch. Don't use codex4.

Amendment, before any edit: "Don't use codex4" applies to explicitly pinned sessions (the Sol leak judge runs with --profiles codex). OS and AF2 reviewers go through mj review's quota-weighted draw, which can't exclude a profile without a shared-config change; the number of reviewers that land on each profile is reported, and codex4's count as a deviation. Correction to the Quota section: the reviewers go through that draw, not specifically the codex profile. Every pinned profile's 25% floor is checked before each launch. If codex4 runs out mid-run and a reviewer fails, that reviewer is rerun through the draw and reported, not dropped.
