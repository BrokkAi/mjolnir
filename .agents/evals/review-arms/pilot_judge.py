#!/usr/bin/env python3
"""Prepare both blinded passes, then run or collect independent Fable/high judgments."""
import argparse
from collections import defaultdict
import fcntl
import hashlib
import json
import os
from pathlib import Path
import random
import secrets
import subprocess
import sys
from tempfile import NamedTemporaryFile

HERE = Path(__file__).resolve().parent
DEFAULT_MANIFEST = HERE / "brief/rql-census/pilot/manifest.json"
DEFAULT_ROOT = Path("/mnt/optane/review-arms-pilot")
KEY_PATH = Path("judge-private/key.json")
INPUT_PATH = Path("judge-input")
HAND_PATH = Path("hand-check-input")
DEFECT_FIELDS = ("defect", "location", "trigger", "match_rule", "near_misses",
                 "why_findable", "intro_commit", "fix_commit")
JUDGMENT_FIELDS = {
    "reviewer": ("catch", "recovered_facts", "matched_defects"),
    "writer": ("contains_decisive", "contained_facts", "states_defect"),
}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def load(path):
    return json.loads(path.read_text(encoding="utf-8"))


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as temp:
        json.dump(value, temp, indent=2)
        temp.write("\n")
    Path(temp.name).replace(path)


def load_snapshot(manifest_path, manifest):
    """Read and verify the frozen primary labels beside the manifest."""
    path = manifest_path.with_name("labels-snapshot.jsonl")
    data = path.read_bytes()
    expected = manifest["inputs"]["labels_snapshot_sha256"]
    if (actual := sha(data)) != expected:
        raise ValueError(f"{path}: SHA-256 mismatch (manifest {expected}, file {actual})")

    labels = {}
    for line in data.splitlines():
        label = json.loads(line)
        key = label["repo"], label["id"]
        if (key in labels or label["labeller"] != "primary"
                or label["defect_check"] != "confirmed" or not label["relation_bug"]):
            raise ValueError(f"{path}: duplicate or ineligible label {key}")
        labels[key] = decisive_sets(label)
    expected_keys = {(diff["repo"], diff["id"]) for diff in manifest["diffs"]}
    if labels.keys() != expected_keys:
        raise ValueError(f"{path}: label keys differ from the manifest")
    return {diff["key"]: labels[diff["repo"], diff["id"]] for diff in manifest["diffs"]}


def decisive_sets(label):
    return [
        [{"id": f"{s}/{f}", **{k: fact[k] for k in ("claim", "sites", "in_diff")}}
         for f, fact in enumerate(group["facts"], 1)]
        for s, group in enumerate(label["decisive_sets"], 1)
    ]


def load_defects(manifest):
    """Read the catch rubric from the selected development corpus records."""
    from census_label import HERE as corpus_root, rows

    corpora = {"mjolnir": corpus_root / "corpus/corpus.jsonl",
               "bifrost-dev": corpus_root / "corpus-bifrost/corpus.jsonl"}
    records = {repo: {str(row["id"]): row for row in rows(corpora[repo])}
               for repo in {diff["repo"] for diff in manifest["diffs"]}}
    defects = {}
    for diff in manifest["diffs"]:
        row = records[diff["repo"]][diff["id"]]
        if (row["intro_commit"], row["base_commit"]) != (diff["intro"], diff["base"]):
            raise ValueError(f"{diff['key']}: manifest revisions differ from the corpus defect")
        defects[diff["key"]] = [{"id": diff["id"], **{field: row[field] for field in DEFECT_FIELDS}}]
    return defects


def artifact(job):
    base = Path("runs") / job["diff"] / job["arm"] / f"r{job['replicate']}"
    if job["kind"] == "writer":
        return base / "writer/brief.md"
    return base / "reviewers" / str(job["slot"]) / "output.md"


def opaque(existing):
    while (value := secrets.token_hex(8)) in existing:
        pass
    return value


def group_jobs(manifest):
    by_diff = defaultdict(list)
    for job in manifest["jobs"]:
        by_diff[job["diff"]].append(job)
    return by_diff


def write_task(path, payload):
    if (path / "items.json").exists() and load(path / "items.json") != payload:
        raise ValueError(f"{path}: existing blinded input differs")
    save(path / "items.json", payload)
    for name in ("SPEC.md", "PILOT.md"):
        spec = (HERE / "judge" / name).read_bytes()
        target = path / name
        if target.exists() and target.read_bytes() != spec:
            raise ValueError(f"{path}: existing {name} differs")
        target.write_bytes(spec)
    # The verifier treats this file as the task's readiness marker.
    (path / "INSTRUCTIONS.md").write_text(
        "Follow PILOT.md for every item in items.json. Write verdicts.jsonl.\n"
    )


def prepare(root, manifest, snapshot, defects, selected=None, *, manifest_sha256=None):
    key_file = root / KEY_PATH
    key = load(key_file) if key_file.exists() else {"version": 1, "manifest": manifest, "outputs": {}}
    if key["manifest"] != manifest:
        raise ValueError("manifest differs from the private judge key")
    outputs = key["outputs"]
    by_job = {entry["job"]: oid for oid, entry in outputs.items()}
    occupied_batches = {entry["batch"] for entry in outputs.values()}
    by_diff = group_jobs(manifest)
    diffs = sorted(by_diff)
    random.Random(manifest["seed"]).shuffle(diffs)
    for diff in diffs:
        if selected is not None and diff not in selected:
            continue
        jobs = by_diff[diff]
        paths = [artifact(job) for job in jobs]
        if not all((root / path).is_file() and (root / path.parent / "done.json").is_file()
                   and (root / path.parent / "usage.json").is_file() for path in paths):
            continue
        if any(job["kind"] == "writer" and job["arm"] in {"R", "R-loc"}
               and not (root / path.parent / "queries.jsonl").is_file() for job, path in zip(jobs, paths)):
            continue
        sets = snapshot[diff]
        facts = [fact for group in sets for fact in group]
        outside = [fact for fact in facts if not fact["in_diff"]]
        minimal_sets = [[fact["id"] for fact in group] for group in sets]
        for kind in JUDGMENT_FIELDS:
            group = sorted((j for j in jobs if j["kind"] == kind), key=lambda j: j["job"])
            if not group:
                continue
            random.Random(diff).shuffle(group)
            first = outputs.get(by_job.get(group[0]["job"]))
            batch = first["batch"] if first else opaque(occupied_batches)
            occupied_batches.add(batch)
            texts = []
            for job in group:
                path = artifact(job)
                data = (root / path).read_bytes()
                digest = sha(data)
                done = load(root / path.parent / "done.json")
                digest_field = "brief_sha256" if kind == "writer" else "output_sha256"
                if done.get(digest_field) != digest:
                    raise ValueError(f"{job['job']}: published artifact hash differs")
                if manifest_sha256 is not None and done["inputs"]["manifest_sha256"] != manifest_sha256:
                    raise ValueError(f"{job['job']}: published artifact uses a different manifest")
                entry = {**job, "batch": batch, "artifact": str(path), "text_sha256": digest,
                         "defect_ids": [d["id"] for d in defects[diff]]}
                if kind == "writer" and job["arm"] in {"R", "R-loc"}:
                    log = root / path.parent / "queries.jsonl"
                    entry["query_sha256"] = sha(log.read_bytes())
                oid = by_job.get(job["job"])
                if oid is None:
                    oid = opaque(outputs)
                    outputs[oid] = entry
                elif outputs[oid] != entry:
                    raise ValueError(f"{job['job']}: input or provenance changed since queueing")
                texts.append({"output_id": oid, "text": data.decode("utf-8")})
            payload = {"kind": "review" if kind == "reviewer" else "brief", "defects": defects[diff],
                       "facts": outside if kind == "reviewer" else facts, "items": texts}
            if kind == "writer":
                payload["minimal_sets"] = minimal_sets
            # Publish the key first so an interrupted batch keeps the same opaque IDs.
            save(key_file, key)
            write_task(root / INPUT_PATH / batch, payload)
            hand_items = texts.copy()
            random.Random(f"{diff}/{kind}/hand-check").shuffle(hand_items)
            write_task(root / HAND_PATH / batch, {**payload, "items": hand_items})
    if not key_file.exists():
        save(key_file, key)
    return key


def normalize(record, entry):
    kind = entry["kind"]
    fact_field = "recovered_facts" if kind == "reviewer" else "contained_facts"
    valid_facts = (entry["outside_fact_ids"] if kind == "reviewer"
                   else [fact for group in entry["minimal_sets"] for fact in group])
    facts = record[fact_field]
    if (not isinstance(facts, list) or any(not isinstance(fact, str) for fact in facts)
            or len(facts) != len(set(facts)) or not set(facts) <= set(valid_facts)):
        raise ValueError(f"invalid {fact_field}: {facts}")
    defects = record["defects"]
    if (not isinstance(defects, list)
            or sorted(d["defect_id"] for d in defects) != sorted(entry["defect_ids"])):
        raise ValueError("defect decisions must cover each supplied defect exactly once")
    for defect in defects:
        if (defect["label"] not in {"caught", "partial", "missed"}
                or defect["confidence"] not in {"high", "medium", "low"}
                or not isinstance(defect["rationale"], str)
                or (defect["matching_finding"] is not None
                    and not isinstance(defect["matching_finding"], str))):
            raise ValueError("invalid defect decision")
    matches = sorted(d["defect_id"] for d in defects if d["label"] == "caught")
    if kind == "reviewer":
        count = record["finding_count"]
        if type(count) is not int or count < 0:
            raise ValueError("invalid finding count")
        return {"catch": bool(matches), fact_field: sorted(facts), "matched_defects": matches}
    return {"contains_decisive": any(set(group) <= set(facts) for group in entry["minimal_sets"]),
            fact_field: sorted(facts), "states_defect": bool(matches)}


def validate_payload_verdicts(payload, data):
    """Validate exact batch coverage and each decision before publishing anything."""
    kind = "writer" if payload["kind"] == "brief" else "reviewer"
    rubric = {"kind": kind, "defect_ids": [d["id"] for d in payload["defects"]],
              "outside_fact_ids": [f["id"] for f in payload["facts"] if not f["in_diff"]],
              "minimal_sets": payload.get("minimal_sets", [])}
    expected = {item["output_id"] for item in payload["items"]}
    submitted = {}
    for line in data.decode("utf-8").splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        oid = record["output_id"]
        if oid in submitted or oid not in expected:
            raise ValueError(f"duplicate or unknown verdict ID {oid}")
        submitted[oid] = normalize(record, rubric)
    if set(submitted) != expected:
        raise ValueError(f"missing verdicts {sorted(expected - set(submitted))}")
    return submitted


def collect(root, key, *, hand=False, profile=None):
    inputs, dest = (HAND_PATH, "hand-check") if hand else (INPUT_PATH, "judge")
    batches = defaultdict(dict)
    for oid, entry in key["outputs"].items():
        batch = root / inputs / entry["batch"]
        if batch.is_dir():
            batches[batch][oid] = entry
    for batch, entries in batches.items():
        verdicts = batch / "verdicts.jsonl"
        if not verdicts.exists():
            continue
        data = verdicts.read_bytes()
        proof = batch.parent / "out" / f"task-{batch.name}.json"
        if not proof.is_file():
            continue
        producer = load(proof)["producer"]
        if ((profile is not None and producer["profile"] != profile)
                or producer["config"].get("model") != "fable"
                or producer["config"].get("effort") != "high"
                or producer["profile"] != producer["launch_profile"]):
            raise ValueError(f"{batch.name}: verdicts lack pinned Fable/high provenance")
        if producer.get("output_sha256") != sha(data):
            raise ValueError(f"{batch.name}: verdict file differs from the fetched output")
        other = root / (INPUT_PATH if hand else HAND_PATH) / "out" / proof.name
        if other.exists() and load(other)["producer"]["session_id"] == producer["session_id"]:
            raise ValueError(f"{batch.name}: both passes used the same session")
        payload = load(batch / "items.json")
        bound = {item["output_id"]: sha(item["text"].encode("utf-8")) for item in payload["items"]}
        if set(bound) != set(entries):
            raise ValueError(f"{batch.name}: blinded inputs differ from the private key")
        for oid, entry in entries.items():
            if (bound[oid] != entry["text_sha256"]
                    or sha((root / entry["artifact"]).read_bytes()) != entry["text_sha256"]):
                raise ValueError(f"{oid}: verdict input differs from the hash-bound text")
        submitted = validate_payload_verdicts(payload, data)
        for oid, verdict in submitted.items():
            path = root / dest / f"{oid}.json"
            if path.exists() and load(path) != verdict:
                raise ValueError(f"{path}: verdict differs from the published judgment")
            save(path, verdict)


def records(root, key, directory):
    paths = {oid: root / directory / f"{oid}.json" for oid in key["outputs"]}
    return {oid: load(path) for oid, path in paths.items() if path.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "run", "collect", "hand-check", "collect-hand-check"))
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--diff", action="append", help="prepare only these full diff keys")
    parser.add_argument("--profile", help="pinned Claude profile for run or hand-check")
    parser.add_argument("--concurrency", type=int, default=1)
    args = parser.parse_args()
    runs_sessions = args.action in {"run", "hand-check"}
    hand = args.action in {"hand-check", "collect-hand-check"}
    if runs_sessions and (not args.profile or "," in args.profile):
        parser.error("run and hand-check require one pinned Claude --profile")
    raw_manifest = args.manifest.read_bytes()
    manifest = json.loads(raw_manifest)
    snapshot = load_snapshot(args.manifest, manifest)
    root = args.root.resolve()
    key_path = root / KEY_PATH
    key_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (key_path.parent / "workflow.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.action == "prepare":
            selected = set(args.diff) if args.diff else None
            if selected and selected - set(group_jobs(manifest)):
                parser.error(f"unknown diff keys: {sorted(selected - set(group_jobs(manifest)))}")
            key = prepare(root, manifest, snapshot, load_defects(manifest), selected,
                          manifest_sha256=sha(raw_manifest))
            print(json.dumps({"queued_outputs": len(key["outputs"])}))
            return
        key = load(key_path)
        if key["manifest"] != manifest:
            raise ValueError("manifest differs from the private judge key")
        if runs_sessions:
            tasks = root / (HAND_PATH if hand else INPUT_PATH)
            result = subprocess.run(
                [sys.executable, str(HERE / "verify_driver.py"), str(tasks),
                 "--kind", "task", "--text-only", "--output", "verdicts.jsonl",
                 "--model", "fable", "--profiles", args.profile, "--quota-scope", "fable",
                 "--concurrency", str(args.concurrency)],
                env={**os.environ, "REVIEW_ARMS_DIR": str(root)},
            )
            collect(root, key, hand=hand, profile=args.profile)
            if result.returncode:
                raise SystemExit(result.returncode)
        else:
            collect(root, key, hand=hand, profile=args.profile)


if __name__ == "__main__":
    main()
