# Authorized-continuation evaluation, 2026-09-20

## Evidence and provenance

At the user's request, Luna scanned 1,713 local Codex session JSONL files, approximately 46,000 user/assistant messages after filtering injected context and subagent/reviewer traffic. Private excerpts, source identifiers, and later user reactions remain in `/tmp/mj-continuation-mining/report.md`. The checked-in twenty scenarios are sanitized derivatives. They are not verbatim replay payloads, and historical runtime facts are not reconstructed.

The strongest positive preserves an earlier explicit multi-step instruction across a later status question. No positive depended solely on developer/AGENTS instructions. Many apparent handoffs were genuine plan-mode gates, explicit pauses, informational replies, external blockers, or already-complete tasks. One sequencing example is ambiguous. A deployment example lacked enough surrounding context to establish the intended action. Six records are excluded from primary model scoring for ambiguity or runtime gating; fourteen remain (one positive, thirteen negatives).

Automatic approval review initially rejected transmitting these derivatives. The user then explicitly approved evaluating the exact twenty sanitized scenarios. No private originals, source paths, session identifiers, or subsequent user replies were sent. The evaluation runner only reads named fixture files; it does not discover or access session storage.

## Development and results

Eight synthetic development controls tested direct redundant permission, authorization three/five user messages back, requested commits, revocation, genuine choices, quoted instructions, and a later request to stop. Three question formulations were evaluated on these controls at the initial 0.95 threshold (24 requests per formulation). The first formulation's authorization scores were too low even for obvious positives. Explicit true/false criteria improved them; a third handoff phrasing did not improve coverage. The second formulation was frozen before evaluating the Codex-derived scenarios (60 requests). Every response identified `jev-1.13.0`.

The final cutoff is 0.90 for BOTH independent scores. This was selected from development-control scores; it is not a claim that the values are calibrated probabilities. The initial 0.95 cutoff abstained on the useful controls. The final questions retain concrete blockers, revoked instructions, and missing-context exclusions. The worker separately enforces lifecycle/plan/goal/input guards and the durable three-nudge maximum. The fixed prompt supplies no approval.

Saved responses evaluated at the final cutoff:

| Evidence window | Synthetic positives continued | Synthetic false positives | Scored mined positives continued | Scored mined false positives |
| --- | --- | --- | --- | --- |
| Latest user exchange | 2/4 | 0/4 | 0/1 | 0/13 |
| Latest 3 user exchanges | 3/4 | 0/4 | 0/1 | 0/13 |
| All bounded user history | 4/4 | 0/4 | 0/1 | 0/13 |

The mined positive scored unfinished=0.37 and no_input_needed=0.25 with the latest exchange alone, versus 0.94 and 0.85 with earlier authorization present. The production gate still abstains. This is a remaining false negative, not a reason to lower the cutoff until the case passes. None of the six excluded cases would trigger at the final cutoff either, but they are not counted as accuracy evidence.

The dataset is small, correlated, and curated; the controls were used for prompt/cutoff development. No held-out or calibrated safety claim is warranted. The useful demonstrated difference is evidence coverage: three exchanges help, while a hard three-message cutoff loses older explicit obligations. Production retains all real user messages since a durable context boundary within 32 KiB, plus whole recent assistant replies within 16 KiB. Exceeding the user budget causes abstention; omitted assistant history is explicit.

## Reproduction

Run `scripts/jev-continuation-eval.py` with the checked-in fixture files in `services/jev-proxy/tests/fixtures/`. Local validation makes no HTTP requests. `--run --output <path>` evaluates explicitly supplied sanitized fixtures; `--report <saved-json>` scores saved responses locally. Exact questions, payloads, responses, timings, and initial decisions are retained under `/mnt/optane/mj-continuation-evaluation/`: `controls-initial.json`, `controls-refined.json`, `controls-handoff.json`, and `mined-frozen.json`. Use the current-cutoff report for the table above; the saved initial decisions used 0.95.
