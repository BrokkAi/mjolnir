#!/usr/bin/env python3
"""Rank measured repos for the test cohort by expected relation diffs.

Usage: select_cohort.py INDEX_COST.jsonl YIELD.json index-pins.txt > cohort.md

Rule (PLAN-rql-census.md, Data, revised 2026-11-19): rank by B, the repo's capped
2025+ relation estimate (yield.json, wide_2025.per_repo_relation_estimate_capped).
The index measurement only excludes: a repo whose cold index or warm restart did not
reach ready (over the cap, or a Bifrost failure), or whose current record was taken at
another revision or cap, can't be selected. Indexing CPU-hours,
cold + (B - 1) x warm from the scope cgroup's cpu_to_ready_usec, are reported and break
ties. Walking down the ranking, at most one repo per shared-history group is
provisionally selected; every member stays ranked. A repo enters the test set only
after tracing admits at least 8 relation diffs, so tracing walks this list and a repo
that misses the floor gives its place to the next.

One ranking uses one measurement provenance: every record that ran Bifrost must share
the binary and the driver's environment key, or the script refuses. Until every pinned
repo has a record, it prints the table and exits non-zero.
"""
import json
import pathlib
import sys

from measure_index import latest_records  # the producer's definition of a repo's current record

COHORT = 12
MEMORY_MAX = "48G"  # the plan's measurement cap (PLAN-rql-census.md, Data)


def main():
    log = pathlib.Path(sys.argv[1])
    if not log.exists():  # missing evidence is an error, not an empty cohort
        sys.exit(f"{log} does not exist")
    measured = latest_records(log)
    pins = {e[0]: e[1] for e in (l.split() for l in open(sys.argv[3])) if e}
    provenance = {(r.get("binary_sha256"), json.dumps(r.get("environment"), sort_keys=True))
                  for repo, r in measured.items() if repo in pins and r.get("binary_sha256")}
    if len(provenance) > 1:
        sys.exit(f"{log} mixes {len(provenance)} Bifrost builds or measurement environments; "
                 "measure every pinned repo under one before ranking")
    y = json.load(open(sys.argv[2]))
    estimate = y["windows"]["wide_2025"]["per_repo_relation_estimate_capped"]
    group_of = y["group_of"]

    rows = []
    for repo in pins:
        r = measured.get(repo, {"error": "not measured yet"})
        cold, warm = r.get("cold") or {}, r.get("warm_restart") or {}
        b = estimate.get(repo, 0.0)
        failure = None
        if r.get("error"):  # the latest attempt failed before or around its phases
            failure = r["error"]
        elif r.get("head") != pins[repo] or r.get("memory_max") != MEMORY_MAX:
            failure = f"measured at {str(r.get('head'))[:10]} under {r.get('memory_max')}, not the pin and cap"
        elif not cold.get("ready"):
            failure = cold.get("error") or "cold index not ready"
        elif not warm.get("ready"):
            failure = warm.get("error") or warm.get("skipped") or "warm restart not ready"
        c_h = (cold.get("cpu_to_ready_usec") or 0) / 3.6e9  # null when the phase failed
        w_h = (warm.get("cpu_to_ready_usec") or 0) / 3.6e9
        hours = c_h + max(b - 1, 0) * w_h
        rows.append({"repo": repo, "b": b, "cold_h": c_h, "warm_h": w_h, "hours": hours,
                     "peak_gib": max(cold.get("peak_tree_kib") or 0,
                                     warm.get("peak_tree_kib") or 0) / 2**20,
                     "failure": failure, "group": tuple(group_of.get(repo) or ())})
    rows.sort(key=lambda x: (x["failure"] is not None, -x["b"], x["hours"], x["repo"]))

    taken, picked = set(), 0
    print("| Rank | Repo | B (capped est.) | Cold CPU-h | Warm CPU-h | Index CPU-h | Peak GiB | Status |")
    print("|---|---|---|---|---|---|---|---|")
    for i, x in enumerate(rows, 1):
        if x["failure"]:
            status = "excluded: " + x["failure"].split("\n")[0][:120]
        elif x["group"] and x["group"] in taken:
            status = "reserve (shared-history group already selected)"
        elif picked < COHORT:
            picked += 1
            status = f"provisional cohort #{picked}"
            if x["group"]:
                taken.add(x["group"])
        else:
            status = "reserve"
        print(f"| {i} | {x['repo']} | {x['b']:.1f} | {x['cold_h']:.3f} | {x['warm_h']:.3f} | "
              f"{x['hours']:.2f} | {x['peak_gib']:.1f} | {status} |")
    total = sum(x["hours"] for x in rows if not x["failure"])
    print(f"\nIndexing CPU-hours, all measured repos that reached ready: {total:.1f}")
    missing = [repo for repo in pins if repo not in measured]
    if missing:
        sys.exit(f"incomplete batch: no record yet for {', '.join(missing)}")


if __name__ == "__main__":
    main()
