# Jev input detection: real decisions and contrasting nonrequests

This experiment implements `.agents/plans/jev-user-decision-detection.md`. Luna mined real local session logs, and the main agent reviewed labels against the current reply and preceding instructions. The experiment asks whether a current unanswered request for a user decision can be recognized independently of ongoing background work, without turning optional offers or reports about other agents' decisions into input alerts.

## Questions and provenance

The exact question bundle used by the incident's installed 2.23.3 worker is [incident-questions.json](jev-input-detection-20260930/incident-questions.json). It has three axes and differs from the current four-axis baseline. The recorded required-input confidence was 0.41. Do not describe that number as a live result of the current baseline.

The current baseline and two candidates are preserved in [baseline-questions.json](jev-input-detection-20260930/baseline-questions.json), [candidate-a-questions.json](jev-input-detection-20260930/candidate-a-questions.json), and [candidate-b-questions.json](jev-input-detection-20260930/candidate-b-questions.json). The candidates change only the input question. Both treat explicit, unresolved decisions directed at the user as requests even without a question mark and while other work continues. They distinguish prior authorization, answered or rhetorical questions, optional additional help, autonomous decisions, and requests to somebody else. A is more explanatory; B is shorter. Neither includes the incident's literal sentence.

## Comparison method

Additions to `mj-core/tests/jev-scenarios/` retain captured evidence and source session, decision ID, timestamp, recorded verdict, and independent expected labels. Runtime quietness is left unknown when the full process snapshot was not recorded. Ambiguous labels have `context.strict_input_scoring=false` and do not count as strict input successes or failures.

The [frozen split](jev-input-detection-20260930/split.json) has ten tuning cases and seven held-out cases, grouped by complete source session with no overlapping sessions. Adjacent replies stay together. Select a candidate using tuning cases, then freeze it before comparing against the baseline on held-out cases and the existing 77-case suite. Run each comparison three times per case. The evaluator's frozen prompts, labeled fixtures, and evidence hashes prevent resuming an experiment with changed inputs. Report required-input detection at 0.85, missed requests, confident false alerts on controls, and harmful action changes separately from whole-verdict agreement. Keep thresholds fixed. Some regression sessions overlap new cases; report the regression set separately rather than counting them as additional independent sessions.

Luna supplied 18 cases. Review removed D17 because it duplicated existing S35 and initially contradicted that fixture's label. The final 17 cases span 11 sessions: seven required-input examples, seven nonrequests, and three ambiguous cases (D04, D10, D13). Labels were reviewed and frozen before any live calls. The new examples include a user-owned decision while a benchmark continues, a choice of ticket contents, a quiet execution window, an approval after a denied operation, a requested external CI action, a promised missing commit SHA, and a missing login-status fact. Controls include answered advice with optional follow-up, completed requested edits with a prohibited rerun, autonomous build/test waits, and completed handbacks. Existing redundant-permission cases remain in the regression set.

## Current result and authorization

No live prompt comparison has run. Automatic approval review rejected sending conversation evidence to `api.typesafe.ai` without explicit disclosure permission. An approval request is pending for credential-scanned user instructions and assistant replies from the existing and newly mined fixtures, including repository/session context. Local preparation and validation continue; the runtime question bundle is adopted only after the measured acceptance criteria pass.

Ten offline evaluator tests and all three Rust scenario tests pass on the final 94-case corpus. Every new fixture passed the extractor's credential scanner, and an extraction smoke check recovered the real incident using the fixed Mjolnir worker-directory default. Raw comparison output belongs under `/mnt/optane/mj-jev-input-20260930/`, outside Git. Baseline and tuning experiment directories already contain reviewable frozen questions, fixtures, and run manifests, with no live results.

To resume after disclosure consent, run from the repository root:

    python3 scripts/jev-scenarios-eval.py --only D01,D02,D03,D04,D05,D07,D08,D09,D10,D14 --questions .agents/docs/jev-input-detection-20260930/baseline-questions.json --repeats 3 --output /mnt/optane/mj-jev-input-20260930/baseline-tuning

Repeat with `candidate-a-questions.json` and `candidate-b-questions.json`, using the prepared `candidate-a-tuning` and `candidate-b-tuning` output directories. Select using tuning results before sending either candidate's held-out requests. Held-out IDs are `D06,D11,D12,D13,D15,D16,D18`; baseline output is `baseline-held-out`, and the selected candidate gets a fresh corresponding output directory. The regression IDs are explicit in `split.json`; use them with `baseline-existing` and a fresh selected-candidate output directory. Optionally replay D01 alone using `incident-questions.json` to distinguish the recorded historical behavior from current baseline behavior.
