"""Host-side, complete answers to frozen RQL queries."""
from contextlib import ExitStack, contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
from driver import expected_provider_model, repair_progress

from .evidence import save_json, sync_directory
from .grammar import MENU, parse
from .prompt import PROMPT, REPAIR, semantics_only
from .runner import (EFFORT, MODEL, QUERY_SESSION_PROTOCOL, RUN_ERRORS, SESSION_ID, DestroyUncertain,
                     IsolationFailure, LaunchHeld, QuotaTurnFailure, SealedRunner, TurnFailure)
from .snapshot import OID, blob_reader, eligible_diffs, manifest_sources, prepared_snapshot, verifier_snapshot
from .validation import canonical, decode, ordered_rows, validate_answer
from .verifier import verify


ROOT = Path(os.environ.get("REVIEW_ARMS_ENGINE_ROOT", "/mnt/optane/review-arms-pilot"))
CONFIG = ROOT / "engine.json"
DOCUMENT_KEYS = ("catalogue", "steps", "menu", "eligible_labels")


def frozen_inputs(manifest_path):
    return read_frozen_inputs(manifest_path, manifest_path.read_bytes())


def read_frozen_inputs(manifest_path, raw):
    manifest = json.loads(raw)
    paths, hashes = manifest["input_paths"], manifest["inputs"]
    data = {}
    for key in DOCUMENT_KEYS:
        path = manifest_path.parent / paths[key]
        content = path.read_bytes()
        expected = hashes[f"{key}_sha256"]
        actual = hashlib.sha256(content).hexdigest()
        if actual != expected:
            raise ValueError(f"frozen engine input digest does not match for {key} at {path}. "
                             f"Expected {expected}, got {actual}.")
        data[key] = content
    provider_version = expected_provider_model(manifest, role="engine")
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


def input_hash(catalogue, menu, model_version, served_steps, menu_sha256, verifier_sha256):
    return digest(catalogue, menu, semantics_only(menu, served_steps), menu_sha256,
                  canonical(served_steps), MODEL, model_version, EFFORT, PROMPT, REPAIR,
                  QUERY_SESSION_PROTOCOL, verifier_sha256)


def file_sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


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


def finish_session(directory, runner, *, accept_answer=False):
    retired, record = move_to_retired(directory)
    try:
        runner.retire(retired, record, require_clean_audit=accept_answer)
    except RUN_ERRORS as error:
        save_json(retired / "retirement-failure.json", {"session_id": record["session_id"], "reason": str(error)})
        # A missing acknowledgement after capture and audit cannot invalidate the answer.
        if accept_answer and not isinstance(error, DestroyUncertain):
            raise


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
                 *, served_steps=tuple(MENU), menu_sha256="", manifest_sha256="",
                 verifier=None, verifier_source=None):
        self.cache = cache
        self.catalogue, self.menu = documents
        self.source = source
        self.runner = runner
        self.model_version = model_version
        self.served_steps = served_steps
        self.verifier = verifier
        self.verifier_source = verifier_source
        self.verifier_sha256 = file_sha256(verifier) if verifier is not None else ""
        self.identity = {"manifest_sha256": manifest_sha256,
                         "engine_inputs_sha256": input_hash(self.catalogue, self.menu, model_version,
                                                            served_steps, menu_sha256, self.verifier_sha256)}

    def session(self, commit, snapshot, record_path):
        runner = self.runner()
        if record_path.exists():
            # An interrupted query or a session left by the old protocol is never reused.
            finish_session(record_path.parent.parent, runner)
        record_path.parent.mkdir(parents=True, exist_ok=True)
        identity = {"commit": commit, **snapshot}
        # An interrupted launch may leave an engine without a recorded ID.
        # Keep the intent until it can be reconciled.
        save_json(record_path, identity)
        try:
            sid = runner.start(snapshot, commit)
        except LaunchHeld:
            record_path.unlink()
            raise
        save_json(record_path, {**identity, "session_id": sid, "instance": runner.instance,
                                "model_version": self.model_version})
        return runner, sid

    def answer(self, commit, query_text, *, fresh=False):
        provenance = {"model": MODEL, "effort": EFFORT, "steps": [], "turns": [],
                      "validation_retries": 0, "session": None, "turn": None, "tokens": {},
                      "fresh": fresh, "context_id": None}

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
            inputs = self.identity["engine_inputs_sha256"]
            provenance["model_version"] = self.model_version
            key = cache_key(commit, query, inputs)
            provenance.update({"inputs_sha256": inputs, "normalized_query": query})
            directory = self.cache / commit
            entry = directory / (key + ".json")
            with locked(directory / ".lock"), ExitStack() as resources:
                if not fresh and entry.exists():
                    return load(entry)
                read_blob, snapshot = self.source(commit)
                verifier_steps = [step for step in steps if step in MENU]
                if verifier_steps:
                    if self.verifier is None or self.verifier_source is None:
                        raise RuntimeError("added RQL steps require a configured host verifier")
                    root = resources.enter_context(self.verifier_source(snapshot))
                active = directory / "active"

                def add_turn(record):
                    provenance["turns"].append(record)
                    provenance["turn"] = record["turn"]
                    provenance["context_id"] = record["context_id"]
                    for counter, tokens in (record["tokens"] or {}).items():
                        previous = provenance["tokens"].get(counter, 0)
                        provenance["tokens"][counter] = previous + tokens

                provenance["retired_sessions"] = []
                for attempt in range(2):
                    runner, sid = self.session(commit, snapshot, active / "session.json")
                    provenance["session"] = sid
                    valid = False
                    try:
                        runner.prepare(sid, snapshot, active)
                        errors = []
                        previous_errors = None
                        while True:
                            output, record, errors = runner.turn(
                                sid, query, self.catalogue, self.menu, errors, active)
                            add_turn(record)
                            evidence = active / "answers" / record["context_id"]
                            findings = {}
                            if errors:
                                normalized = {("answer-capture",)}
                            else:
                                try:
                                    answer = decode(output)
                                except (ValueError, RecursionError) as error:
                                    errors = [f"invalid JSON: {error}"]
                                    normalized = {("invalid-json",)}
                                else:
                                    rows, coverage, errors = validate_answer(answer, read_blob)
                                    normalized = {("validation", *issue.key) for issue in errors}
                                    if verifier_steps and file_sha256(self.verifier) != self.verifier_sha256:
                                        raise RuntimeError("frozen RQL verifier digest changed")
                                    for step in verifier_steps:
                                        findings[step] = verify(self.verifier, root, step, query, answer)
                                        for issue in findings[step]["errors"]:
                                            site = issue["site"]
                                            normalized.add((issue["code"], issue["row"],
                                                            site if isinstance(site, str) else canonical(site)))
                                            errors.append(canonical(issue))
                            if findings:
                                save_json(evidence / "verifier.json", findings)
                            save_json(evidence / "validation.json", {"errors": errors})
                            if not errors:
                                result = response("ok", ordered_rows(rows), coverage)
                                result["excluded"] = answer["excluded"]
                                save_json(evidence / "result.json", result)
                                valid = True
                                break
                            if not repair_progress(previous_errors, normalized):
                                return response("engine_error", message="; ".join(errors))
                            previous_errors = normalized
                            provenance["validation_retries"] += 1
                    except IsolationFailure as error:
                        add_turn(error.record)
                        save_json(active / "void.json", {"session_id": sid, "reason": str(error)})
                        provenance["retired_sessions"].append({"session": sid, "reason": str(error)})
                        if attempt:
                            raise
                    except TurnFailure as error:
                        add_turn(error.record)
                        raise
                    finally:
                        finish_session(directory, runner, accept_answer=valid)
                    if valid:
                        if fresh:
                            entry = directory / "stability" / key / (record["context_id"] + ".json")
                            entry.parent.mkdir(parents=True, exist_ok=True)
                        save(entry, result)
                        return result
        except (LaunchHeld, QuotaTurnFailure) as error:
            return response("quota_hold", message=str(error))
        except (RuntimeError, ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
            return response("engine_error", message=str(error))


def load_engine(config_path=None, *, manifest_path=None):
    config_path = CONFIG if config_path is None else config_path
    config = load(config_path)
    manifest_path = Path(config["manifest"]) if manifest_path is None else manifest_path
    raw = manifest_path.read_bytes()
    manifest, documents, version = read_frozen_inputs(manifest_path, raw)
    verifier = Path(config["verifier"])
    if not verifier.is_absolute() or not verifier.is_file() or not os.access(verifier, os.X_OK):
        raise ValueError(f"RQL verifier is not an absolute executable path: {verifier}")

    def source(revision):
        repo = manifest_sources(manifest, config["repos"])[revision]
        return blob_reader(repo, revision), prepared_snapshot(repo, config["snapshot_repo"], revision)

    engine = Engine(config_path.parent / "engine-cache", documents, source,
                    lambda: SealedRunner({**config, "model_version": version,
                                          "model_display": manifest["model_versions"]["claude"]}, manifest), version,
                    served_steps=manifest["served_steps"], menu_sha256=manifest["inputs"]["menu_sha256"],
                    manifest_sha256=hashlib.sha256(raw).hexdigest(), verifier=verifier,
                    verifier_source=lambda snapshot: verifier_snapshot(config["snapshot_repo"], snapshot["orphan"]))
    engine.manifest = manifest
    engine.config = config
    return engine


def answer(commit, query, *, fresh=False):
    """Answer on the host, optionally making a fresh stability re-ask."""
    return load_engine().answer(commit, query, fresh=fresh)
