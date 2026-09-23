# Continuation evaluation fixtures

`continuation-controls.json` contains eight synthetic development controls. Four are redundant handoffs; four require abstention. Earlier authorization appears three and five user messages back in two controls.

`continuation-mined.json` contains twenty sanitized scenarios derived from read-only local Codex-session mining. Source files, session identifiers, exact private excerpts, and subsequent user replies are retained only in the local mining report. Later user replies inform curation but never enter the evaluated evidence. The user explicitly authorized sending these sanitized scenarios to TypeSafe.

One mined record is a positive, one is ambiguous, and eighteen are negative candidates. Six records are excluded from primary model scoring: ambiguous/incomplete context or runtime admission gates. They remain useful for manual review and runtime tests. The scored set has one positive and thirteen negatives. These are derived scenarios, not verbatim real-session replays or a calibrated accuracy benchmark.

Run `python3 scripts/jev-continuation-eval.py <fixture-file>` from the repository root for local validation. `--run --output <artifact.json>` explicitly invokes TypeSafe using the existing local credential; do not run it on private source transcripts. The runner compares the latest one, three, and all user exchanges with fixed shared questions. `--report <artifact.json>` recomputes decisions locally with the current cutoff, without making requests. Model evaluation is opt-in and is not part of `npm test` or Cargo tests.
