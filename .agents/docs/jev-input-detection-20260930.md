# Jev input detection: real decisions and contrasting nonrequests

This experiment implements `.agents/plans/jev-user-decision-detection.md`. Luna mined local session logs; the main agent reviewed the labels against each reply and preceding instructions. The selected input question treats unresolved user decisions as needing input even while independent work continues, and distinguishes them from optional offers after a delivered answer. Only the input question changed; the 0.85 attention and 0.90 automation thresholds stayed fixed.

## Questions and provenance

The motivating 06:50 CDT assessment in session `2ddbd4ceaa5916f6c58138d97e62d8c1` did fire. Its installed 2.23.3 worker chose required input at 0.41 and authorized unfinished work at 0.75, yielding `assessment_uncertain -> KeepCurrent`; one background command kept the TUI busy. The full decision handoff was in the evidence. The exact older three-axis prompt is [incident-questions.json](jev-input-detection-20260930/incident-questions.json); its recorded 0.41 is not a replay of the current four-axis baseline.

Preserved prompts: [baseline](jev-input-detection-20260930/baseline-questions.json), [A](jev-input-detection-20260930/candidate-a-questions.json), [B](jev-input-detection-20260930/candidate-b-questions.json), and [selected C](jev-input-detection-20260930/candidate-c-questions.json). C defines a current unresolved user request independently of remaining work. Declarative decisions, approval, missing facts, review, and external actions count. It establishes requested scope chronologically: an implementation offer after an advice-only answer is optional, while a choice governing an already requested result still needs input. Previously given permission remains redundant. None of the prompts matches the incident's literal sentence.

## Corpus and comparison method

The 17 added fixtures span 11 sessions: seven required-input examples, seven nonrequests, and three ambiguous cases. They preserve captured evidence and source provenance. Runtime quietness remains unknown when the process snapshot was unavailable. D04, D10, and D13 have `context.strict_input_scoring=false`; their ambiguous scope/recipient labels are excluded from strict input scoring. D17 was removed before evaluation because it duplicated existing S35 and contradicted its label. No labels changed after requests began.

The [frozen split](jev-input-detection-20260930/split.json) groups ten tuning cases and seven held-out cases by source session, with no session overlap. Adjacent replies stay together. The existing 77 cases are a separate regression set; some of its sessions overlap new cases, so it adds coverage rather than 77 independent sessions. Every comparison repeats each case three times. Repeats measure variability, not independent samples or calibrated probabilities.

The evaluator freezes questions, labeled fixtures, effective evidence hashes, and run manifests before requests. Resume rejects changed prompts or evidence; offline reports use the snapshots. Input detection is scored independently from work/failure: required at confidence at least 0.85 counts as detected, and required at that confidence on a control is a false alert. Uncertain requests count as misses. Action expectations and explicitly harmful actions are also scored.

## Authorization and experiments

The user explicitly authorized sending credential-scanned fixture evidence, including user instructions, assistant replies, and repository/session context, to `api.typesafe.ai`. All 94 fixtures passed the extractor's credential scan before submission. Requests used the existing key without printing it; no live daemon, worker, or store was changed.

The initial A/B runs each returned 30 HTTP 422 errors because their input questions omitted `type: "choice"`. Those failures remain preserved as `candidate-a-tuning` and `candidate-b-tuning`. The corrected prompts ran in fresh `candidate-a-tuning-valid` and `candidate-b-tuning-valid` directories. An offline validator now rejects malformed prompts before reading credentials or sending evidence.

A and B each detected all nine required tuning requests but falsely alerted on all three repeats of D02, an optional implementation offer after a completed advice answer. C was revised using tuning cases only and detected 9/9 with 0/15 false alerts. [selection.json](jev-input-detection-20260930/selection.json) freezes C and its digest before held-out or regression results were inspected. C was not revised afterward.

## Measured result

At the unchanged 0.85 input threshold:

| set | required requests | baseline detected | C detected | controls | baseline false alerts | C false alerts |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| tuning | 9 | 3 | 9 | 15 | 0 | 0 |
| held-out | 12 | 3 | 6 | 6 | 0 | 0 |
| existing suite | 42 | 9 | 16 | 189 | 0 | 0 |

The incident D01 scored required at 0.67, 0.73, and 0.74 under the current baseline, and 0.88, 0.89, and 0.88 under C: all three C replies yield `await_input` despite the background benchmark. Held-out gains came from D15's missing promised commit SHA (0.93–0.95); D18's missing login-status fact remained detected (0.99). Held-out misses remain: D06's approval following a denied operation scored 0.53–0.60 and D11's requested CI cancellation scored 0.82–0.84. D16 was incorrectly classified required at 0.69–0.77, below the alert threshold. D12's input-none answer was confident, but work-waiting remained uncertain. Unscored D10 gets a confident input alert while D13 stays uncertain; their recipient ambiguity still needs better evidence.

Existing-suite gains include N04 signoff, S02, S35 sample review, and one R07 repeat. Existing misses remain, and N03 regressed from two detected repeats to zero. N01's input answer shifted to confidently none (0.86–0.91) despite the frozen required label, though low work confidence kept its action uncertain. Redundant-permission choice agreement fell from 9/15 to 0/15; neither prompt detected those controls as required above threshold or continued them automatically. S16 became uncertain in all three repeats instead of finished, and S17 in two; S21 lost one wait action. These are material abstention regressions, not hidden successes.

Expected actions across the 231 existing requests rose from 106 to 109. Both prompts took the same three explicitly harmful actions: A03 was finished despite its continuing program goal. C introduced no additional listed harmful action, but did not fix that existing failure. Overall strict input gains and zero confident control alerts satisfy this experiment's adoption criteria; the remaining misses and uncertainty regressions limit the claim. This is a measured partial improvement, not a comprehensive fix for busy sessions.

There were 624 successful requests across comparisons and 60 schema-error requests retained separately. [comparisons.json](jev-input-detection-20260930/comparisons.json) records exact counts and prompt/fixture digests. Raw answers, reports, manifests, and evidence snapshots remain under `/mnt/optane/mj-jev-input-20260930/`, outside Git. Completed successful runs: `baseline-tuning`, `candidate-a-tuning-valid`, `candidate-b-tuning-valid`, `candidate-c-tuning`, `baseline-held-out`, `candidate-c-held-out`, `baseline-existing`, and `candidate-c-existing`.

## Adoption and validation

C's input question is copied into `mj-core/src/activity/verdict_questions.json`, shared by direct clients and the v6 proxy. Failure, work, background, evidence, policy, and thresholds are unchanged. Installed workers and the hosted proxy have not been upgraded or published; the change takes effect through their ordinary rebuild/release/deployment paths.

Eleven offline evaluator tests pass, including rejection of malformed prompts before credential access. All three focused Rust fixture tests and the three proxy test files pass; TypeScript checking also passes. The proxy deployment dry run passes without publication. Its sandboxed attempt built successfully but could not write Wrangler logs; the elevated rerun completed cleanly. Validation uses the normal dev-profile Cargo/mbx configuration; no daemon or default-instance data is exercised.
