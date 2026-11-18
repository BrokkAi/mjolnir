#!/usr/bin/env python3
"""Turn scan.py's dates and capped blame samples into per-repo yield estimates.

Usage: yield_rank.py CANDIDATES.jsonl SCAN_DIR > yield.json

For each (repo, fix window) cell: expected strong multi-site candidates =
(candidates in the cell) x (share of the cell's blamed sample that is strong, multi-site,
completely traced, and introduced on or after the window start). Only the commits in the
current plan's sample count, and a failed draw stays in the denominator as unconfirmed. Cells
with no blamed sample contribute nothing. Realistic yield multiplies by BUG_PRECISION (bug-word filter,
eyeballed on 30 onelines, 2026-11-18) and ADMISSION (bifrost-dev SPEC admission after
attribution, 31/67), both stated in the plan.
"""
import collections
import datetime as dt
import json
import pathlib
import sys

BUG_PRECISION = 0.85
ADMISSION = 31 / 67
WINDOWS = {  # label: (intro and fix on or after, fix windows counted)
    "wide_2025": (dt.datetime(2025, 1, 1, tzinfo=dt.timezone.utc),
                  ["2025H1", "2025H2", "2026H1", "post2026-06"]),
    "ext_2024": (dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc),
                 ["2024", "2025H1", "2025H2", "2026H1", "post2026-06"]),
    "mid_2026": (dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc), ["2026H1", "post2026-06"]),
    "strict_2026_07": (dt.datetime(2026, 7, 1, tzinfo=dt.timezone.utc), ["post2026-06"]),
}


def window(ts):
    d = dt.datetime.fromtimestamp(ts, dt.timezone.utc)
    if d.year < 2024:
        return "pre2024"
    if d.year == 2024:
        return "2024"
    if d.year == 2025:
        return "2025H1" if d.month <= 6 else "2025H2"
    if d.year == 2026 and d.month <= 6:
        return "2026H1"
    return "post2026-06"


def main():
    cands = [json.loads(l) for l in open(sys.argv[1])]
    scan = pathlib.Path(sys.argv[2])
    dates = {}
    for l in open(scan / "dates.jsonl"):
        r = json.loads(l)
        for h, t in (r.get("dates") or {}).items():
            dates[(r["repo"], h)] = t
    n = collections.Counter()
    undated = 0
    for c in cands:
        t = dates.get((c["repo"], c["hash"]))
        if t is None:
            undated += 1
            continue
        n[(c["repo"], window(t))] += 1
    # Only the current plan's sampled commits count; failed draws stay in the denominator.
    current = set(json.load(open(scan / "plan.json"))["sample_ids"])
    sample = collections.defaultdict(list)
    errors = 0
    for l in open(scan / "blame.jsonl"):
        r = json.loads(l)
        if f"{r['repo']} {r['hash']}" not in current:
            continue
        errors += bool(r.get("error"))
        sample[(r["repo"], r["window"])].append(r)

    out = {"undated_candidates": undated, "blame_errors": errors,
           "incomplete_traces": sum(1 for rs in sample.values() for r in rs if r.get("trace_complete") is False),
           "bug_precision": BUG_PRECISION, "admission": ADMISSION, "windows": {}}
    for label, (start, wins) in WINDOWS.items():
        per = collections.Counter()
        for (repo, w), count in n.items():
            if w not in wins or not sample.get((repo, w)):
                continue
            s = sample[(repo, w)]
            good = sum(1 for r in s if not r.get("error") and r.get("strong") and r.get("multi_site_proxy")
                       and r.get("trace_complete") is True
                       and r.get("intro_ct") and r["intro_ct"] >= start.timestamp())
            per[repo] += count * good / len(s)
        realistic = {r: v * BUG_PRECISION * ADMISSION for r, v in per.items()}
        out["windows"][label] = {
            "upper_bound_total": round(sum(per.values()), 1),
            "realistic_total": round(sum(realistic.values()), 1),
            "repos_realistic_ge": {k: sum(v >= k for v in realistic.values()) for k in (5, 10, 15, 25)},
            "per_repo_realistic": dict(sorted(((r, round(v, 1)) for r, v in realistic.items()),
                                              key=lambda x: -x[1])),
        }
    json.dump(out, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
