# Sol 6.2 medium evaluation commands

Run from `.agents/evals/review-arms` using the installed Mjolnir CLI and the
existing study workspace records. The journaled logs and PID files are in
`REVIEW_ARMS_DIR`.

```sh
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
export REVIEW_ARMS_INSTANCE=default
export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/candidates-all.jsonl"
export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
unset REVIEW_ARMS_MJ REVIEW_ARMS_MJ_WORKER REVIEW_ARMS_SCRIPTS REVIEW_ARMS_REPO
unset REVIEW_ARMS_BUNDLE REVIEW_ARMS_REPO_DIR
R=$REVIEW_ARMS_DIR
jq 'map(.arm = "O")' "$R/arm-N-jobs.json" > "$R/arm-O-jobs.json"
jq '[.[] | select(.bug == "diff-05bc8c1e68")]' "$R/arm-O-jobs.json" > "$R/arm-O-canary.json"
jq -s '([.[0][] | select(.arm == "A" or .arm == "B" or .arm == "E")] + .[1])' \
  "$R/stage6-ready-jobs.json" "$R/arm-O-jobs.json" > "$R/stage7-ready-jobs.json"
python3 -B driver.py --jobs "$R/arm-O-canary.json" --concurrency 1 --same-image "$R/arm-O-image.json"
python3 -B driver.py --jobs "$R/arm-O-jobs.json" --concurrency 16 --same-image "$R/arm-O-image.json"
python3 -B "$R/arm-O-audit.py" "$R"
mkdir -p "$R/verify7"
cp "$R/verify6/SPEC.md" "$R/verify7/SPEC.md"
python3 -B build_verify.py verify7 --arms A,B,E,O \
  --configs gpt-6.1-terra:high,gpt-6.1-sol:medium,gpt-6.2-sol:medium \
  --ready-jobs "$R/stage7-ready-jobs.json" --bugs-from "$R/arm-O-jobs.json"
python3 -B "$R/verify7/audit.py" "$R" pooled
python3 -B verify_driver.py verify7 --kind verify --target localhost \
  --model gemini-4-neon --effort medium --profiles gemini,gemini2 --quota-scope gemini --concurrency 16
python3 -B "$R/verify7/audit.py" "$R" judged
python3 -B score.py verify7
python3 -B consistency.py verify6 verify7
for sev in high,medium,low high,medium; do
  python3 -B compare.py verify7 Sol/medium gpt-6.2-sol/medium --severities "$sev"
  python3 -B compare.py verify7 Sol/medium+Terra/high+Terra/high \
    gpt-6.2-sol/medium+Terra/high+Terra/high Terra/high+Terra/high --severities "$sev"
  python3 -B pareto.py verify7 --sessions Sol/medium=2,gpt-6.2-sol/medium=1 --severities "$sev"
done
```

The second driver invocation skips the audited canary. Check all 70 reviews
before preparing the blinded inputs. The verifier run uses the saved checkout
at `3c3fa4b13` and a byte-for-byte copy of the verify6 specification.

## Campaign recovery

- The first canary pass on the installed 2.74.0 CLI captured the accepted
  review and then failed on the obsolete suspend flag. The same canary command
  resumed collection after the flag removal. The four raw finding bullets were
  all retained. Logs are `arm-O-canary.log` and
  `arm-O-canary-recovery.log`.
- The original concurrency-16 driver loaded the version of `driver.py` at
  `38edc6ec1`, whose creation-and-startup semaphore had two permits. It stayed
  running through completion. Commit `9c4d616e0` removed this extra gate for
  future invocations. Eleven reviewer turns overlapped at the recorded peak.
- The one failed carrier never reached review start. Mjolnir resumed a Create
  interrupted by daemon handoff, then refused an event-chain digest mismatch.
  The failed session had no live target and ordinary suspend dry-run returned
  HTTP 409. Its original artifacts are in
  `runs/diff-660c78e96c/O-failed-bootstrap-dddc797142ac/`.
  The tracking issue is [Mjolnir #1675](https://github.com/BrokkAi/mjolnir/issues/1675).
  After the first driver exited the single affected job was retried as follows.

```sh
jq '[.[] | select(.bug == "diff-660c78e96c")]' "$R/arm-O-jobs.json" > "$R/arm-O-retry-bootstrap-jobs.json"
python3 -B driver.py --jobs "$R/arm-O-retry-bootstrap-jobs.json" --concurrency 1 \
  --same-image "$R/arm-O-image.json"
```

The first verifier invocation rejected all 34 workspace lookups before creating
any judge session. The saved verifier workspace no longer existed on the default
instance. Under the existing setup lock, the obsolete
`default-instance/verify/workspace.json` was archived as
`verify7/workspace-stale.json`. Repeating the same verifier command created the
named workspace with the existing helper and submitted the same 34 batches.
Logs are `verify7/driver.log` for the refused setup and
`verify7/retry-workspace.log` for the accepted setup.

The additional judge provenance check joins each accepted output to the
stored selection for its stable prompt command ID. It reads the installed
instance's SQLite store in read-only mode.

```sh
python3 -B "$R/verify7/attest-judges.py" "$R"
(cd "$R" && sha256sum --quiet -c verify7/verify6-before.sha256)
```

`verify7/score.sh` runs the strict result audit, scoring, checks for matched
70-diff coverage and 431 sessions, recorded usage summary, consistency,
comparison, and Pareto commands. The commands work on the existing raw data
and do not relaunch any sessions.
