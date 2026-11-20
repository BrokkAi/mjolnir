"""Host-side, complete answers to frozen RQL queries."""
from contextlib import contextmanager
import fcntl
import hashlib
import json
from pathlib import Path
import re
import subprocess

from .evidence import save_json, sync_directory
from .grammar import MENU, parse
from .prompt import CONTEXT_PROTOCOL, CONTEXT_RECALL, CONTEXT_SEED, PROMPT, REPAIR, semantics_only
from .runner import EFFORT, MODEL, RUN_ERRORS, SESSION_ID, IsolationFailure, SealedRunner, TurnFailure
from .snapshot import OID, blob_reader, eligible_diffs, manifest_sources, prepared_snapshot
from .validation import canonical, ordered_rows, validate


ROOT = Path("/mnt/optane/review-arms-pilot")
CACHE_ROOT = ROOT / "engine-cache"
CONFIG = ROOT / "engine.json"
DOCUMENT_KEYS = ("catalogue", "steps", "menu", "eligible_labels")


def frozen_inputs(manifest_path):
    manifest = load(manifest_path)
    paths, hashes = manifest["input_paths"], manifest["inputs"]
    data = {}
    for key in DOCUMENT_KEYS:
        path = manifest_path.parent / paths[key]
        content = path.read_bytes()
        if hashlib.sha256(content).hexdigest() != hashes[f"{key}_sha256"]:
            raise ValueError(f"frozen engine input digest does not match: {key}")
        data[key] = content
    provider_version = manifest["model_versions"]["fable_provider_id"]
    if not re.fullmatch(r"claude-fable-[0-9]+(?:-[0-9]+)+", provider_version):
        raise ValueError("frozen manifest must name a concrete Fable provider id")
    served = manifest.get("served_steps", list(MENU))
    if (not isinstance(served, list) or any(not isinstance(step, str) or step not in MENU for step in served)
            or len(set(served)) != len(served)):
        raise ValueError("frozen manifest has invalid served menu steps")
    documents = tuple(data[key].decode("utf-8") for key in ("catalogue", "steps"))
    eligible = eligible_diffs(data["eligible_labels"], manifest["diffs"])
    return {**manifest, "served_steps": served, "eligible_diffs": eligible}, documents, provider_version


def digest(*parts):
    h = hashlib.sha256()
    for part in parts:
        data = part.encode("utf-8")
        h.update(len(data).to_bytes(8, "big"))
        h.update(data)
    return h.hexdigest()


def cache_key(commit, query, inputs):
    return digest(commit, query, inputs)


def input_hash(catalogue, menu, model_version, served_steps, menu_sha256):
    return digest(catalogue, menu, semantics_only(menu, served_steps), menu_sha256,
                  canonical(served_steps), MODEL, model_version, EFFORT, PROMPT, REPAIR,
                  CONTEXT_PROTOCOL, CONTEXT_SEED, CONTEXT_RECALL)


def load(path):
    return json.loads(path.read_text(encoding="utf-8"))


def save(path, value):
    # Callers own the commit lock, so no other process writes this temp file.
    temp = path.with_suffix(".tmp")
    temp.write_text(canonical(value) + "\n", encoding="utf-8")
    temp.replace(path)


@contextmanager
def locked(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def move_to_retired(directory):
    active = directory / "active"
    record = load(active / "session.json")
    sid = record.get("session_id")
    if not isinstance(sid, str) or not SESSION_ID.fullmatch(sid):
        raise RuntimeError(f"cannot retire an unconfirmed engine launch: {active}")
    retired = directory / "retired" / sid
    retired.parent.mkdir(parents=True, exist_ok=True)
    active.rename(retired)
    sync_directory(retired.parent)
    sync_directory(directory)
    return retired, record


def teardown(cache, runner):
    """Retire recorded sessions, including previous attempts with failed capture."""
    accepted, failures = 0, []
    for directory in sorted(cache.glob("*")):
        if not directory.is_dir() or not OID.fullmatch(directory.name):
            continue
        with locked(directory / ".lock"):
            if (directory / "active").exists():
                try:
                    move_to_retired(directory)
                except RUN_ERRORS as error:
                    failures.append(f"{directory.name}: {error}")
            for retired in sorted((directory / "retired").glob("*")):
                if (retired / "destroy-receipt.json").exists():
                    continue
                try:
                    record = load(retired / "session.json")
                    if retired.name != record["session_id"] or record["commit"] != directory.name:
                        raise RuntimeError("retirement directory does not match the recorded session")
                    runner.retire(retired, record)
                    accepted += 1
                except RUN_ERRORS as error:
                    failures.append(f"{retired}: {error}")
    return accepted, failures


class Engine:
    def __init__(self, cache, documents, source, runner, model_version,
                 *, served_steps=tuple(MENU), menu_sha256=""):
        self.cache = cache
        self.catalogue, self.menu = documents
        self.source = source
        self.runner = runner
        self.model_version = model_version
        self.served_steps = served_steps
        self.menu_sha256 = menu_sha256

    def session(self, commit, snapshot, record_path):
        record_path.parent.mkdir(parents=True, exist_ok=True)
        identity = {"commit": commit, **snapshot}
        runner = self.runner()
        record = load(record_path) if record_path.exists() else None
        if record is not None:
            if any(record.get(key) != value for key, value in identity.items()):
                raise RuntimeError(f"existing engine session belongs to a different snapshot: {record_path}")
            if not record.get("session_id"):
                raise RuntimeError(f"uncertain launch for {commit}; reconcile {record_path} before retrying")
        else:
            # An interrupted launch can leave a live engine even if mj never
            # returned its ID. Refuse duplicate engines until it is reconciled.
            save(record_path, identity)
            record = {**identity, "session_id": runner.start(snapshot, commit),
                      "instance": runner.instance, "model_version": self.model_version()}
            save(record_path, record)
        if not record.get("prepared"):
            runner.prepare(record["session_id"], snapshot, record_path.parent)
            save(record_path, {**record, "prepared": True})
        return runner, record["session_id"]

    def answer(self, commit, query_text, *, fresh=False):
        provenance = {"model": MODEL, "effort": EFFORT, "steps": [], "turns": [],
                      "validation_retries": 0, "session": None, "turn": None, "tokens": {},
                      "fresh": fresh, "context_id": None, "context_probe": None}

        def response(status, rows=(), coverage=None, message=None):
            result = {"status": status, "rows": list(rows), "total": len(rows), "provenance": provenance,
                      "coverage": coverage if coverage is not None else {"complete": False, "open": []}}
            if message:
                result["message"] = message
            return result

        try:
            if not isinstance(query_text, str):
                raise ValueError("RQL query must be a string")
            query, steps = parse(query_text, self.catalogue, self.served_steps)
        except ValueError as error:
            return response("parse_error", message=str(error))

        provenance["steps"] = steps
        try:
            if not isinstance(commit, str) or not OID.fullmatch(commit):
                raise ValueError("expected a full introducing commit ID")
            model_version = self.model_version()
            inputs = input_hash(self.catalogue, self.menu, model_version, self.served_steps, self.menu_sha256)
            provenance["model_version"] = model_version
            key = cache_key(commit, query, inputs)
            provenance.update({"inputs_sha256": inputs, "normalized_query": query})
            directory = self.cache / commit
            entry = directory / (key + ".json")
            with locked(directory / ".lock"):
                if not fresh and entry.exists():
                    return load(entry)
                read_blob, snapshot = self.source(commit)
                active = directory / "active"

                def add_turn(record):
                    provenance["turns"].append(record)
                    provenance["turn"] = record["turn"]
                    provenance["context_id"] = record["context_id"]
                    for counter, tokens in (record["tokens"] or {}).items():
                        previous = provenance["tokens"].get(counter, 0)
                        provenance["tokens"][counter] = previous + tokens

                provenance["retired_sessions"] = []
                for replacement in range(2):
                    runner, sid = self.session(commit, snapshot, active / "session.json")
                    provenance["session"] = sid
                    probe = runner.context_probe(sid, active)
                    provenance["context_probe"] = probe
                    if probe["status"] != "passed":
                        raise RuntimeError(probe["message"])
                    errors = []
                    while True:
                        try:
                            output, record, errors = runner.turn(
                                sid, query, self.catalogue, self.menu, errors, active)
                        except IsolationFailure as error:
                            add_turn(error.record)
                            retired, session_record = move_to_retired(directory)
                            provenance["retired_sessions"].append({"session": sid, "context_probe": probe})
                            runner.retire(retired, session_record)
                            if replacement:
                                raise
                            break
                        except TurnFailure as error:
                            add_turn(error.record)
                            raise
                        add_turn(record)
                        if not errors:
                            rows, coverage, errors = validate(output, read_blob)
                        evidence = active / "answers" / record["context_id"]
                        save_json(evidence / "validation.json", {"errors": errors})
                        if not errors:
                            result = response("ok", ordered_rows(rows), coverage)
                            save_json(evidence / "result.json", result)
                            if fresh:
                                context = provenance["context_id"]
                                if not context:
                                    raise RuntimeError("fresh engine answer lacks a confirmed context id")
                                entry = directory / "stability" / key / (context + ".json")
                                entry.parent.mkdir(parents=True, exist_ok=True)
                            save(entry, result)
                            return result
                        if provenance["validation_retries"]:
                            return response("engine_error", message="; ".join(errors))
                        provenance["validation_retries"] += 1
        except (RuntimeError, ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
            return response("engine_error", message=str(error))


def answer(commit, query_text, *, fresh=False):
    """Answer a writer query, or make a separate host-only stability re-ask."""
    try:
        config = load(CONFIG)
        manifest, documents, version = frozen_inputs(Path(config["manifest"]))
    except (OSError, KeyError, ValueError, TypeError) as error:
        return {"status": "engine_error", "rows": [], "total": 0, "provenance": {"fresh": fresh},
                "coverage": {"complete": False, "open": []}, "message": f"cannot verify frozen engine inputs: {error}"}

    def source(revision):
        repo = manifest_sources(manifest, config["repos"])[revision]
        return blob_reader(repo, revision), prepared_snapshot(repo, config["snapshot_repo"], revision)

    return Engine(CACHE_ROOT, documents, source,
                  lambda: SealedRunner({**config, "model_version": version}, manifest), lambda: version,
                  served_steps=manifest["served_steps"], menu_sha256=manifest["inputs"]["menu_sha256"]
                  ).answer(commit, query_text, fresh=fresh)
