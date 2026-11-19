"""Compare independent Stage 1 labels by repo and overall.

Facts match by cited code sites. Comparing whether their claims agree would
require a judge.
"""
import argparse
from collections import defaultdict
import json
from pathlib import Path

from census_label import (
    LABELS, RUST, canonical_site, checkout_revision, exclude_families, input_documents, inputs_sha, item_sets, labeller_item,
    read_current_labels,
)


def match_facts(primary, replication):
    """Maximize paired facts, then pairs that cite identical sites. Pairing uses site evidence
    only, never the categories the report scores. Each side is first put in a canonical order
    (sites, then claim text), so ties between equal site pairings don't depend on the order
    facts were listed in."""
    def canonical(facts):
        return sorted(facts, key=lambda fact: (sorted(canonical_site(s) for s in fact["sites"]),
                                               fact.get("claim") or ""))
    primary, replication = canonical(primary), canonical(replication)
    left = [{canonical_site(s) for s in fact["sites"]} for fact in primary]
    right = [{canonical_site(s) for s in fact["sites"]} for fact in replication]
    # Vertex indices for the replication side start after the primary side.
    edges = [(i, j, int(a != b))
             for i, a in enumerate(left)
             for j, b in enumerate(right, start=len(left))
             if a <= b or b <= a]
    assigned = {}
    vertex_count = len(left) + len(right)
    while True:
        # Find the cheapest augmenting path from any unpaired primary fact.
        # A containment pair costs one and an exact pair costs zero. Reversing
        # an assigned edge refunds its cost, so each augmentation maximizes
        # exact pairs among matchings of the resulting size.
        paired_left = set(assigned.values())
        distance = {i: 0 for i in range(len(left)) if i not in paired_left}
        previous = {}
        residual = [(j, i, -cost) if assigned.get(j) == i else (i, j, cost)
                    for i, j, cost in edges]
        for _ in range(vertex_count - 1):
            changed = False
            for source, target, cost in residual:
                if source not in distance:
                    continue
                candidate = distance[source] + cost
                if target not in distance or candidate < distance[target]:
                    distance[target] = candidate
                    previous[target] = source
                    changed = True
            if not changed:
                break
        end = min((j for j in range(len(left), vertex_count) if j in distance and j not in assigned),
                  key=distance.__getitem__, default=None)
        if end is None:
            break
        while end is not None:
            i = previous[end]
            assigned[end] = i
            end = previous.get(i)
    return [(primary[i], replication[j - len(left)]) for j, i in sorted(assigned.items())]


def overlap(primary, replication):
    """Mean best site-based Jaccard overlap in both directions across alternative sets."""
    scores = []
    for left, right in ((primary, replication), (replication, primary)):
        for facts in left:
            common = (len(match_facts(facts, other)) for other in right)
            scores.append(max(n / (len(facts) + len(other) - n)
                              for n, other in zip(common, right)))
    return sum(scores) / len(scores)


def unique_facts(sets):
    """One entry per distinct fact across alternative sets. When a labeller repeats a fact with
    different scored categories, those categories are marked conflicting, so they count as
    disagreement whatever the order of the sets."""
    occurrences = {}
    for group in sets:
        for fact in group:
            key = (fact["claim"], tuple(sorted({canonical_site(s) for s in fact["sites"]})))
            occurrences.setdefault(key, []).append(fact)
    facts = []
    for repeats in occurrences.values():
        seen = [categories(fact) for fact in repeats]
        conflicting = {key for key in seen[0] if len({json.dumps(c[key]) for c in seen}) > 1}
        facts.append(dict(repeats[0], _conflicting=sorted(conflicting)) if conflicting else repeats[0])
    return facts


def categories(fact):
    rql = fact["rql"] or {}
    return {
        "rql.class": rql.get("class"),
        "rql.locator_present": (rql["locator_query"] is not None) if fact["in_diff"] is False else None,
        "kind": fact["kind"],
        "anchors_on_touched": (fact["anchoring"] or {}).get("anchors_on_touched"),
    }


def category_comparisons(a, b):
    second = categories(b)
    conflicting = set(a.get("_conflicting", ())) | set(b.get("_conflicting", ()))
    for category, value in categories(a).items():
        other = second[category]
        if category in conflicting:
            yield category, False
            continue
        if value is None and other is None:
            continue
        if category == "rql.locator_present" and (value is None or other is None):
            continue
        yield category, value == other


def comparison(pairs):
    categories_by_fact = {category: {"agree": 0, "n": 0}
                          for category in ("rql.class", "rql.locator_present", "kind", "anchors_on_touched")}
    relation, total_overlap, matched = 0, 0, 0
    defects = {"both_confirmed": 0, "both_not_confirmed": 0, "split": 0}
    for first, second in pairs:
        relation += first["relation_bug"] == second["relation_bug"]
        if first["defect_check"] == second["defect_check"]:
            defects[f"both_{first['defect_check']}"] += 1
        else:
            defects["split"] += 1
        left, right = ([item["facts"] for item in label["decisive_sets"]] for label in (first, second))
        total_overlap += overlap(left, right)
        aligned = match_facts(unique_facts(left), unique_facts(right))
        matched += len(aligned)
        for a, b in aligned:
            for category, agree in category_comparisons(a, b):
                stats = categories_by_fact[category]
                stats["n"] += 1
                stats["agree"] += agree
    paired = len(pairs)
    return {
        "relation_bug": {"agree": relation if paired else None, "n": paired},
        "defect_check": {"agree": paired - defects["split"] if paired else None, "n": paired, **defects},
        "decisive_set_site_overlap": {"mean": total_overlap / paired if paired else None,
                                     "n": paired},
        "matched_facts": matched,
        **categories_by_fact,
    }


def compare_split(directory, split, cohort, documents, heads):
    items = {(item["repo"], item["id"]): labeller_item(item, split) for item in cohort}

    def read_role(role):
        expected = {key: inputs_sha(item, documents, role, heads[key[0]]) for key, item in items.items()}
        return read_current_labels(directory / f"{split}-{role}.jsonl", expected)

    first, stale_primary = read_role("primary")
    second, stale_replication = read_role("replication")
    groups = defaultdict(set)
    all_keys = items.keys()
    for key in all_keys:
        groups[key[0]].add(key)

    def summary(keys, repo=None):
        primary = keys & first.keys()
        replication = keys & second.keys()
        both = primary & replication
        pairs = [(first[key], second[key]) for key in sorted(both)]
        return {**comparison(pairs), "paired": len(both),
                "missing_both": len(keys - (primary | replication)),
                "missing_primary_only": len(replication - primary),
                "missing_replication_only": len(primary - replication),
                "stale_rows": {"primary": sum(stale_primary.values()) if repo is None else stale_primary[repo],
                               "replication": sum(stale_replication.values()) if repo is None else stale_replication[repo]}}

    return {"overall": summary(all_keys),
            "by_repo": {repo: summary(keys, repo) for repo, keys in sorted(groups.items())}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--labels-dir", type=Path, default=LABELS)
    parser.add_argument("--check-clones", type=Path, default=RUST / "check-clones")
    args = parser.parse_args()
    cohorts, pins, _ = item_sets()
    exclude_families(cohorts, args.check_clones)
    documents = input_documents()
    repos = {item["repo"] for cohort in cohorts.values() for item in cohort}
    heads = {repo: checkout_revision(repo, pins) for repo in repos}
    report = {
        "field_descriptions": {
            "stale_rows": "Saved rows with missing or mismatched input hashes or outside the current cohort.",
            "rql.locator_present": "Agreement on whether a locator query was supplied, "
                                   "among matched facts both labellers marked outside the diff.",
            "decisive_set_site_overlap": "Facts match by their cited sites. This measures whether "
                                         "labellers point at the same code. Comparing claim text "
                                         "would require a judge.",
        },
        **{split: compare_split(args.labels_dir, split, cohort, documents, heads) for split, cohort in cohorts.items()},
    }
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
