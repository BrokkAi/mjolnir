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
  python3 -B "$here/verify_driver.py" "$verify" --concurrency 7 || echo "verify_driver reported failures; see the log above"
  [ "$reviewing" = no ] && break
  sleep 300
done
echo "pipeline finished"
