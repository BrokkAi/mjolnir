#!/usr/bin/env python3
"""Prepare both blinded passes, then run or collect independent Sonnet/high judgments."""
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

from audit import PASSING_HIT_KINDS
from pilot_lock import pilot_lock

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


def run_artifact(runs, job, manifest_sha256=None):
    path = artifact(job)
    source = runs / path.relative_to("runs")
    directory = source.parent
    if not all(p.is_file() for p in (source, directory / "done.json", directory / "usage.json")):
        return None
    done = load(directory / "done.json")
    data = source.read_bytes()
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
            return None
        entry["query_sha256"] = sha(log.read_bytes())
    if kind == "reviewer" and job["arm"] != "N":
        supplied = done.get("inputs", {})
        entry["brief"] = {"session_id": supplied.get("writer_session_id"), "sha256": supplied.get("brief_sha256")}
    return entry, data


def brief_binding(writer):
    return {"session_id": writer["session_id"], "sha256": writer["text_sha256"]}


def run_artifacts(root, jobs, manifest_sha256):
    """Return only current completed job artifacts and their checked receipts."""
    current = {}
    for job in jobs:
        if result := run_artifact(root / "runs", job, manifest_sha256):
            current[job["job"]] = result

    for job, (entry, _) in list(current.items()):
        if "brief" not in entry:
            continue
        writer_job = job.rsplit("/", 1)[0] + "/writer"
        writer = current.get(writer_job)
        if writer and entry["brief"] == brief_binding(writer[0]):
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


def isolation_evidence(root, runs):
    for kind, pattern in (("writer", "**/writer*"), ("reviewer", "**/reviewers/*")):
        for directory in runs.glob(pattern):
            yield kind, directory, "launch.json", Path("runs") / directory.relative_to(runs)
    for pattern in ("*/active", "*/retired/*"):
        for directory in (root / "engine-cache").glob(pattern):
            yield "engine", directory, "session.json", directory.relative_to(root)
    for role, kind in (("terra", "engine"), ("adjudicator", "judge")):
        for directory in (root / "fidelity").glob(f"**/{role}"):
            yield kind, directory, "session.json", directory.relative_to(root)
    retired = root / KEY_PATH.parent / "retired"
    retired_batches = sorted(retired.iterdir()) if retired.is_dir() else ()
    for base in (root, *retired_batches):
        for phase in (INPUT_PATH, HAND_PATH):
            for directory in (base / phase).glob("sessions/*/*"):
                yield "judge", directory, "launch.json", directory.relative_to(root)


def isolation_report(egress=(), evidence=()):
    kinds = ("writer", "reviewer", "engine", "judge")
    void = defaultdict(lambda: defaultdict(dict))
    screened, edits, attempts = defaultdict(set), defaultdict(set), []

    def blocking(reason):
        return not isinstance(reason, dict) or reason.get("kind") not in PASSING_HIT_KINDS

    def record_void(kind, sid, reasons):
        for reason in reasons:
            if blocking(reason):
                void[kind][sid][json.dumps(reason, sort_keys=True)] = reason

    for kind, directory, launch_name, relative in evidence:
        launch = directory / launch_name
        if not launch.is_file() or not (sid := load(launch).get("session_id")):
            continue
        attempts.append({"kind": kind, "session_id": sid, "path": relative.as_posix()})
        marker = directory / "void.json"
        if marker.is_file():
            record_void(kind, sid, [load(marker)["reason"]])
        failed = directory / "failed.json"
        if failed.is_file() and (reason := load(failed).get("void_reason")):
            record_void(kind, sid, reason if isinstance(reason, list) else [reason])
        for path in sorted(directory.glob("*audit.json")):
            report = load(path)
            hits = report.get("hits", ())
            if report["passed"] is False:
                reasons = [hit for hit in hits if blocking(hit)]
                record_void(kind, sid, reasons or ["transcript audit failed"])
            for hit in hits:
                if hit["kind"] == "interpreter-screened":
                    # Turn and retirement audits may cover the same transcript.
                    screened[kind].add((sid, hit["seq"]))
                elif hit["kind"] == "in-place-edit":
                    edits[kind].add((sid, hit["seq"], hit.get("edit_index", 0)))
    return {
        **({"egress_source": "missing"} if egress is None else {"egress_measurements": list(egress)}),
        "void_sessions": {kind: len(void[kind]) for kind in kinds},
        "void_reasons": {kind: {sid: [reasons[key] for key in sorted(reasons)]
                               for sid, reasons in sorted(void[kind].items())} for kind in kinds},
        "interpreter_screened": {kind: len(screened[kind]) for kind in kinds},
        "in_place_edits": {kind: len(edits[kind]) for kind in kinds},
        "attempts": sorted(attempts, key=lambda attempt: (attempt["kind"], attempt["path"], attempt["session_id"])),
        "enforcement_gap": "Podman does not restrict pilot egress to providers. The engine's probes "
                           "measure reachability of selected non-provider hosts. Stored transcript audits "
                           "screen observed commands and output and cannot establish unobserved activity.",
        "scope": "Counts cover stored current and superseded attempts. Engine counts include fidelity "
                 "enumerators. Judge counts include fidelity adjudicators and blind hand checks.",
    }


def read_egress(path, *, required=False, expected_sha256=None):
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        if required:
            raise
        return {"path": str(path), "sha256": None, "status": "uncaptured"}, None
    source = {"path": str(path), "sha256": sha(raw)}
    if expected_sha256 is not None and source["sha256"] != expected_sha256:
        return source, None
    records = [json.loads(line) for line in raw.splitlines() if line.strip()]
    return source, records


def frozen_egress(root, key, egress_log):
    previous = key.get("egress_source")
    default_path = Path(previous["path"]) if previous is not None else (root / "egress.jsonl").resolve()
    path = egress_log.resolve() if egress_log is not None else default_path
    same_source = previous is not None and str(path) == previous["path"]
    if same_source and previous["sha256"] is not None:
        if egress_log is not None:
            # An explicit path must still exist even when its capture is immutable.
            with path.open("rb"):
                pass
        return previous, key["isolation"]["egress_measurements"]
    return read_egress(path, required=egress_log is not None)


def prepare(root, manifest, snapshot, defects, selected=None, *, manifest_sha256=None, egress_log=None):
    with pilot_lock(root):
        return _prepare_locked(root, manifest, snapshot, defects, selected, manifest_sha256, egress_log)


def _prepare_locked(root, manifest, snapshot, defects, selected, manifest_sha256, egress_log):
    key_file = root / KEY_PATH
    key = load(key_file) if key_file.exists() else {"version": 1, "manifest": manifest, "outputs": {}}
    if key["manifest"] != manifest:
        raise ValueError("manifest differs from the private judge key")
    pinned = key.get("manifest_sha256")
    if pinned is not None and manifest_sha256 is not None and pinned != manifest_sha256:
        raise ValueError("manifest SHA-256 differs from the private judge key")
    pin_manifest = pinned is None and manifest_sha256 is not None
    if pin_manifest:
        key["manifest_sha256"] = manifest_sha256
    previous = key.get("egress_source"), key.get("isolation")
    source, egress = frozen_egress(root, key, egress_log)
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
    isolation = isolation_report(egress, isolation_evidence(root, root / "runs"))
    if previous[0] is not None and previous[0]["path"] != source["path"]:
        key.setdefault("egress_source_history", []).append(previous[0])
    key.update(egress_source=source, isolation=isolation)
    if not key_file.exists() or pin_manifest or previous != (source, isolation):
        save(key_file, key)
    return key


EVIDENCE_FILES = ("transcript.json", "usage.json", "audit.json", "verdicts.jsonl", "verification.json")


def verify_capture(attempt, receipt, expected):
    """Recheck the host evidence before publishing a judgment or retiring its session."""
    from driver import provider_model

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
    claims = {f"matched_defect:{defect}" for defect in matches}
    if kind == "reviewer":
        count = record["finding_count"]
        if type(count) is not int or count < 0:
            raise ValueError("invalid finding count")
        claims.update(f"recovered_fact:{fact}" for fact in facts)
        if matches:
            claims.add("catch")
        return {"catch": bool(matches), fact_field: sorted(facts), "matched_defects": matches}, claims
    claims.update(f"contained_fact:{fact}" for fact in facts)
    if matches:
        claims.add("leak")
    return ({"contains_decisive": any(set(group) <= set(facts) for group in entry["minimal_sets"]),
             fact_field: sorted(facts), "states_defect": bool(matches)}, claims)


def quote_errors(record, text, claims):
    evidence = record.get("evidence", [])
    if not isinstance(evidence, list):
        yield "invalid-evidence", "evidence"
        evidence = []
    output = " ".join(text.split())
    credited = set()
    for item in evidence:
        if (not isinstance(item, dict) or set(item) != {"claim", "quote"}
                or not isinstance(item["claim"], str) or not isinstance(item["quote"], str)):
            yield "invalid-evidence", "evidence"
            continue
        claim = item["claim"]
        if claim in claims:
            credited.add(claim)
        quote = " ".join(item["quote"].split())
        if not quote or quote not in output:
            yield "unmatched-quote", claim if claim in claims else "evidence"
    for claim in claims - credited:
        yield "missing-quote", claim


def check_payload_verdicts(payload, data):
    """Return verified rows and deterministic errors keyed by code, row and site."""
    kind = "writer" if payload["kind"] == "brief" else "reviewer"
    rubric = {"kind": kind, "defect_ids": [d["id"] for d in payload["defects"]],
              "outside_fact_ids": [f["id"] for f in payload["facts"] if not f["in_diff"]],
              "minimal_sets": payload.get("minimal_sets", [])}
    texts = {item["output_id"]: item["text"] for item in payload["items"]}
    submitted, seen, errors = {}, set(), set()
    try:
        lines = data.decode("utf-8").split("\n")
    except UnicodeDecodeError:
        lines = []
        errors.add(("invalid-utf8", "", "verdicts.jsonl"))
    for number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError:
            errors.add(("invalid-json", "", f"line {number}"))
            continue
        oid = record.get("output_id") if isinstance(record, dict) else None
        if not isinstance(oid, str) or oid not in texts:
            errors.add(("unknown-verdict-id", "", f"line {number}"))
            continue
        if oid in seen:
            submitted.pop(oid, None)
            errors.add(("duplicate-verdict-id", oid, "output_id"))
            continue
        seen.add(oid)
        try:
            verdict, claims = normalize(record, rubric)
        except (KeyError, TypeError, ValueError):
            errors.add(("invalid-verdict", oid, "record"))
            continue
        problems = set(quote_errors(record, texts[oid], claims))
        if problems:
            errors.update((code, oid, site) for code, site in problems)
        else:
            submitted[oid] = verdict
    errors.update(("missing-verdict", oid, "output_id") for oid in texts.keys() - seen)
    if any(not row for _, row, _ in errors):
        submitted.clear()  # Ambiguous records cannot be assigned to individual outputs.
    return submitted, [{"code": code, "row": row, "site": site} for code, row, site in sorted(errors)]


def error_set(errors):
    return {(error["code"], error["row"], error["site"]) for error in errors}


def verification_record(errors):
    return {"state": "unscored" if errors else "verified", "errors": errors}


def validate_payload_verdicts(payload, data, *, verification=None):
    submitted, errors = check_payload_verdicts(payload, data)
    if verification is None:
        if errors:
            raise ValueError(f"invalid judgment: {errors}")
    elif verification != verification_record(errors):
        raise ValueError("judgment verification differs from the recorded errors")
    return submitted


def collect(root, key, *, hand=False, profile=None):
    from driver import PILOT_CLAUDE_EFFORT, PILOT_CLAUDE_MODEL, expected_provider_model

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
        attempt = batch.parent / "sessions" / batch.name / "attempt"
        producer = verify_capture(attempt, receipt, expected_provider_model(key["manifest"]))
        if ((profile is not None and producer["profile"] != profile)
                or producer["config"].get("model") != PILOT_CLAUDE_MODEL
                or producer["config"].get("effort") != PILOT_CLAUDE_EFFORT
                or producer["profile"] != producer["launch_profile"]):
            raise ValueError(f"{batch.name}: verdicts lack pinned Sonnet/high provenance")
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
        submitted = validate_payload_verdicts(payload, data, verification=load(attempt / "verification.json"))
        for oid, verdict in submitted.items():
            path = root / dest / f"{oid}.json"
            if path.exists() and load(path) != verdict:
                raise ValueError(f"{path}: verdict differs from the published judgment")
            save(path, verdict)


def records(root, key, directory):
    paths = {oid: root / directory / f"{oid}.json" for oid in key["outputs"]}
    return {oid: load(path) for oid, path in paths.items() if path.is_file()}


def main():
    from driver import PILOT_CLAUDE_MODEL, expected_provider_model

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
    parser.add_argument("--egress-log", type=Path, help="stored record_egress.py JSONL to freeze in the judge key")
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
                          manifest_sha256=sha(raw_manifest), egress_log=args.egress_log)
            print(json.dumps({"queued_outputs": len(key["outputs"])}))
            return
        key = load(key_path)
        if key["manifest"] != manifest:
            raise ValueError("manifest differs from the private judge key")
        selected = {entry["diff"] for entry in key["outputs"].values()}
        if not selected:
            return
        defects = load_defects(manifest)
        key = prepare(root, manifest, snapshot, defects, selected, manifest_sha256=sha(raw_manifest),
                      egress_log=args.egress_log)
        if runs_sessions:
            tasks = root / (HAND_PATH if hand else INPUT_PATH)
            if not tasks.is_dir():
                return
            result = subprocess.run(
                [sys.executable, str(HERE / "verify_driver.py"), str(tasks),
                 "--kind", "task", "--text-only", "--output", "verdicts.jsonl",
                 "--model", PILOT_CLAUDE_MODEL, "--profiles", args.profile,
                 "--concurrency", str(args.concurrency), "--instance", args.instance,
                 "--bundle", args.bundle, "--repo-commit", args.repo_commit,
                 "--seal-probe", str(args.seal_probe.resolve()),
                 "--provider-model", expected_provider_model(manifest),
                 "--model-version", manifest["model_versions"]["claude"]],
                env={**os.environ, "REVIEW_ARMS_DIR": str(root)},
            )
            key = prepare(root, manifest, snapshot, defects, selected, manifest_sha256=sha(raw_manifest),
                          egress_log=args.egress_log)
            collect(root, key, hand=hand, profile=args.profile)
            if result.returncode:
                raise SystemExit(result.returncode)
        else:
            collect(root, key, hand=hand, profile=args.profile)


if __name__ == "__main__":
    main()
