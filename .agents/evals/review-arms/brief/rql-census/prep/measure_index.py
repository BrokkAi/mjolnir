#!/usr/bin/env python3
"""Cold index and warm-restart re-index cost of Bifrost on candidate Rust repos.

Usage: measure_index.py REPOS.txt CLONES_DIR BIFROST_BIN OUT_DIR

For each owner__repo: clone it from /nfs/morannon (read only) into CLONES_DIR if absent,
add a detached worktree at HEAD~50 (first-parent), start a resident `bifrost --mcp` with a
fresh cache root, and poll get_active_workspace until analyzer, usage index and semantic
packs are ready and the workspace build has completed (the readiness rule of the
incremental-acceptance bench, bench.py). Then stop it, move the worktree to HEAD, start
again on the same cache root, and time readiness again. Records wall, CPU (server process
tree, from RUSAGE_CHILDREN after it exits), peak RSS and cache bytes per phase, one JSON
line per repo, resuming past repos already written.
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

SRC = pathlib.Path("/nfs/morannon/nvme/repo-clones")
ENV = {"BIFROST_OPEN_PACKS_DISABLE": "1", "BIFROST_SEMANTIC_PACK_DOWNLOAD": "off",
       "GIT_NO_LAZY_FETCH": "1", "GIT_LFS_SKIP_SMUDGE": "1"}  # mjolnir-warm-tools.json


# Deadlines from the incremental-acceptance bench this mirrors (bifrost-dev
# .agents/scripts/incremental-acceptance: mjolnir-warm-tools.json sets readiness_timeout_seconds
# 3600 and request_watchdog_seconds 600; bench.py stop() waits 120 s, then terminates). A
# wedged server then fails its repo visibly instead of stalling the shortlist.
READINESS_SECONDS = 3600
REQUEST_SECONDS = 600
STOP_SECONDS = 120


class Server:
    def __init__(self, binary, root, cache, log):
        env = dict(os.environ, **ENV, BIFROST_CACHE_ROOT=str(cache))
        env.pop("BIFROST_CACHE_DIR", None)  # it would take precedence over the fresh cache root
        self.log = open(log, "ab")
        self.proc = subprocess.Popen([binary, "--root", str(root), "--mcp", "extended|workspace"],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
                                     env=env, cwd=root)
        self.seq = 0
        # A reader thread owns stdout, so a deadline can bound each wait even when several
        # replies arrive in one read (select on a buffered pipe would miss buffered lines).
        self.lines = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)  # EOF

    def request(self, method, params):
        self.seq += 1
        self.proc.stdin.write((json.dumps(dict(jsonrpc="2.0", id=self.seq, method=method, params=params)) + "\n").encode())
        self.proc.stdin.flush()
        deadline = time.monotonic() + REQUEST_SECONDS
        while True:
            try:
                line = self.lines.get(timeout=max(0, deadline - time.monotonic()))
            except queue.Empty:
                raise TimeoutError(f"no reply to {method} within {REQUEST_SECONDS} s")
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
            msg = self.request("tools/call", dict(name="get_active_workspace", arguments={}))
            err = msg.get("error")
            if err:
                incomplete = (err.get("data") or {}).get("io.bifrost/workspace-incomplete") or {}
                if incomplete.get("reason") == "busy":
                    time.sleep(1)
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
            time.sleep(1)

    def peak_rss_kib(self):
        try:
            for line in open(f"/proc/{self.proc.pid}/status"):
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
        except OSError:
            return None

    def stop(self):
        self.proc.stdin.close()
        try:
            rc = self.proc.wait(timeout=STOP_SECONDS)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                rc = self.proc.wait(timeout=STOP_SECONDS)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                rc = self.proc.wait()
        self.log.close()
        return rc


def du(path):
    return sum(f.stat().st_size for f in pathlib.Path(path).rglob("*") if f.is_file())


def phase(binary, root, cache, log):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.monotonic()
    server = Server(binary, root, cache, log)
    rec = {}
    try:
        server.initialize()
        payload, polls = server.wait_ready()
        rec.update(wall=time.monotonic() - t0, polls=polls, peak_rss_kib=server.peak_rss_kib(),
                   ready=True, readiness={k: payload.get(k) for k in
                                          ("analyzer_ready", "usage_index_ready", "semantic_packs_ready", "workspace_build")})
    except Exception as e:
        rec.update(wall=time.monotonic() - t0, ready=False, error=repr(e)[:1000])
    rec["exit"] = server.stop()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    rec.update(cpu_user=after.ru_utime - before.ru_utime, cpu_sys=after.ru_stime - before.ru_stime,
               cache_bytes=du(cache))
    return rec


def git(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def main():
    # Each line is `owner__repo`, or `owner__repo <pinned sha> <origin url>`. The pinned form
    # clones from the origin (for hosts without /nfs/morannon) and measures at that commit.
    entries = [l.split() for l in open(sys.argv[1]) if l.strip()]
    pins = {e[0]: (e[1], e[2]) for e in entries if len(e) >= 3}
    repos = [e[0] for e in entries]
    clones, binary, out = pathlib.Path(sys.argv[2]).resolve(), str(pathlib.Path(sys.argv[3]).resolve()), pathlib.Path(sys.argv[4]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    results = out / "index-cost.jsonl"
    # A repo is done only once a record shows both phases ready with no error; failed or
    # unready measurements are retried on the next run (the latest record wins downstream).
    done = set()
    if results.exists():
        for l in open(results):
            r = json.loads(l)
            if not r.get("error") and all((r.get(ph) or {}).get("ready") for ph in ("cold", "warm_restart")):
                done.add(r["repo"])
    for repo in repos:
        if repo in done:
            continue
        rec = {"repo": repo}
        try:
            clone = clones / repo
            if repo in pins:
                sha, url = pins[repo]
                url = url.replace("git@github.com:", "https://github.com/")
                if not clone.exists():
                    git("clone", "-q", "--no-checkout", url, str(clone))
                head = git("rev-parse", f"{sha}^{{commit}}", cwd=clone)
            else:
                if not clone.exists():
                    git("clone", "-q", "--no-hardlinks", str(SRC / repo), str(clone))
                head = git("rev-parse", "HEAD", cwd=clone)
            base = git("rev-parse", f"{head}~50", cwd=clone)  # first parent
            wt = out / "wt" / repo
            if wt.exists():
                git("worktree", "remove", "--force", str(wt), cwd=clone)
            git("worktree", "add", "-q", "--detach", str(wt), base, cwd=clone)
            rec.update(head=head, base=base,
                       rust_files=len([p for p in subprocess.run(
                           ["git", "-C", str(wt), "ls-files", "-z", "*.rs"], check=True,
                           capture_output=True).stdout.split(b"\0") if p]))
            cache = out / "cache" / repo
            shutil.rmtree(cache, ignore_errors=True)
            cache.mkdir(parents=True)
            rec["cold"] = phase(binary, wt, cache, out / f"{repo}.cold.log")
            git("checkout", "-q", "--detach", head, cwd=wt)
            rec["changed_files"] = len(git("diff", "--name-only", base, head, cwd=clone).splitlines())
            rec["warm_restart"] = phase(binary, wt, cache, out / f"{repo}.warm.log")
            git("worktree", "remove", "--force", str(wt), cwd=clone)
            shutil.rmtree(cache, ignore_errors=True)  # cache size is recorded; keep disk bounded
        except Exception as e:
            import traceback
            rec["error"] = repr(e)[:1000]
            rec["traceback"] = traceback.format_exc()[-2000:]
        with open(results, "a") as f:
            f.write(json.dumps(rec) + "\n")
        print(json.dumps({k: rec.get(k) for k in ("repo", "error")}), flush=True)


if __name__ == "__main__":
    main()
