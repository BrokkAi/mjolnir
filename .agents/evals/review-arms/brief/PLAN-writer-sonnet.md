# Brief writer model: can Claude Sonnet/high write the brief?

Jonathan asked on 2026-11-17, after `PLAN-writer-terra.md` read "Sol writes", that Sonnet/high also be tested as the brief writer. This plan mirrors that one, so the three writers compare on the same diffs, reviewers and judging. Registered before any run.

## Diffs

The same 22 Hard-won relation diffs: 18 primary and 4 secondary (`results/missing-facts.jsonl`). Primary is the reading set.

## Arms

- **TS (Sonnet writer):** a blind brief written by Claude Sonnet/high with exactly stage B's writer prompt, tools and blinding (`brief_driver.py`), then read by 3 Terra/high reviewers per diff. That's 22 writer sessions and 66 reviewer sessions. Writers run on a Claude profile with quota. The reviewers are GPT-6.1 Terra/high through mj review's normal selection, as in round J.
- **BT (Sol writer)** and **no brief:** the stored stage B sessions, not rerun, as in round J.
- **TT (Terra writer):** round J's stored sessions, reported beside the other arms.

Each Sonnet brief gets stage B's checks, bound to the brief hash: the paired leak check and the lint and fix rounds.

## Judging

Round K judges TS, BT and no-brief together, in blind mixed batches, as round J did. Two GPT-6.1 Sol/high judges label every output, and a hand check covers every caught or partial verdict and every disagreement. All three arms are scored in the same round, so judge drift between rounds can't confound the writer comparison. Round J's labels for BT and no-brief are reported beside round K's as a consistency check. A difference over 10 points for either arm is a caveat; it doesn't decide the outcome.

## Reading

Terra/high reviewers only, mean catch rate over the 18 primary diffs, with paired 95% bootstrap intervals over diffs. All three arms use round K's labels.

- **Sonnet writes:** TS ≥ BT − 10 points, and TS − no-brief has its interval above 0. Sonnet/high becomes an acceptable writer.
- **Sol writes:** otherwise.

Also reported, without changing the outcome: the 4 secondary diffs; writer cost, tokens and wall time per brief, against Sol and Terra; and whether each brief contains the decisive fact, by the same blind contains rule as round J.
