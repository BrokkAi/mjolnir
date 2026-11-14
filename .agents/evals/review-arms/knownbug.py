"""Expected number of known Hard-won bugs each reviewer option catches.

Usage: python3 knownbug.py <verify-dir> <known-bug.jsonl> [--labels match,partial]

known-bug.jsonl maps each Hard-won diff to the verified group that is its
known defect (label match, partial or none). For each option of up to three
reviewers, each diff contributes the exact chance, over every assignment of
distinct sessions, that at least one session reported that group.
"""
import argparse
import collections
import csv
import itertools
import json
import os
import pathlib
import statistics

LABELS = {"gpt-6.1-terra": "Terra", "gpt-6.1-sol": "Sol"}
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
p = argparse.ArgumentParser()
p.add_argument("verify")
p.add_argument("known")
p.add_argument("--labels", default="match")
a = p.parse_args()
V = ROOT / a.verify
labels = set(a.labels.split(","))
known = [json.loads(l) for l in open(a.known)]
target = {k["bug"]: k["group"] for k in known if k["label"] in labels and k["group"]}
reported = collections.defaultdict(set)
for row in csv.DictReader(open(V / "session_groups.csv")):
    reported[row["group"]].add(row["session"])
by_diff = collections.defaultdict(lambda: collections.defaultdict(list))
for s in csv.DictReader(open(V / "sessions.csv")):
    # Keyed by model and effort, as in pareto.py, so efforts are never pooled.
    by_diff[s["bug"]][f"{LABELS.get(s['model'], s['model'])}/{s['effort']}"].append(s["session"])
models = sorted({m for d in by_diff.values() for m in d})
options = [o for n in (1, 2, 3) for o in itertools.combinations_with_replacement(models, n)]


def caught(bug, option):
    group = target.get(bug)
    if group is None:
        return 0.0
    need = collections.Counter(option)
    pools = [list(itertools.combinations(by_diff[bug][m], k)) for m, k in sorted(need.items())]
    return statistics.fmean(
        any(s in reported[group] for part in parts for s in part) for parts in itertools.product(*pools))


print(f"Hard-won diffs: {len(known)}; with a {'/'.join(sorted(labels))} group: {len(target)}")
for o in options:
    total = sum(caught(k["bug"], o) for k in known)
    print(f"  {' + '.join(o):22} expected known bugs caught {total:5.2f} of {len(known)}")
