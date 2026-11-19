#!/usr/bin/env python3
"""Cold index and warm-restart re-index cost of Bifrost on candidate Rust repos (EC2 only).

Usage: measure_index.py PINS.txt CLONES_DIR BIFROST_BIN OUT_DIR

PINS.txt lines are `owner__repo <sha> <origin url>`. For each repo: clone it from its origin
into CLONES_DIR, add a detached worktree at <sha>~50 (first parent), and start a resident
`bifrost --mcp` in its own memory-capped systemd scope with a fresh cache root. Poll
get_active_workspace until analyzer, usage index and semantic packs are ready and the workspace
build has completed (the incremental-acceptance bench's readiness rule). Then stop it, move the
worktree to <sha>, start again on the same cache, and time readiness again. Per phase it records
wall time, the scope's CPU up to the ready reply (cpu_to_ready_usec, used for ranking), the
scope's whole-tree peak memory, the scope's result (oom-kill) and cache size: one JSON line per
repo, resuming past matching records.
"""
import json
import os
import pathlib
import resource
import queue
import shutil
import subprocess
import sys
import threading
import time

ENV = {"BIFROST_OPEN_PACKS_DISABLE": "1", "BIFROST_SEMANTIC_PACK_DOWNLOAD": "off",
       "GIT_NO_LAZY_FETCH": "1", "GIT_LFS_SKIP_SMUDGE": "1"}  # mjolnir-warm-tools.json


# Deadlines from the incremental-acceptance bench this mirrors (bifrost-dev
# .agents/scripts/incremental-acceptance: mjolnir-warm-tools.json sets readiness_timeout_seconds
# 3600 and request_watchdog_seconds 600; bench.py stop() waits 120 s, then terminates). A
# wedged server then fails its repo visibly instead of stalling the shortlist.
READINESS_SECONDS = 3600
# Per-server memory cap (the scope's MemoryMax): 48 GiB on the 64 GiB measurement host, leaving
# room for the OS and this driver; an index that exceeds it is a recorded failure.
MEMORY_MAX = os.environ.get("RQL_INDEX_MEMORY_MAX", "48G")
REQUEST_SECONDS = 600
STOP_SECONDS = 120


# SIGTERM/SIGINT never raise into the driver: they set CANCEL, which the request and readiness
# waits poll, so cancellation always runs through the ordinary teardown path (one owner, no
# window between launching a server and protecting it).
CANCEL = threading.Event()


class Cancelled(RuntimeError):
    """The operator cancelled the batch; the current server is torn down normally."""


class TeardownUnconfirmed(RuntimeError):
    """A server tree may still be alive. The batch must halt without touching its files."""


class Server:
    def __init__(self, binary, root, cache, log):
        env = dict(os.environ, **ENV, BIFROST_CACHE_ROOT=str(cache))
        env.pop("BIFROST_CACHE_DIR", None)  # it would take precedence over the fresh cache root
        self.log = open(log, "ab")
        # Each server runs in its own transient systemd scope: the scope's cgroup holds Bifrost
        # and every process it starts, enforces the memory cap on that whole tree, and its
        # memory.peak is the tree's peak, not just the parent's VmHWM.
        self.unit = f"rql-bifrost-{os.getpid()}-{time.monotonic_ns()}"
        self.proc = subprocess.Popen(
            ["systemd-run", "--user", "--scope", "--quiet", f"--unit={self.unit}",
             "-p", f"MemoryMax={MEMORY_MAX}", "-p", "MemorySwapMax=0",
             binary, "--root", str(root), "--mcp", "extended|workspace"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, env=env, cwd=root)
        try:
            self._start()
        except BaseException:
            # Setup failed (or SIGTERM arrived) after the scope launched: tear it down before
            # propagating, so the batch never advances past a live scope.
            self.sample = getattr(self, "sample", {})
            self.cgroup = getattr(self, "cgroup", None)
            self.stop()
            raise

    def _start(self):
        self.seq = 0
        # A reader thread owns stdout, so a deadline can bound each wait even when several
        # replies arrive in one read (select on a buffered pipe would miss buffered lines).
        self.lines = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        # The scope's cgroup vanishes when the tree exits, so sample it while it lives: the
        # last memory.peak, OOM kills and tree CPU survive a timeout or an OOM kill.
        # The sampler reads once a second, so a peak in the last second before exit can be
        # missed; the record says so (sample_interval_s). An OOM kill is still caught through
        # systemd's Result. Ranking uses cpu_to_ready_usec, read from cpu.stat at readiness.
        self.sample = {"peak_tree_kib": None, "oom_kill": 0, "tree_cpu_usec": None, "sample_interval_s": 1}
        self.cgroup = None
        self.sampling = True
        threading.Thread(target=self._sample, daemon=True).start()

    def _cgroup_dir(self):
        """The server's own scope cgroup, or None until systemd-run has moved the process into it
        (an earlier read would see the driver's cgroup and its unrelated counters)."""
        try:
            path = open(f"/proc/{self.proc.pid}/cgroup").read().split("::", 1)[1].strip()
        except (OSError, IndexError):
            return None
        return "/sys/fs/cgroup" + path if path.endswith(f"/{self.unit}.scope") else None

    def _sample(self):
        while self.sampling and self.proc.poll() is None:
            d = self._cgroup_dir()
            if d:
                self.cgroup = d
            if d:  # each file on its own, so one unreadable file can't hide the others
                try:
                    self.sample["peak_tree_kib"] = int(open(f"{d}/memory.peak").read()) // 1024
                except (OSError, ValueError):
                    pass
                try:
                    events = dict(l.split() for l in open(f"{d}/memory.events"))
                    self.sample["oom_kill"] = int(events.get("oom_kill", 0))
                except (OSError, ValueError):
                    pass
                try:
                    cpu = dict(l.split() for l in open(f"{d}/cpu.stat"))
                    self.sample["tree_cpu_usec"] = int(cpu["usage_usec"])
                except (OSError, ValueError, KeyError):
                    pass
            time.sleep(1)

    def _read(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)  # EOF

    def request(self, method, params, deadline=None):
        self.seq += 1
        self.proc.stdin.write((json.dumps(dict(jsonrpc="2.0", id=self.seq, method=method, params=params)) + "\n").encode())
        self.proc.stdin.flush()
        deadline = min(time.monotonic() + REQUEST_SECONDS, deadline or float("inf"))
        while True:
            if CANCEL.is_set():
                raise Cancelled("cancelled while waiting for a reply")
            if time.monotonic() >= deadline:  # also while notifications keep arriving
                raise TimeoutError(f"no reply to {method} before its deadline (request {REQUEST_SECONDS} s, "
                                   f"readiness {READINESS_SECONDS} s)")
            try:
                line = self.lines.get(timeout=max(0, min(1.0, deadline - time.monotonic())))
            except queue.Empty:
                if time.monotonic() < deadline:
                    continue
                raise TimeoutError(f"no reply to {method} before its deadline (request {REQUEST_SECONDS} s, "
                                   f"readiness {READINESS_SECONDS} s)")
            if not line:
                raise RuntimeError(f"server exited with {self.proc.poll()}")
            msg = json.loads(line)
            if msg.get("id") == self.seq:
                return msg

    def initialize(self):
        self.request("initialize", dict(protocolVersion="2025-11-25", capabilities={},
                                        clientInfo=dict(name="rql-census-measure", version="1")))
        self.proc.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        self.proc.stdin.flush()

    def wait_ready(self):
        polls = 0
        deadline = time.monotonic() + READINESS_SECONDS
        while True:
            if time.monotonic() > deadline:
                raise TimeoutError(f"workspace not ready within {READINESS_SECONDS} s")
            polls += 1
            msg = self.request("tools/call", dict(name="get_active_workspace", arguments={}), deadline=deadline)
            err = msg.get("error")
            if err:
                incomplete = (err.get("data") or {}).get("io.bifrost/workspace-incomplete") or {}
                if incomplete.get("reason") == "busy":
                    if CANCEL.wait(1):
                        raise Cancelled("cancelled while waiting for readiness")
                    continue
                raise RuntimeError(f"readiness error: {json.dumps(err)[:1000]}")
            result = msg["result"]
            payload = result.get("structuredContent")
            if payload is None:
                payload = json.loads((result.get("content") or [{}])[0].get("text", "null"))
            status = (payload.get("workspace_build") or {}).get("status")
            if status == "failed":
                raise RuntimeError(f"workspace build failed: {json.dumps(payload)[:1000]}")
            if all(payload.get(k) is True for k in ("analyzer_ready", "usage_index_ready", "semantic_packs_ready")) \
                    and status in ("ready", "complete", "completed"):
                return payload, polls
            if CANCEL.wait(1):
                raise Cancelled("cancelled while waiting for readiness")


    def stop(self):
        try:
            return self._stop()
        except TeardownUnconfirmed:
            raise
        except BaseException as e:  # any failure before the proof leaves the tree unaccounted for
            raise TeardownUnconfirmed(f"{self.unit}.scope: teardown failed before confirmation: {e!r}") from e

    def _stop(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            rc = self.proc.wait(timeout=STOP_SECONDS)
        except subprocess.TimeoutExpired:
            rc = None
        scope = f"{self.unit}.scope"
        # Stop the scope in every case: it kills the whole process tree (Bifrost and anything it
        # started). Then verify the tree is gone before the caller touches the worktree or cache.
        stopped = run_bounded(["systemctl", "--user", "stop", scope], capture_output=True, text=True)
        if rc is None:
            try:
                rc = self.proc.wait(timeout=STOP_SECONDS)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                try:
                    rc = self.proc.wait(timeout=STOP_SECONDS)
                except subprocess.TimeoutExpired as e:
                    raise TeardownUnconfirmed(f"{scope}: server survived SIGKILL") from e
        # Proof the tree is gone: systemd reports the scope inactive or failed (a bus error gives
        # neither), and its cgroup holds no process. Anything else stops the batch for this repo.
        deadline = time.monotonic() + STOP_SECONDS
        while True:
            state = run_bounded(["systemctl", "--user", "show", scope, "-p", "ActiveState", "--value"],
                                   capture_output=True, text=True).stdout.strip()
            procs = ""
            if self.cgroup:
                try:
                    procs = open(f"{self.cgroup}/cgroup.procs").read().strip()
                except FileNotFoundError:
                    procs = ""
            if state in ("inactive", "failed") and not procs:
                # Read the Result only now, after every process in the scope has exited, so an
                # OOM kill of a child during shutdown is caught too (the uncollected failed
                # scope keeps it until reset-failed).
                self.sample["scope_result"] = run_bounded(
                    ["systemctl", "--user", "show", scope, "-p", "Result", "--value"],
                    capture_output=True, text=True).stdout.strip()
                break
            if time.monotonic() > deadline:
                raise TeardownUnconfirmed(f"{scope} not confirmed stopped (state={state!r}, procs={procs[:80]!r}, "
                                   f"stop exit {stopped.returncode}: {stopped.stderr.strip()[-200:]}); "
                                   "refusing to touch its worktree")
            time.sleep(1)
        run_bounded(["systemctl", "--user", "reset-failed", scope], capture_output=True)
        self.sampling = False
        self.log.close()
        return rc


def run_bounded(cmd, **kw):
    """A systemctl call that can't stall teardown: it raises TimeoutExpired after STOP_SECONDS,
    which teardown turns into TeardownUnconfirmed and the reaper into an exit."""
    return subprocess.run(cmd, timeout=STOP_SECONDS, **kw)


def du(path):
    return sum(f.stat().st_size for f in pathlib.Path(path).rglob("*") if f.is_file())


def phase(binary, root, cache, log):
    if CANCEL.is_set():  # never launch a server after cancellation (e.g. during a checkout)
        raise Cancelled("cancelled before launching a server")
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.monotonic()
    server = Server(binary, root, cache, log)
    rec = {}
    try:
        server.initialize()
        payload, polls = server.wait_ready()
        # The ranking's CPU: the whole scope's CPU up to the ready reply, read from its cgroup,
        # so it covers every descendant and excludes shutdown and the driver's own commands.
        d = server._cgroup_dir()
        try:
            rec["cpu_to_ready_usec"] = int(dict(l.split() for l in open(f"{d}/cpu.stat"))["usage_usec"]) if d else None
        except (OSError, ValueError, KeyError):
            rec["cpu_to_ready_usec"] = None
        if rec["cpu_to_ready_usec"] is None:
            raise RuntimeError("cpu.stat unreadable at readiness: the ranking's CPU is missing")
        rec.update(wall=time.monotonic() - t0, polls=polls,
                   ready=True, readiness={k: payload.get(k) for k in
                                          ("analyzer_ready", "usage_index_ready", "semantic_packs_ready", "workspace_build")})
    except Exception as e:
        rec.update(wall=time.monotonic() - t0, ready=False, error=repr(e)[:1000])
    rec["exit"] = server.stop()
    if CANCEL.is_set():
        # Torn down; now stop the batch before any checkout or another index launch.
        raise Cancelled("cancelled; this phase's server was torn down")
    if rec.get("ready") and rec["exit"] != 0:
        # A crash or kill after the ready reply can leave the cache half-persisted: the pair
        # isn't a valid cold/warm measurement.
        rec["ready"] = False
        rec["error"] = f"unclean shutdown after ready (exit {rec['exit']})"
    rec.update(server.sample)  # peak_tree_kib, oom_kill, tree_cpu_usec, also on failure
    if rec.get("scope_result") == "oom-kill" or rec.get("oom_kill"):
        # Hitting the cap at any point, even after a ready reply, invalidates the phase.
        rec["ready"] = False
        rec.setdefault("error", f"memory cap {MEMORY_MAX} reached (scope_result={rec.get('scope_result')})")
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    rec.update(cpu_user=after.ru_utime - before.ru_utime, cpu_sys=after.ru_stime - before.ru_stime,
               cache_bytes=du(cache))
    return rec


def git(*args, cwd=None):
    """Run git, but stop it if the batch is cancelled: a long clone or fetch mustn't delay
    cancellation, and nothing launches after it (phase() checks CANCEL first)."""
    proc = subprocess.Popen(["git", *args], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    while True:
        try:
            out, err = proc.communicate(timeout=1)
            break
        except subprocess.TimeoutExpired:
            if CANCEL.is_set():
                proc.terminate()
                proc.communicate()
                raise Cancelled(f"cancelled during git {args[0]}")
    if proc.returncode:
        raise subprocess.CalledProcessError(proc.returncode, ["git", *args], out, err)
    return out.strip()


def require_ec2():
    """Refuse to index anywhere but an EC2 instance: an unbounded index crashed the user's
    workstation twice (bifrost-dev #14480). The per-server scope cap applies on EC2 too."""
    try:
        vendor = open("/sys/devices/virtual/dmi/id/sys_vendor").read().strip()
    except OSError:
        vendor = ""
    if vendor != "Amazon EC2":
        sys.exit(f"refusing to run Bifrost indexing on a non-EC2 host (sys_vendor={vendor!r})")
    # The cap must fit the host with room to spare, and the user manager must enforce it: prove
    # it by running a scope capped at 64M that tries to allocate 256M and must be OOM-killed.
    mem_total_kib = int(next(l for l in open("/proc/meminfo") if l.startswith("MemTotal:")).split()[1])
    units = {"K": 1, "M": 2**10, "G": 2**20, "T": 2**30}
    cap_kib = int(MEMORY_MAX[:-1]) * units[MEMORY_MAX[-1].upper()] if MEMORY_MAX[-1].isalpha() else int(MEMORY_MAX) // 1024
    if cap_kib > 0.8 * mem_total_kib:
        sys.exit(f"memory cap {MEMORY_MAX} exceeds 80% of this host's {mem_total_kib // 2**20} GiB")
    probe = f"rql-cap-probe-{os.getpid()}"
    run_bounded(["systemd-run", "--user", "--scope", "--quiet", f"--unit={probe}", "-p", "MemoryMax=64M",
                    "-p", "MemorySwapMax=0", sys.executable, "-c", "x = bytearray(256 * 2**20)"], capture_output=True)
    result = run_bounded(["systemctl", "--user", "show", f"{probe}.scope", "-p", "Result", "--value"],
                            capture_output=True, text=True).stdout.strip()
    run_bounded(["systemctl", "--user", "reset-failed", f"{probe}.scope"], capture_output=True)
    if result != "oom-kill":
        sys.exit(f"the user manager doesn't enforce MemoryMax (probe result {result!r}); refusing to index")


def reap_leftover_scopes():
    """Before any cleanup, stop every scope a previous run left behind and prove it's gone.
    The driver runs one server at a time, so any live rql-bifrost scope at startup belongs to an
    earlier invocation whose stop couldn't be confirmed. If one survives, exit without touching
    any worktree or cache."""
    units = run_bounded(["systemctl", "--user", "list-units", "--all", "--type=scope", "--no-legend", "--plain"],
                           capture_output=True, text=True, check=True).stdout.split("\n")
    for unit in [u.split()[0] for u in units if u.strip().startswith("rql-bifrost-")]:
        run_bounded(["systemctl", "--user", "stop", unit], capture_output=True)
        deadline = time.monotonic() + STOP_SECONDS
        while True:
            state = run_bounded(["systemctl", "--user", "show", unit, "-p", "ActiveState", "--value"],
                                   capture_output=True, text=True).stdout.strip()
            shown = run_bounded(["systemctl", "--user", "show", unit, "-p", "ControlGroup", "--value"],
                                capture_output=True, text=True)
            if shown.returncode:  # a failed lookup proves nothing: treat the tree as possibly alive
                if time.monotonic() > deadline:
                    sys.exit(f"leftover {unit}: ControlGroup lookup failed ({shown.stderr.strip()[-200:]}); "
                             "refusing to clean up any worktree")
                time.sleep(1)
                continue
            cg = shown.stdout.strip()  # empty for a stopped unit, which has no cgroup
            try:
                procs = open(f"/sys/fs/cgroup{cg}/cgroup.procs").read().strip() if cg else ""
            except FileNotFoundError:
                procs = ""
            # "failed" can follow a stop timeout with a process still alive: require an empty cgroup.
            if state in ("inactive", "failed") and not procs:
                run_bounded(["systemctl", "--user", "reset-failed", unit], capture_output=True)
                break
            if time.monotonic() > deadline:
                sys.exit(f"leftover {unit} won't stop (state={state!r}); refusing to clean up any worktree")
            time.sleep(1)


def main():
    import signal
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda *_: CANCEL.set())
    require_ec2()
    # One driver per host: a second invocation would reap the first's live scopes and race on
    # its worktrees. The lock is held for the process lifetime.
    import fcntl
    lock = open(pathlib.Path.home() / ".rql-index-measure.lock", "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        sys.exit("another measure_index.py is running on this host")
    reap_leftover_scopes()
    # Each line is `owner__repo <pinned sha> <origin url>`: the repo is cloned from its origin
    # and measured at that commit, so a reused record always matches its revision.
    entries = [l.split() for l in open(sys.argv[1]) if l.strip()]
    bad = [e for e in entries if len(e) < 3]
    if bad:
        sys.exit(f"every line needs `owner__repo <pinned sha> <origin url>`; unpinned: {bad[:3]}")
    pins = {e[0]: (e[1], e[2]) for e in entries}
    repos = [e[0] for e in entries]
    clones, binary, out = pathlib.Path(sys.argv[2]).resolve(), str(pathlib.Path(sys.argv[3]).resolve()), pathlib.Path(sys.argv[4]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    results = out / "index-cost.jsonl"
    # A repo is done only once a record shows both phases ready with no error; failed or
    # unready measurements are retried on the next run (the latest record wins downstream).
    import hashlib
    with open(binary, "rb") as f:
        binary_sha = hashlib.file_digest(f, "sha256").hexdigest()
    # A record is reusable only for the same repo, pinned revision and Bifrost binary.
    latest = {}  # the latest record per repo is the one readers use; resume follows it too
    if results.exists():
        for l in open(results):
            r = json.loads(l)
            latest[r["repo"]] = r
    done = {repo for repo, r in latest.items()
            if repo in pins and not r.get("error") and all((r.get(ph) or {}).get("ready") for ph in ("cold", "warm_restart"))
            and r.get("binary_sha256") == binary_sha and r.get("memory_max") == MEMORY_MAX
            and r.get("head") == pins[repo][0]}
    for repo in repos:
        if CANCEL.is_set():
            sys.exit("cancelled; the last server's scope was torn down")
        if repo in done:
            continue
        rec = {"repo": repo}
        try:
            clone = clones / repo
            sha, url = pins[repo]
            url = url.replace("git@github.com:", "https://github.com/")
            if not clone.exists():
                git("clone", "-q", "--no-checkout", url, str(clone))
            try:
                head = git("rev-parse", f"{sha}^{{commit}}", cwd=clone)
            except subprocess.CalledProcessError:
                # A revised pin can name a commit newer than this clone: fetch it, then retry.
                git("fetch", "-q", "origin", sha, cwd=clone)
                head = git("rev-parse", f"{sha}^{{commit}}", cwd=clone)
            base = git("rev-parse", f"{head}~50", cwd=clone)  # first parent
            wt = out / "wt" / repo
            # A previous run may have left this worktree (registered or not). Its server scope is
            # already proven gone by reap_leftover_scopes(), so removing the files is safe.
            if wt.exists():
                subprocess.run(["git", "worktree", "remove", "--force", str(wt)], cwd=clone, capture_output=True)
                shutil.rmtree(wt, ignore_errors=True)
            git("worktree", "prune", cwd=clone)
            git("worktree", "add", "-q", "--detach", str(wt), base, cwd=clone)
            rec.update(head=head, base=base, binary_sha256=binary_sha, memory_max=MEMORY_MAX,
                       rust_files=len([p for p in subprocess.run(
                           ["git", "-C", str(wt), "ls-files", "-z", "*.rs"], check=True,
                           capture_output=True).stdout.split(b"\0") if p]))
            cache = out / "cache" / repo
            shutil.rmtree(cache, ignore_errors=True)
            cache.mkdir(parents=True)
            rec["cold"] = phase(binary, wt, cache, out / f"{repo}.cold.log")
            git("checkout", "-q", "--detach", head, cwd=wt)
            rec["changed_files"] = len(git("diff", "--name-only", base, head, cwd=clone).splitlines())
            if rec["cold"].get("ready"):
                rec["warm_restart"] = phase(binary, wt, cache, out / f"{repo}.warm.log")
            else:  # a warm restart on a failed cold cache measures nothing usable
                rec["warm_restart"] = {"ready": False, "skipped": "cold index failed"}
            git("worktree", "remove", "--force", str(wt), cwd=clone)
            shutil.rmtree(cache, ignore_errors=True)  # cache size is recorded; keep disk bounded
        except Exception as e:
            import traceback
            rec["error"] = repr(e)[:1000]
            rec["traceback"] = traceback.format_exc()[-2000:]
            rec["teardown_unconfirmed"] = isinstance(e, TeardownUnconfirmed)
            rec["cancelled"] = isinstance(e, Cancelled)
        with open(results, "a") as f:
            f.write(json.dumps(rec) + "\n")
        print(json.dumps({"repo": repo, "error": rec.get("error"), **{
            ph: {k: (rec.get(ph) or {}).get(k) for k in ("ready", "skipped", "scope_result", "peak_tree_kib", "error")}
            for ph in ("cold", "warm_restart")}}), flush=True)
        if rec.get("cancelled"):
            sys.exit(f"{repo}: cancelled; its server was torn down and the batch stopped")
        if rec.get("teardown_unconfirmed"):
            # A server tree may still be alive: stop the batch here, leaving its worktree and
            # cache untouched. The next run's reap_leftover_scopes() must prove it gone first.
            sys.exit(f"{repo}: server scope not confirmed stopped; batch halted")


if __name__ == "__main__":
    main()
