# Five further Jev input-prompt iterations

Five iterations are complete. Round 4 is adopted locally: required-input detections improve from 29/54 to 36/54, with zero false alerts among 216 development control replays and 21 fresh-session control replays. Correct finished detections improve from 22/69 to 27/69 on development cases. The change has not been deployed to running workers or the hosted proxy.

## Method and label corrections

All 94 prior fixtures are development/regression data, including the previously inspected holdout. Each candidate runs three repetitions per case. Only the input question changes; policy thresholds and the failure/work/background questions remain fixed. Luna mined eight fresh source sessions for a final holdout. [selection.json](jev-input-five-20260930/selection.json) freezes round 4 before any fresh-session API results.

The user explicitly authorized sending credential-scanned fixture evidence and context to api.typesafe.ai. All 94 fixtures passed the credential scan. Full prompt/evidence manifests and raw answers live under `/mnt/optane/mj-jev-input-five-20260930/`.

During round 2 the user requested correcting incorrect labels. The [label review](jev-input-five-20260930/label-review.json) records evidence-based reasons and before/after expectations. N01, N02, and N06 now count as answered advice with optional implementation rather than required input. S19 now counts as required: its otherwise complete report explicitly assigns the campaign-scope decision to the user. S09 is ambiguous/unscored: its captured evidence omits the planning-mode approval boundary visible only in later context. Evidence itself is unchanged. Human-readable fixture titles were subsequently updated to match the review, without changing labels or evidence.

Original baseline/round-1/round-2 snapshots remain intact. Their `*-reviewed` directories copy the identical raw answers and verified-identical evidence under the corrected labels; `rescore.json` records that derivation. These are rescored responses, not new API samples. Labels were frozen before round 3. Original historical results in the earlier experiment remain historical and should not be compared across changed denominators.

## Five development comparisons

The reviewed corpus has 18 required cases, 72 controls, and four ambiguous cases. Three repeats yield 54 required requests and 216 controls. Repeats measure variability, not independent observations.

| Prompt | Required detected / 54 | False input / 216 | Expected finished / 69 | All expected actions / 282 | Explicitly wrong actions |
| --- | ---: | ---: | ---: | ---: | ---: |
| Current baseline C | 29 | 0 | 22 | 135 | 3 |
| Round 1 | 44 | 18 | 27 | 151 | 22 |
| Round 2 | 27 | 0 | 28 | 138 | 4 |
| Round 3 | 32 | 6 | 25 | 138 | 12 |
| Round 4, selected | 36 | 0 | 27 | 143 | 3 |
| Round 5 | 46 | 3 | 24 | 143 | 6 |

Round 1 reads the final reply first and explicitly names approval gates and external actions. It improves recall and completed handbacks, but overcalls previously authorized permission requests and optional offers. Round 2 preserves the baseline's chronological scope rules and adds exceptions; it restores several finished handbacks and catches some new approval/external-action cases, but loses existing required detections and produces an additional harmful continue on S09. Round 3 tries shorter contrasting rules but falsely alerts on optional cleanup and already authorized work.

Round 4 makes a smaller baseline edit: distinguish new approval gates after denial or deferral, make external actions independent of other completed work, and clarify completed handbacks. It is the only candidate that improves required detections without additional false input alerts or listed harmful actions, and it improves finished detections. Round 5 adds an explicit project-direction/acceptance/scope decision rule: recall rises, but optional followup D07 falsely alerts on all three repeats and the S16 finished recovery is lost. The higher raw recall therefore does not win selection.

All [five prompts and iteration notes](jev-input-five-20260930/iterations.json), [development IDs](jev-input-five-20260930/development.json), and [compact comparisons](jev-input-five-20260930/comparisons.json) are preserved. The execution plan is `.agents/plans/jev-five-input-iterations.md`.

## Selected candidate limits

Round 4 gains all three D11 external-action detections and all three R07 merge-approval detections, plus one each on D03 scope selection and N05 fresh approval. It restores S16's three finished actions and S17's remaining two. D01, the original incident, regresses from three detections to two (0.85, 0.83, 0.85); this is an explicit tradeoff, not an improvement on that specific case. P13 loses three continue actions to uncertainty, and one S43 response changes from expected uncertain to wait. All other development action changes are accounted for by those cases.

D06's denied-operation approval request, N03's deferred experiment, N07/S19's campaign decisions, and R06's shared-resource repair remain below threshold. N05 is detected only once. Baseline and selection both incorrectly finish A03 in all three repeats despite remaining program work. These are unresolved limitations of the full classifier; no thresholds or non-input questions were changed to hide them.

## Fresh-session evaluation and adoption

Luna delivered E01–E08 from eight sessions absent from the development corpus. The cases cover artifact delivery, optional issue/implementation/investigation offers, authorized continuation, and completed work. Root reviewed them before any API results and marked E06 ambiguous because the user combines an informational question with a preference that could imply an implementation request. Seven are strict no-input controls; one is unscored. All lack complete runtime snapshots, so quietness is unknown. All eight were credential scanned.

The [holdout manifest](jev-input-five-20260930/holdout.json) was frozen before requests. Both baseline and selection produced zero false input alerts and zero listed harmful actions across 21 scored control replays. Expected finished actions increased from 4/18 to 5/18; total expected actions, including the ambiguous case's uncertainty, increased from 7/24 to 8/24. Most remaining failures are uncertainty, often on the work axis.

No defensible fresh required-input examples were delivered within this mining pass. This holdout therefore tests false alerts only and cannot validate recall generalization. The measured recall improvement comes from development cases used during iteration. No prompt or labels were changed after the holdout evaluation. The [adoption record](jev-input-five-20260930/adoption.json) records the decision and limitation.

## Billing interruption and evaluator correction

The initial round-3 run returned HTTP 402 on all 282 requests. A follow-up diagnostic confirmed `billing_error`, no available TypeSafe API credits. The user replenished credits; the identical prompt and fixtures were retried in `round-3-restored`. Both runs remain preserved. The baseline plus five successful iterations contain 1,692 model responses; the failed run and diagnostic contain 283 billing errors, excluded from accuracy comparisons and reported separately.

The outage exposed eager submission of the whole corpus. The evaluator now keeps at most two requests admitted. HTTP 401 or 402 stops admission, drains admitted calls, preserves responses, writes an explicit incomplete-run marker, and exits unsuccessfully. A stopped run must be retried in a new directory. Tests prove it submits at most two of 60 requested jobs after an account error, and completes all repetitions when requests succeed.

## Validation and delivery

All 13 offline evaluator tests pass, including account-error handling and complete successful repetitions. All three dev-profile Rust fixture tests pass across all 102 fixtures and the selected bundle. Label and evaluator corrections were committed as `4ff1ec84`. The three proxy test files and TypeScript checking pass. All 102 fixtures pass credential scans; the runtime bundle equals the frozen selected prompt, and non-input questions are unchanged. No daemon or live store was exercised. There were 1,740 successful API responses including the holdout, plus the 283 separately preserved billing errors. Only the local shared input question was adopted; no deployment or push was performed.
