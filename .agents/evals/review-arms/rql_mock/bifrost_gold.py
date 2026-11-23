"""Real Bifrost runner and candidate normalizer for documented fidelity queries."""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import selectors
import shutil
import signal
import subprocess
import threading
import time
from uuid import uuid4

import driver

from .engine.grammar import parse_query
from .engine.snapshot import blob_reader
from .engine.validation import SourceEvidence, canonical, ordered_rows


# Documented maximum terminal row limit in `bifrost --help query_code` ("maximum 1000").
BIFROST_RESULT_LIMIT = 1000
# Cgroup memory cap from PLAN-rql-census.md ("systemd-run --user -p MemoryMax=48G -p MemorySwapMax=0").
BIFROST_MEMORY_CAP_GIB = 48
# Index readiness and per-query boundaries matching bifrost-today/report/README.md.
BIFROST_INDEX_TIMEOUT_S = 3600
BIFROST_QUERY_TIMEOUT_S = 1800
BIFROST_INTERNAL_KEYS = frozenset({
    "provenance", "id", "site_id", "program_point_id", "artifact_id", "ast_id", "target_id",
})
QUERY_FILENAME = ".rql-fidelity-query.rql"


def sha256_file(path: Path) -> str:
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def wrap_query_with_limit(query_text: str, limit: int = BIFROST_RESULT_LIMIT) -> str:
    """Wrap an un-limited documented RQL query in `(limit N ...)` so Bifrost uses its max cap."""
    parsed = parse_query(query_text)
    if isinstance(parsed, list) and parsed and parsed[0] == "limit":
        return query_text
    return f"(limit {limit} {query_text})"


def _canonical_relpath(path):
    if not isinstance(path, str):
        return None
    cleaned = path.replace("\\", "/")
    while cleaned.startswith("./"):
        cleaned = cleaned[2:]
    if (not cleaned or "\x00" in cleaned or PurePosixPath(cleaned).is_absolute()
            or any(part in {"", ".", "..", ".git"} for part in cleaned.split("/"))):
        return None
    return cleaned


def _extract_span(obj):
    if not isinstance(obj, dict):
        return None
    span = obj.get("range")
    if isinstance(span, dict) and type(span.get("start_line")) is int and type(span.get("end_line")) is int:
        return span["start_line"], span["end_line"]
    if type(obj.get("start_line")) is int and type(obj.get("end_line")) is int:
        return obj["start_line"], obj["end_line"]
    return None


def _collect_nested_sites(value, default_path, source, seen, sites):
    if isinstance(value, list):
        for item in value:
            _collect_nested_sites(item, default_path, source, seen, sites)
        return
    if not isinstance(value, dict):
        return
    local_path = _canonical_relpath(value.get("path")) if "path" in value else default_path
    span = _extract_span(value)
    if local_path is not None and span is not None:
        lines = source.lines(local_path)
        start, end = span
        if lines is not None and 1 <= start <= end <= len(lines):
            key = (local_path, start, end)
            if key not in seen:
                seen.add(key)
                sites.append({"path": local_path, "start_line": start, "end_line": end})
    for key, child in value.items():
        if key in BIFROST_INTERNAL_KEYS or key in {"range", "path", "start_line", "end_line"}:
            continue
        if isinstance(child, (dict, list)):
            _collect_nested_sites(child, local_path or default_path, source, seen, sites)


def _strip_internals(value, *, top=False):
    if isinstance(value, list):
        return [_strip_internals(item) for item in value]
    if isinstance(value, dict):
        skip = BIFROST_INTERNAL_KEYS | ({"result_type"} if top else set())
        return {k: _strip_internals(v) for k, v in value.items() if k not in skip}
    return value


def normalize_bifrost_results(raw, read_blob):
    """Translate raw `bifrost --query-file` JSON into anonymous normalized fidelity candidates."""
    if not isinstance(raw, dict):
        raise RuntimeError("bifrost returned non-object JSON")
    if raw.get("isError"):
        raise RuntimeError(f"bifrost reported query error: {raw.get('error') or raw}")
    content = raw.get("structuredContent")
    if not isinstance(content, dict) or not isinstance(content.get("results"), list):
        raise RuntimeError("bifrost output is missing structuredContent.results")

    results = content["results"]
    truncated = bool(content.get("truncated", False))
    diagnostics = content.get("diagnostics") or []
    source = SourceEvidence(read_blob)
    rows = []
    skipped_external = 0
    skipped_empty_file = 0

    for index, item in enumerate(results):
        if not isinstance(item, dict):
            raise ValueError(f"bifrost results[{index}] is not an object")
        kind = item.get("result_type")
        if not isinstance(kind, str) or not kind:
            raise ValueError(f"bifrost results[{index}] is missing result_type")
        raw_path = item.get("path")
        if raw_path is None and isinstance(item.get("declaration"), dict):
            raw_path = item["declaration"].get("path")
        relpath = _canonical_relpath(raw_path)
        lines = source.lines(relpath) if relpath is not None else None
        if lines is None:
            skipped_external += 1
            continue

        span = _extract_span(item)
        if span is None and isinstance(item.get("declaration"), dict):
            decl = item["declaration"]
            decl_path = _canonical_relpath(decl.get("path"))
            if decl_path in (None, relpath):
                span = _extract_span(decl)
        if span is None:
            if kind == "file":
                if not lines:
                    skipped_empty_file += 1
                    continue
                span = (1, 1)
            else:
                raise ValueError(f"bifrost results[{index}] ({kind}) has no line span")
        start, end = span
        if not 1 <= start <= end <= len(lines):
            raise ValueError(
                f"bifrost results[{index}] span {relpath}:{start}-{end} is outside its {len(lines)} lines"
            )

        primary = {"path": relpath, "start_line": start, "end_line": end}
        sites = [primary]
        seen = {(relpath, start, end)}
        _collect_nested_sites(
            {k: v for k, v in item.items() if k not in BIFROST_INTERNAL_KEYS and k not in {"range", "path", "start_line", "end_line"}},
            relpath, source, seen, sites,
        )
        rows.append({"kind": kind, "sites": sites, "fields": _strip_internals(item, top=True)})

    deduped = list({canonical(row): row for row in rows}.values())
    incomplete = truncated or any(
        isinstance(diag, dict) and diag.get("impact") == "incomplete" for diag in diagnostics
    )
    meta = {
        "raw_row_count": len(results),
        "truncated": truncated,
        "diagnostics": diagnostics,
        "complete": not incomplete,
        "skipped_external": skipped_external,
        "skipped_empty_file": skipped_empty_file,
    }
    return ordered_rows(deduped), meta


def _systemd_env(extra_env=None):
    env = dict(os.environ)
    env.pop("BIFROST_CACHE_DIR", None)  # would override BIFROST_CACHE_ROOT
    if not env.get("XDG_RUNTIME_DIR"):
        env["XDG_RUNTIME_DIR"] = f"/run/user/{os.getuid()}"
    if extra_env:
        env.update(extra_env)
    return env


def _stop_systemd_unit(unit):
    subprocess.run(
        ["systemctl", "--user", "stop", f"{unit}.scope"],
        env=_systemd_env(), capture_output=True, timeout=30, check=False,
    )


def _systemd_command(unit, memory_cap_gib, command):
    return [
        "systemd-run", "--user", "--scope", "--quiet",
        f"--unit={unit}",
        "-p", f"MemoryMax={memory_cap_gib}G",
        "-p", "MemorySwapMax=0",
        "--",
        *command,
    ]


def _git(repo_dir, *args):
    completed = subprocess.run(
        ["git", "-C", str(repo_dir), *args],
        capture_output=True, text=True, check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed in {repo_dir}: {completed.stderr.strip()}")
    return completed.stdout.strip()


class BifrostRunner:
    """Run pinned real Bifrost on documented fidelity queries with cgroup memory cap and checkpoints."""

    def __init__(self, binary, repos, work_root, checkpoint_root, *,
                 memory_cap_gib=BIFROST_MEMORY_CAP_GIB, result_limit=BIFROST_RESULT_LIMIT,
                 index_timeout_s=BIFROST_INDEX_TIMEOUT_S, query_timeout_s=BIFROST_QUERY_TIMEOUT_S,
                 use_systemd_cap=True, warm_index=True):
        self.binary = Path(binary)
        if not self.binary.is_absolute() or not self.binary.is_file() or not os.access(self.binary, os.X_OK):
            raise ValueError(f"Bifrost binary is not an absolute executable path: {self.binary}")
        self.repos = {name: Path(path) for name, path in repos.items()}
        self._repo_locks = {name: threading.Lock() for name in self.repos}
        self.work_root = Path(work_root)
        self.checkpoint_root = Path(checkpoint_root)
        self.memory_cap_gib = memory_cap_gib
        self.result_limit = result_limit
        self.index_timeout_s = index_timeout_s
        self.query_timeout_s = query_timeout_s
        self.use_systemd_cap = use_systemd_cap
        self.warm_index = warm_index
        self.binary_sha256 = sha256_file(self.binary)
        ident = subprocess.run(
            [str(self.binary), "--build-identity"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        if ident.returncode != 0 or not ident.stdout.strip():
            raise RuntimeError(
                f"bifrost --build-identity failed ({ident.returncode}): {ident.stderr.strip()}"
            )
        self.build_identity = ident.stdout.strip().splitlines()[0].strip()

    def _cache_env(self, repo):
        base = self.work_root / "cache" / repo
        paths = {
            "BIFROST_CACHE_ROOT": str(base / "bifrost"),
            "XDG_CACHE_HOME": str(base / "xdg"),
            "TMPDIR": str(base / "tmp"),
            "RUST_BACKTRACE": "1",
        }
        for key in ("PATH", "RUSTUP_HOME", "CARGO_HOME"):
            if os.environ.get(key):
                paths[key] = os.environ[key]
        for key in ("BIFROST_CACHE_ROOT", "XDG_CACHE_HOME", "TMPDIR"):
            Path(paths[key]).mkdir(parents=True, exist_ok=True)
        return paths

    def _checkout_commit(self, repo, commit):
        if repo not in self.repos:
            raise ValueError(f"unknown Bifrost repository {repo!r}")
        source_repo = self.repos[repo]
        checkout = self.work_root / "checkouts" / repo
        if checkout.exists() and not (checkout / ".git").exists():
            shutil.rmtree(checkout)
        if not (checkout / ".git").exists():
            checkout.parent.mkdir(parents=True, exist_ok=True)
            completed = subprocess.run(
                ["git", "clone", "--no-checkout", "--no-hardlinks", str(source_repo), str(checkout)],
                capture_output=True, text=True, check=False,
            )
            if completed.returncode != 0:
                if checkout.exists():
                    shutil.rmtree(checkout, ignore_errors=True)
                raise RuntimeError(f"git clone failed for {repo}: {completed.stderr.strip()}")
        has_commit = subprocess.run(
            ["git", "-C", str(checkout), "cat-file", "-e", f"{commit}^{{commit}}"],
            capture_output=True, check=False,
        ).returncode == 0
        if not has_commit:
            _git(checkout, "fetch", "--no-tags", str(source_repo), commit)
        _git(checkout, "checkout", "--detach", "--force", commit)
        _git(checkout, "clean", "-fdx")
        head = _git(checkout, "rev-parse", "HEAD")
        if head != commit:
            raise RuntimeError(f"bifrost checkout expected {commit}, got {head}")
        return checkout

    def _ensure_warm(self, repo, commit, checkout, cache_env):
        if not self.warm_index:
            return None
        marker = self.work_root / "warmed" / f"{repo}-{commit}-{self.binary_sha256[:12]}.json"
        if marker.is_file():
            return json.loads(marker.read_bytes()).get("ready_after_s")
        marker.parent.mkdir(parents=True, exist_ok=True)
        stderr_log = marker.with_suffix(".stderr.log")
        base_cmd = [str(self.binary), "--root", str(checkout), "--mcp", "extended|workspace"]
        unit = f"rql-bifrost-warm-{uuid4().hex[:12]}"
        if self.use_systemd_cap and self.memory_cap_gib is not None:
            cmd = _systemd_command(unit, self.memory_cap_gib, base_cmd)
            env = _systemd_env(cache_env)
            popen_kwargs = {}
        else:
            cmd = base_cmd
            env = _systemd_env(cache_env)
            popen_kwargs = {"start_new_session": True}

        t0 = time.monotonic()
        with stderr_log.open("wb") as err_handle, subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err_handle,
            env=env, bufsize=0, **popen_kwargs,
        ) as proc:
            nid = 0
            buffer = b""
            selector = selectors.DefaultSelector()
            selector.register(proc.stdout, selectors.EVENT_READ)

            def read_line():
                nonlocal buffer
                while b"\n" not in buffer:
                    remaining = self.index_timeout_s - (time.monotonic() - t0)
                    if remaining <= 0:
                        raise TimeoutError(
                            f"bifrost warm-up timed out after {self.index_timeout_s}s on {repo}@{commit[:12]}"
                        )
                    events = selector.select(timeout=remaining)
                    if not events:
                        raise TimeoutError(
                            f"bifrost warm-up timed out after {self.index_timeout_s}s on {repo}@{commit[:12]}"
                        )
                    chunk = os.read(proc.stdout.fileno(), 65536)
                    if not chunk:
                        if buffer:
                            line = buffer
                            buffer = b""
                            return line.decode("utf-8", errors="replace")
                        return ""
                    buffer += chunk
                line, buffer = buffer.split(b"\n", 1)
                return line.decode("utf-8", errors="replace")

            def rpc(method, params=None, *, notify=False):
                nonlocal nid
                msg = {"jsonrpc": "2.0", "method": method}
                if params is not None:
                    msg["params"] = params
                if not notify:
                    nid += 1
                    msg["id"] = nid
                proc.stdin.write((json.dumps(msg) + "\n").encode("utf-8"))
                proc.stdin.flush()
                if notify:
                    return None
                while True:
                    line = read_line()
                    if not line:
                        tail = stderr_log.read_text(errors="replace")[-500:]
                        raise RuntimeError(f"bifrost warm-up server closed stdout on {repo}@{commit[:12]}: {tail}")
                    try:
                        reply = json.loads(line)
                    except ValueError:
                        continue
                    if reply.get("id") == nid:
                        return reply

            try:
                rpc("initialize", {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "rql-fidelity-warm", "version": "1"},
                })
                rpc("notifications/initialized", notify=True)
                while True:
                    if time.monotonic() - t0 > self.index_timeout_s:
                        raise TimeoutError(f"bifrost warm-up timed out after {self.index_timeout_s}s on {repo}@{commit[:12]}")
                    reply = rpc("tools/call", {"name": "get_active_workspace", "arguments": {}})
                    err = reply.get("error")
                    if err:
                        incomplete = (err.get("data") or {}).get("io.bifrost/workspace-incomplete") or {}
                        if incomplete.get("reason") == "busy":
                            time.sleep(2)
                            continue
                        raise RuntimeError(
                            f"bifrost get_active_workspace error on {repo}@{commit[:12]}: {json.dumps(err)[:500]}"
                        )
                    result = reply.get("result") or {}
                    if result.get("isError"):
                        raise RuntimeError(
                            f"bifrost get_active_workspace tool error on {repo}@{commit[:12]}: {json.dumps(result)[:500]}"
                        )
                    status = result.get("structuredContent") or {}
                    if not status:
                        for item in result.get("content") or ():
                            try:
                                status = json.loads(item.get("text", ""))
                                break
                            except ValueError:
                                continue
                    needed = ("usage_index_ready", "semantic_packs_ready")
                    missing = [k for k in needed if not isinstance(status, dict) or k not in status]
                    if missing:
                        raise RuntimeError(
                            f"bifrost get_active_workspace reply on {repo}@{commit[:12]} lacks {missing}: "
                            f"{json.dumps(status)[:300]}"
                        )
                    build = status.get("workspace_build")
                    if isinstance(build, dict) and build.get("status") == "failed":
                        raise RuntimeError(f"bifrost workspace build failed on {repo}@{commit[:12]}: {status}")
                    analyzer_ok = status.get("analyzer_ready", True) is True
                    build_ok = (
                        not isinstance(build, dict)
                        or build.get("status") in ("ready", "complete", "completed")
                    )
                    if (status.get("usage_index_ready") is True
                            and status.get("semantic_packs_ready") is True
                            and analyzer_ok and build_ok):
                        break
                    time.sleep(2)
                proc.stdin.close()
                proc.wait(timeout=60)
            except BaseException:
                try:
                    proc.kill()
                except OSError:
                    pass
                if self.use_systemd_cap and self.memory_cap_gib is not None:
                    _stop_systemd_unit(unit)
                else:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except OSError:
                        pass
                try:
                    proc.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    pass
                raise
            finally:
                selector.close()

        ready_s = round(time.monotonic() - t0, 2)
        driver.save(marker, {
            "repo": repo, "commit": commit, "binary_sha256": self.binary_sha256,
            "build_identity": self.build_identity, "ready_after_s": ready_s,
        })
        return ready_s

    def _run_query_file(self, repo, commit, checkout, cache_env, query_text, raw_path, stderr_path):
        query_file = checkout / QUERY_FILENAME
        query_file.write_text(query_text + "\n", encoding="utf-8")
        base_cmd = [
            str(self.binary), "--root", str(checkout),
            "--query-file", QUERY_FILENAME, "--workspace-scaled-limits",
        ]
        unit = f"rql-bifrost-query-{uuid4().hex[:12]}"
        if self.use_systemd_cap and self.memory_cap_gib is not None:
            cmd = _systemd_command(unit, self.memory_cap_gib, base_cmd)
            env = _systemd_env(cache_env)
            run_kwargs = {}
        else:
            cmd = base_cmd
            env = _systemd_env(cache_env)
            run_kwargs = {"start_new_session": True}

        t0 = time.monotonic()
        try:
            with raw_path.open("wb") as out_handle, stderr_path.open("wb") as err_handle:
                with subprocess.Popen(cmd, stdout=out_handle, stderr=err_handle, env=env, **run_kwargs) as proc:
                    try:
                        returncode = proc.wait(timeout=self.query_timeout_s)
                    except BaseException:
                        try:
                            proc.kill()
                        except OSError:
                            pass
                        if self.use_systemd_cap and self.memory_cap_gib is not None:
                            _stop_systemd_unit(unit)
                        else:
                            try:
                                os.killpg(proc.pid, signal.SIGKILL)
                            except OSError:
                                pass
                        try:
                            proc.wait(timeout=15)
                        except subprocess.TimeoutExpired:
                            pass
                        raise
        finally:
            try:
                query_file.unlink()
            except FileNotFoundError:
                pass
        wall_s = round(time.monotonic() - t0, 2)
        if returncode != 0:
            tail = stderr_path.read_text(errors="replace")[-500:]
            raise RuntimeError(
                f"bifrost query exited {returncode} on {repo}@{commit[:12]}: {tail}"
            )
        return wall_s

    def query(self, repo, commit, query_text):
        """Run or resume a checkpointed real Bifrost documented query."""
        if repo not in self._repo_locks:
            raise ValueError(f"unknown Bifrost repository {repo!r}")
        query_key = hashlib.sha256(
            canonical(["documented", repo, commit, query_text]).encode()
        ).hexdigest()
        directory = self.checkpoint_root / query_key / "bifrost"
        intent_path = directory / "intent.json"
        completed_path = directory / "completed.json"
        raw_path = directory / "raw.json"
        stderr_path = directory / "query.stderr.log"

        executed_query = wrap_query_with_limit(query_text, self.result_limit)
        intent = {
            "repo": repo, "commit": commit, "query": query_text,
            "executed_query": executed_query,
            "bifrost_binary_sha256": self.binary_sha256,
            "bifrost_build_identity": self.build_identity,
            "workspace_scaled_limits": True, "limit": self.result_limit,
        }
        with self._repo_locks[repo]:
            directory.mkdir(parents=True, exist_ok=True)
            if intent_path.exists() and json.loads(intent_path.read_bytes()) != intent:
                raise ValueError(f"bifrost checkpoint inputs changed at {directory}")
            if completed_path.exists():
                if not intent_path.exists():
                    raise ValueError(f"bifrost checkpoint is missing intent.json at {directory}")
                return json.loads(completed_path.read_bytes())

            driver.save(intent_path, intent)
            checkout = self._checkout_commit(repo, commit)
            cache_env = self._cache_env(repo)
            index_wall_s = self._ensure_warm(repo, commit, checkout, cache_env)
            wall_s = self._run_query_file(repo, commit, checkout, cache_env, executed_query, raw_path, stderr_path)
            try:
                raw = json.loads(raw_path.read_bytes())
            except ValueError as error:
                tail = stderr_path.read_text(errors="replace")[-500:]
                raise RuntimeError(
                    f"bifrost produced invalid JSON on {repo}@{commit[:12]}: {error}; stderr: {tail}"
                ) from error
            read = blob_reader(self.repos[repo], commit)
            rows, meta = normalize_bifrost_results(raw, read)
            record = {
                "rows": rows,
                **meta,
                "wall_s": wall_s,
                "index_wall_s": index_wall_s,
                "bifrost_build_identity": self.build_identity,
                "bifrost_binary_sha256": self.binary_sha256,
            }
            driver.save(completed_path, record)
            return record
