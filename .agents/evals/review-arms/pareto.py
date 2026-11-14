"""Latency vs. verified findings Pareto frontier for reviewer combinations of up to 3.

Usage: python3 pareto.py <verify-dir> [--samples 4000] [--bootstrap 1000]

Reads <verify-dir>/sessions.csv, groups.csv and session_groups.csv (score.py)
and writes <verify-dir>/pareto.csv and pareto.md. The method follows the
earlier speed/coverage study (session b8556873, /tmp/review-eval/pareto):

- Options are all multisets of 1 to 3 reviewer configurations (model and
  effort). A repeated configuration uses distinct sessions of that diff.
- Findings: confirmed distinct problems in the union of an option's
  sessions, summed over diffs. Each diff contributes the exact mean over every
  assignment of distinct sessions.
- Latency: reviewers run in parallel, so an option's latency on a diff is
  its slowest member's. The reported value is the median over diffs, averaged
  over random session assignments. A session without a clock is never imputed;
  assignments that need it are skipped for latency only.
- Only diffs with enough sessions of every configuration for every option are
  used, so all options are compared on the same diffs.
- An option is on the frontier when no other option is at least as fast and
  finds at least as many, with one strict improvement.
- Bootstrap over diffs: how often each option is on the frontier.

Timings come from separate runs; they assume simultaneous starts and ignore
contention between members, and exclude session startup.
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
p.add_argument("--samples", type=int, default=4000)
p.add_argument("--bootstrap", type=int, default=1000)
p.add_argument("--verdicts", default="confirmed", help="comma-separated verdicts that count as findings")
p.add_argument("--severities", default="high,medium,low",
               help="comma-separated severities of counted groups (rejected groups have none)")
p.add_argument("--min-sessions", type=int, default=3,
               help="sessions each configuration needs per diff; also the most repeats of one configuration")
a = p.parse_args()
V = ROOT / a.verify
counted = set(a.verdicts.split(","))
severities = set(a.severities.split(","))

sessions = list(csv.DictReader(open(V / "sessions.csv")))
groups = {g["group"]: g for g in csv.DictReader(open(V / "groups.csv"))}
found = collections.defaultdict(set)
for row in csv.DictReader(open(V / "session_groups.csv")):
    g = groups[row["group"]]
    if g["verdict"] in counted and g["severity"] in severities:
        found[row["session"]].add(row["group"])


def config(s):
    return f"{LABELS.get(s['model'], s['model'])}/{s['effort']}"


by_diff = collections.defaultdict(lambda: collections.defaultdict(list))
info = {}
for s in sessions:
    by_diff[s["bug"]][config(s)].append(s["session"])
    info[s["session"]] = {"latency": float(s["latency_s"]) if s["latency_s"] else None,
                          "tokens": int(s["total_tokens"]),
                          "cost": float(s["cost_usd"]) if s.get("cost_usd") else None}
configs = sorted({config(s) for s in sessions})
options = [o for n in (1, 2, 3) for o in itertools.combinations_with_replacement(configs, n)
           if max(collections.Counter(o).values()) <= a.min_sessions]
diffs = sorted(d for d, per in by_diff.items() if all(len(per[c]) >= a.min_sessions for c in configs))
skipped = sorted(set(by_diff) - set(diffs))


def assignments(diff, option):
    """Every choice of distinct sessions for the option on this diff."""
    need = collections.Counter(option)
    pools = [list(itertools.combinations(by_diff[diff][c], k)) for c, k in sorted(need.items())]
    for parts in itertools.product(*pools):
        yield [s for part in parts for s in part]


# Exact per-diff means, and the assignment lists for latency sampling.
per = {}
for o in options:
    for d in diffs:
        rows = []
        for members in assignments(d, o):
            hit = set().union(*(found[s] for s in members))
            lats = [info[s]["latency"] for s in members]
            rows.append((len(hit), max(lats) if None not in lats else None,
                         sum(info[s]["tokens"] for s in members),
                         # An unpriced member leaves the assignment unpriced, never free.
                         None if any(info[s]["cost"] is None for s in members)
                         else sum(info[s]["cost"] for s in members)))
        per[o, d] = rows


def summarize(o, ds, rng, samples):
    findings = sum(statistics.fmean(r[0] for r in per[o, d]) for d in ds)
    medians = []
    for _ in range(samples):
        lats = [x for x in (rng.choice(per[o, d])[1] for d in ds) if x is not None]
        if lats:
            medians.append(statistics.median(lats))
    tokens = statistics.fmean(statistics.fmean(r[2] for r in per[o, d]) for d in ds)
    priced = [[r[3] for r in per[o, d] if r[3] is not None] for d in ds]
    cost = statistics.fmean(statistics.fmean(c) for c in priced) if all(priced) else None
    return {"findings": findings, "latency": statistics.fmean(medians) if medians else None,
            "tokens_per_diff": tokens, "cost_per_diff": cost}


def frontier(stats):
    on = set()
    for o, s in stats.items():
        dominated = any(t["latency"] <= s["latency"] and t["findings"] >= s["findings"]
                        and (t["latency"] < s["latency"] or t["findings"] > s["findings"])
                        for q, t in stats.items() if q != o)
        if not dominated:
            on.add(o)
    return on


rng = random.Random(20261114)
stats = {o: summarize(o, diffs, rng, a.samples) for o in options}
main_frontier = frontier(stats)
# Options with unpriced sessions on any diff are left out of the cost frontier.
cost_stats = {o: dict(s, latency=s["cost_per_diff"]) for o, s in stats.items() if s["cost_per_diff"] is not None}
cost_frontier = frontier(cost_stats)
hits = collections.Counter()
for _ in range(a.bootstrap):
    ds = [rng.choice(diffs) for _ in diffs]
    hits.update(frontier({o: summarize(o, ds, rng, 20) for o in options}))


def label(o):
    return " + ".join(f"{n}× {c}" if n > 1 else c for c, n in sorted(collections.Counter(o).items()))


rows = sorted(({"option": label(o), "reviewers": len(o), "findings": round(s["findings"], 2),
                "findings_per_diff": round(s["findings"] / len(diffs), 3),
                "median_latency_s": round(s["latency"], 1), "tokens_per_diff": round(s["tokens_per_diff"]),
                "cost_per_diff_usd": None if s["cost_per_diff"] is None else round(s["cost_per_diff"], 3),
                "frontier": o in main_frontier,
                "cost_frontier": o in cost_frontier, "bootstrap_frontier_share": round(hits[o] / a.bootstrap, 3)}
               for o, s in stats.items()), key=lambda r: r["median_latency_s"])
suffix = ("" if counted == {"confirmed"} else "-" + "-".join(sorted(counted))) + \
         ("" if severities == {"high", "medium", "low"} else "-sev-" + "-".join(sorted(severities)))
# Each verdict or severity variant gets its own files.
with open(V / f"pareto{suffix}.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=list(rows[0]))
    w.writeheader()
    w.writerows(rows)
lines = [f"Verdicts counted: {', '.join(sorted(counted))}; severities: {', '.join(sorted(severities))}. "
         f"Diffs: {len(diffs)} (skipped for missing sessions: {len(skipped)}).", "",
         "| Reviewers | Median latency (s) | Findings (total) | Per diff | Est. cost per diff ($) | Frontier | Cost frontier | Bootstrap frontier share |",
         "|---|---:|---:|---:|---:|:---:|:---:|---:|"]
for r in rows:
    lines.append(f"| {r['option']} | {r['median_latency_s']} | {r['findings']} | {r['findings_per_diff']} | "
                 f"{r['cost_per_diff_usd']} | {'yes' if r['frontier'] else ''} | {'yes' if r['cost_frontier'] else ''} | "
                 f"{r['bootstrap_frontier_share']} |")
(V / f"pareto{suffix}.md").write_text("\n".join(lines) + "\n")
print("\n".join(lines))
