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
    # A resumed scan appends rows: a repo is fine once any row for it succeeded.
    undatable, date_ok, date_err = {}, set(), {}
    for l in open(scan / "dates.jsonl"):
        x = json.loads(l)
        if x.get("error"):
            date_err[x["repo"]] = x["error"]
            continue
        date_ok.add(x["repo"])
        undatable.update({f"{x['repo']} {h}": e for h, e in (x.get("undatable") or {}).items()})
    failed = sorted(set(date_err) - date_ok)
    if failed:
        sys.exit(f"dates unavailable for {failed[:5]}: {date_err[failed[0]]}")
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
    present = set()
    for l in open(scan / "blame.jsonl"):
        r = json.loads(l)
        present.add(f"{r['repo']} {r['hash']}")
    missing = current - present
    if missing:
        sys.exit(f"{len(missing)} planned draws have no trace record; finish the scan before ranking")
    sample = collections.defaultdict(list)
    errors = 0
    for l in open(scan / "blame.jsonl"):
        r = json.loads(l)
        if f"{r['repo']} {r['hash']}" not in current:
            continue
        errors += bool(r.get("error"))
        sample[(r["repo"], r["window"])].append(r)

    # Clones that share history (forks, renames, subtrees) are one repository: group repos whose
    # candidate fix commits or root commits overlap, and keep the member with the most candidates.
    by_hash = collections.defaultdict(set)
    for c in cands:
        by_hash[c["hash"]].add(c["repo"])
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x
    root_ok, root_err = set(), {}
    for l in open(scan / "roots.jsonl"):
        x = json.loads(l)
        if x.get("error"):
            root_err[x["repo"]] = x["error"]
            continue
        root_ok.add(x["repo"])
        for root in x["roots"]:
            by_hash["root:" + root].add(x["repo"])
    failed = sorted(set(root_err) - root_ok)
    if failed:
        sys.exit(f"root commits unavailable for {failed[:5]}: {root_err[failed[0]]}")
    for repos in by_hash.values():
        repos = sorted(repos)
        for other in repos[1:]:
            parent[find(other)] = find(repos[0])
    size = collections.Counter(c["repo"] for c in cands)
    groups = collections.defaultdict(list)
    for repo in size:
        groups[find(repo)].append(repo)
    keep = {max(members, key=lambda r: (size[r], r)) for members in groups.values()}
    shared = {max(m, key=lambda r: (size[r], r)): sorted(m) for m in groups.values() if len(m) > 1}

    out = {"undated_candidates": undated, "undatable": undatable, "shared_history_groups": shared, "blame_errors": errors,
           "incomplete_traces": sum(1 for rs in sample.values() for r in rs if r.get("trace_complete") is False),
           "bug_precision": BUG_PRECISION, "admission": ADMISSION, "windows": {}}
    for label, (start, wins) in WINDOWS.items():
        per = collections.Counter()
        for (repo, w), count in n.items():
            if repo not in keep or w not in wins or not sample.get((repo, w)):
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
