#!/usr/bin/env python3
"""Choose the census menu of added RQL steps from the development labels.

The rule is PLAN-rql-census.md's "Menu rule", written before the development census:
- use the current primary labels of the development split, and leave out bugs whose label
  is not_confirmed;
- a missing-step kind decides a relation bug when some decisive set has every fact outside
  the diff either documented or missing with that kind, and at least one of the latter;
- each development repo's share is the fraction of its relation bugs the kind decides; kinds
  rank by the mean share over development repos, ties broken by the total bugs decided;
- the top k = 3 kinds are added, never "other".
"""
import json
import sys
from collections import defaultdict

import census_label as c

K = 3


def decided_kinds(label):
    """The missing-step kinds that alone complete some decisive set of this label."""
    kinds = set()
    for decisive_set in label["decisive_sets"]:
        outside = [fact for fact in decisive_set["facts"] if fact["in_diff"] is False]
        missing = {fact["rql"]["missing_step"]["kind"] for fact in outside if fact["rql"]["class"] == "missing"}
        documented_or_missing = all(fact["rql"]["class"] in ("documented", "missing") for fact in outside)
        if documented_or_missing and len(missing) == 1:
            kinds |= missing
    return kinds


def main():
    items, pins, _ = c.item_sets()
    documents = c.input_documents()
    selected = [c.labeller_item(row, "development") for row in items["development"]]
    heads = {repo: c.checkout_revision(repo, pins) for repo in {item["repo"] for item in selected}}
    expected = {(item["repo"], item["id"]): c.inputs_sha(item, documents, "primary", heads[item["repo"]])
                for item in selected}
    current, stale = c.read_current_labels(c.LABELS / "development-primary.jsonl", expected)
    missing_labels = sorted(set(expected) - set(current))
    if missing_labels:
        sys.exit(f"{len(missing_labels)} development items have no current primary label")

    relation_bugs = defaultdict(int)
    decided = defaultdict(lambda: defaultdict(int))
    excluded = defaultdict(int)
    for (repo, _), label in current.items():
        if label["defect_check"] != "confirmed":
            excluded[repo] += 1
            continue
        if not label["relation_bug"]:
            continue
        relation_bugs[repo] += 1
        for kind in decided_kinds(label):
            decided[kind][repo] += 1

    def rank(repos):
        ranking = []
        for kind in c.MISSING_KINDS:
            shares = {repo: decided[kind][repo] / relation_bugs[repo] for repo in repos}
            ranking.append({"kind": kind, "mean_share": sum(shares.values()) / len(repos),
                            "total_decided": sum(decided[kind][repo] for repo in repos),
                            "per_repo": {repo: [decided[kind][repo], relation_bugs[repo]] for repo in repos}})
        ranking.sort(key=lambda row: (-row["mean_share"], -row["total_decided"], row["kind"]))
        # Only a kind that decides at least one development relation bug can be added; with
        # fewer such kinds the menu is shorter, and with none it is empty.
        return ranking, [row["kind"] for row in ranking
                         if row["kind"] != "other" and row["total_decided"] > 0][:K]

    repos = sorted(relation_bugs)
    ranking, menu = rank(repos)
    # The plan's stability report: leave out one test repo's development bugs, then all of them.
    test_repos = [repo for repo in repos if repo not in ("mjolnir", "bifrost-dev")]
    stability = {f"without {repo}": rank([r for r in repos if r != repo])[1] for repo in test_repos}
    stability["without all test repos"] = rank([r for r in repos if r not in test_repos])[1]
    print(json.dumps({"menu": menu, "stability": stability, "stable": all(set(m) == set(menu) for m in stability.values()),
                      "ranking": ranking, "relation_bugs": dict(relation_bugs),
                      "not_confirmed_excluded": dict(excluded),
                      "stale_rows_ignored": sum(stale.values())}, indent=1))


if __name__ == "__main__":
    main()
