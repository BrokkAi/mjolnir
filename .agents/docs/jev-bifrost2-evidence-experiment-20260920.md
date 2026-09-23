# Jev evidence experiment: local bifrost2, 2026-09-20

Raw historical tool-command text was the strongest source of uncertainty in this incident. Replacing it with short factual tool descriptions increased the post-reply `finished` confidence from 52–56% to 99% across three trials. More input did not necessarily help: adding lifecycle explanations while retaining noisy command text could lower confidence.

## Incident and scope

Session `240b3367c2002893047b919f0bc7f4b3`, local `/home/jonathan/Projects/bifrost2`, was displaying a long-running turn after its assistant had reported the requested merge and instruction edits complete. At 12:09:55 CDT, production Jev returned `Finished`, confidence 0.47, and the worker kept its current state. That request reported no foreground tools, one background command, and a final-looking assistant message that also said implementation had resumed.

The session became idle at 12:13:14 CDT when its normal ACP response completed with `EndTurn`. Jev did not make that transition. A post-reply classification at 12:13:15 returned `Finished`, confidence 0.48, and again applied no state change. The captured post-reply evidence reported zero background commands.

The experiment made no changes to the live session, live configuration, classifier questions, thresholds, or runtime code. The user explicitly authorized sending this captured evidence to TypeSafe for experiments.

## Method

- Replayed the first logged `running` and `replied` evidence objects to the same direct TypeSafe endpoint and request schema used by the worker.
- Requested `jev-latest`; every response identified `jev-1.13.0`.
- Ran one initial shape-check request, then 26 conditions with three repetitions each: 79 requests, zero request errors, 80,528 input tokens and 6,104 output tokens reported by the API.
- Shuffled requests within each of two batches and used concurrency two. Reported numbers are the API's `confidence` field, which is distinct from its per-choice probability. Production acts at confidence >=0.85.
- Kept the captured evidence and production questions unchanged except for each named intervention. Synthetic controls and counterfactual changes are explicitly identified in the case manifests.
- Artifacts and executable Python harnesses: `/mnt/optane/mj-jev-experiments-20260920/` (`captured-evidence.json`, `cases.json`, `results.jsonl`, `followup-cases.json`, `followup-results.jsonl`, `probe.py`, `experiment.py`, `followup.py`). These contain no API credentials; scripts resolve credentials at execution time and do not print them.

## Results

| Condition | Verdict in all three trials | Confidence range | Median |
| --- | --- | --- | --- |
| `running_baseline` | finished | 40%–44% | 43% |
| `replied_baseline` | finished | 52%–56% | 52% |
| `running_no_tool_history` | finished | 64%–70% | 65% |
| `replied_no_tool_history` | finished | 86%–88% | 88% |
| `replied_completion_fact` | finished | 36%–44% | 39% |
| `replied_history_explained` | finished | 53%–63% | 55% |
| `replied_no_history_completion` | finished | 92%–96% | 94% |
| `replied_turn_scope_question` | finished | 89%–91% | 91% |
| `running_turn_scope_question` | finished | 88%–89% | 88% |
| `replied_scope_and_completion` | finished | 89%–93% | 91% |
| `replied_without_resumed_clause` | finished | 59%–63% | 60% |
| `running_background_inventory` | background_work | 89%–91% | 90% |
| `control_finished` | finished | 100%–100% | 100% |
| `control_waiting_build` | background_work | 100%–100% | 100% |
| `control_needs_user` | user | 100%–100% | 100% |
| `replied_history_renamed` | finished | 54%–57% | 55% |
| `replied_structured_history` | finished | 65%–72% | 70% |
| `replied_clean_tool_summaries` | finished | 99%–99% | 99% |
| `running_clean_tool_summaries` | finished | 83%–85% | 84% |
| `replied_history_warning` | finished | 41%–50% | 46% |
| `running_history_warning` | background_work | 34%–41% | 39% |
| `replied_response_pending_task` | background_work | 85%–88% | 87% |
| `replied_response_user_question` | user | 99%–100% | 99% |
| `replied_response_waiting_text` | background_work | 100%–100% | 100% |
| `running_response_not_received` | background_work | 62%–65% | 65% |
| `running_scope_pending_task` | background_work | 89%–92% | 91% |

## Interpretation

1. **Keep tool history separate from current work, and remove command-body noise.** The current evidence takes the *tail* of each tool title. In this incident those titles contained shell/Python source, quoted planning-document text, and phrases such as “combined validation remains outstanding.” They were historical command arguments, not current progress reports. Omitting that history raised post-reply confidence to 86–88%; replacing it with manually written factual descriptions raised it to 99%. Renaming the field alone gave 54–57%, and wrapping the same raw text as historical gave 65–72%. A prose warning in the question was ineffective. The successful short summaries described inspections, instruction edits, commits, and Guardian Review; they did not rewrite the assistant's message.

2. **Supply explicit lifecycle facts alongside clean evidence.** With history omitted, adding `{received: true, stop_reason: "EndTurn"}` raised post-reply confidence to 92–96%. Adding a longer lifecycle caveat while preserving raw history instead produced 36–44%. These are separate interventions: the results do not establish that any completion field automatically improves confidence.

3. **Describe current background work by identity, status, and relationship to the turn.** The real running evidence supplied only a count of one. A counterfactual named Guardian Review task with `status: running` but no blocking relationship produced `background_work` at 62–65%; explicitly making it a foreground-completion blocker produced 89–91%. The blocking relationship was experimental, not a verified fact about the real review. Production should send that relationship only when actually known. Include task/result state, rather than inferring task completion from silence or from a final-looking sentence.

4. **Define turn scope, but check for false confidence.** Asking about the current requested action instead of the entire project raised the real post-reply case to 89–91%. However, it also classified the real still-running evidence as `finished` at 88–89%, despite its background count. Supplying a concrete pending-test task switched this wording to `background_work` at 89–92%. Scope wording alone is therefore not a sufficient fix.

5. **Preserve contrary evidence.** With clean history and an `EndTurn` fact, counterfactual unfinished tests still yielded `background_work` at 85–88%; explicit text waiting for an untracked subagent yielded 100%; a user approval request yielded `user` at 99–100%. Crisp positive controls reached 100%. These checks show that completion metadata need not force a finished answer, but they are not broad accuracy validation.

## Implementation implications

Prefer concise structured evidence: the current user request and assistant message; actual ACP response state and stop reason; foreground tool names/status/age; named background tasks with known status and blocking/continuation relationships; and short historical operation descriptions with an explicit historical role. Avoid passing shell bodies and quoted document contents as tool history. If reliable descriptions are unavailable, omitting historical command text while retaining live work evidence is a candidate worth testing on a broader corpus.

Do not lower the confidence threshold based on these experiments. Three repeats on one real incident test local repeatability, not calibration or general accuracy; the 99% result uses manual summaries and needs validation with a real extraction strategy and additional sessions.

Improving confidence alone would not have changed this incident's running badge. In `mj-core/src/activity/verdict.rs`, `decide(Running, Finished)` always returns `KeepCurrent`; only a confident `User` verdict changes a running turn to awaiting input. Finished/user verdicts can infer idle only in the `Replied` phase. The running-versus-completed lifecycle contract is separate from classifier confidence and should not be changed implicitly by an evidence-format improvement.

## Shared summary implementation replay

On 2026-09-20, replayed the actual shared Rust projection from canonical archive frontier 116381 (6139 items), retaining the latest eight tool calls with details and applying the production 48 KiB summary budget. The snapshot contained no items created or tool calls changed after the original Running capture, and its latest assistant message matched the captured evidence. This avoids introducing future evidence into the Running comparison. Lifecycle fields stayed as captured. Both direct production questions and frozen v1 baseline questions were used as appropriate.

Eighteen TypeSafe requests (six conditions, three repeats, concurrency two) all succeeded. Request bytes include the model and questions: old evidence 2936–2937 bytes; shared-summary evidence 53153–53154 bytes. Running Finished confidence was 0.38/0.40/0.44 for baseline and 0.53/0.60/0.68 with shared history. Replied Finished confidence was 0.47/0.50/0.51 versus 0.65/0.65/0.68. Counterfactual explicit user approval and pending-subagent work were classified as User and BackgroundWork respectively at 1.0 in all repeats.

The eight full recent calls preserve considerably more noise than the earlier names-only experiment, and neither real phase crossed the unchanged 0.85 threshold. Running/Finished would still preserve activity regardless of confidence under the unchanged decision policy. This change centralizes truthful evidence rather than forcing a confident decision.

Artifacts under `/mnt/optane/mj-jev-experiments-20260920`: `summary-input-snapshot.json`, `shared-summary.json`, `shared-summary-replay.py`, `shared-summary-cases.json`, and `shared-summary-results.jsonl`. The Rust `summary_probe` example used for the offline projection reads a canonical snapshot from stdin and prints JSON-encoded `TranscriptSummary::from_snapshot(&snapshot).render(48 * 1024)`. No live configuration, database, or session was modified. Follow-up for explicit model sub-turn grouping: https://github.com/BrokkAi/mjolnir/issues/1110.
