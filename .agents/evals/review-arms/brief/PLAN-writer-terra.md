# Brief writer model: can Terra/high write the brief instead of Sol/high?

The review-tool MVP adds a brief writer stage to `mj review`. A writer session
reads the change and writes a relation brief, and the Terra reviewers then
read it. In stage B the writer was Sol/high (`brief/PLAN.md`, the blind
brief). Jonathan asked, before the writer ships, whether Terra can be the
writer (2026-11-16). Terra is the reviewer the study recommends and costs
less. Registered before any run.

## Diffs

The 22 Hard-won relation diffs of the census: 18 primary and 4 secondary
(`results/missing-facts.jsonl`). Primary is the reading set.

## Arms

- **TT (Terra writer):** a blind brief written by Terra/high with exactly
  stage B's writer prompt, tools and blinding (`brief_driver.py`, brief kind
  `blind-terra`), then read by 3 Terra/high reviewers per diff. 22 writer
  sessions and 66 reviewer sessions.
- **BT (Sol writer):** stage B's stored blind briefs and stored Terra/high
  reviewer sessions on the same diffs. They are not rerun.
- **No brief:** the stored no-brief Terra/high sessions.

Each Terra brief gets stage B's checks, bound to the brief hash: the leak
check (judge/SPEC.md on the brief as a review output must answer `missed`),
and the lint and fix rounds `brief_driver.py` already runs.

## Judging

Round J: `brief_score.py build --arms TT,BT --bugs-from <the 22 diffs>`,
together with the stored no-brief sessions. Two gemini-4-carbon/high judges,
or Opus/high if Carbon quota is out (one model for every arm, decided before
the round starts and recorded). A hand check of every caught or partial
verdict and every disagreement.

Amendment, 2026-11-17, before any judging. Round J's judges and its hand
check are GPT-6.1 Sol/high on codex4: one model for every arm. Carbon
(gemini2, about 13% of its week) and Opus (Claude quota nearly gone) can't
cover round J's estimated 16 judge sessions as well as the leak check and
the bifrost-dev census. The leak check and the contains comparison stay on
Carbon. The comparison TT vs BT is within round J, so it doesn't depend on
which model judges. Judge consistency: on the 10 ablation diffs, round J's
labels for BT and no-brief are compared with round G's Carbon labels for
the same outputs, and a difference over 10 points for any arm is reported
as a caveat. To save judge tokens, round J may leave out the no-brief Sol
sessions, which the reading doesn't use.

## Reading

Terra/high reviewers only, mean catch rate over the 18 primary diffs, with
paired 95% bootstrap intervals over diffs.

- **Terra writes:** TT ≥ BT − 10 points, and TT − no-brief has its interval
  above 0. The MVP's default writer is Terra/high.
- **Sol writes:** otherwise. The default stays Sol/high.

Also reported, without changing the outcome: the 4 secondary diffs;
writer cost, tokens and wall time per brief for each model; and whether each
brief contains the decisive fact (the stage A contains rule).
