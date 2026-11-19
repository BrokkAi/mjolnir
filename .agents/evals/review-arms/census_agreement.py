"""Compare independent Stage 1 labels by repo and overall."""
import argparse
from collections import defaultdict
import json
from pathlib import Path

from census_label import LABELS, RUST, exclude_families, item_sets, rows


def match_facts(primary, replication):
    """Match facts one to one when the smaller site set is contained in the other."""
    sites = [set(fact["sites"]) for fact in replication]
    edges = []
    for fact in primary:
        left = set(fact["sites"])
        candidates = [(len(left & right), i) for i, right in enumerate(sites)
                      if left <= right or right <= left]
        edges.append([i for _, i in sorted(candidates, key=lambda pair: (-pair[0], pair[1]))])
    assigned = {}

    def assign(i, visited):
        for j in edges[i]:
            if j in visited:
                continue
            visited.add(j)
            if j not in assigned or assign(assigned[j], visited):
                assigned[j] = i
                return True
        return False

    for i in range(len(primary)):
        assign(i, set())
    return [(primary[i], replication[j]) for j, i in sorted(assigned.items())]


def overlap(primary, replication):
    """Mean best Jaccard overlap in both directions, preserving alternative sets."""
    scores = []
    for left, right in ((primary, replication), (replication, primary)):
        for facts in left:
            common = (len(match_facts(facts, other)) for other in right)
            scores.append(max(n / (len(facts) + len(other) - n)
                              for n, other in zip(common, right)))
    return sum(scores) / len(scores)


def unique_facts(sets):
    facts = {}
    for group in sets:
        for fact in group:
            key = (fact["claim"], tuple(sorted(set(fact["sites"]))))
            facts.setdefault(key, fact)
    return list(facts.values())


def categories(fact):
    return {
        "rql.class": (fact["rql"] or {}).get("class"),
        "kind": fact["kind"],
        "anchors_on_touched": (fact["anchoring"] or {}).get("anchors_on_touched"),
    }


def comparison(pairs, cohort_size):
    categories_by_fact = {category: {"agree": 0, "n": 0}
                          for category in ("rql.class", "kind", "anchors_on_touched")}
    relation, total_overlap, matched = 0, 0, 0
    for first, second in pairs:
        relation += first["relation_bug"] == second["relation_bug"]
        left, right = ([item["facts"] for item in label["decisive_sets"]] for label in (first, second))
        total_overlap += overlap(left, right)
        aligned = match_facts(unique_facts(left), unique_facts(right))
        matched += len(aligned)
        for a, b in aligned:
            values = categories(b)
            for category, value in categories(a).items():
                other = values[category]
                if value is not None or other is not None:
                    stats = categories_by_fact[category]
                    stats["n"] += 1
                    stats["agree"] += value == other
    return {
        "relation_bug": {"agree": relation, "n": cohort_size},
        "decisive_set_overlap": {"mean": total_overlap / cohort_size if cohort_size else None,
                                 "n": cohort_size},
        "matched_facts": matched,
        **categories_by_fact,
    }


def read_role(directory, split, role):
    path = directory / f"{split}-{role}.jsonl"
    result = {}
    for item in rows(path) if path.exists() else ():
        key = item["repo"], item["id"]
        if key in result:
            raise ValueError(f"{path}: duplicate repo/id {key}")
        result[key] = item
    return result


def compare_split(directory, split, cohort):
    first = read_role(directory, split, "primary")
    second = read_role(directory, split, "replication")
    groups = defaultdict(set)
    all_keys = {(item["repo"], item["id"]) for item in cohort}
    for key in all_keys:
        groups[key[0]].add(key)
    outside = (first.keys() | second.keys()) - all_keys
    if outside:
        raise ValueError(f"{split}: labels outside the cohort: {sorted(outside)}")

    def summary(keys):
        primary = keys & first.keys()
        replication = keys & second.keys()
        both = primary & replication
        pairs = [(first[key], second[key]) for key in sorted(both)]
        # A missing label contributes no observed agreement or overlap.
        return {**comparison(pairs, len(keys)), "paired": len(both),
                "missing_both": len(keys - (primary | replication)),
                "missing_primary_only": len(replication - primary),
                "missing_replication_only": len(primary - replication)}

    return {"overall": summary(all_keys),
            "by_repo": {repo: summary(keys) for repo, keys in sorted(groups.items())}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--labels-dir", type=Path, default=LABELS)
    parser.add_argument("--check-clones", type=Path, default=RUST / "check-clones")
    args = parser.parse_args()
    cohorts, _, _ = item_sets()
    exclude_families(cohorts, args.check_clones)
    print(json.dumps({split: compare_split(args.labels_dir, split, cohort)
                      for split, cohort in cohorts.items()}, indent=2))


if __name__ == "__main__":
    main()
