"""Paired comparison of reviewer options over diffs, with a bootstrap interval.

Usage: python3 compare.py <verify-dir> "<option>" "<option>" [...] [--severities high,medium]
An option is configurations joined by "+", e.g. "Terra+Terra+Terra" or
"Terra/high+Sol/high". A bare model name stands for its only effort.

Each diff contributes the exact expected number of counted groups found by the
option, averaged over every assignment of distinct sessions. Reports the
paired difference of each option against the first, its bootstrap 95%
interval over diffs, and per-diff wins, losses and ties.
"""
import argparse
import collections
import csv
import itertools
import os
import pathlib
import random
import statistics

LABELS = {"gpt-6.1-terra": "Terra", "gpt-6.1-sol": "Sol", "muse-ember-1.0": "Ember"}
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
p = argparse.ArgumentParser()
p.add_argument("verify")
p.add_argument("options", nargs="+")
p.add_argument("--verdicts", default="confirmed")
p.add_argument("--severities", default="high,medium,low")
p.add_argument("--bootstrap", type=int, default=20000)
a = p.parse_args()
V = ROOT / a.verify
verdicts, severities = set(a.verdicts.split(",")), set(a.severities.split(","))
groups = {g["group"]: g for g in csv.DictReader(open(V / "groups.csv"))}
found = collections.defaultdict(set)
for row in csv.DictReader(open(V / "session_groups.csv")):
    g = groups[row["group"]]
    if g["verdict"] in verdicts and g["severity"] in severities:
        found[row["session"]].add(row["group"])
by_diff = collections.defaultdict(lambda: collections.defaultdict(list))
for s in csv.DictReader(open(V / "sessions.csv")):
    by_diff[s["bug"]][f"{LABELS.get(s['model'], s['model'])}/{s['effort']}"].append(s["session"])
configs = {c for d in by_diff.values() for c in d}


def config_key(token, configs):
    """Resolves "Terra" or "Terra/high" to a configuration; a bare model name is
    accepted only while that model ran at a single effort."""
    if "/" in token:
        assert token in configs, f"no sessions for {token}; have {sorted(configs)}"
        return token
    matches = sorted(c for c in configs if c.split("/")[0] == token)
    assert len(matches) == 1, f"{token} is ambiguous or unknown; name one of {matches or sorted(configs)}"
    return matches[0]


def expected(diff, option):
    need = collections.Counter(config_key(t, configs) for t in option.split("+"))
    pools = [list(itertools.combinations(by_diff[diff][m], k)) for m, k in sorted(need.items())]
    counts = [len(set().union(*(found[s] for part in parts for s in part))) for parts in itertools.product(*pools)]
    return statistics.fmean(counts) if counts else None


diffs = sorted(d for d in by_diff if all(expected(d, o) is not None for o in a.options))
values = {o: [expected(d, o) for d in diffs] for o in a.options}
rng = random.Random(20261114)
base = a.options[0]
print(f"diffs {len(diffs)}; verdicts {sorted(verdicts)}; severities {sorted(severities)}")
for o in a.options:
    print(f"  {o:28} total {sum(values[o]):7.2f}")
for o in a.options[1:]:
    d = [x - y for x, y in zip(values[o], values[base])]
    boot = sorted(sum(rng.choice(d) for _ in d) for _ in range(a.bootstrap))
    lo, hi = boot[int(0.025 * a.bootstrap)], boot[int(0.975 * a.bootstrap)]
    print(f"  {o} - {base}: {sum(d):+.2f} (95% {lo:+.2f} to {hi:+.2f}); "
          f"diffs better {sum(x > 1e-9 for x in d)}, worse {sum(x < -1e-9 for x in d)}, tied {sum(abs(x) <= 1e-9 for x in d)}")
