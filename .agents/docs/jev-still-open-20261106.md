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
| c6 | c5 + a question asked before the user's latest message is set aside by it unless a later reply restates it | adopted |

## Result of c6 (adopted)

Held-out half, three repeats: 72/84 non-menu requests (86%, baseline 50/84), 0/93 false alerts (baseline 0/93), offers 12/15 (baseline 6/15); identical to c5. Development half plus existing suite, two repeats: existing required 102/104, carry-over questions 24/28 (baseline 32/42), false alerts on existing `none` 4/346 (baseline 11/519; c5 7/346). The harmful-action and action-rate figures below were measured on c5. Harmful actions fell on both corpora (3/375 vs 15/465; 9/464 vs 15/696); on the existing suite, correct `finished`, `continue`, and `wait` rates did not fall.

Remaining misses and alerts: L2308 (an evaluation lane's "decide whether the latency guard should be rerun" to its judge, 0.55 required), D15 (pausing for a pending SHA), W16/W17 near the ratio; W01, D07 (long-standing) and L2310 (1 of 2) false alerts. S03 still leans required (0.65-0.69) but stays below the ratio.

## Question locator

The locator (`mj-core/src/activity/question_locator.json`) picks the reply that holds the open request so the dashboard and viewer can scroll to it. Its wording excluded replies that "offer optional extras", which the input rule now counts, so an alert raised by an offer would have had no target (found in review). The adopted wording counts offers and menus that wait for the user's answer and closing status items that name the user, and narrows the exclusion to decisions the user already made. L fixtures with a required label now carry `outcome.request_message_ids` (their request replies inside the locator window: 90 fixtures, 76 with two or more replies). Two repeats on all 106 locator-eligible fixtures: L 131/152 correct versus 113/152; W 60/60 versus 53/60. Remaining misses are the trimmed reports (L0303, L2208), ambiguous cases (L0301, L2106, L2710), and implicit report-backs (L2311, L3011).

## Limits

- The bundled questions grew by 813 bytes. Released workers size evidence with their own bundle, so a maximal request through the updated proxy can exceed 64 KiB by that much; the proxy checks total size only on item routes.
- Repeats vary by a few points; differences of one or two requests are noise.
- The corpus is biased toward long final reports; 35 of 155 final replies exceed 6 KiB.

Artifacts: `/mnt/optane/tmp/jev-still-open-20261106/` (mined matches, blind labels, candidate questions `cand/c1-c5.json`, every replay's `results.jsonl`, scoring scripts in `tools/`).
