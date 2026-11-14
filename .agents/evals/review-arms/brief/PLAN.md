# Relation briefs: does a reviewer catch more known defects with code-graph facts?

Requested 2026-11-14 by the Bifrost CPG session (202f2573) for Jonathan, who
chose a staged version: stage A first, stage B only if stage A shows an
effect. The answer decides whether a brief generator is worth building in
Bifrost. A null result must be conclusive, so stage A guarantees that the
decisive fact is in the prompt.

This file is committed before any reviewer run, and its sets and rules do not
change after that.

## Frozen analysis sets (30 Hard-won diffs)

From `results/missing-facts.jsonl` (first labeler), checked by a second
independent Opus/high labeler under the same spec
(`$REVIEW_ARMS_DIR/label-facts/second/workflow-out.json`): the two agree on
relation versus not for 29 of 30 and on kind for 28 of 30. They disagreed on
whether `journal-valid-json-corruption` is visible in the diff; both name the
same fact, whose decisive line (`validate_relay_event`, snapshot.rs:939) is
outside the diff, so it stays a relation. Both labelers are the same model,
so their agreement is not fully independent.

- **Primary (P, 18):** the decisive fact is a relation in the first pass.
  archive-resume-hint-without-prompt, catalog-cache-key-collision,
  child-model-wrong-efforts, codex-effort-alias-lost,
  export-relative-read-root, github-token-in-container-argv,
  held-prompt-ignores-interrupt, journal-valid-json-corruption,
  path-completion-lost-on-step-redraw, pending-assessment-hidden-when-idle,
  projection-mixed-sqlite-snapshots, reserved-workspace-internal-error,
  restore-through-profile-symlink, stale-model-launch-spec,
  symbol-change-keeps-cached-transcript, terminal-id-reused,
  unused-thread-above-archive-floor, web-feed-self-deadlock.
- **Secondary (S, 4):** visible in the diff in the first pass, a relation in
  the second pass (`results/missing-facts-pass2.jsonl`).
  activity-recorder-kills-api, dropdown-click-hits-covered-field,
  metadata-checkpoint-overwrites-checkout,
  shortcut-escape-opens-unvisited-manager.
- **Control (C, 8):** not a relation. Outside the repository:
  container-env-lost-during-worker-bootstrap, handback-reminder-stops-work,
  osc-payload-in-transcript, pinned-worker-disappears,
  ssh-user-hostkey-policy-overridden, stop-awk-multiline-variable,
  stop-grep-matches-itself. Logic: palette-scattered-match-beats-literal.

## Briefs

1. **Blind brief** (`brief_driver.py`, `brief/WRITER.md`): one writer per
   diff, gpt-6.1-sol/high, with the reviewers' isolation (checkout at the
   introducing commit with no later history, Podman target, no network,
   memory, GitHub, builds or helper agents, no corpus records). Sections
   Affected callers, Paths and Value flow. Every item is a relation with
   `file:line` anchors, at least one outside the changed lines; no judgment
   words and no advice; diff order; at most 25 items and about 1,500 tokens.
   A lint checks the mechanical rules and the writer fixes violations.
   Writer transcripts are audited with `audit.py`.
2. **Oracle brief** (P and S): a judge with the census record labels whether
   the blind brief contains the decisive fact (yes, partial, no). For partial
   or no, an editor inserts the census relation as one item at its diff-order
   position, with the consequence clause removed (only the relation, not what
   goes wrong), and without the census `query` field. For S, the inserted
   item may relate two changed locations. For C, the oracle brief is the
   blind brief.
3. **Leak check:** `judge/SPEC.md` runs on each oracle brief's text as if it
   were a review output. A brief judged caught or partial is rewritten to
   remove the judgment while keeping its relations. Some decisive relations
   are close to the defect by themselves (a function that holds a lock while
   calling one that takes it); if a neutral relation alone is still judged
   caught or partial, it is kept and listed in the report, because removing
   it would remove the fact under test. Every inserted item is read by hand.

## Injection

`driver.py` arms `OT` (3 Terra/high) and `OS` (3 Sol/high) append the oracle
brief to `historical-context.txt` after the commit message, under
"Relations computed from the repository", with the fixed sentence "These are
relations computed from the repository at this commit, not findings. They may
be incomplete." Arms `BT` and `BS` do the same with the blind brief (stage B).
A pilot on 4 diffs confirms in reviewer transcripts that reviewers received
the brief. Everything else is as in stage 2.

## Stage A runs

`OT` and `OS` on all 30 Hard-won diffs: 180 reviewer sessions. The no-brief
baseline is the stored Terra/high and Sol/high sessions on the same diffs
(arms A, B, D, D2; at least 3 of each model per diff), not a fresh run.

## Scoring

Known-defect verdicts per session with `judge/SPEC.md`, for both the stored
baseline sessions and the new ones, mixed in the same blind batches with
random output IDs. Two independent Opus/high judges label every output; every
caught or partial verdict from either judge and every disagreement is checked
by hand. Outputs both judges label missed are accepted without a hand check.
A session catches its defect when the final verdict is `caught`; `partial` is
reported separately.

The catch rate of a set, arm and model is the mean over its diffs of the
share of that diff's sessions that caught the defect. Intervals are 95%
bootstrap intervals over diffs (10,000 resamples). Tokens, latency and tool
calls are recorded per session.

## Decision rule (stage A)

Effect = oracle catch rate minus stored no-brief catch rate on P, per model.

- **Null:** the effect is under 15 points for both models. Stop: a brief
  generator would not improve review of these defects. Stage B is not run.
- **Supports building:** the effect is at least 30 points for either model,
  and control 1 passes. Run stage B.
- **Between:** otherwise. Run stage B, and recommend tracing about 40 more
  Hard-won markers under `corpus/SPEC.md` with the added rule "the decisive
  fact is a relation" before a final decision.
- **Control 1:** on C, the gain from the brief is under 10 points for both
  models. A larger gain means the brief helps as a second reviewer would,
  not through its facts; the P effect is then reported net of the C gain and
  the decision uses the net effect.

S is reported separately and does not enter the decision.

## Stage B (only if stage A is not null)

Blind briefs `BT` and `BS` on the 30 Hard-won diffs (180 sessions), and `BT`
on the 40 ordinary diffs (120 sessions), whose findings are pooled through
the verify pipeline with arms hidden; each rejected finding that cites a
brief item is marked. Control 2: confirmed findings per session on ordinary
diffs stay at or above 90% of no-brief, and rejected findings citing the brief
stay at or below 1 per 5 diffs.

## Report

Catch rate by arm, model and set with intervals; the share of P and S whose
blind brief contained the decisive fact; expected catches of 30 for one
reviewer with a brief against 11.3 for stored Sol + 2× Terra; tokens and
latency by arm; the list of decisive facts with kinds and anchors
(`results/missing-facts.jsonl`).
