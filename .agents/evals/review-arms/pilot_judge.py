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
import re
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


def run_artifacts(root, jobs, manifest_sha256):
    """Return only current completed job artifacts and their checked receipts."""
    current = {}
    for job in jobs:
        path = artifact(job)
        directory = root / path.parent
        if not all(p.is_file() for p in (root / path, directory / "done.json", directory / "usage.json")):
            continue
        done = load(directory / "done.json")
        data = (root / path).read_bytes()
        kind = job["kind"]
        digest_field = "brief_sha256" if kind == "writer" else "output_sha256"
        if done[digest_field] != sha(data) or not done["session_id"]:
            raise ValueError(f"{job['job']}: published artifact lacks a matching receipt")
        if manifest_sha256 is not None and done["inputs"]["manifest_sha256"] != manifest_sha256:
            raise ValueError(f"{job['job']}: published artifact uses a different manifest")
        entry = {**job, "artifact": str(path), "text_sha256": done[digest_field],
                 "session_id": done["session_id"]}
        if kind == "writer" and job["arm"] in {"R", "R-loc"}:
            log = directory / f"queries-{entry['session_id']}.jsonl"
            if not log.is_file():
                continue
            entry["query_sha256"] = sha(log.read_bytes())
        if kind == "reviewer" and job["arm"] != "N":
            supplied = done.get("inputs", {})
            entry["brief"] = {"session_id": supplied.get("writer_session_id"), "sha256": supplied.get("brief_sha256")}
        current[job["job"]] = entry, data

    for job, (entry, _) in list(current.items()):
        if "brief" not in entry:
            continue
        writer_job = job.rsplit("/", 1)[0] + "/writer"
        writer = current.get(writer_job)
        if writer and entry["brief"] == {"session_id": writer[0]["session_id"], "sha256": writer[0]["text_sha256"]}:
            continue
        print(json.dumps({"job": job, "state": "stale-review", "reviewed_brief": entry["brief"],
                          "current_writer": writer[0]["session_id"] if writer else None}), file=sys.stderr)
        del current[job]
    return current


def retire_batch(root, key, batch):
    """Keep old batches, verdicts and evidence outside the active collection."""
    destination = root / KEY_PATH.parent / "retired" / batch
    members = {oid: entry for oid, entry in key["outputs"].items() if entry["batch"] == batch}
    if not (destination / "key.json").is_file():
        save(destination / "key.json", members)
    paths = []
    for inputs, judgments in ((INPUT_PATH, "judge"), (HAND_PATH, "hand-check")):
        paths.extend(inputs / part for part in (
            Path(batch), Path("out") / f"task-{batch}.json", Path("out") / f"invalid-{batch}.json",
            Path("sessions") / batch, Path("sessions") / f"batch-{batch}.json",
            Path("sessions") / f"usage-{batch}.json", Path("sessions") / f"unit-{batch}.lock",
            Path("prompts") / f"prompt-{batch}.txt",
        ))
        paths.extend(Path(judgments) / f"{oid}.json" for oid in members)
    for path in paths:
        source, target = root / path, destination / path
        if source.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            source.rename(target)
    for oid in members:
        del key["outputs"][oid]
    save(root / KEY_PATH, key)


def prepare(root, manifest, snapshot, defects, selected=None, *, manifest_sha256=None):
    key_file = root / KEY_PATH
    key = load(key_file) if key_file.exists() else {"version": 1, "manifest": manifest, "outputs": {}}
    if key["manifest"] != manifest:
        raise ValueError("manifest differs from the private judge key")
    outputs = key["outputs"]
    retired = root / KEY_PATH.parent / "retired"
    for batch in {entry["batch"] for entry in outputs.values()}:
        if (retired / batch / "key.json").is_file():
            retire_batch(root, key, batch)
    occupied_batches = {entry["batch"] for entry in outputs.values()}
    if retired.is_dir():
        occupied_batches.update(path.name for path in retired.iterdir())
    by_diff = group_jobs(manifest)
    diffs = sorted(by_diff)
    random.Random(manifest["seed"]).shuffle(diffs)
    for diff in diffs:
        if selected is not None and diff not in selected:
            continue
        jobs = by_diff[diff]
        current = run_artifacts(root, jobs, manifest_sha256)
        defect_ids = [d["id"] for d in defects[diff]]
        scoring_inputs = sha(json.dumps({
            "defects": defects[diff],
            "pilot_sha256": sha((HERE / "judge/PILOT.md").read_bytes()),
            "spec_sha256": sha((HERE / "judge/SPEC.md").read_bytes()),
        }, sort_keys=True, separators=(",", ":")).encode())
        for entry, _ in current.values():
            entry["defect_ids"] = defect_ids
            entry["scoring_inputs"] = scoring_inputs
        for batch in {entry["batch"] for entry in outputs.values()
                      if entry["diff"] == diff and (entry["job"] not in current
                          or entry != {**current[entry["job"]][0], "batch": entry["batch"]})}:
            retire_batch(root, key, batch)
        if len(current) != len(jobs):
            continue
        sets = snapshot[diff]
        facts = [fact for group in sets for fact in group]
        outside = [fact for fact in facts if not fact["in_diff"]]
        minimal_sets = [[fact["id"] for fact in group] for group in sets]
        by_job = {entry["job"]: oid for oid, entry in outputs.items() if entry["diff"] == diff}
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
                name = job["job"]
                entry, data = current[name]
                oid = by_job.get(name)
                if oid is None:
                    oid = opaque(outputs)
                    outputs[oid] = {**entry, "batch": batch}
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


def expected_provider_model(manifest):
    version = manifest["model_versions"]["fable"]
    if not re.fullmatch(r"Fable [0-9]+(?:\.[0-9]+)*", version):
        raise ValueError(f"unrecognized frozen Fable version {version!r}")
    return "claude-fable-" + version.removeprefix("Fable ").replace(".", "-")


def provider_model(usage, command, turn, expected):
    turns = usage["turns"]
    if len(turns) != 1 or (turns[0]["command_id"], turns[0]["accepted_ordinal"]) != (command, turn):
        raise ValueError("provider usage does not identify exactly the judge's turn")
    if usage["turn_selections"].get(command) != {"model": "fable", "effort": "high"}:
        raise ValueError("provider usage does not attribute the judge turn to Fable/high")
    models = turns[0]["usage"]["provider_details"]["model_usage"]
    fable = {model.split("[", 1)[0] for model in models if model.startswith("claude-fable-")}
    if fable != {expected}:
        raise ValueError(f"expected provider usage for {expected}, found {sorted(fable)}")
    return expected


EVIDENCE_FILES = ("transcript.json", "usage.json", "audit.json", "verdicts.jsonl")


def verify_capture(attempt, receipt, expected):
    """Recheck the host evidence before publishing a judgment or retiring its session."""
    producer = receipt["producer"]
    launch = load(attempt / "launch.json")
    if not producer.get("sealed") or producer["session_id"] != launch["session_id"]:
        raise ValueError(f"{attempt}: receipt does not match the sealed launch")
    for name in EVIDENCE_FILES:
        if sha((attempt / name).read_bytes()) != receipt["evidence_sha256"][name]:
            raise ValueError(f"{attempt / name}: evidence differs from its receipt")
    if load(attempt / "audit.json")["passed"] is not True:
        raise ValueError(f"{attempt}: transcript audit did not pass")
    model = provider_model(load(attempt / "usage.json"), producer["command"], producer["turn"], expected)
    if producer["provider_model"] != model:
        raise ValueError(f"{attempt}: receipt's provider model differs from usage")
    return producer


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
        receipt = load(proof)
        producer = verify_capture(batch.parent / "sessions" / batch.name / "attempt",
                                  receipt, expected_provider_model(key["manifest"]))
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
    parser.add_argument("--instance", help="dedicated named Mjolnir instance")
    parser.add_argument("--bundle", help="bundle with a single empty parentless Git commit")
    parser.add_argument("--repo-commit", help="the empty bundle's pinned commit")
    parser.add_argument("--seal-probe", type=Path, help="trusted host isolation probe; receives the container ID")
    args = parser.parse_args()
    runs_sessions = args.action in {"run", "hand-check"}
    hand = args.action in {"hand-check", "collect-hand-check"}
    if runs_sessions and (not args.profile or "," in args.profile):
        parser.error("run and hand-check require one pinned Claude --profile")
    if runs_sessions and not all((args.instance, args.bundle, args.repo_commit, args.seal_probe)):
        parser.error("run and hand-check require --instance, --bundle, --repo-commit and --seal-probe")
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
        selected = {entry["diff"] for entry in key["outputs"].values()}
        if not selected:
            return
        defects = load_defects(manifest)
        key = prepare(root, manifest, snapshot, defects, selected, manifest_sha256=sha(raw_manifest))
        if runs_sessions:
            tasks = root / (HAND_PATH if hand else INPUT_PATH)
            if not tasks.is_dir():
                return
            result = subprocess.run(
                [sys.executable, str(HERE / "verify_driver.py"), str(tasks),
                 "--kind", "task", "--text-only", "--output", "verdicts.jsonl",
                 "--model", "fable", "--profiles", args.profile, "--quota-scope", "fable",
                 "--concurrency", str(args.concurrency), "--instance", args.instance,
                 "--bundle", args.bundle, "--repo-commit", args.repo_commit,
                 "--seal-probe", str(args.seal_probe.resolve()),
                 "--provider-model", expected_provider_model(manifest)],
                env={**os.environ, "REVIEW_ARMS_DIR": str(root)},
            )
            key = prepare(root, manifest, snapshot, defects, selected, manifest_sha256=sha(raw_manifest))
            collect(root, key, hand=hand, profile=args.profile)
            if result.returncode:
                raise SystemExit(result.returncode)
        else:
            collect(root, key, hand=hand, profile=args.profile)


if __name__ == "__main__":
    main()
