# Reproduce the Sol 6.2 expansion and Opus verification

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
C=$R/sol-62-repeats
S=results/sol-62-opus-high
```

The saved `jobs.json` uses the original 70 O jobs in their saved order.
For the zero-based diff index `i`, it emits four new jobs, using arm
`"QRST"[(i+n)%4]` for each `n` from 0 through 3. Q and S select Sol 6.2 low
and R and T select Sol 6.2 medium. `canaries.json` selects Q and R on
`diff-05bc8c1e68` from that manifest. `image.json` matches the original O
pin. The first run captured the live user prompt for each canary with
`mjx transcript --session "$child" --after-seq 0 --limit 20 --role user --json`
into `prompt-Q.json` and `prompt-R.json`.

```sh
python3 -B driver.py --jobs "$C/canaries.json" --concurrency 2 --same-image "$C/image.json" --stop-on-failure
python3 -B "$S/audit_reviewers.py" "$R" --canaries > "$C/canary-audit.json"
python3 -B driver.py --jobs "$C/jobs.json" --concurrency 16 --same-image "$C/image.json"
python3 -B "$S/audit_reviewers.py" "$R" --hash-file "$C/new-reviewers.sha256" > "$C/reviewer-audit.json"
```

The full driver skips completed jobs, including both canaries. The recovered
historical cohort is in the campaign's `preflight` directory with its frozen
job manifest, session manifest and checksums.

```sh
PREFLIGHT="$C/preflight"
(cd "$R" && sha256sum --quiet -c "$PREFLIGHT/historical-and-op.sha256" &&
  sha256sum --quiet -c "$PREFLIGHT/previous-rounds.sha256")
jq -s '.[0] + .[1]' "$PREFLIGHT/historical-and-op-ready-jobs.json" "$C/jobs.json" > "$C/ready-jobs.json"
python3 -B "$S/audit_round.py" "$R" ready
mkdir "$R/verify9"
cp "$R/verify5/SPEC.md" "$R/verify9/SPEC.md"
python3 -B build_verify.py verify9 --arms A,B,D,D2,E,G-base,G-plus,H,I,J,K,L,N,O,P,Q,R,S,T \
  --configs gpt-6.1-terra:high,gpt-6.1-terra:xhigh,gpt-6.1-sol:high,gpt-6.1-sol:medium,gpt-6.1-sol:xhigh,greptile:base,greptile:plus,deepseek-flash:low,muse-ember-1.0:xhigh,gemini-4-hydrogen:high,gemini-4-hydrogen:medium,kimi-code/k3.1-pro:high,gpt-6.2-sol:low,gpt-6.2-sol:medium \
  --ready-jobs "$C/ready-jobs.json" --bugs-from "$C/jobs.json"
python3 -B "$S/audit_round.py" "$R" pool "$PREFLIGHT/historical-and-op-sessions.jsonl"
```

The one-batch Opus verification uses batch 0 in the same round as the full
driver. Verify the actual provider and effort before launching the remaining
batches. The full driver reuses the attested batch 0 output.

`bash "$S/run_verification.sh"` runs the complete sequence after the reviewer
driver exits, including the pool, canary and full judge audits. It checks the
frozen source and blinded batch hashes before scoring. The commands here show
the individual steps managed by the script.

```sh
STORE=~/.local/share/mjolnir/mj.sqlite3
judge=(python3 -B verify_driver.py verify9 --kind verify --target localhost
  --repo-commit 3c3fa4b13bc32ba99fc349355081ee0d007831fe
  --model opus --effort high --profiles claude4 --quota-scope claude)
"${judge[@]}" --concurrency 1 --batch 0
python3 -B "$S/audit_round.py" "$R" judges "$STORE" --batch 0 > "$C/judge-canary-audit.json"
"${judge[@]}" --concurrency 16
python3 -B "$S/audit_round.py" "$R" judges "$STORE" > "$C/judge-audit.json"
(cd "$R" && sha256sum --quiet -c "$PREFLIGHT/historical-and-op.sha256" &&
  sha256sum --quiet -c "$PREFLIGHT/previous-rounds.sha256")
```

Score from the fresh Opus groups and the complete reviewer session key. Use
the historical sample caps, with three samples each for the two Sol 6.2
efforts. DSF/low remains in the judged pool. Its partial diff coverage limits
the primary 70-diff frontier to the other configurations.

```sh
python3 -B score.py verify9
caps=Sol/medium=2,Terra/xhigh=2,Sol/xhigh=2,Greptile/base=1,Greptile/plus=1,Ember/xhigh=1,Gemini/high=1,Gemini/medium=1,Kimi/high=1,gpt-6.2-sol/low=3,gpt-6.2-sol/medium=3
for sev in high medium low high,medium high,medium,low; do
  python3 -B pareto.py verify9 --exclude DSF/low --bootstrap 300 --sessions "$caps" --severities "$sev"
done
```

`bash "$S/run_analysis.sh"` runs the frontier and 20,000-resample paired
comparisons for the three-sample mixtures, with the results saved under
`verify9` and `sol-62-repeats` respectively.
