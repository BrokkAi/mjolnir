"""Expected number of known Hard-won bugs each reviewer option catches.

Usage: python3 knownbug.py <verify-dir> <known-bug.jsonl> [--labels match,partial] [--groups-from <verify-dir>]

known-bug.jsonl maps each Hard-won diff to the verified group that is its
known defect (label match, partial or none). For each option of up to three
reviewers, each diff contributes the exact chance, over every assignment of
distinct sessions, that at least one session reported that group.

Group IDs belong to one verification. With --groups-from, known-bug.jsonl names
groups of that earlier verification; each is carried into <verify-dir> through
its findings, identified by (reviewer session, handback position), and becomes
the group there that holds most of them. A split can put the rest with another
problem (such as one the known-bug judge ruled out), so splits are printed.
"""
import argparse
import collections
import csv
import glob
import itertools
import json
import os
import pathlib
import statistics

LABELS = {"gpt-6.1-terra": "Terra", "gpt-6.1-sol": "Sol", "muse-ember-1.0": "Ember"}
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
p = argparse.ArgumentParser()
p.add_argument("verify")
p.add_argument("known")
p.add_argument("--labels", default="match")
p.add_argument("--groups-from", help="verification whose group IDs known-bug.jsonl names")
a = p.parse_args()
V = ROOT / a.verify
labels = set(a.labels.split(","))
known = [json.loads(l) for l in open(a.known)]
target = {k["bug"]: {k["group"]} for k in known if k["label"] in labels and k["group"]}


def finding_groups(name):
    """(reviewer session, handback position) -> group ID, named as score.py names groups."""
    key = json.loads((ROOT / "private" / f"key-{name}.json").read_text())["findings"]
    out = {}
    for path in glob.glob(str(ROOT / name / "out/verify-out-*.json")):
        for c in json.load(open(path))["commits"]:
            for g in c["groups"]:
                for f in g["findings"]:
                    out[key[f]["child"], key[f]["index"]] = f"{key[f]['bug']}/{g['group']}"
    return out


if a.groups_from:
    old, new = finding_groups(a.groups_from), finding_groups(a.verify)
    for bug, (group,) in list(target.items()):
        members = [f for f, g in old.items() if g == group]
        assert members and all(f in new for f in members), f"{group}: findings missing from {a.verify}"
        counts = collections.Counter(new[f] for f in members).most_common()
        assert len(counts) == 1 or counts[0][1] > counts[1][1], f"{group}: no majority in {counts}"
        target[bug] = {counts[0][0]}
        if len(counts) > 1:
            print(f"  {group} split {dict(counts)}; using {counts[0][0]}")
reported = collections.defaultdict(set)
for row in csv.DictReader(open(V / "session_groups.csv")):
    reported[row["group"]].add(row["session"])
by_diff = collections.defaultdict(lambda: collections.defaultdict(list))
for s in csv.DictReader(open(V / "sessions.csv")):
    # Keyed by model and effort, as in pareto.py, so efforts are never pooled.
    by_diff[s["bug"]][f"{LABELS.get(s['model'], s['model'])}/{s['effort']}"].append(s["session"])
models = sorted({m for d in by_diff.values() for m in d})
# A configuration repeats at most as often as every Hard-won diff has sessions of it.
cap = {m: min(len(by_diff[k["bug"]][m]) for k in known) for m in models}
options = [o for n in (1, 2, 3) for o in itertools.combinations_with_replacement(models, n)
           if all(n <= cap[m] for m, n in collections.Counter(o).items())]


def caught(bug, option):
    groups = target.get(bug)
    if groups is None:
        return 0.0
    need = collections.Counter(option)
    pools = [list(itertools.combinations(by_diff[bug][m], k)) for m, k in sorted(need.items())]
    return statistics.fmean(
        any(s in reported[g] for g in groups for part in parts for s in part) for parts in itertools.product(*pools))


print(f"Hard-won diffs: {len(known)}; with a {'/'.join(sorted(labels))} group: {len(target)}")
for o in options:
    total = sum(caught(k["bug"], o) for k in known)
    print(f"  {' + '.join(o):22} expected known bugs caught {total:5.2f} of {len(known)}")
