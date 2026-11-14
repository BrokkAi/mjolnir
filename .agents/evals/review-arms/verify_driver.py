"""Runs Opus/high verifier sessions for every unverified batch in a verify dir.

Usage: python3 verify_driver.py <verify-dir> [--concurrency 7]

Uses the default mj instance (Claude logins belong to its daemon). Each batch
gets one Claude Code session on the next profile in PROFILES, created without
a prompt so that model and effort can be set and checked first. The session
reads the batch, writes verify-out.json in its checkout, and the driver
fetches it, checks that every finding ID appears exactly once, and suspends
the session. A session record makes the step resumable: a batch with a
session is never given a second one.
"""
import argparse
import collections
import concurrent.futures
import itertools
import json
import os
import pathlib
import subprocess
import threading
import time

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
# Subscription Claude Code profiles that offer opus; claude-aws bills Bedrock.
# claude6 left out: its login failed with ACP auth_required on 2026-11-14.
PROFILES = ["claude", "claude2", "claude3", "claude4", "claude5", "claude7"]
p = argparse.ArgumentParser()
p.add_argument("verify")
p.add_argument("--concurrency", type=int, default=7)
a = p.parse_args()
V = ROOT / a.verify
REPO = ROOT / "verify/repo"  # history through 2026-11-13, no refs to the eval results
for sub in ("out", "sessions", "prompts"):
    (V / sub).mkdir(exist_ok=True)
WORKSPACE = json.loads((ROOT / "verify/workspace.json").read_text())["workspace"]["id"]


def mj(*args, timeout=300):
    r = subprocess.run(["mj", *map(str, args), "--json"], capture_output=True, timeout=timeout)
    if r.returncode:
        raise RuntimeError(f"mj {args[0]}: {r.stderr.decode()[-1200:]}")
    return json.loads(r.stdout) if r.stdout.strip() else {}


def session(sid):
    v = mj("sessions", "--session", sid)
    return v["sessions"][0] if "sessions" in v else v


def save(path, value):
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    tmp.replace(path)


def check(batch_path, out):
    ids = {f["id"] for c in json.loads(batch_path.read_text()) for f in c["findings"]}
    got = collections.Counter(f for c in out["commits"] for g in c["groups"] for f in g["findings"])
    return {"missing": sorted(ids - set(got)), "extra": sorted(set(got) - ids),
            "duplicated": sorted(f for f, n in got.items() if n > 1)}


profiles = itertools.cycle(PROFILES)
lock = threading.Lock()


def run(batch_path):
    n = batch_path.stem.split("-")[1]
    out_path, rec_path = V / "out" / f"verify-out-{n}.json", V / "sessions" / f"batch-{n}.json"
    if out_path.exists():
        return n, "done"
    if rec_path.exists():
        sid = json.loads(rec_path.read_text())["session_id"]
    else:
        with lock:
            profile = next(profiles)
        prompt = V / "prompts" / f"prompt-{n}.txt"
        prompt.write_text(f"""Read {V}/SPEC.md and follow it exactly.

Your batch: {batch_path}

Read only SPEC.md, your batch file, and the repository in your working directory. Do not open anything else under {ROOT} or /home/jonathan; other files there reveal which model wrote which finding.
""")
        sid = mj("new", "--workspace-id", WORKSPACE, "--profile", profile, "--target", "localhost",
                 "--project-directory", REPO, "--effort", "high", "--review", "off", "--no-project-memory",
                 "--no-github-events", "--subagents", "none", "--title", f"review-arms {V.name} {n}",
                 timeout=900)["session_id"]
        save(rec_path, {"session_id": sid, "profile": profile, "time_ns": time.time_ns()})
        while not (session(sid).get("is_idle") and session(sid).get("lifecycle") == "live"):
            time.sleep(5)
        for k, v in [("model", "opus"), ("effort", "high")]:
            mj("set-config", "--session", sid, "--key", k, "--value", v)
        cfg = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
        assert cfg.get("model") == "opus" and cfg.get("effort") == "high", cfg
        save(rec_path, {**json.loads(rec_path.read_text()), "config": cfg})
        mj("message", "--session", sid, "--prompt-file", prompt)
        time.sleep(30)
    while True:
        s = session(sid)
        if s.get("attention") == "needs_input":
            raise RuntimeError(f"verifier {sid} is waiting for input (permission request?)")
        if s.get("is_idle") and s.get("last_turn_outcome"):
            break
        time.sleep(60)
    r = subprocess.run(["mj", "fetch", "--session", sid, "--path", "verify-out.json", "--stdout"],
                       capture_output=True, timeout=300)
    if r.returncode:
        raise RuntimeError(f"verifier {sid} left no verify-out.json: {r.stderr.decode()[-400:]}")
    out = json.loads(r.stdout)
    problems = check(batch_path, out)
    if any(problems.values()):
        save(V / "out" / f"invalid-{n}.json", {"session_id": sid, "problems": problems})
        raise RuntimeError(f"batch {n} output does not cover its findings exactly once: "
                           f"{ {k: len(v) for k, v in problems.items()} }")
    save(out_path, out)
    mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900)
    return n, "done"


batches = sorted(V.glob("batch-*.json"), key=lambda f: int(f.stem.split("-")[1]))
failed = 0
with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
    futures = {pool.submit(run, b): b for b in batches}
    for f in concurrent.futures.as_completed(futures):
        try:
            n, state = f.result()
            print(json.dumps({"batch": n, "state": state}), flush=True)
        except Exception as e:  # report every failure with its batch
            failed += 1
            print(json.dumps({"batch": futures[f].stem, "state": "failed", "error": str(e)}), flush=True)
raise SystemExit(1 if failed else 0)
