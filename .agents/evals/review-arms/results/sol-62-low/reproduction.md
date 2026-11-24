# Sol 6.2 low evaluation commands

Run from `.agents/evals/review-arms` with the installed Mjolnir CLI and the
existing default-instance study workspaces.

```sh
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
export REVIEW_ARMS_INSTANCE=default
export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/candidates-all.jsonl"
export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
unset REVIEW_ARMS_MJ REVIEW_ARMS_MJ_WORKER REVIEW_ARMS_SCRIPTS REVIEW_ARMS_REPO
unset REVIEW_ARMS_BUNDLE REVIEW_ARMS_REPO_DIR
R=$REVIEW_ARMS_DIR
jq 'map(.arm = "P")' "$R/arm-O-jobs.json" > "$R/arm-P-jobs.json"
jq '[.[] | select(.bug == "diff-05bc8c1e68")]' "$R/arm-P-jobs.json" > "$R/arm-P-canary.json"
jq -s 'add' "$R/stage7-ready-jobs.json" "$R/arm-P-jobs.json" > "$R/stage8-ready-jobs.json"
cp "$R/arm-O-image.json" "$R/arm-P-image.json"
python3 -B driver.py --jobs "$R/arm-P-canary.json" --concurrency 1 --same-image "$R/arm-P-image.json"
python3 -B driver.py --jobs "$R/arm-P-jobs.json" --concurrency 16 --same-image "$R/arm-P-image.json"
python3 -B "$R/arm-P-audit.py" "$R"
mkdir -p "$R/verify8"
cp "$R/verify7/SPEC.md" "$R/verify8/SPEC.md"
python3 -B build_verify.py verify8 --arms A,B,E,O,P \
  --configs gpt-6.1-terra:high,gpt-6.1-sol:medium,gpt-6.2-sol:medium,gpt-6.2-sol:low \
  --ready-jobs "$R/stage8-ready-jobs.json" --bugs-from "$R/arm-P-jobs.json"
python3 -B "$R/verify8/audit.py" "$R" pooled
python3 -B verify_driver.py verify8 --kind verify --target localhost \
  --model gemini-4-neon --effort medium --profiles gemini,gemini2 --quota-scope gemini --concurrency 16
python3 -B "$R/verify8/audit.py" "$R" judged
python3 -B "$R/verify8/attest-judges.py" "$R"
python3 -B score.py verify8
python3 -B "$R/verify8/count-severities.py" "$R"
python3 -B consistency.py verify7 verify8
for sev in low medium high high,medium high,medium,low; do
  for base in Sol/medium gpt-6.2-sol/medium; do
    python3 -B compare.py verify8 "$base" gpt-6.2-sol/low --severities "$sev"
    python3 -B compare.py verify8 "$base+Terra/high+Terra/high" \
      gpt-6.2-sol/low+Terra/high+Terra/high Terra/high+Terra/high --severities "$sev"
  done
done
python3 -B pareto.py verify8 --sessions Sol/medium=2,gpt-6.2-sol/medium=1,gpt-6.2-sol/low=1
(cd "$R" && sha256sum --quiet -c verify8/previous-rounds.sha256)
```

The second reviewer invocation skips the audited canary. The verifier uses
the saved checkout at `3c3fa4b13` and the same specification as round
`verify7`. The commands after verification read the saved judge outputs and
do not launch further sessions. The durable reviewer and verifier logs are
in the raw campaign directory.
