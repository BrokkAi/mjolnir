"""Labels the fact each defect's reviewer needed that was not in the diff.

Usage:
  python3 label_facts.py build [--verify verify3] [--batch-size 5]
  (run label/workflow.js with the Workflow tool, args = {"dir": "$REVIEW_ARMS_DIR/label-facts",
   "batches": item IDs of each batch-N.json in order}; save its result as
   $REVIEW_ARMS_DIR/label-facts/workflow-out.json)
  python3 label_facts.py collect   # writes results/missing-facts.jsonl and a summary
  python3 label_facts.py build-pass2   # items labeled "none: in_diff" -> label-facts/pass2/
  (run label/workflow-pass2.js, args = {"dir": "$REVIEW_ARMS_DIR/label-facts/pass2",
   "ids": the IDs it prints}; save its result as label-facts/pass2/workflow-out.json)
  python3 label_facts.py collect-pass2  # writes results/missing-facts-pass2.jsonl
  python3 label_facts.py build-second   # Hard-won items, reshuffled, -> label-facts/second/
  (run label/workflow.js with args {"dir": "$REVIEW_ARMS_DIR/label-facts/second", "batches": ...};
   save its result as label-facts/second/workflow-out.json)
  python3 label_facts.py collect-second # writes results/missing-facts-second.jsonl, prints agreement

Items: every Hard-won diff with its known defect, and every sampled diff that
has a confirmed problem in <verify>, with its most severe one (then highest
confidence, then first group). Sampled diffs with no confirmed problem are
left out. Each batch goes to one Opus/high labeler that follows label/SPEC.md
and returns its labels as structured output. The second pass gives each item
the first pass labeled "visible in the diff" to one Opus/high labeler that
follows label/SPEC-pass2.md, with the first pass's fact and evidence. The
second labeler relabels the 30 Hard-won items independently under
label/SPEC.md, in a different batch order, to check the first.
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
p.add_argument("step", choices=["build", "collect", "build-pass2", "collect-pass2", "build-second", "collect-second"])
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


def build_pass2():
    items = {it["id"]: it for it in json.loads((L / "items.json").read_text())}
    P = L / "pass2"
    P.mkdir(exist_ok=True)
    ids = []
    for line in open(HERE / "results/missing-facts.jsonl"):
        r = json.loads(line)
        if r["kind"] == "none" and r["none_reason"] == "in_diff":
            item = {k: v for k, v in items[r["id"]].items() if k not in ("source", "group", "severity")}
            item["first_pass"] = {k: r[k] for k in ("fact", "evidence", "other_facts")}
            save(P / f"item-{r['id']}.json", item)
            ids.append(r["id"])
    (P / "SPEC.md").write_text((HERE / "label/SPEC-pass2.md").read_text())
    print(json.dumps(ids))


def collect_pass2():
    first = {json.loads(l)["id"]: json.loads(l) for l in open(HERE / "results/missing-facts.jsonl")}
    out = json.loads((L / "pass2/workflow-out.json").read_text())
    assert not out["missing"], f"unlabeled items: {out['missing']}"
    rows = [{**{k: first[x["id"]][k] for k in ("id", "source", "intro", "defect")},
             **{k: v for k, v in x.items() if k != "id"}} for x in sorted(out["labels"], key=lambda x: x["id"])]
    (HERE / "results/missing-facts-pass2.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
    print(json.dumps({"kind": collections.Counter(r["kind"] for r in rows),
                      "specificity": collections.Counter(r["specificity"] for r in rows)}, indent=1))


def build_second():
    items = [i for i in json.loads((L / "items.json").read_text()) if i["source"] == "hard-won"]
    random.Random(20261115).shuffle(items)
    S = L / "second"
    S.mkdir(exist_ok=True)
    batches = []
    for n in range(0, len(items), a.batch_size):
        batch = [{k: v for k, v in it.items() if k not in ("source", "group", "severity")}
                 for it in items[n:n + a.batch_size]]
        save(S / f"batch-{n // a.batch_size}.json", batch)
        batches.append([it["id"] for it in batch])
    (S / "SPEC.md").write_text((HERE / "label/SPEC.md").read_text())
    print(json.dumps({"dir": str(S), "batches": batches}))


def collect_second():
    first = {json.loads(l)["id"]: json.loads(l) for l in open(HERE / "results/missing-facts.jsonl")}
    out = json.loads((L / "second/workflow-out.json").read_text())
    assert not out["missing"], f"unlabeled items: {out['missing']}"
    rows = [{**{k: first[x["id"]][k] for k in ("id", "source", "intro", "defect")},
             **{k: v for k, v in x.items() if k != "id"}} for x in sorted(out["labels"], key=lambda x: x["id"])]
    (HERE / "results/missing-facts-second.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
    relation = lambda kind: kind != "none"
    print(json.dumps({
        "relation_agree": sum(relation(r["kind"]) == relation(first[r["id"]]["kind"]) for r in rows),
        "kind_agree": sum(r["kind"] == first[r["id"]]["kind"] for r in rows),
        "items": len(rows),
        "disagreements": [{"id": r["id"], "first": first[r["id"]]["kind"], "second": r["kind"]}
                          for r in rows if r["kind"] != first[r["id"]]["kind"]]}, indent=1))


{"build": build, "collect": collect, "build-pass2": build_pass2, "collect-pass2": collect_pass2,
 "build-second": build_second, "collect-second": collect_second}[a.step]()
