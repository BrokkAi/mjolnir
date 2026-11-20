"""Host-side, complete answers to frozen RQL queries."""
from contextlib import contextmanager
import fcntl
import hashlib
import json
from pathlib import Path
import subprocess

from census_label import CENSUS

from .grammar import parse
from .prompt import PROMPT, REPAIR, semantics_only
from .runner import EFFORT, MODEL, SealedRunner, TurnFailure
from .snapshot import OID, blob_reader, manifest_sources, prepared_snapshot
from .validation import canonical, ordered_rows, validate


ROOT = Path("/mnt/optane/review-arms-pilot")
CACHE_ROOT = ROOT / "engine-cache"
CONFIG = ROOT / "engine.json"
DOCUMENTS = ("rql-documented-steps.md", "menu-steps.md")


def read_documents():
    staging = Path(__file__).resolve().parents[2] / "brief/rql-census"
    return tuple(next(path for path in (staging / name, CENSUS / name) if path.is_file())
                 .read_text(encoding="utf-8") for name in DOCUMENTS)


def digest(*parts):
    h = hashlib.sha256()
    for part in parts:
        data = part.encode("utf-8")
        h.update(len(data).to_bytes(8, "big"))
        h.update(data)
    return h.hexdigest()


def cache_key(commit, query, inputs):
    return digest(commit, query, inputs)


def input_hash(catalogue, menu, model_version):
    return digest(catalogue, menu, semantics_only(menu), MODEL, model_version, EFFORT, PROMPT, REPAIR)


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


class Engine:
    def __init__(self, cache, documents, source, runner, model_version):
        self.cache = cache
        self.catalogue, self.menu = documents
        self.source = source
        self.runner = runner
        self.model_version = model_version

    def session(self, commit, snapshot, record_path):
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
            record = {**identity, "session_id": runner.start(snapshot, commit)}
            save(record_path, record)
        if not record.get("prepared"):
            runner.prepare(record["session_id"], snapshot, record_path.parent)
            save(record_path, {**record, "prepared": True})
        return runner, record["session_id"]

    def answer(self, commit, query_text):
        provenance = {"model": MODEL, "effort": EFFORT, "steps": [], "turns": [],
                      "validation_retries": 0, "session": None, "turn": None, "tokens": {}}

        def response(status, rows=(), coverage=None, message=None):
            result = {"status": status, "rows": list(rows), "total": len(rows), "provenance": provenance,
                      "coverage": coverage if coverage is not None else {"complete": False, "open": []}}
            if message:
                result["message"] = message
            return result

        try:
            if not isinstance(query_text, str):
                raise ValueError("RQL query must be a string")
            query, steps = parse(query_text, self.catalogue)
        except ValueError as error:
            return response("parse_error", message=str(error))

        provenance["steps"] = steps
        try:
            if not isinstance(commit, str) or not OID.fullmatch(commit):
                raise ValueError("expected a full introducing commit ID")
            model_version = self.model_version()
            inputs = input_hash(self.catalogue, self.menu, model_version)
            provenance["model_version"] = model_version
            key = cache_key(commit, query, inputs)
            provenance.update({"inputs_sha256": inputs, "normalized_query": query})
            directory = self.cache / commit
            entry = directory / (key + ".json")
            with locked(directory / ".lock"):
                if entry.exists():
                    return load(entry)
                read_blob, snapshot = self.source(commit)
                runner, sid = self.session(commit, snapshot, directory / "session.json")
                provenance["session"] = sid

                def add_turn(record):
                    provenance["turns"].append(record)
                    provenance["turn"] = record["turn"]
                    for counter, tokens in (record["tokens"] or {}).items():
                        previous = provenance["tokens"].get(counter, 0)
                        provenance["tokens"][counter] = previous + tokens

                errors = []
                for attempt in range(2):
                    provenance["validation_retries"] = attempt
                    try:
                        output, record, errors = runner.turn(sid, query, self.catalogue, self.menu, errors, directory)
                    except TurnFailure as error:
                        add_turn(error.record)
                        raise
                    add_turn(record)
                    if not errors:
                        rows, coverage, errors = validate(output, read_blob)
                    if not errors:
                        result = response("ok", ordered_rows(rows), coverage)
                        save(entry, result)
                        return result
                return response("engine_error", message="; ".join(errors))
        except (RuntimeError, ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
            return response("engine_error", message=str(error))


def answer(commit, query_text):
    """Server entry point. Parsing and cache hits require no session commands."""
    config, manifest = None, None

    def configuration():
        nonlocal config
        if config is None:
            config = load(CONFIG)
        return config

    def model_version():
        version = configuration()["model_version"]
        if not isinstance(version, str) or not version.startswith("claude-fable-") or version == "claude-fable-":
            raise ValueError("configure the resolved Claude Fable model version")
        return version

    def study_manifest():
        nonlocal manifest
        if manifest is None:
            manifest = load(Path(configuration()["manifest"]))
        return manifest

    def source(revision):
        settings = configuration()
        repos = manifest_sources(study_manifest(), settings["repos"])
        repo = repos[revision]
        return blob_reader(repo, revision), prepared_snapshot(repo, settings["snapshot_repo"], revision)

    try:
        documents = read_documents()
    except (OSError, StopIteration, UnicodeError) as error:
        return {"status": "engine_error", "rows": [], "total": 0, "provenance": {},
                "coverage": {"complete": False, "open": []}, "message": f"cannot read frozen engine documents: {error}"}
    return Engine(CACHE_ROOT, documents, source, lambda: SealedRunner(configuration(), study_manifest()),
                  model_version).answer(commit, query_text)
