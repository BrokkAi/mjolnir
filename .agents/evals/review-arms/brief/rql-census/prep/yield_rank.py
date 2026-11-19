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
TRACE_TARGET = 25  # the plan's per-repo tracing stop, in admitted relation diffs
# ADMISSION covers all admitted bifrost-dev bugs; only 17 of its 30 admitted bugs were relation
# diffs. The multi-site proxy already leans toward relation bugs, so applying 17/30 again is
# conservative, but the cap is in relation diffs and the joint rate on proxy-positive fixes is
# unmeasured. Selection therefore caps a conservative relation estimate.
RELATION_SHARE = 17 / 30
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
        if x.get("error") is not None:
            date_err[x["repo"]] = x["error"] or "empty error"
            continue
        date_ok.add(x["repo"])
        undatable.update({f"{x['repo']} {h}": e for h, e in (x.get("undatable") or {}).items()})
    failed = sorted(set(date_err) - date_ok)
    if failed:
        sys.exit(f"dates unavailable for {failed[:5]}: {date_err[failed[0]]}")
    n = collections.Counter()
    undated = 0
    unaccounted = []
    for c in cands:
        t = dates.get((c["repo"], c["hash"]))
        if t is None:
            undated += 1
            if f"{c['repo']} {c['hash']}" not in undatable:
                unaccounted.append(f"{c['repo']} {c['hash']}")
            continue
        n[(c["repo"], window(t))] += 1
    if unaccounted:
        # A failed retry after a partial success would otherwise shrink the repo's candidate pool
        # silently: every candidate must be dated or recorded as undatable.
        sys.exit(f"{len(unaccounted)} candidates are neither dated nor recorded undatable, e.g. {unaccounted[:3]}")
    # Only the current plan's sampled commits count; failed draws stay in the denominator.
    plan = json.load(open(scan / "plan.json"))
    # The sample plan must come from the same candidate population the ranker extrapolates to.
    import hashlib
    population = hashlib.sha256("\n".join(sorted(f"{c['repo']} {c['hash']}" for c in cands)).encode()).hexdigest()
    if plan.get("candidates_sha256") != population:
        sys.exit("plan.json was drawn from a different candidate population; rerun scan.py on this file")
    # The sample must also have been drawn from the same dated cells the ranker scales by.
    sizes = {f"{repo} {w}": k for (repo, w), k in n.items()}
    if plan.get("cell_sizes") != sizes:
        sys.exit("plan.json was drawn before the current dates; rerun scan.py to redraw the sample")
    current = set(plan["sample_ids"])
    present = set()
    for l in open(scan / "blame.jsonl"):
        r = json.loads(l)
        present.add(f"{r['repo']} {r['hash']}")
    missing = current - present
    if missing:
        sys.exit(f"{len(missing)} planned draws have no trace record; finish the scan before ranking")
    latest = {}  # a resumed scan appends retries: the latest row per draw counts
    for l in open(scan / "blame.jsonl"):
        r = json.loads(l)
        if f"{r['repo']} {r['hash']}" in current:
            latest[(r["repo"], r["hash"])] = r
    sample = collections.defaultdict(list)
    errors = 0
    for r in latest.values():
        errors += bool(r.get("error"))
        sample[(r["repo"], r["window"])].append(r)

    # Clones that share history (forks, renames, subtrees) are one repository: group repos whose
    # candidate fix commits or root commits overlap. All members stay ranked; selection takes
    # at most one per group.
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
        if x.get("error") is not None:
            root_err[x["repo"]] = x["error"] or "empty error"
            continue
        root_ok.add(x["repo"])
        for root in x["roots"]:
            by_hash["root:" + root].add(x["repo"])
    failed = sorted(set(root_err) - root_ok)
    if failed:
        sys.exit(f"root commits unavailable for {failed[:5]}: {root_err[failed[0]]}")
    # Every candidate repo needs its roots: a missing row would let two forks with disjoint
    # sampled fixes rank as independent repositories.
    missing_roots = sorted({c["repo"] for c in cands} - root_ok)
    if missing_roots:
        sys.exit(f"no root-commit record for {len(missing_roots)} repos, e.g. {missing_roots[:3]}; rerun scan.py")
    for repos in by_hash.values():
        repos = sorted(repos)
        for other in repos[1:]:
            parent[find(other)] = find(repos[0])
    size = collections.Counter(c["repo"] for c in cands)
    groups = collections.defaultdict(list)
    for repo in size:
        groups[find(repo)].append(repo)
    group_of = {repo: tuple(sorted(members)) for members in groups.values() for repo in members}
    shared = [sorted(m) for m in groups.values() if len(m) > 1]

    out = {"undated_candidates": undated, "undatable": undatable, "shared_history_groups": shared,
           "group_of": {r: list(g) for r, g in group_of.items() if len(g) > 1}, "blame_errors": errors,
           "incomplete_traces": sum(1 for rs in sample.values() for r in rs if r.get("trace_complete") is False),
           "bug_precision": BUG_PRECISION, "admission": ADMISSION, "windows": {}}
    for label, (start, wins) in WINDOWS.items():
        per = collections.Counter()
        seen = collections.defaultdict(set)  # repo -> introductions already counted in this window
        # The counting unit is the introducing change. Cells are visited oldest first, and an
        # introduction counts only in the first cell whose sample shows it, so one introduced
        # behind fixes in several half-years counts once. Within a cell this is a ratio
        # estimate (count x new distinct / sampled), which can still overstate distinct
        # introductions when repeat fixes cluster; tracing counts exactly.
        for (repo, w) in sorted((k for k in n if k[1] in wins), key=lambda k: (k[0], wins.index(k[1]))):
            s = sample.get((repo, w))
            if not s:
                continue
            new = {r["intro_sha"] for r in s if not r.get("error") and r.get("strong")
                   and r.get("multi_site_proxy") and r.get("trace_complete") is True
                   and r.get("intro_ct") and r["intro_ct"] >= start.timestamp()} - seen[repo]
            seen[repo] |= new
            per[repo] += n[(repo, w)] * len(new) / len(s)
        # Every clone keeps its own estimate, so a related clone stays eligible when the yield
        # leader turns out costly to index. Cohort selection takes at most one repo per
        # shared-history group; the counts below count each group once, at its best member.
        realistic = {r: v * BUG_PRECISION * ADMISSION for r, v in per.items()}
        best = {}
        for repo, v in realistic.items():
            g = group_of.get(repo, (repo,))
            best[g] = max(best.get(g, 0), v)
        out["windows"][label] = {
            "upper_bound_total": round(sum(per.values()), 1),
            "realistic_total": round(sum(realistic.values()), 1),
            "repos_realistic_ge": {k: sum(v >= k for v in best.values()) for k in (5, 10, 15, 25)},
            "realistic_total_one_per_group": round(sum(best.values()), 1),
            "per_repo_realistic": dict(sorted(((r, round(v, 1)) for r, v in realistic.items()),
                                              key=lambda x: -x[1])),
            # realistic is an all-bug proxy. Tracing stops at TRACE_TARGET admitted relation diffs,
            # so selection ranks on a conservative relation estimate, capped at the stop (bugs per
            # indexing hour uses this figure).
            "per_repo_relation_estimate_capped": dict(sorted(
                ((r, round(min(v * RELATION_SHARE, TRACE_TARGET), 1)) for r, v in realistic.items()),
                key=lambda x: -x[1])),
        }
    json.dump(out, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
