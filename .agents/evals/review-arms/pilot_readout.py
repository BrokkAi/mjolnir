#!/usr/bin/env python3
"""Reproduce the lean RQL pilot readout from frozen labels, judgments and delivery logs."""
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re
from statistics import mean, median

from pilot_judge import (DEFAULT_MANIFEST, DEFAULT_ROOT, JUDGMENT_FIELDS, KEY_PATH, artifact,
                         group_jobs, load, load_snapshot, records, save, sha)

ARMS = ("N", "G", "R", "R-loc")
CONTRASTS = (("R", "G"), ("R-loc", "R"))


def complete_diffs(key, judged):
    by_job = {entry["job"]: oid for oid, entry in key["outputs"].items()}
    return {
        diff: [by_job[job["job"]] for job in jobs]
        for diff, jobs in group_jobs(key["manifest"]).items()
        if all(by_job.get(job["job"]) in judged for job in jobs)
    }


def fact_ids(entry, facts):
    return [fact["id"] for fact in facts if entry["kind"] == "writer" or not fact["in_diff"]]


def arm_outcomes(entries, scored, ids, facts):
    cells = defaultdict(lambda: defaultdict(list))
    for oid in ids:
        entry, verdict = entries[oid], scored[oid]
        cell = cells[entry["kind"], entry["arm"]]
        for field in JUDGMENT_FIELDS[entry["kind"]]:
            value = verdict[field]
            if type(value) is bool:
                cell[field].append(value)
            else:
                possible = entry["defect_ids"] if field == "matched_defects" else fact_ids(entry, facts[entry["diff"]])
                for item in possible:
                    cell[f"{field}:{item}"].append(item in value)
    return {cell: {name: mean(values) for name, values in metrics.items()}
            for cell, metrics in cells.items()}


def average(values):
    values = list(values)
    return mean(values) if values else None


def arm_summary(cells, entries, scored, ids, deliveries, facts):
    summary = {}
    for arm in ARMS:
        reviews = [cell["reviewer", arm] for cell in cells.values() if ("reviewer", arm) in cell]
        writers = [cell["writer", arm] for cell in cells.values() if ("writer", arm) in cell]
        catches = [row["catch"] for row in reviews]
        result = {"catch_rate": average(catches), "diffs": len(catches),
                  "review_outputs": sum(entries[oid]["arm"] == arm and entries[oid]["kind"] == "reviewer"
                                        for oid in ids)}
        if writers:
            brief_ids = [oid for oid in ids if entries[oid]["arm"] == arm and entries[oid]["kind"] == "writer"]
            result.update(contains_decisive_full_sample=mean(row["contains_decisive"] for row in writers),
                          states_defect=mean(scored[oid]["states_defect"] for oid in brief_ids),
                          briefs=len(brief_ids))
        result["per_fact_by_diff"] = per_fact(cells, arm)
        for field in ("recovered_facts", "contained_facts", "outside_contained_facts"):
            result[field + "_rate"] = average(value
                                              for diff in result["per_fact_by_diff"].values()
                                              for value in diff.get(field, {}).values())
        if arm in {"R", "R-loc"}:
            result["querying"] = query_summary(entries, scored,
                                               [oid for oid in ids if entries[oid]["arm"] == arm], deliveries, facts)
        summary[arm] = result
    return summary


def per_fact(cells, arm):
    result = {}
    for diff, cell in cells.items():
        row = {}
        for kind, field in (("reviewer", "recovered_facts"), ("writer", "contained_facts")):
            if (kind, arm) in cell:
                prefix = field + ":"
                row[field] = {key[len(prefix):]: value for key, value in cell[kind, arm].items()
                              if key.startswith(prefix)}
        if "contained_facts" in row:
            row["outside_contained_facts"] = {fact: value for fact, value in row["contained_facts"].items()
                                              if fact in row.get("recovered_facts", {})}
        result[diff] = row
    return result


def delivered_sites(events, fact_sites):
    """Coverage of each outside fact's cited sites by actual delivered pages."""
    spans = defaultdict(list)
    for event in events:
        if event["status"] != "ok":
            continue
        for row in event["payload"]["rows"]:
            for site in row["sites"]:
                spans[site["path"]].append((site["start_line"], site["end_line"]))
    coverage = {}
    for fact, sites in fact_sites.items():
        reached = []
        for site in sites:
            match = re.fullmatch(r"(.+):(\d+)(?:-(\d+))?", site)
            if match is None:
                raise ValueError(f"invalid census site {site!r}")
            path, start, end = match.groups()
            start, end = int(start), int(end or start)
            reached.extend(any(lo <= line <= hi for lo, hi in spans[path]) for line in range(start, end + 1))
        coverage[fact] = "all" if all(reached) else "some" if any(reached) else "none"
    return coverage


def query_summary(entries, scored, ids, deliveries, facts):
    brief_by_key = {(entries[oid]["diff"], entries[oid]["replicate"]): oid for oid in ids
                    if entries[oid]["kind"] == "writer"}
    recovery = defaultdict(list)
    for oid in ids:
        entry = entries[oid]
        if entry["kind"] == "reviewer":
            key = entry["diff"], entry["replicate"]
            for fact in fact_ids(entry, facts[entry["diff"]]):
                recovery[key, fact].append(fact in scored[oid]["recovered_facts"])

    covered = []
    queried_containment = defaultdict(list)
    writers = Counter()
    calls = Counter()
    latencies = defaultdict(list)
    for key, oid in brief_by_key.items():
        entry, verdict = entries[oid], scored[oid]
        events = deliveries.get(entry["job"])
        if events is None:
            writers["log_missing"] += 1
            continue
        ok = False
        for event in events:
            status = event["status"]
            calls[status] += 1
            latencies[status].append(event["latency_ms"])
            latencies["all"].append(event["latency_ms"])
            ok |= status == "ok"
        writers["queried" if events else "no_query"] += 1
        if ok:
            writers["exposed"] += 1
            queried_containment[key[0]].append(verdict["contains_decisive"])
        outside_sites = {fact["id"]: fact["sites"] for fact in facts[key[0]] if not fact["in_diff"]}
        for fact, coverage in delivered_sites(events, outside_sites).items():
            covered.append({"diff": key[0], "replicate": key[1], "fact": fact, "site_coverage": coverage,
                            "contained": fact in verdict["contained_facts"],
                            "reviewer_recovery_rate": average(recovery[key, fact])})
    return {**{name: writers[name] for name in ("queried", "exposed", "no_query", "log_missing")},
            "queries": sum(calls.values()), "ok_answers": calls["ok"],
            "parse_error": calls["parse_error"], "engine_error": calls["engine_error"],
            "other_errors": sum(count for status, count in calls.items()
                                if status not in {"ok", "parse_error", "engine_error"}),
            "latency_ms": {status: {"count": len(values), "mean": mean(values), "median": median(values)}
                           for status, values in latencies.items()},
            "exposed_only_contains_decisive": average(mean(values) for values in queried_containment.values()),
            "site_coverage_counts": dict(Counter(row["site_coverage"] for row in covered)),
            "by_brief_and_fact": covered}


def brief_variance(entries, scored, ids):
    """Estimate within-diff brief variance from two independent briefs per diff.

    Half the squared difference of two brief means also contains reviewer
    sampling noise. Subtract half the sum of their estimated mean variances.
    """
    reviewers = defaultdict(list)
    for oid in ids:
        entry = entries[oid]
        if entry["kind"] == "reviewer" and entry["arm"] in {"G", "R"}:
            reviewers[entry["arm"], entry["diff"], entry["replicate"]].append(scored[oid]["catch"])
    result = {}
    for arm in ("G", "R"):
        paired = {}
        for a, diff, rep in reviewers:
            if a != arm or rep != 1 or (arm, diff, 2) not in reviewers:
                continue
            first, second = reviewers[arm, diff, 1], reviewers[arm, diff, 2]
            if len(first) != 2 or len(second) != 2:
                raise ValueError("brief variance needs two reviewers per brief")
            squared = (mean(first) - mean(second)) ** 2 / 2
            noise = ((first[0] - first[1]) ** 2 + (second[0] - second[1]) ** 2) / 8
            paired[diff] = {"half_squared_brief_difference": squared, "reviewer_noise": noise}
        raw = average(row["half_squared_brief_difference"] - row["reviewer_noise"]
                      for row in paired.values())
        result[arm] = {"replicate_diffs": len(paired), "unconstrained_variance": raw,
                       "brief_variance": max(0, raw) if raw is not None else None,
                       "by_diff": paired}
    return result


def paired(cells, a, b, *, metric="catch"):
    if metric == "catch":
        return {diff: cell["reviewer", a]["catch"] - cell["reviewer", b]["catch"]
                for diff, cell in cells.items()}
    result = {}
    for diff, cell in cells.items():
        facts = [key for key in cell["reviewer", a] if key.startswith("recovered_facts:")]
        differences = [cell["writer", a][key.replace("recovered", "contained", 1)]
                       - cell["writer", b][key.replace("recovered", "contained", 1)] for key in facts]
        if differences:
            result[diff] = mean(differences)
    return result


def agreement(entries, judged, checked, ids, facts):
    """Compare the first-pass decisions with the independent hand checks."""
    counts = defaultdict(Counter)

    def tally(kind, field, agrees):
        counts[kind, field]["agreements"] += agrees
        counts[kind, field]["decisions"] += 1

    for oid in ids:
        entry = entries[oid]
        kind = entry["kind"]
        first, second = judged[oid], checked[oid]
        tally(kind, "whole_record", first == second)
        for field in JUDGMENT_FIELDS[kind]:
            expected, actual = first[field], second[field]
            if type(expected) is bool:
                tally(kind, field, expected == actual)
            else:
                candidates = entry["defect_ids"] if field == "matched_defects" else fact_ids(entry, facts[entry["diff"]])
                for candidate in candidates:
                    tally(kind, field, (candidate in expected) == (candidate in actual))
    result = {kind: {} for kind in JUDGMENT_FIELDS}
    for (kind, field), row in counts.items():
        result[kind][field] = {**row, "rate": row["agreements"] / row["decisions"]}
    return result


def effect(deltas):
    return {"mean": average(deltas.values()), "by_diff": deltas}


def readout(manifest, judgments, deliveries, snapshot):
    """Compute from the manifest, keyed judgments, delivery events, and frozen labels."""
    key, judged, checked = (judgments[field] for field in ("key", "judge", "hand_check"))
    if manifest != key["manifest"]:
        raise ValueError("manifest differs from the stored judgment key")
    entries = key["outputs"]
    facts = {diff: [fact for group in groups for fact in group] for diff, groups in snapshot.items()}
    judge_complete = complete_diffs(key, judged)
    hand_complete = complete_diffs(key, checked)
    complete = {diff: ids for diff, ids in judge_complete.items() if diff in hand_complete}
    ids = [oid for diff in complete.values() for oid in diff]

    def summarize(scored):
        cells = {diff: arm_outcomes(entries, scored, diff_ids, facts) for diff, diff_ids in complete.items()}
        return cells, {"arms": arm_summary(cells, entries, scored, ids, deliveries, facts),
                       "brief_level_catch_variance": brief_variance(entries, scored, ids)}

    judged_cells, judged_summary = summarize(judged)
    checked_cells, checked_summary = summarize(checked)
    contrasts = {}
    for a, b in CONTRASTS:
        contrasts[f"{a} - {b}"] = {
            metric: {"as_judged": effect(paired(judged_cells, a, b, metric=metric)),
                     "hand_checked": effect(paired(checked_cells, a, b, metric=metric))}
            for metric in ("catch", "outside_fact_containment")
        }

    contrasts["G - N"] = {"catch": {"as_judged": effect(paired(judged_cells, "G", "N")),
                                     "hand_checked": effect(paired(checked_cells, "G", "N"))}}

    def without_leaks(scores):
        eligible = {diff: cell for diff, cell in scores.items()
                    if not any(row["states_defect"] for (kind, _), row in cell.items() if kind == "writer")}
        return effect(paired(eligible, "R", "G"))

    return {
        "complete_diffs": list(complete),
        "unfinished_diffs": [d["key"] for d in key["manifest"]["diffs"] if d["key"] not in complete],
        "judge_complete_diffs": list(judge_complete),
        "hand_check_complete_diffs": list(hand_complete),
        "as_judged": judged_summary,
        "hand_checked": checked_summary,
        "judge_hand_check_agreement": agreement(entries, judged, checked, ids, facts),
        "paired_contrasts": contrasts,
        "contrasts_not_estimated": {
            "ceiling_review": ["O-fact - O-loc", "O-fact - O-base", "O-loc - O-base"],
            "ceiling_writer": ["W-fact - W-loc", "W-fact - W-base", "W-loc - W-base"],
            "documented_steps": ["R - R0", "R0 - G", "R0-loc - R0", "R0-loc - G"],
            "compute_matched": ["R - H", "R-loc - H"],
            "degraded_recall": ["D - G", "R - D"],
        },
        "descriptive_R_minus_G_excluding_any_leak": {"as_judged": without_leaks(judged_cells),
                                                    "hand_checked": without_leaks(checked_cells)},
        "methods": {
            "catch": "Equal-weight diff means of the arm's reviewer catches, pooling both briefs where repeated.",
            "per_fact": "Arm means within each diff and fact, with equal weight per diff-fact in overall rates.",
            "containment": "Contrasts use full paired samples. The exposed-only statistic includes writers that received an ok answer.",
            "site_coverage": "Coverage uses spans in the pages returned to the writer. A row can state a fact "
                             "without spanning its census sites. Stated-in-row judging is deferred to test.",
            "analysis_cohort": "Both passes cover every job for each included diff. The hand check supplies "
                               "the final labels. Agreement compares the common cohort.",
            "leak_exclusion": "Descriptive subset selected by brief outcomes after assignment.",
            "pilot_inference": "Contrasts are descriptive with two development repos.",
        },
    }


def load_deliveries(runs, key):
    deliveries = {}
    for entry in key["outputs"].values():
        if entry["kind"] != "writer" or entry["arm"] not in {"R", "R-loc"}:
            continue
        path = runs / artifact(entry).relative_to("runs").parent / "queries.jsonl"
        data = path.read_bytes()
        if sha(data) != entry["query_sha256"]:
            raise ValueError(f"{path}: delivery log differs from the judge input's frozen run")
        records = [json.loads(line) for line in data.splitlines() if line.strip()]
        deliveries[entry["job"]] = [record for record in records if record.get("delivered") is True]
    return deliveries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT, help="root containing stored judgments")
    parser.add_argument("--runs", type=Path, help="writer delivery logs under this runs directory")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    manifest = load(args.manifest)
    snapshot = load_snapshot(args.manifest, manifest)
    key = load(args.root / KEY_PATH)
    judgments = {"key": key, "judge": records(args.root, key, "judge"),
                 "hand_check": records(args.root, key, "hand-check")}
    result = readout(manifest, judgments, load_deliveries(args.runs or args.root / "runs", key), snapshot)
    if args.output:
        save(args.output, result)
    else:
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
