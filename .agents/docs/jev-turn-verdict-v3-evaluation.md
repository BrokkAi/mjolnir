# Synthetic Jev v3 evaluation

Evaluated on 2026-09-21 using `jev-latest` through the direct TypeSafe endpoint. All nine scenarios are invented in `scripts/jev-turn-verdict-eval.py`; no private session transcripts or live stores were read. Each wording version was evaluated once per scenario. These observations do not establish accuracy on real sessions or compare history windows.

## Observed model scores

| Scenario | Input need | Work state | Work confidence |
| --- | ---: | --- | ---: |
| approval_and_heap | 0.93 | background_work | 0.98 |
| background_only | 0.05 | background_work | 1.00 |
| finished | 0.06 | finished | 0.95 |
| parent_work | 0.07 | still_working | 1.00 |
| optional_offer | 0.10 | finished | 0.92 |
| answered_request | 0.13 | still_working | 1.00 |
| rhetorical | 0.11 | still_working | 1.00 |
| missing_information | 0.92 | unclear | 0.46 |
| unclear | 0.15 | unclear | 0.43 |

The initial wording scored approval-plus-heap at 0.67 input need and 0.99 background-work confidence, below the input action threshold. Rephrasing the Noul question as a binary predicate and explicitly allowing approval requests during independent work produced 0.93 and 0.98. The 0.85 action threshold and 0.15 confident-absence threshold were not changed. Both raw runs retain the exact questions in `jev-turn-verdict-v3-synthetic-initial.json` and `jev-turn-verdict-v3-synthetic.json`.

## Deterministic validation

Rust tests supply fixed probabilities to verify input precedence, phase rules, inclusive threshold boundaries, uncertain scores, malformed responses, stale evidence rejection, and retention of running children and stop controls. A real ACP bridge fixture emits child lifecycle, message, and tool events throughout the production one-minute silence interval. HTTP fakes exercise direct and hosted requests and exact diagnostic fields. These tests validate mechanics; the observed model scores above are separate evidence.

To repeat the synthetic evaluation, run from the repository root:

    python scripts/jev-turn-verdict-eval.py --run --output /path/to/new-results.json

Omit `--run` to check the built-in scenarios without network requests. Publication is separate: deploy `/v3/turn-verdict` before distributing new clients; legacy v1/v2 remain available.
