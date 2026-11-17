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
  # Another canonical corpus, with no sampled reviews or verification:
  python3 label_facts.py build --known-only --corpus <corpus.jsonl> --labels-dir <labels>
  python3 label_facts.py build-second --labels-dir <labels> --second-sources hard-won,bug-issue
  # Task units use the same SPEC and {"labels": [...]} output as workflow-out.json:
  python3 label_facts.py collect --labels-dir <labels> --results-dir <results> --task-dir <tasks>
  python3 label_facts.py collect-second --labels-dir <labels> --results-dir <results> \
    --task-dir <independent-tasks> --second-sources hard-won,bug-issue

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
SEV = {"high": 3, "medium": 2, "low": 1}

p = argparse.ArgumentParser()
p.add_argument("step", choices=["build", "collect", "build-pass2", "collect-pass2", "build-second", "collect-second"])
p.add_argument("--verify", default="verify3")
p.add_argument("--batch-size", type=int, default=5)
p.add_argument("--corpus", type=pathlib.Path, default=HERE / "corpus/corpus.jsonl")
p.add_argument("--diffs", type=pathlib.Path, default=HERE / "corpus/diffs.jsonl")
p.add_argument("--groups", type=pathlib.Path, help="defaults to results/stage3/groups.csv for verify3, otherwise results/groups.csv")
p.add_argument("--known-only", action="store_true", help="build only canonical defects; do not read sampled findings or verification")
p.add_argument("--labels-dir", type=pathlib.Path, default=ROOT / "label-facts")
p.add_argument("--results-dir", type=pathlib.Path, default=HERE / "results")
p.add_argument("--second-sources", default="hard-won", help="comma-separated canonical sources included in the independent second census")
p.add_argument("--task-dir", type=pathlib.Path, help="collect/collect-second: task units containing batch.json and label-out.json")
a = p.parse_args()
if a.batch_size <= 0:
    p.error("--batch-size must be positive")
if a.task_dir is not None and a.step not in ("collect", "collect-second"):
    p.error("--task-dir applies only to collect and collect-second")
L = a.labels_dir
SECOND_SOURCES = set(a.second_sources.split(","))
if not all(SECOND_SOURCES):
    p.error("--second-sources must name nonempty sources")


def save(path, value):
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    tmp.replace(path)


def stratification(item):
    """Keep canonical provenance in results, never in labeler inputs."""
    return {k: v for k, v in item.items() if k == "found_by" or k.startswith("issue")}


def batch_item(item):
    private = {"source", "group", "severity"} | stratification(item).keys()
    return {k: v for k, v in item.items() if k not in private}


def indexed(items, context):
    if not isinstance(items, list):
        raise ValueError(f"{context}: expected a list")
    counts = collections.Counter()
    for item in items:
        if not isinstance(item, dict) or not isinstance(item.get("id"), str):
            raise ValueError(f"{context}: each item needs a string id")
        counts[item["id"]] += 1
    duplicates = sorted(k for k, n in counts.items() if n > 1)
    if duplicates:
        raise ValueError(f"{context}: duplicate IDs: {duplicates}")
    return {item["id"]: item for item in items}


def exact_ids(actual, expected, context):
    missing, extra = sorted(set(expected) - set(actual)), sorted(set(actual) - set(expected))
    if missing or extra:
        raise ValueError(f"{context}: missing IDs: {missing}; extra IDs: {extra}")


def validate_label(label):
    # The interface is label/workflow.js's schema plus SPEC.md's null semantics.
    strings = {"id", "defect_check", "fact", "kind", "evidence", "confidence"}
    nullable = {"none_reason", "alt_kind", "query"}
    required = strings | nullable | {"other_facts"}
    missing = sorted(required - label.keys())
    if missing:
        raise ValueError(f"label {label.get('id')}: missing fields: {missing}")
    if any(not isinstance(label[k], str) for k in strings):
        raise ValueError(f"label {label['id']}: expected string fields")
    if any(label[k] is not None and not isinstance(label[k], str) for k in nullable):
        raise ValueError(f"label {label['id']}: expected string or null")
    if not isinstance(label["other_facts"], list) or any(not isinstance(x, str) for x in label["other_facts"]):
        raise ValueError(f"label {label['id']}: other_facts must be a list of strings")
    kinds = {"affected callers", "path", "value flow", "none"}
    if (label["kind"] not in kinds or label["alt_kind"] not in kinds | {None}
            or label["confidence"] not in {"high", "medium", "low"}
            or label["none_reason"] not in {"in_diff", "intent", "external", None}):
        raise ValueError(f"label {label['id']}: invalid enum")
    if label["kind"] == "none":
        valid_nulls = label["none_reason"] is not None and label["query"] is None
    else:
        valid_nulls = label["none_reason"] is None and isinstance(label["query"], str)
    if not valid_nulls:
        raise ValueError(f"label {label['id']}: inconsistent kind, none_reason and query")


def label_output(path, expected):
    out = json.loads(path.read_text())
    if not isinstance(out, dict) or "labels" not in out:
        raise ValueError(f"{path}: expected an object with labels")
    if out.get("missing"):
        raise ValueError(f"{path}: reported missing labels: {out['missing']}")
    labels = indexed(out["labels"], str(path))
    exact_ids(labels, expected, str(path))
    for label in labels.values():
        validate_label(label)
    return labels


def read_labels(expected, workflow):
    if a.task_dir is None:
        return label_output(workflow, expected)
    labels, batch_ids = [], []
    # Driver bookkeeping directories contain no batch.json and are not units.
    for unit in sorted(a.task_dir.iterdir()):
        if not unit.is_dir():
            continue
        batch = unit / "batch.json"
        if not batch.exists():
            if (unit / "INSTRUCTIONS.md").exists() or (unit / "label-out.json").exists():
                raise ValueError(f"{unit}: missing batch.json")
            continue
        inputs = indexed(json.loads(batch.read_text()), str(batch))
        batch_ids.extend(inputs.values())
        labels.extend(label_output(unit / "label-out.json", inputs).values())
    batches = indexed(batch_ids, f"{a.task_dir}: all batches")
    exact_ids(batches, expected, f"{a.task_dir}: all batches")
    result = indexed(labels, f"{a.task_dir}: all labels")
    exact_ids(result, expected, f"{a.task_dir}: all labels")
    return result


def read_items():
    return indexed(json.loads((L / "items.json").read_text()), str(L / "items.json"))


def result_metadata(item, severity=False):
    return {**{k: item[k] for k in ("id", "source", "intro", "defect")},
            **({"severity": item["severity"]} if severity and "severity" in item else {}),
            **stratification(item)}


def write_results(name, rows):
    # All validation precedes creating or replacing any result file.
    text = "".join(json.dumps(r) + "\n" for r in rows)
    a.results_dir.mkdir(parents=True, exist_ok=True)
    path = a.results_dir / name
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text)
    tmp.replace(path)


def build():
    items = []
    for line in open(a.corpus):
        b = json.loads(line)
        items.append({"id": b["id"], "source": b.get("source", "hard-won"), "intro": b["intro_commit"], "base": b["base_commit"],
                      "defect": b["defect"], "location": b["location"], "trigger": b["trigger"],
                      "fix_commit": b["fix_commit"], **stratification(b)})
    if not a.known_only:
        evidence = {}  # (intro, group) -> verifier evidence
        for f in (ROOT / a.verify / "out").glob("verify-out-*.json"):
            for c in json.loads(f.read_text())["commits"]:
                for g in c["groups"]:
                    evidence[(c["intro"], g["group"])] = g.get("evidence")
        groups = collections.defaultdict(list)
        stage = a.groups or a.results_dir / ("stage3/groups.csv" if a.verify == "verify3" else "groups.csv")
        for i, r in enumerate(csv.DictReader(open(stage))):
            if r["verdict"] == "confirmed":
                groups[r["bug"]].append((-SEV.get(r["severity"], 0), -SEV.get(r["confidence"], 0), i, r))
        for line in open(a.diffs):
            d = json.loads(line)
            if not groups.get(d["id"]):
                continue
            r = min(groups[d["id"]])[3]
            ev = evidence.get((d["intro_commit"], r["group"].split("/")[-1]))
            assert ev, f"no verifier evidence for {r['group']}"
            items.append({"id": d["id"], "source": "sampled", "intro": d["intro_commit"], "base": d["base_commit"],
                          "defect": r["summary"], "severity": r["severity"], "group": r["group"], "evidence": ev})
    indexed(items, "canonical and sampled items")
    random.Random(20261114).shuffle(items)
    L.mkdir(parents=True, exist_ok=True)
    save(L / "items.json", items)
    for n in range(0, len(items), a.batch_size):
        # Batch files carry only what the labeler needs.
        batch = [batch_item(it)
                 for it in items[n:n + a.batch_size]]
        save(L / f"batch-{n // a.batch_size}.json", batch)
    (L / "SPEC.md").write_text((HERE / "label/SPEC.md").read_text())
    print(json.dumps({"items": len(items), "batches": -(-len(items) // a.batch_size),
                      "by_source": collections.Counter(i["source"] for i in items)}))


def collect():
    items = read_items()
    labels = read_labels(items, L / "workflow-out.json")
    rows = [{**result_metadata(items[i], severity=True),
             **{k: v for k, v in labels[i].items() if k != "id"}} for i in sorted(items)]
    write_results("missing-facts.jsonl", rows)
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
    for line in open(a.results_dir / "missing-facts.jsonl"):
        r = json.loads(line)
        if r["kind"] == "none" and r["none_reason"] == "in_diff":
            item = batch_item(items[r["id"]])
            item["first_pass"] = {k: r[k] for k in ("fact", "evidence", "other_facts")}
            save(P / f"item-{r['id']}.json", item)
            ids.append(r["id"])
    (P / "SPEC.md").write_text((HERE / "label/SPEC-pass2.md").read_text())
    print(json.dumps(ids))


def collect_pass2():
    first = {json.loads(l)["id"]: json.loads(l) for l in open(a.results_dir / "missing-facts.jsonl")}
    out = json.loads((L / "pass2/workflow-out.json").read_text())
    assert not out["missing"], f"unlabeled items: {out['missing']}"
    rows = [{**result_metadata(first[x["id"]]),
             **{k: v for k, v in x.items() if k != "id"}} for x in sorted(out["labels"], key=lambda x: x["id"])]
    write_results("missing-facts-pass2.jsonl", rows)
    print(json.dumps({"kind": collections.Counter(r["kind"] for r in rows),
                      "specificity": collections.Counter(r["specificity"] for r in rows)}, indent=1))


def build_second():
    items = [i for i in read_items().values() if i["source"] in SECOND_SOURCES]
    random.Random(20261115).shuffle(items)
    S = L / "second"
    S.mkdir(exist_ok=True)
    batches = []
    for n in range(0, len(items), a.batch_size):
        batch = [batch_item(it)
                 for it in items[n:n + a.batch_size]]
        save(S / f"batch-{n // a.batch_size}.json", batch)
        batches.append([it["id"] for it in batch])
    (S / "SPEC.md").write_text((HERE / "label/SPEC.md").read_text())
    print(json.dumps({"dir": str(S), "batches": batches}))


def collect_second():
    items = {i: item for i, item in read_items().items() if item["source"] in SECOND_SOURCES}
    labels = read_labels(items, L / "second/workflow-out.json")
    first_path = a.results_dir / "missing-facts.jsonl"
    first = indexed([json.loads(line) for line in first_path.read_text().splitlines() if line.strip()], str(first_path))
    if not set(items) <= first.keys():
        raise ValueError(f"{first_path}: missing first-pass IDs: {sorted(set(items) - first.keys())}")
    rows = [{**result_metadata(items[i]),
             **{k: v for k, v in labels[i].items() if k != "id"}} for i in sorted(items)]
    relation = lambda kind: kind != "none"
    summary = {
        "relation_agree": sum(relation(r["kind"]) == relation(first[r["id"]]["kind"]) for r in rows),
        "kind_agree": sum(r["kind"] == first[r["id"]]["kind"] for r in rows),
        "items": len(rows),
        "disagreements": [{"id": r["id"], "first": first[r["id"]]["kind"], "second": r["kind"]}
                          for r in rows if r["kind"] != first[r["id"]]["kind"]]}
    write_results("missing-facts-second.jsonl", rows)
    print(json.dumps(summary, indent=1))


{"build": build, "collect": collect, "build-pass2": build_pass2, "collect-pass2": collect_pass2,
 "build-second": build_second, "collect-second": collect_second}[a.step]()
