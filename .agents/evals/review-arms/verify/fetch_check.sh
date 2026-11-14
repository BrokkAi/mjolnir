#!/bin/bash
# Fetches one verifier's verify-out.json and checks it covers its batch exactly once.
V=/mnt/optane/review-arms-20261113/verify; g=$1
s=$(python3 -c "import json;print(json.load(open('$V/session-v$g.json'))['session_id'])")
mj fetch --session $s --path verify-out.json --stdout > $V/out/verify-out-v$g.json 2>$V/out/fetch-v$g.err || { cat $V/out/fetch-v$g.err; exit 1; }
python3 - $V/out/verify-out-v$g.json $V/batch-v$g.json <<'EOF'
import json, sys, collections
out = json.load(open(sys.argv[1])); batch = json.load(open(sys.argv[2]))
ids = {f["id"] for c in batch for f in c["findings"]}
got = collections.Counter(f for c in out["commits"] for g in c["groups"] for f in g["findings"])
print(sys.argv[1].split("/")[-1], "groups", sum(len(c["groups"]) for c in out["commits"]),
      "verdicts", dict(collections.Counter(g["verdict"] for c in out["commits"] for g in c["groups"])),
      "missing", len(ids - set(got)), "extra", len(set(got) - ids), "dup", sum(n > 1 for n in got.values()))
EOF
