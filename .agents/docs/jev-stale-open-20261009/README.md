# Open-turn stale finish and reply v7 evaluation (2026-10-08)

The 21 O fixtures are labeled against the complete handback or the concrete follow-up that proves work was still active. A replied-phase sample adds final-reply labels so `reply` can be checked against handbacks/questions and explicit next steps.

| Expected reply/work label | Claude | Codex | Kimi | Total |
| --- | ---: | ---: | ---: | ---: |
| O finished handback | 2 | 1 | 0 | 3 |
| O required input | 0 | 2 | 0 | 2 |
| O silent but working | 1 | 10 | 5 | 16 |
| Existing replied-phase `reply=closing` sample | — | — | — | 17 |
| Existing replied-phase `reply=continuing` sample | — | — | — | 24 |

The O fixtures are `mj-core/tests/jev-scenarios/O01` through `O21`. Reconstructed cases preserve `facts_known: false`. The scenario test checks the current `stale_finished` signature through both gates: a threshold-qualified modern Finished assessment and an independent `reply=closing` assessment at its .80 threshold, each only after 40 minutes of silence. Recorded running negatives, including O07/SO-X04's 0.92 Finished verdict at 310 seconds, remain below the silence floor.

## Previous work-wording round

The earlier three-round experiment covered 123 fixtures × 3 repeats. Candidate 3 met its then-current input/wrong-action rule, but the parent rejected it after adding replied-phase finished detections to adoption: it dropped expected-finished `P(finished) >= .80` counts from 51/99 to 42/99 and finished actions from 51/96 to 41/96. Candidate 3 is not adopted. Its original prompts and reports remain under `/mnt/optane/mj-jev-stale-open-20261009/{baseline,candidate-round-1,candidate-round-2,candidate-round-3}/`.

## v7 single-variant replay

`verdict_questions_v6.json` is byte-for-byte `git show HEAD:mj-core/src/activity/verdict_questions.json` (SHA-256 `ee1a8c89f9009ed8eb71888945325938658663f92bafc8e3db79f25d344b3314`). The one tested v7 variant adds the generated Continue/sub-agent notice sentence to both `work` and `input`, plus the new `reply` choice after `work`. It was run over all 123 fixtures, three repeats, with zero errors in each batch.

| Metric | v6 baseline | v7 single-variant trial | Direction |
| --- | ---: | ---: | --- |
| Required-input detections | 58/60 | 57/60 | Worse by 1 |
| Confident false-required alerts | 6 | 6 | Equal |
| Listed wrong actions | 15 | 15 | Equal |
| Expected-finished `P(finished) >= .80` | 53/99 | 54/99 | Better by 1 |
| Finished actions on expected-finished-action cases | 53/96 | 54/96 | Better by 1 |
| Errors | 0 | 0 | Equal |

The notice sentence fails the strict no-regression rule because required detections fell from 58 to 57. It is rejected. The final live bundle restores `work` and `input` instructions byte-for-byte to v6 and retains only the `reply` question. The table above is that rejected wording trial; the exact final bundle is measured separately below.

## Reply results from the single-variant trial

The reply threshold is fixed at `P(closing) >= .80`; it was not tuned. Candidate-trial reply scores:

- O01 with no authorization: `P(closing) = [1.00, 1.00, 1.00]`.
- O01 with reconstructed authorization: `P(closing) = [1.00, 1.00, 1.00]`. It is above .80 on all repeats.
- Running-phase closing detections: 12/15 labeled closing requests.
- False closing across fixtures labeled continuing: 12/120 (10.0%). On the replied-phase continuing sample specifically: 9/72 (12.5%).
- O07/SO-X04 is a notable false-closing control: despite its expected `continuing` label, `P(closing)` was 0.96, 0.94, 0.96. In the recorded turn it resumed after about 310 seconds, so the 40-minute floor prevented a stale close; the score is a residual risk if it persists to the floor.

The candidate report lists `P(closing)` by repeat for every labeled case. Overall mean `P(closing)` is 0.905 for expected closing and 0.191 for expected continuing. The auth variant uses 24 whole user messages (6,967 bytes) and 38 assistant messages (16,363 bytes), with the generated sub-agent notice removed. It was assembled from a read-only SQLite transcript because `mj transcript` refused expired-log cleanup and an isolated retry could not bind; no live session was changed.

Run snapshots, reports, and `selection.json` are under `/mnt/optane/mj-jev-stale-open-20261009/reply-v7/`. The full runs are `baseline-v6/` and `candidate-v7/`; the three-repeat authorization-only measurement is `incident-auth-v7/`.

## Exact final-bundle replay

The live final bundle was replayed against the frozen v6 baseline over all 123 fixtures, three repeats each. Fixture snapshots match exactly, the baseline question snapshot equals `verdict_questions_v6.json`, and both runs had zero request errors.

| Metric | v6 baseline | Exact final v7 | Change |
| --- | ---: | ---: | ---: |
| Required detections | 58/60 | 57/60 | -1 |
| Confident false-required alerts | 6 | 6 | 0 |
| Listed wrong actions | 15 | 16 | +1 |
| Expected-finished `P(finished) >= .80` | 53/99 | 52/99 | -1 |
| Finished actions on expected-finished-action cases | 53/96 | 52/96 | -1 |
| Errors | 0 | 0 | 0 |

**Verdict:** The final bundle does not meet “no worse than baseline” on every existing-axis metric. In the observed replay, the added `reply` question coincided with one fewer required detection, one more listed wrong action, and one fewer expected-finished detection/action. False-required alerts and errors were unchanged. The service uses live `jev-latest`, so this comparison cannot prove the question alone caused each score change independently of model variation.

Exact final-bundle reply results at the fixed `P(closing) >= .80` threshold: O01 scored `[1.00, 1.00, 1.00]` without authorization and `[1.00, 1.00, 1.00]` with reconstructed authorization; false closing on labeled continuing fixtures was 12/120 (10%); running-phase closing detections were 12/15; expected-closing detections were 57/66. The complete comparison and replay outputs are under `/mnt/optane/mj-jev-stale-open-20261009/reply-v7/`, particularly `exact-final-comparison.md` and `final-live-v7-network/report.md`.
