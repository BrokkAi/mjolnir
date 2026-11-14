"""Agreement between two independent verifications of the same findings.

Usage: python3 consistency.py <verify-dir-a> <verify-dir-b>

A finding is identified by (reviewer session, position in its handback), which
both keys record. For each finding verified in both, compares the verdict and
severity of the group it was placed in.
"""
import collections
import glob
import json
import os
import pathlib
import sys

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))


def load(name):
    raw = json.loads((ROOT / "private" / f"key-{name}.json").read_text())
    key = raw.get("findings", raw)
    out = {}
    for path in glob.glob(str(ROOT / name / "out/verify-out-*.json")):
        for c in json.load(open(path))["commits"]:
            for g in c["groups"]:
                for f in g["findings"]:
                    k = key[f]
                    out[(k["child"], k["index"])] = (g["verdict"], g.get("severity") or "none", k["bug"])
    return out


a, b = load(sys.argv[1]), load(sys.argv[2])
common = sorted(set(a) & set(b))
verdicts = collections.Counter((a[k][0], b[k][0]) for k in common)
severity = collections.Counter((a[k][1], b[k][1]) for k in common if a[k][0] != "rejected" and b[k][0] != "rejected")
agree = sum(n for (x, y), n in verdicts.items() if x == y)
binary = sum(n for (x, y), n in verdicts.items() if (x == "confirmed") == (y == "confirmed"))
print(json.dumps({
    "findings_in_both": len(common), "diffs": len({a[k][2] for k in common}),
    "verdict_agreement": round(agree / len(common), 3),
    "confirmed_vs_not_agreement": round(binary / len(common), 3),
    "verdict_pairs": {f"{x} -> {y}": n for (x, y), n in sorted(verdicts.items())},
    "severity_agreement_when_both_kept": round(sum(n for (x, y), n in severity.items() if x == y) / max(1, sum(severity.values())), 3),
    "severity_pairs": {f"{x} -> {y}": n for (x, y), n in sorted(severity.items())},
}, indent=1))
