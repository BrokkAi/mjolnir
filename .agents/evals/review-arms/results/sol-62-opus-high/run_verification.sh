#!/usr/bin/env bash
# Run after the reviewer driver exits. A failed gate leaves its evidence for diagnosis.
# The ready audit creates the combined manifest or validates the existing copy.
set -euo pipefail
cd "$(dirname "$0")/../.."

export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113 REVIEW_ARMS_INSTANCE=default
export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/candidates-all.jsonl"
export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
unset REVIEW_ARMS_MJ REVIEW_ARMS_MJ_WORKER REVIEW_ARMS_SCRIPTS REVIEW_ARMS_REPO
unset REVIEW_ARMS_BUNDLE REVIEW_ARMS_REPO_DIR
R=$REVIEW_ARMS_DIR
C="$R/sol-62-repeats"
S=results/sol-62-opus-high
PREFLIGHT="$C/preflight"
STORE=~/.local/share/mjolnir/mj.sqlite3

judge=(python3 -B verify_driver.py verify9 --kind verify --target localhost
  --repo-commit 3c3fa4b13bc32ba99fc349355081ee0d007831fe
  --model opus --effort high --profiles claude4 --quota-scope claude)

check_input_hashes() {
  (cd "$R" && sha256sum --quiet -c "$PREFLIGHT/historical-and-op.sha256" &&
    sha256sum --quiet -c "$PREFLIGHT/previous-rounds.sha256" &&
    sha256sum --quiet -c "$C/new-reviewers.sha256")
}

python3 -B "$S/audit_round.py" "$R" ready | tee "$C/ready-audit.json"
python3 -B "$S/audit_reviewers.py" "$R" --hash-file "$C/new-reviewers.sha256" > "$C/reviewer-audit.json"
check_input_hashes
mkdir "$R/verify9"
cp "$R/verify5/SPEC.md" "$R/verify9/SPEC.md"
python3 -B build_verify.py verify9 --arms A,B,D,D2,E,G-base,G-plus,H,I,J,K,L,N,O,P,Q,R,S,T \
  --configs gpt-6.1-terra:high,gpt-6.1-terra:xhigh,gpt-6.1-sol:high,gpt-6.1-sol:medium,gpt-6.1-sol:xhigh,greptile:base,greptile:plus,deepseek-flash:low,muse-ember-1.0:xhigh,gemini-4-hydrogen:high,gemini-4-hydrogen:medium,kimi-code/k3.1-pro:high,gpt-6.2-sol:low,gpt-6.2-sol:medium \
  --ready-jobs "$C/ready-jobs.json" --bugs-from "$C/jobs.json" | tee "$C/build-verify9.log"
python3 -B "$S/audit_round.py" "$R" pool "$PREFLIGHT/historical-and-op-sessions.jsonl" | tee "$C/pool-audit.json"
(cd "$R" && sha256sum verify9/SPEC.md verify9/batch-*.json private/key-verify9.json) > "$C/verify9-pool.sha256"

"${judge[@]}" --concurrency 1 --batch 0 | tee "$C/judge-canary.log"
python3 -B "$S/audit_round.py" "$R" judges "$STORE" --batch 0 > "$C/judge-canary-audit.json"
(cd "$R" && sha256sum --quiet -c "$C/verify9-pool.sha256")

"${judge[@]}" --concurrency 16 | tee "$C/judges.log"
python3 -B "$S/audit_round.py" "$R" judges "$STORE" > "$C/judge-audit.json"
(cd "$R" && sha256sum --quiet -c "$C/verify9-pool.sha256")
check_input_hashes
python3 -B score.py verify9 | tee "$C/score.json"
