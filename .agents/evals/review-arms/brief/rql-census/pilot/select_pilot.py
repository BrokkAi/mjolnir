#!/usr/bin/env python3
"""Select and freeze the deterministic lean RQL census pilot."""
import argparse
import fcntl
import hashlib
import json
import os
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
# The three added steps frozen in menu-steps.md, in its order.
MENU_STEPS = ("contract-cases", "value-transfers", "guarded-paths")
REPO_COUNTS = (("mjolnir", 17), ("bifrost-dev", 18))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def draw(rows, count, label):
    rows = sorted(rows, key=lambda row: (row["repo"], row["id"]))
    salt = int.from_bytes(hashlib.sha256(label.encode()).digest()[:8], "big")
    random.Random(SEED ^ salt).shuffle(rows)
    return rows[:count]


def refresh_eligible():
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
    return "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows)


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
    parser.add_argument("--served-steps", default=",".join(MENU_STEPS),
                        help="the added menu steps the engine and writers serve; a step that fails the "
                             "fidelity gate is removed by re-freezing with a shorter list")
    parser.add_argument("--plan-commit", required=True,
                        help="the commit whose PLAN-rql-census.md this selection was frozen under; the "
                             "working copy of the plan must match it, so later commits don't change the output")
    args = parser.parse_args()
    served_steps = [step for step in args.served_steps.split(",") if step]
    unknown = [step for step in served_steps if step not in MENU_STEPS]
    if unknown or len(set(served_steps)) != len(served_steps):
        raise SystemExit(f"--served-steps must be distinct names from {MENU_STEPS}; got {served_steps}")
    served_steps = [step for step in MENU_STEPS if step in served_steps]

    # Validate the plan revision before anything is computed or written, so a failed re-freeze
    # can't leave new inputs beside an old manifest.
    plan_path = "brief/PLAN-rql-census.md"
    plan_commit = subprocess.check_output(["git", "rev-parse", "--verify", args.plan_commit + "^{commit}"],
                                          cwd=ROOT, text=True).strip()
    frozen_plan = subprocess.check_output(["git", "show", f"{plan_commit}:.agents/evals/review-arms/{plan_path}"],
                                          cwd=ROOT)
    if frozen_plan != (REVIEW_ARMS / plan_path).read_bytes():
        raise SystemExit(f"{plan_path} differs from its version at {plan_commit}: re-freeze under the new plan")

    eligible_path = Path(__file__).resolve().parent / "eligible-labels.jsonl"
    # The draw reads only the committed eligible file, so it reproduces from a clean checkout;
    # --refresh-eligible rebuilds that file's text in memory, published only if the freeze succeeds.
    eligible_text = refresh_eligible() if args.refresh_eligible else eligible_path.read_text()
    eligible = [json.loads(line) for line in eligible_text.splitlines() if line.strip()]
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
    snapshot_text = ("".join(
        json.dumps(labels[(row["repo"], row["id"])], sort_keys=True) + "\n"
        for row in sorted(chosen, key=lambda r: (r["repo"], r["id"]))))

    census_root = census.CENSUS
    steps_path = census_root / "menu-steps.md"
    pool_bytes = json.dumps(pool, sort_keys=True, separators=(",", ":")).encode()
    manifest = {"version": 1, "seed": SEED, "plan_commit": plan_commit,
                "inputs": {"eligible_labels_sha256": hashlib.sha256(eligible_text.encode()).hexdigest(), "menu_sha256": sha(census_root / "menu.json"),
                           "catalogue_sha256": sha(census_root / "rql-documented-steps.md"),
                           "steps_sha256": sha(steps_path) if steps_path.exists() else None,
                           "labels_snapshot_sha256": hashlib.sha256(snapshot_text.encode()).hexdigest(),
                           "cases_sha256": sha(census_root / "menu-steps-cases.md"),
                           "plan_sha256": hashlib.sha256(frozen_plan).hexdigest()},
                "models": {"writer": "gpt-6.1-sol/high", "reviewer": "gpt-6.1-terra/high",
                           "engine": "sonnet/high", "judge": "sonnet/high"},
                # Engine, fidelity adjudicator, judges and hand checks (study plan, Verifier, 2026-11-21).
                "model_versions": {"claude": "Sonnet 5.5", "claude_provider_id": "claude-sonnet-5-5"},
                "page_rows": 50, "served_steps": served_steps,
                # Paths relative to this manifest; each file's sha256 is in "inputs".
                "input_paths": {"catalogue": "../rql-documented-steps.md", "steps": "../menu-steps.md",
                                "menu": "../menu.json", "cases": "../menu-steps-cases.md",
                                "eligible_labels": "eligible-labels.jsonl", "labels_snapshot": "labels-snapshot.jsonl",
                                "plan": "../../PLAN-rql-census.md"},
                "eligible_pool": pool, "eligible_pool_sha256": hashlib.sha256(pool_bytes).hexdigest(),
                "duplicates": duplicates, "diffs": diffs, "jobs": jobs}
    if args.frozen_at:
        manifest["frozen_at"] = args.frozen_at
    # Publish the freeze only now that everything validated, under one lock so concurrent runs
    # can't interleave. Each file goes to a unique temp name and is renamed into place, and the
    # manifest is renamed last. An interruption before that leaves the old manifest, whose
    # recorded hashes no longer match the new label files, and every reader checks those hashes,
    # so a partial publication fails visibly instead of being read as a freeze.
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with (args.out.parent / ".freeze.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for path, text in ((eligible_path, eligible_text), (snapshot_path, snapshot_text),
                           (args.out, json.dumps(manifest, sort_keys=True, indent=2) + "\n")):
            temp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
            temp.write_text(text)
            os.replace(temp, path)
    print(json.dumps({"diffs": len(diffs), "writer_jobs": sum(j["kind"] == "writer" for j in jobs),
                      "reviewer_jobs": sum(j["kind"] == "reviewer" for j in jobs),
                      "chosen": {repo: [r["id"] for r in sorted(chosen, key=lambda x: (x["repo"], x["id"])) if r["repo"] == repo]
                                 for repo, _ in REPO_COUNTS},
                      "replicates": [f'{r["repo"]}/{r["id"]}' for r in replicate_rows],
                      "duplicates": duplicates}, indent=2))


if __name__ == "__main__":
    main()
