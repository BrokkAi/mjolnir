#!/bin/bash
# Verifies diffs as their reviews finish: batch every ready diff, run the
# verifiers (finished batches are skipped, started ones resumed), repeat until
# the review driver has exited, then make one final pass.
# Usage: REVIEW_ARMS_DIR=... pipeline.sh <jobs.json> <verify-dir>
set -u
here=$(cd "$(dirname "$0")" && pwd)
jobs=$1; verify=$2
while :; do
  reviewing=$(pgrep -f "driver.py --jobs $jobs" >/dev/null && echo yes || echo no)
  python3 -B "$here/build_verify.py" "$verify" --append --ready-jobs "$jobs" || exit 1
  failed=no
  python3 -B "$here/verify_driver.py" "$verify" --concurrency 7 || failed=yes
  if [ "$reviewing" = no ]; then
    # The last pass decides the result: unverified batches would drop diffs from scoring.
    [ "$failed" = yes ] && { echo "pipeline incomplete: verification failed on the final pass; see the log above"; exit 1; }
    # A review job that failed or was interrupted never became ready, so its diff
    # was never batched; name it rather than report a complete evaluation.
    unfinished=$(python3 -c '
import json, os, pathlib, sys
root = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
print(" ".join("%s/%s" % (j["bug"], j["arm"]) for j in json.load(open(sys.argv[1]))
               if not (root / "runs" / j["bug"] / j["arm"] / "done.json").exists()))' "$jobs")
    [ -n "$unfinished" ] && { echo "pipeline incomplete: review jobs without done.json: $unfinished"; exit 1; }
    break
  fi
  [ "$failed" = yes ] && echo "verify_driver reported failures; retrying on the next pass"
  sleep 300
done
echo "pipeline finished"
