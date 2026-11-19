"""Compare independent Stage 1 labels by repo and overall."""
import argparse
from collections import defaultdict
import json
from pathlib import Path

from census_label import LABELS, rows


def match_facts(primary, replication):
    """Find a maximum one-to-one matching of facts that share a path:line."""
    sites = [set(fact["sites"]) for fact in replication]
    edges = []
    for fact in primary:
        left = set(fact["sites"])
        candidates = [(len(left & right), i) for i, right in enumerate(sites) if left & right]
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


def comparison(pairs):
    categories_by_fact = {category: {"agree": 0, "n": 0}
                          for category in ("rql.class", "kind", "anchors_on_touched")}
    relation, overlaps, matched = 0, [], 0
    for first, second in pairs:
        relation += first["relation_bug"] == second["relation_bug"]
        left, right = ([item["facts"] for item in label["decisive_sets"]] for label in (first, second))
        overlaps.append(overlap(left, right))
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
        "relation_bug": {"agree": relation, "n": len(pairs)},
        "decisive_set_overlap": {"mean": sum(overlaps) / len(overlaps) if overlaps else None,
                                 "n": len(overlaps)},
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


def compare_split(directory, split):
    first = read_role(directory, split, "primary")
    second = read_role(directory, split, "replication")
    groups = defaultdict(list)
    for key in sorted(first.keys() & second.keys()):
        groups[key[0]].append((first[key], second[key]))

    def summary(repo=None):
        primary = {key for key in first if repo is None or key[0] == repo}
        replication = {key for key in second if repo is None or key[0] == repo}
        pairs = groups[repo] if repo is not None else [pair for group in groups.values() for pair in group]
        return {**comparison(pairs), "primary_only": len(primary - replication),
                "replication_only": len(replication - primary)}

    return {"overall": summary(),
            "by_repo": {repo: summary(repo) for repo in sorted({repo for repo, _ in first.keys() | second.keys()})}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--labels-dir", type=Path, default=LABELS)
    args = parser.parse_args()
    print(json.dumps({split: compare_split(args.labels_dir, split) for split in ("development", "test")},
                     indent=2))


if __name__ == "__main__":
    main()
