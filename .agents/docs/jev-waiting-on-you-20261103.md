# Jev “waiting on you” audit — 2026-11-03

The missing question priority had two causes. The TUI discarded completed-turn
Jev assessments, even when the worker had selected `AwaitInput`; commit
`684033081` fixes that propagation. Separately, the required-input margin can
reject a reply whose most likely classification is `required`.

The fix was pushed in merge `7f5c3765e`. Its regression exercises a worker
publication through the background projection queue and the rendered question
marker, including stale assessments and later user responses. Changed-crate
test suites, workspace Clippy and formatting passed. The regression passed
again after merging upstream, and [CI passed for the pushed merge](https://github.com/BrokkAi/mjolnir/actions/runs/41727995435).

## Corpus and reference labels

Search the controller's retained materialized transcript table, read-only, for
the case-insensitive substring `waiting on you`. This includes `waiting on
your …`, quotations and negations. Only actual agent message text counts; native
harness files and this audit's own session are excluded.

There were **351 matching agent messages in 36 Mjolnir sessions**. Grouping by
completed turn produced 339 candidate intervals. Three intervals shared a
closing message with another interval; deduplicating by session and final
stable message ID left **336 unique closing replies**. Of these, 315 still
contain the phrase in the final reply. All 299 available recorded turn outcomes
are completed turns; 37 boundaries are inferred between user messages. No
matching message was still streaming. The corpus contains 186 Codex and 150
Claude replies, dated 2026-08-16 through 2026-11-03.

Configured `straightforward` subagents (`gpt-6-luna`, high effort) judged the
conversation text without seeing Jev results. Large batches that stopped early
were replaced with smaller assignments; every final case has a completed
judgment. Disputed permission scopes received additional review using context
cut before each target reply, followed by parent adjudication. This prevents a
later user answer from being mistaken for earlier authorization. Original
judgments, replacements and adjudications remain in the local artifacts.

Labels were frozen before historical scoring or live requests:

| Reference label | Replies |
| --- | ---: |
| Genuine unresolved user guidance or external action | 240 |
| Optional offer, no request, negation, resolved request or background-only wait | 90 |
| Permission already explicitly granted | 4 |
| Ambiguous conditional authorization; excluded from scoring | 2 |

These are model judgments with explicit review, not human-authored ground
truth. The pause case in the retained control set is a scope judgment: the
latest user requested a pause, so the reference label treats the recorded
future design choices as dormant. Changing that interpretation affects one
historical control; it does not explain the margin misses.

## Results

“Detected” means the production input gate passes: `required` wins,
`P(required) >= 0.50`, and `P(required) >= 2.5 × max(P(other input choices))`.
It is distinct from merely selecting `required` as the most likely class.

| Evidence source | Genuine requests detected | False alerts on controls |
| --- | ---: | ---: |
| Retained historical worker decisions: 31 replies | **23/30 (76.7%)** | 1/1; too few controls to estimate specificity |
| Reconstructed unknown inputs: 305 replies, three repeats, majority per case | **206/210 (98.1%)** | **10/93 (10.8%)** |

The retained decisions cover only 9.2% of the corpus, from four sessions. All
seven historical misses are in one session, and six repeat the same proposed
review decision. They are not seven independent kinds of failure. The retained
sample is not a random estimate of production accuracy.

The user authorized replay only where historical decisions were unknown. The
31 known cases were reused without replay. The other 305 cases received three
requests each: **915 calls**, all successful, with no retries. The served model
was **`jev-1.14.0`** on every call. No request was sent before explicit approval
of the private-excerpt export.

| Replay repetition | Genuine requests detected | False alerts |
| --- | ---: | ---: |
| 1 | 206/210 | 10/93 |
| 2 | 205/210 | 10/93 |
| 3 | 206/210 | 10/93 |

Three cases changed their binary input-gate result across repetitions. The
session-weighted mean recall on replay sessions with a positive case was 97.6%.
For the replay subset whose closing reply itself still contains the phrase,
the gate detected 195/199 genuine questions, with 6/83 false alerts.

Combining historical decisions with unknown-case majority replays gives
229/240 detections and 11/94 control alerts. That is a mixed historical and
reconstructed result, not the current model's measured production accuracy.

## What the failures show

Jev selected `required` as its top choice for **all 30 historically confirmed
questions and all 210 replayed confirmed questions**. Every missed question
in this audit was rejected by the probability margin, not by a different
winning input class. For example, a historical reply scored required 0.68
versus none 0.32: required wins and exceeds 0.50, but its 2.125 ratio fails 2.5.

The four replay misses are reproducible by case ID and transcript position:

| Case | Session prefix | Position | Gate passes across three calls |
| --- | --- | ---: | ---: |
| Wbaac87f526 | 91742f585097 | 43036 | 0/3 |
| Wb49ff8049e | 0305d10cd40f | 231436 | 1/3 |
| W6f3f8f364f | 91742f585097 | 21510 | 0/3 |
| Wf4d0468fbe | 054dfa0752db | 20052 | 0/3 |

False alerts expose separate limitations. Four replay alerts repeat a
permission the user had explicitly granted. The other six treat an optional
follow-up or an already resolved/background-only handoff as requiring input.
The audit therefore does not justify lowering production thresholds on recall
alone. The UI fix retains the existing classifier policy.

## Evidence limits and reproduction

Unknown historical runtime evidence cannot be recovered from text alone. The
305 replays reconstruct chronological user/assistant context with the current
32 KiB user and 16 KiB assistant budgets, individual-message head/tail limits
and 2 KiB final-reply tail. Stored context-clear markers are honored. Tools,
background commands and queued commands are assumed empty; authorization is
marked incomplete rather than granting automation from reconstructed context.
Consequently, these replays measure classification of reconstructed prose,
not exactly what the old worker saw or what its UI displayed.

The corpus is enriched for one phrase, includes repeated reminders and is
clustered by session. Its recall and control-alert rates must not be generalized
to arbitrary assistant turns. Two conditional-approval cases are explicitly
excluded. Labels were not changed after viewing Jev outcomes.

Private transcripts and payloads remain outside Git at:

    /mnt/optane/tmp/jev-waiting-on-you-20261103/

`selected-cases.json` contains the 336 cases; `labels-final.jsonl` and
`labels-frozen.json` preserve the frozen reference labels. `judges.json`,
`small-judges.json`, `repair-judges.json`, `paged24-judges.json` and
`chronology-review-judges.json` record judge session IDs. The coordinator is
`9592bba2961bdc098994a47e6615c455`.

`historical-scored.json` contains the known decisions. `requests-unknown.jsonl`
and `replay-execution.json` define the authorized replay; `results.jsonl`
contains all 915 responses. `replay-majority.json`, `scores.json` and
`audit-results.csv` provide case-level and aggregate results. `verification.json`
checks complete coverage, request hashes, frozen labels and zero known-case
replays. Raw excerpts are deliberately absent from this repository report.

The label SHA-256 is
`cb82c4f8bb04c3abab22406738c9b4bb48cefac687835322431211d24cacb2a3`;
the deduplicated corpus SHA-256 is
`2dd546e3555f0039245bac9b1915c4f083f768d5bf39c4662be56c25aa2ec49f`.

To rebuild scores without sending any requests, run the following from the
repository root:

    python3 /mnt/optane/tmp/jev-waiting-on-you-20261103/score.py

Python syntax checks and the existing 19 offline evaluator tests passed.
Successful replay calls must not be repeated merely to regenerate a report.
