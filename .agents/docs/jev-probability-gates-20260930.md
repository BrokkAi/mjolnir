# Compare probability-based Jev gates

The best observed compromise uses separate gates: for required input, the winning probability must be at least 0.50 and 2.5 times the runner-up; for finished work, require probability at least 0.80 while preserving the existing input-none and failure gates. Automatic continuation and recovery stay unchanged. This is an experimental recommendation; runtime policy was not changed.

## What was measured

TypeSafe returns the complete choice distribution and a separate derived confidence statistic. Confidence is not the winning probability: one replay returned required probability 0.82 with confidence 0.75. The [official confidence documentation](https://docs.typesafe.ai/confidence) explicitly permits alternative calculations; the [API reference](https://docs.typesafe.ai/api) describes the returned probability map. Earlier evaluator runs discarded that map. The evaluator now preserves full typed answers alongside its backward-compatible compact verdict.

All 102 previously authorized fixture payloads were credential scanned. The current frozen round-4 question bundle was replayed three times, then the candidate grid was evaluated offline against identical responses. The user requested several variations; the grid covers 113 settings across provider-confidence cutoffs, winning-probability floors, winner-minus-runner gaps, normalized entropy concentration, and winner/runner ratios with optional floors. No prompt or label changed.

Six input and four finished candidates were frozen before a second three-repeat replay. Their [confirmation results](jev-gates-20260930/confirmation.json) measure stochastic stability on the same cases, not new-case generalization. After inspecting those results, the entire predefined grid was also scored on the second replay. The final finished recommendation, probability 0.80, was in that original grid but not the narrower confirmation finalist list; its final selection is exploratory rather than an independently validated choice.

There are 612 successful API responses across two replays. Each contains 54 scored required requests, 237 controls, and 15 ambiguous requests. Across both runs, completed-work positives contribute 174 responses and replied-turn cases explicitly forbidding finished contribute 258. Runtime admission still applies after Jev classification; these comparisons do not execute worker actions. Running-turn finished predictions are excluded from the main false-finish count.

## Required-input tradeoffs

All ratio rows below include a winner-probability floor of 0.50. Recall is successful input alerts divided by 108 required replays. False alerts are divided by 474 control replays.

| Gate | Detected | Recall | False alerts | False-alert rate |
| --- | ---: | ---: | ---: | ---: |
| Current confidence >= 0.85 | 71/108 | 65.7% | 1/474 | 0.2% |
| Winner >= 2x runner-up | 108/108 | 100% | 23/474 | 4.9% |
| Winner >= 2.5x runner-up | 102/108 | 94.4% | 13/474 | 2.7% |
| Winner >= 3x runner-up | 98/108 | 90.7% | 12/474 | 2.5% |
| Winner >= 4x runner-up | 88/108 | 81.5% | 8/474 | 1.7% |
| Confidence >= 0.75 | 86/108 | 79.6% | 6/474 | 1.3% |
| Normalized concentration >= 0.70 | 76/108 | 70.4% | 1/474 | 0.2% |

A 2.5x ratio trades one additional false alert versus 3x for four additional detections; 2x adds six detections but ten false alerts. There is no unique best gate without a cost for missed requests versus interruptions. The recommendation favors recovering real user decisions while cutting roughly half of the 2x rule's false alerts. If preserving the current false-alert rate matters most, normalized concentration yields a much smaller gain at the same observed error count.

The remaining 2.5x false alerts are D07's optional followup, R04's redundant request to resume already-authorized work, and one D16 background-cleanup reply. These are classification errors, not an absence of a clear winner. In one R04 replay the wrong required answer has probability 0.86 versus runner-up 0.10, so a larger ratio alone cannot distinguish it from lower-probability genuine requests.

## Finished-state tradeoffs

These rows change only the work gate when its choice is finished. The existing failure-none confidence >= 0.90 and input-none confidence >= 0.85 requirements remain. False finishes count only replied-turn cases whose frozen labels explicitly forbid finished.

| Work gate | Correct finished / 174 | False finished / 258 |
| --- | ---: | ---: |
| Current confidence >= 0.85 | 63 | 6 |
| 2x runner-up, probability >= 0.50 | 114 | 27 |
| 2.5x runner-up, probability >= 0.50 | 111 | 17 |
| 3x runner-up, probability >= 0.50 | 109 | 15 |
| 4x runner-up, probability >= 0.50 | 104 | 6 |
| Winning probability >= 0.80 | 104 | 6 |
| Confidence >= 0.70 | 107 | 8 |
| Confidence >= 0.75 | 97 | 6 |

The 0.70 confidence cutoff initially looked strongest at no additional error cost, but the second replay incorrectly finished P06 twice. Probability 0.80 and the 4x rule each deliver 52 correct finishes and the same three A03 false finishes in both runs. Probability 0.80 is easier to interpret and guarantees 4:1 odds against all alternatives combined, rather than only the strongest alternative. Both still miss many completed answers because of the input gate or a wrong work choice.

Applying 2x indiscriminately to all gates is worse. In the first replay it produces four prohibited continuations, plus false failure-based input actions. The activity-only 2x experiment also produces three running-turn finished predictions that runtime admission would block; those are retained in the raw policy report but excluded from the table above. No automatic-action gates are recommended for relaxation.

## Generalization and boundaries

A pure ratio can accept a weak plurality: probabilities 0.40/0.20/0.20/0.20 pass 2x. An absolute floor prevents that. On these replays the 0.50 floor does not change 2x results; it is an interpretable guard for flatter distributions elsewhere. API probabilities are rounded; the analysis uses a 1e-12 tolerance only for floating-point representation at exact boundaries.

The 102 fixtures comprise 46 real source sessions plus one grouped synthetic family. Five-fold analysis keeps all cases and repetitions from a session together. Selection uses four folds and evaluates on the fifth. With a training false-input budget of 2.5%, pooled validation detects 47/54 required replies with 6/237 false alerts. Zero training false alerts still produces 3/237 validation false alerts. Finished-work selection minimizing errors while maximizing recovery yields 53/87 correct finishes with five false finishes, versus baseline 31/87 and three. These exploratory checks show that the improvements are plausible and zero-extra-error claims are fragile.

This is a curated suite rich in known problems, not a random sample of live use. Replays are correlated, and both new runs use already inspected cases. The counts do not estimate production error rates or establish probability calibration. The full grid's final ranking uses both replays; do not call the second replay an untouched holdout for that ranking.

## Reproduction, artifacts, and validation

Full snapshots, raw answer distributions, manifests, and reports live under `/mnt/optane/mj-jev-gates-20260930/`, in `replay/` and `confirmation/`. Offline scripts and compact results are in [jev-gates-20260930](jev-gates-20260930/summary.json). Run from the repository root:

    python3 .agents/docs/jev-gates-20260930/compare.py /mnt/optane/mj-jev-gates-20260930/replay --output /mnt/optane/mj-jev-gates-20260930/comparison.json
    python3 .agents/docs/jev-gates-20260930/sweep.py /mnt/optane/mj-jev-gates-20260930/replay --output /mnt/optane/mj-jev-gates-20260930/sweep.json

Fourteen evaluator tests and four analysis tests pass. They cover retaining probability separately from confidence, numerical boundary/floor behavior, preserving finished gates in input-only comparisons, keeping related cases grouped, and explicitly reporting an unattainable training error budget. The scripts verify complete response counts and baseline agreement with the existing action policy. No Rust, daemon, live store, prompt, or runtime gate was changed.
