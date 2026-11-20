#!/usr/bin/env python3
"""Select and freeze the deterministic lean RQL census pilot."""
import argparse
import hashlib
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile

# This file lives at .agents/evals/review-arms/brief/rql-census/pilot/.
REVIEW_ARMS = Path(__file__).resolve().parents[3]
ROOT = REVIEW_ARMS.parents[2]
sys.path.insert(0, str(REVIEW_ARMS))
import census_label as census  # noqa: E402

SEED = 20261120
REPO_COUNTS = (("mjolnir", 17), ("bifrost-dev", 18))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def draw(rows, count, label):
    rows = sorted(rows, key=lambda row: (row["repo"], row["id"]))
    salt = int.from_bytes(hashlib.sha256(label.encode()).digest()[:8], "big")
    random.Random(SEED ^ salt).shuffle(rows)
    return rows[:count]


def refresh_eligible(eligible_path):
    """Write every eligible label (confirmed relation bugs in the pilot repos) from ONE read of the
    live label file, parsed with the shared reader's record-boundary rules, so labels, line numbers
    and the digest all describe the same bytes."""
    items, pins, _ = census.item_sets()
    docs = census.input_documents()
    selected_items = [census.labeller_item(row, "development") for row in items["development"]]
    heads = {repo: census.checkout_revision(repo, pins) for repo in {item["repo"] for item in selected_items}}
    expected = {(item["repo"], item["id"]): census.inputs_sha(item, docs, "primary", heads[item["repo"]])
                for item in selected_items}
    data = (census.LABELS / "development-primary.jsonl").read_bytes()
    with tempfile.TemporaryDirectory(dir=REVIEW_ARMS.parents[2] / ".mj") as scratch:
        frozen = Path(scratch) / "development-primary.jsonl"
        frozen.write_bytes(data)
        labels, _ = census.read_current_labels(frozen, expected)
        line_numbers = {}
        for number, row in enumerate_records(frozen):
            key = row["repo"], row["id"]
            if key in labels and row.get("inputs_sha") == expected[key]:
                line_numbers[key] = number
    items_by_key = {(row["repo"], row["id"]): row for row in selected_items}
    rows = []
    for (repo, item_id), label in sorted(labels.items()):
        if repo not in dict(REPO_COUNTS) or label["defect_check"] != "confirmed" or not label["relation_bug"]:
            continue
        item = items_by_key[(repo, item_id)]
        rows.append({"repo": repo, "id": str(item_id), "intro": item["intro_commit"], "base": item["base_commit"],
                     "label_line": line_numbers[(repo, item_id)], "label": label,
                     "source_sha256": hashlib.sha256(data).hexdigest()})
    eligible_path.write_text("".join(json.dumps(row, sort_keys=True) + "\n" for row in rows))


def enumerate_records(path):
    """Line-numbered records under census_label.rows's boundary rule: an unterminated final line is
    an incomplete record and is skipped."""
    with path.open() as source:
        for number, line in enumerate(source, 1):
            if line.strip() and line.endswith("\n"):
                yield number, json.loads(line)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=Path(__file__).resolve().parent / "manifest.json")
    parser.add_argument("--frozen-at", help="Optional explicit timestamp recorded in the manifest")
    parser.add_argument("--refresh-eligible", action="store_true",
                        help="regenerate eligible-labels.jsonl from the live label file (once, before freezing)")
    parser.add_argument("--plan-commit", required=True,
                        help="the commit whose PLAN-rql-census.md this selection was frozen under; the "
                             "working copy of the plan must match it, so later commits don't change the output")
    args = parser.parse_args()

    eligible_path = Path(__file__).resolve().parent / "eligible-labels.jsonl"
    if args.refresh_eligible:
        refresh_eligible(eligible_path)
    # The draw reads only this committed file, so it reproduces from a clean checkout.
    eligible = [json.loads(line) for line in eligible_path.read_text().splitlines() if line.strip()]
    labels = {(row["label"]["repo"], row["label"]["id"]): row["label"] for row in eligible}
    candidates = [{key: row[key] for key in ("repo", "id", "intro", "base", "label_line")} for row in eligible]

    deduped, duplicates = {}, []
    for row in sorted(candidates, key=lambda r: (r["repo"], r["intro"], r["id"])):
        key = row["repo"], row["intro"]
        if key in deduped:
            duplicates.append({"repo": row["repo"], "intro": row["intro"],
                               "kept_id": deduped[key]["id"], "dropped_id": row["id"]})
        else:
            deduped[key] = row
    pool = sorted(deduped.values(), key=lambda r: (r["repo"], r["id"]))
    pool_counts = {repo: sum(row["repo"] == repo for row in pool) for repo, _ in REPO_COUNTS}
    if len(pool) != 54 or pool_counts != {"mjolnir": 26, "bifrost-dev": 28}:
        raise SystemExit(f"expected eligible pool 54 (26 + 28), got {len(pool)} ({pool_counts})")

    chosen, replicate_rows = [], []
    for repo, count in REPO_COUNTS:
        repo_rows = [r for r in pool if r["repo"] == repo]
        repo_chosen = draw(repo_rows, count, "select/" + repo)
        chosen.extend(repo_chosen)
        replicate_rows.extend(draw(repo_chosen, 5, "replicate/" + repo))
    replicates = {(r["repo"], r["id"]) for r in replicate_rows}
    diffs, jobs = [], []
    for row in sorted(chosen, key=lambda r: (r["repo"], r["id"])):
        key = f'{row["repo"]}/{row["id"]}'
        counts = {arm: (2 if (row["repo"], row["id"]) in replicates and arm in ("G", "R") else 1)
                  for arm in ("G", "R", "R-loc")}
        diffs.append({"key": key, **row, "replicates": counts})
        for arm in ("G", "R", "R-loc"):
            for rep in range(1, counts[arm] + 1):
                jobs.append({"job": f"{key}/{arm}/r{rep}/writer", "diff": key, "arm": arm,
                             "replicate": rep, "kind": "writer", "slot": None})
                for slot in (1, 2):
                    jobs.append({"job": f"{key}/{arm}/r{rep}/reviewer-{slot}", "diff": key,
                                 "arm": arm, "replicate": rep, "kind": "reviewer", "slot": slot})
        for slot in (1, 2):
            jobs.append({"job": f"{key}/N/r1/reviewer-{slot}", "diff": key, "arm": "N",
                         "replicate": 1, "kind": "reviewer", "slot": slot})

    # The readout reads census facts and sites only from this frozen copy of the chosen diffs'
    # current labels, so a later repair of the label file can't change a replayed readout.
    snapshot_path = args.out.parent / "labels-snapshot.jsonl"
    snapshot_path.parent.mkdir(parents=True, exist_ok=True)
    snapshot_path.write_text("".join(
        json.dumps(labels[(row["repo"], row["id"])], sort_keys=True) + "\n"
        for row in sorted(chosen, key=lambda r: (r["repo"], r["id"]))))

    census_root = census.CENSUS
    steps_path = census_root / "menu-steps.md"
    plan_path = "brief/PLAN-rql-census.md"
    plan_commit = subprocess.check_output(["git", "rev-parse", "--verify", args.plan_commit + "^{commit}"],
                                          cwd=ROOT, text=True).strip()
    frozen_plan = subprocess.check_output(["git", "show", f"{plan_commit}:.agents/evals/review-arms/{plan_path}"],
                                          cwd=ROOT)
    if frozen_plan != (REVIEW_ARMS / plan_path).read_bytes():
        raise SystemExit(f"{plan_path} differs from its version at {plan_commit}: re-freeze under the new plan")
    pool_bytes = json.dumps(pool, sort_keys=True, separators=(",", ":")).encode()
    manifest = {"version": 1, "seed": SEED, "plan_commit": plan_commit,
                "inputs": {"eligible_labels_sha256": sha(eligible_path), "menu_sha256": sha(census_root / "menu.json"),
                           "catalogue_sha256": sha(census_root / "rql-documented-steps.md"),
                           "steps_sha256": sha(steps_path) if steps_path.exists() else None,
                           "labels_snapshot_sha256": sha(snapshot_path),
                           "plan_sha256": hashlib.sha256(frozen_plan).hexdigest()},
                "models": {"writer": "gpt-6.1-sol/high", "reviewer": "gpt-6.1-terra/high",
                           "engine": "fable/high", "judge": "fable/high"},
                "model_versions": {"fable": "Fable 5.2"}, "page_rows": 50,
                "eligible_pool": pool, "eligible_pool_sha256": hashlib.sha256(pool_bytes).hexdigest(),
                "duplicates": duplicates, "diffs": diffs, "jobs": jobs}
    if args.frozen_at:
        manifest["frozen_at"] = args.frozen_at
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
    print(json.dumps({"diffs": len(diffs), "writer_jobs": sum(j["kind"] == "writer" for j in jobs),
                      "reviewer_jobs": sum(j["kind"] == "reviewer" for j in jobs),
                      "chosen": {repo: [r["id"] for r in sorted(chosen, key=lambda x: (x["repo"], x["id"])) if r["repo"] == repo]
                                 for repo, _ in REPO_COUNTS},
                      "replicates": [f'{r["repo"]}/{r["id"]}' for r in replicate_rows],
                      "duplicates": duplicates}, indent=2))


if __name__ == "__main__":
    main()
