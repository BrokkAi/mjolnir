# Jev input detection on "still open" and "needs you" turn endings (2026-11-06)

## Question

Does Jev recognize that a session needs the user when an agent's turn-ending reply lists what is "still open" or says something "needs you"? Jonathan suspected it did not.

## Corpus

Mined from the controller database (Mjolnir records only, never harness-native files): every agent reply in a mjolnir or bifrost-dev session that contains "still open" or "needs you" and ends a turn. 1,163 matches, 196 turn endings, 106 sessions. At most three cases per session gave 158 fixtures; `L3101` (this analysis session) and `L0102`/`L0103` (harness `deepseek`, which `HarnessKind` cannot represent) were removed, leaving 155 (`mj-core/tests/jev-scenarios/L*.json`).

Labels: a first pass by straightforward children, then a blind second pass by subtle children that read every reply since the latest real user message. The passes disagreed on 27 of 145 cases, mostly first-pass `none` where a request existed, so the blind labels are authoritative. A third check of the 19 hardest misses confirmed 10, marked 7 ambiguous (excluded from strict scoring), and moved 2 to `none`. Each fixture's `outcome` keeps the label, request kind (`approval_gate`, `blocking_decision`, `user_action`, `next_work_menu`), request positions, and rationale. The labels cover only the input axis: `expected` gives `failure`, `input` and the harmful `wrong_actions`, and leaves `work`, `reply` and `action` out because nobody judged them (the fixture schema and `scripts/jev-scenarios-eval.py` treat an absent axis as unknown, so these fixtures do not distort the work and reply statistics).

## Baseline

Three replays per case with the then-current questions and the input gate (P >= 0.50 and >= 2.5x runner-up): 129/207 non-menu requests detected (62%); 3/192 false alerts on `none` cases. Approval gates were caught least. Loosening only the ratio would have raised recall to 77% (1.5x) or 85% (1.0x) here, but raised false alerts on the existing suite from 11 to 21 or 40 of about 537, mostly the "Waiting on X" fixtures, so the gate stayed.

Miss patterns: a decision stated as a fact in a closing "Still open"/"Needs your attention" list; the optional-cleanup clause winning over a real approval request; `redundant_request` splitting mass from `required`; report-backs to a parent; an earlier request followed by "nothing here needs your decision". Evidence trimming (6 KiB head and tail per message) hid the request in only 2 of about 26 missed cases.

## Decisions (Jonathan, 2026-11-06)

- A question the user saw and did not answer is set aside by the user's next message; if the agent still needs the answer, it must ask again in a later reply. S03 (a menu followed by an unrelated user question) therefore stays `none`, and the question locator's window (replies since the latest user message) matches the rule.
- Any approval request needs the user, whether or not the user asked for that step. Asking permission for work the user already assigned stays `redundant_request`, so automatic continuation still applies (confirmed after review).
- A menu of next steps was to count only when the user's instructions asked for it. Measured, Jev could not separate solicited from unsolicited menus (candidate c2 flagged 1 of 14 either way), so Jonathan's fallback applies: any menu, choice, or offer awaiting the user's answer needs the user. Exceptions: already authorized work (`redundant_request`), answered or rhetorical questions, questions addressed to another agent.
- Existing fixtures relabeled under that rule: D02, E07, E08, N01, N02 (offers after a delivered result), O01 ("I'll stop here unless you want me to watch for it"). Only `input`, `action` and `wrong_actions` changed; `work` and `reply` are independent axes and keep their labels (a delivered answer that ends with an offer is still `finished` and `closing`). Each records the old label in `outcome.policy`.

## Candidates

The corpus was split by session into development (90 fixtures) and held-out (65) halves; the existing suite served as the regression check. Only the `input` question changed.

| Candidate | Change | Result |
|---|---|---|
| c1 | owner rule for open items; destructive steps as approval gates; mixed redundant + real request is required; solicited-menu rule | dev recall 64% to 71%; no new false alerts |
| c2 | c1 + approval of outside operations always required | solicited menus still missed; dropped |
| c3 | c1 + any question, menu, offer, or approval put to the user is required | dev recall 85%; R01-R03 (`redundant_request`) flipped to required |
| c4 | c3 + the redundant-request exception takes precedence | false alerts back to baseline; carry-over questions fell from 32/36 to 26/36 |
| c5 | c4 + an unanswered request in an earlier reply since the user's message still counts | carry-over restored; S03 flagged |
| c6 | c5 + a question asked before the user's latest message is set aside by it unless a later reply restates it | S03 no longer flagged |
| c7 | c6 + the older status-question clause and `required` criterion require the choice to be stated or restated since that question (found in review) | adopted |

## Evidence correction (found in review)

The fixture extractor (`scripts/jev-scenarios-extract.py`) had drifted from `mj_core::assessment::ContextHistory`, which builds the evidence the worker sends. It kept Mjolnir-generated prompts (sub-agent notifications, control-origin notes) as user messages, marked the history incomplete instead of evicting the oldest exchange when the user budget was full, treated `/compact` as a boundary, and could emit 257 messages. It now ports `is_generated_prompt`, `is_generated_prompt_text`, the exchange eviction, the 256-message cap and the `/clear` boundary, and `--refresh` rebuilds a fixture's history while keeping its labels. Refreshing every transcript fixture changed 141 (user messages 3,311 to 1,712; the latest user message moved in 89). Twenty older fixtures no longer match their transcripts and were left as they were; none contains a generated prompt. Every refreshed fixture was replayed again with both the original and the adopted wording; the tables below use those replays and keep the earlier ones for unchanged fixtures. Labels did not change: the labelers had already skipped generated prompts.

## Result of c7 (adopted)

| Slice | Baseline | Adopted |
|---|---:|---:|
| Held-out half, required (three repeats) | 52/84 (62%) | 72/84 (86%) |
| Held-out half, false alerts | 0/93 | 0/93 |
| Development half, required | 81/123 (66%) | 89/103 (86%) |
| Development half, false alerts | 3/99 | 0/78 |
| Existing suite, required | 135/156 (87%) | 103/111 (93%) |
| Existing suite, false alerts | 14/519 (2.7%) | 8/413 (1.9%) |
| Offers and menus | 12/60 | 40/48 |
| Carry-over questions | 33/42 (79%) | 22/29 (76%) |
| Harmful actions, new corpus | 12/441 | 2/394 |
| Harmful actions, existing suite | 23/693 | 11/536 |

Carry-over questions are the one slice that did not improve; the difference is within repeat noise. Differences of two or three requests between c5, c6 and c7 were also within noise; c7 was chosen because it removes the contradiction between the status-question clause and the set-aside rule. D03, whose choice is restated after the status question, scores 0.94-0.95 required. On the existing suite, correct `finished`, `continue`, and `wait` rates did not fall (measured on c5 before the evidence correction).

The figures before the correction (73/84 versus 50/84 held-out) are superseded by the table above.

Remaining misses and alerts: L2308 (an evaluation lane's "decide whether the latency guard should be rerun" to its judge, 0.55 required), D15 (pausing for a pending SHA), W16/W17 near the ratio; W01, D07 (long-standing) and L2310 (1 of 2) false alerts. S03 still leans required (0.65-0.69) but stays below the ratio.

## Question locator

The locator (`mj-core/src/activity/question_locator.json`) picks the reply that holds the open request so the dashboard and viewer can scroll to it. Its wording excluded replies that "offer optional extras", which the input rule now counts, so an alert raised by an offer would have had no target (found in review). The adopted wording counts offers and menus that wait for the user's answer and closing status items that name the user, and narrows the exclusion to decisions the user already made. L fixtures with a required label now carry `outcome.request_message_ids` (their request replies inside the locator window: 90 fixtures, 76 with two or more replies). Two repeats on all locator-eligible fixtures after the evidence correction: L 145/166 correct versus 123/166; W 63/64 versus 58/64. Remaining misses are the trimmed reports (L0303, L2208), ambiguous cases (L0301, L2106, L2710), and implicit report-backs (L2311, L3011).

## Limits

- The bundled questions grew by 813 bytes. Released workers size evidence with their own bundle, so a maximal request through the updated proxy can exceed 64 KiB by that much; the proxy checks total size only on item routes.
- Repeats vary by a few points; differences of one or two requests are noise.
- The corpus is biased toward long final reports; 35 of 155 final replies exceed 6 KiB.

Artifacts: `/mnt/optane/tmp/jev-still-open-20261106/` (mined matches, blind labels, candidate questions `cand/c1-c7.json` and `cand/lc1.json`, reruns after the evidence correction in `cand/r-*`, every replay's `results.jsonl`, scoring scripts in `tools/`).
