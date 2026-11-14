"""Labels the fact each defect's reviewer needed that was not in the diff.

Usage:
  python3 label_facts.py build [--verify verify3] [--batch-size 5]
  (run label/workflow.js with the Workflow tool, args = {"dir": "$REVIEW_ARMS_DIR/label-facts",
   "batches": item IDs of each batch-N.json in order}; save its result as
   $REVIEW_ARMS_DIR/label-facts/workflow-out.json)
  python3 label_facts.py collect   # writes results/missing-facts.jsonl and a summary

Items: every Hard-won diff with its known defect, and every sampled diff that
has a confirmed problem in <verify>, with its most severe one (then highest
confidence, then first group). Sampled diffs with no confirmed problem are
left out. Each batch goes to one Opus/high labeler that follows label/SPEC.md
and returns its labels as structured output.
"""
import argparse
import collections
import csv
import json
import os
import pathlib
import random

HERE = pathlib.Path(__file__).resolve().parent
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
L = ROOT / "label-facts"
SEV = {"high": 3, "medium": 2, "low": 1}

p = argparse.ArgumentParser()
p.add_argument("step", choices=["build", "collect"])
p.add_argument("--verify", default="verify3")
p.add_argument("--batch-size", type=int, default=5)
a = p.parse_args()


def save(path, value):
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    tmp.replace(path)


def build():
    items = []
    for line in open(HERE / "corpus/corpus.jsonl"):
        b = json.loads(line)
        items.append({"id": b["id"], "source": "hard-won", "intro": b["intro_commit"], "base": b["base_commit"],
                      "defect": b["defect"], "location": b["location"], "trigger": b["trigger"],
                      "fix_commit": b["fix_commit"]})
    evidence = {}  # (intro, group) -> verifier evidence
    for f in (ROOT / a.verify / "out").glob("verify-out-*.json"):
        for c in json.loads(f.read_text())["commits"]:
            for g in c["groups"]:
                evidence[(c["intro"], g["group"])] = g.get("evidence")
    groups = collections.defaultdict(list)
    stage = "results/stage3/groups.csv" if a.verify == "verify3" else "results/groups.csv"
    for i, r in enumerate(csv.DictReader(open(HERE / stage))):
        if r["verdict"] == "confirmed":
            groups[r["bug"]].append((-SEV.get(r["severity"], 0), -SEV.get(r["confidence"], 0), i, r))
    for line in open(HERE / "corpus/diffs.jsonl"):
        d = json.loads(line)
        if not groups.get(d["id"]):
            continue
        r = min(groups[d["id"]])[3]
        ev = evidence.get((d["intro_commit"], r["group"].split("/")[-1]))
        assert ev, f"no verifier evidence for {r['group']}"
        items.append({"id": d["id"], "source": "sampled", "intro": d["intro_commit"], "base": d["base_commit"],
                      "defect": r["summary"], "severity": r["severity"], "group": r["group"], "evidence": ev})
    random.Random(20261114).shuffle(items)
    L.mkdir(parents=True, exist_ok=True)
    save(L / "items.json", items)
    for n in range(0, len(items), a.batch_size):
        # Batch files carry only what the labeler needs.
        batch = [{k: v for k, v in it.items() if k not in ("source", "group", "severity")}
                 for it in items[n:n + a.batch_size]]
        save(L / f"batch-{n // a.batch_size}.json", batch)
    (L / "SPEC.md").write_text((HERE / "label/SPEC.md").read_text())
    print(json.dumps({"items": len(items), "batches": -(-len(items) // a.batch_size),
                      "by_source": collections.Counter(i["source"] for i in items)}))


def collect():
    items = {it["id"]: it for it in json.loads((L / "items.json").read_text())}
    out = json.loads((L / "workflow-out.json").read_text())
    labels = {x["id"]: x for x in out["labels"]}
    missing = sorted(set(items) - set(labels))
    assert not missing, f"unlabeled items: {missing}"
    rows = [{**{k: items[i][k] for k in ("id", "source", "intro", "defect")},
             **({"severity": items[i]["severity"]} if "severity" in items[i] else {}),
             **{k: v for k, v in labels[i].items() if k != "id"}} for i in sorted(items)]
    (HERE / "results/missing-facts.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
    table = collections.Counter((r["source"], r["kind"]) for r in rows)
    nones = collections.Counter(r["none_reason"] for r in rows if r["kind"] == "none")
    print(json.dumps({"by_source_kind": {f"{s}/{k}": n for (s, k), n in sorted(table.items())},
                      "none_reasons": nones,
                      "confidence": collections.Counter(r["confidence"] for r in rows),
                      "defect_not_confirmed": [r["id"] for r in rows if not r["defect_check"].startswith("confirmed")]},
                     indent=1))


{"build": build, "collect": collect}[a.step]()
