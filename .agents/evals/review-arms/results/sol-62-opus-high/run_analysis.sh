#!/usr/bin/env bash
# The verification script writes score.json only after all acceptance gates pass.
set -euo pipefail
cd "$(dirname "$0")/../.."
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-20261113
R=$REVIEW_ARMS_DIR
C="$R/sol-62-repeats"
test -s "$C/score.json"
test -s "$C/judge-audit.json"
jq -e '.sessions == 1726 and .diffs == 70 and (.unverified_diffs_left_out | length == 0)' "$C/score.json"

caps=Sol/medium=2,Terra/xhigh=2,Sol/xhigh=2,Greptile/base=1,Greptile/plus=1,Ember/xhigh=1,Gemini/high=1,Gemini/medium=1,Kimi/high=1,gpt-6.2-sol/low=3,gpt-6.2-sol/medium=3
l=gpt-6.2-sol/low
m=gpt-6.2-sol/medium
t=Terra/high
for sev in high medium low high,medium high,medium,low; do
  python3 -B pareto.py verify9 --exclude DSF/low --bootstrap 300 --sessions "$caps" --severities "$sev"
  compare=(python3 -B compare.py verify9 --bootstrap 20000 --severities "$sev")
  {
    "${compare[@]}" "$l" "$m" "$t" "$l+$l" "$l+$m" "$l+$t" "$m+$m" "$m+$t" "$t+$t" \
      "$l+$l+$l" "$l+$l+$m" "$l+$l+$t" "$l+$m+$m" "$l+$m+$t" "$l+$t+$t" \
      "$m+$m+$m" "$m+$m+$t" "$m+$t+$t" "$t+$t+$t"
    "${compare[@]}" "$l+$l" "$l+$m" "$m+$m" "$t+$t" "$l+$l+$l"
    "${compare[@]}" "$m" "$m+$m" "$m+$m+$m"
    "${compare[@]}" "$m+$m" "$m+$m+$m"
    "${compare[@]}" "$l+$l+$l" "$l+$l+$m" "$l+$m+$m" "$m+$m+$m" "$t+$t+$t"
    "${compare[@]}" "$t+$t" "$l+$t+$t" "$m+$t+$t" "$t+$t+$t"
    "${compare[@]}" "$l+$t+$t" "$m+$t+$t" "$t+$t+$t"
    "${compare[@]}" Sol/medium "$l" "$m"
    "${compare[@]}" "Sol/medium+$t+$t" "$l+$t+$t" "$m+$t+$t"
  } | tee "$C/paired-${sev//,/-}.txt"
done
