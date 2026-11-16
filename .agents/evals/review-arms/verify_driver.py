"""Runs verifier sessions (Opus/high by default) for every unverified batch in a verify dir.

Usage: python3 verify_driver.py <verify-dir> [--concurrency 7]
       python3 verify_driver.py <judge-dir> --kind judge [--concurrency 7]

--kind judge runs the relation-brief judging round that `brief_score.py build`
prepared: two independent judges (A and B) per batch, each a session of its own,
that write verdicts.jsonl per judge/SPEC.md. When every judge is done, the
driver writes <judge-dir>/workflow-out.json for `brief_score.py collect`.

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
p.add_argument("--kind", choices=["verify", "judge"], default="verify")
# Model and profiles of the sessions; rounds A to F used Opus on the Claude profiles.
p.add_argument("--model", default="opus")
p.add_argument("--profiles", default=",".join(PROFILES), help="comma-separated mj profiles, used in turn")
# Any mj target: each session gets SPEC.md and its batch uploaded into its workspace (INPUT), so the
# session reads nothing of this host's filesystem and cannot reach the eval's results.
p.add_argument("--target", default="localhost")
# Container targets take a bundle, not a local project directory. A bundle checks out current
# master, which holds this eval's results; the prompt then has the session detach to
# --repo-commit (master before the first eval commit) before it reads anything.
p.add_argument("--bundle")
# Verifiers need later history (fixes) but must not reach this eval's results,
# which map sessions to models. 3c3fa4b13 is master just before the first eval commit.
p.add_argument("--repo-commit", default="3c3fa4b13")
a = p.parse_args()
PROFILES = a.profiles.split(",")
V = ROOT / a.verify
REPO = ROOT / "verify/repo"
SOURCE = subprocess.check_output(["git", "-C", str(pathlib.Path(__file__).resolve().parent),
                                  "rev-parse", "--show-toplevel"], text=True).strip()
if not REPO.exists():
    REPO.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "clone", "-q", "--no-local", SOURCE, str(REPO)], check=True)
    for cmd in (["checkout", "-q", "--detach", a.repo_commit], ["branch", "-f", "master", a.repo_commit],
                ["checkout", "-q", "master"], ["remote", "remove", "origin"],
                ["reflog", "expire", "--expire=now", "--all"], ["gc", "-q", "--prune=now"]):
        subprocess.run(["git", "-C", str(REPO), *cmd], check=True)
    assert subprocess.run(["git", "-C", str(REPO), "cat-file", "-e", "HEAD:.agents/evals"],
                          capture_output=True).returncode != 0, "verifier checkout contains the eval results"
for sub in ("out", "sessions", "prompts"):
    (V / sub).mkdir(parents=True, exist_ok=True)
if a.kind == "verify" and not (V / "SPEC.md").exists():  # verifiers read the spec from the verify dir
    (V / "SPEC.md").write_text((pathlib.Path(__file__).resolve().parent / "verify/SPEC.md").read_text())
if not (ROOT / "verify/workspace.json").exists():  # creates the workspace, or selects one with this name
    r = subprocess.run(["mj", "workspaces", "create", "Review arms verify", "--json"], capture_output=True, check=True)
    (ROOT / "verify/workspace.json").write_bytes(r.stdout)
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


LABELS = {"caught", "partial", "missed"}
VERDICT_FIELDS = {"output_id", "bug", "label", "matching_finding", "rationale", "finding_count", "confidence"}


def check(batch_path, out):
    if a.kind == "judge":
        ids = {o["output_id"] for b in json.loads(batch_path.read_text()) for o in b["outputs"]}
        got = collections.Counter(v.get("output_id") for v in out["verdicts"])
        malformed = sorted(str(v.get("output_id")) for v in out["verdicts"]
                           if VERDICT_FIELDS - set(v) or v.get("label") not in LABELS)
    else:
        ids = {f["id"] for c in json.loads(batch_path.read_text()) for f in c["findings"]}
        got = collections.Counter(f for c in out["commits"] for g in c["groups"] for f in g["findings"])
        malformed = []
    return {"missing": sorted(ids - set(got)), "extra": sorted(set(got) - ids),
            "duplicated": sorted(f for f, n in got.items() if n > 1), "malformed": malformed}


profiles = itertools.cycle(PROFILES)
lock = threading.Lock()
# Profiles whose verifier turn ended at the provider's quota limit during this run.
exhausted = set()


def pick_profile():
    with lock:
        for _ in PROFILES:
            profile = next(profiles)
            if profile not in exhausted:
                return profile
    raise RuntimeError(f"every profile reported QuotaLimit: {sorted(exhausted)}")


INPUT = "review-input"  # workspace folder that receives SPEC.md and the batch


def prompt_text(batch_path, judge):
    spec, batch = f"{INPUT}/SPEC.md", f"{INPUT}/{batch_path.name}"
    first = (f"Before you read anything, run `git checkout -q --detach {a.repo_commit}` in your working "
             "directory, and do not open .agents/evals/ at any point.\n\n") if a.bundle else ""
    if judge is None:
        return first + f"""Read {spec} and follow it exactly.

Your batch: {batch}

Read only {spec}, your batch file, and the repository in your working directory. Do not open anything outside your working directory.
"""
    return first + f"""Read {spec} and follow it exactly, with one change: write verdicts.jsonl in your current working directory, not in a report directory.

Your batch: {batch}
Judge every output in it.

The mjolnir checkout with full history is your current working directory. Read only {spec}, your batch file, and that repository; do not open anything outside your working directory. Do not build, run tests, or edit any file other than verdicts.jsonl.
"""


def upload_inputs(sid, batch_path):
    for src in (V / "SPEC.md", batch_path):
        r = subprocess.run(["mj", "put-file", "--session", sid, "--path", f"{INPUT}/{src.name}", "--overwrite",
                            str(src)], capture_output=True, timeout=300)
        if r.returncode:
            raise RuntimeError(f"put-file {src.name} into {sid}: {r.stderr.decode()[-400:]}")


def fetch_output(sid):
    name = "verdicts.jsonl" if a.kind == "judge" else "verify-out.json"
    r = subprocess.run(["mj", "fetch", "--session", sid, "--path", name, "--stdout"],
                       capture_output=True, timeout=300)
    if r.returncode:
        raise RuntimeError(f"session {sid} left no {name}: {r.stderr.decode()[-400:]}")
    if a.kind == "judge":
        return {"verdicts": [json.loads(line) for line in r.stdout.decode().splitlines() if line.strip()]}
    return json.loads(r.stdout)


def run(batch_path, judge=None):
    n = batch_path.stem.split("-")[1]
    tag = n if judge is None else f"{n}-{judge}"
    out_path = V / "out" / (f"verify-out-{n}.json" if judge is None else f"verdicts-{tag}.json")
    rec_path = V / "sessions" / f"batch-{tag}.json"
    if out_path.exists():
        return tag, "done"
    if rec_path.exists():
        rec = json.loads(rec_path.read_text())
        sid = rec["session_id"]
    else:
        profile = pick_profile()
        source = ["--bundle", a.bundle] if a.bundle else ["--project-directory", REPO]
        sid = mj("new", "--workspace-id", WORKSPACE, "--profile", profile, "--target", a.target,
                 *source, "--effort", "high", "--review", "off", "--no-project-memory",
                 "--no-github-events", "--subagents", "none", "--title", f"review-arms {V.name} {tag}",
                 timeout=900)["session_id"]
        rec = {"session_id": sid, "profile": profile, "time_ns": time.time_ns()}
        save(rec_path, rec)
    if not rec.get("prompted"):
        # A session created by an interrupted run has no prompt yet; set it up here.
        prompt = V / "prompts" / f"prompt-{tag}.txt"
        prompt.write_text(prompt_text(batch_path, judge))
        while not (session(sid).get("is_idle") and session(sid).get("lifecycle") == "live"):
            time.sleep(5)
        for k, v in [("model", a.model), ("effort", "high")]:
            # An idle, live session can still be initializing; mj then refuses with 409 until
            # initialization ends, so retry exactly that refusal.
            while True:
                try:
                    mj("set-config", "--session", sid, "--key", k, "--value", v)
                    break
                except RuntimeError as e:
                    if "initialization is still running" not in str(e):
                        raise
                    time.sleep(10)
        cfg = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
        assert cfg.get("model") == a.model and cfg.get("effort") == "high", cfg
        upload_inputs(sid, batch_path)
        # A stable command ID makes a resend after an interrupted run the same request,
        # which mj accepts once, instead of a second verifier turn.
        mj("message", "--session", sid, "--command-id", f"review-arms-{V.name}-batch-{tag}-{sid[:12]}",
           "--prompt-file", prompt)
        rec = {**rec, "config": cfg, "prompted": time.time_ns()}
        save(rec_path, rec)
        time.sleep(30)
    while True:
        s = session(sid)
        if s.get("attention") == "needs_input":
            raise RuntimeError(f"verifier {sid} is waiting for input (permission request?)")
        if s.get("is_idle") and s.get("last_turn_outcome"):
            break
        time.sleep(60)
    if ((s["last_turn_outcome"].get("outcome") or {}).get("stop_reason")) == "QuotaLimit":
        # The provider stopped the turn; a partial pass is not a verification. Set the
        # session aside and start the batch again on a profile that has not hit its limit.
        # Each retry marks its profile exhausted, so the retries end.
        with lock:
            exhausted.update({rec["profile"], s.get("profile_id")} - {None})
        mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900)
        rec_path.rename(V / "sessions" / f"quota-batch-{tag}-{sid[:12]}.json")
        print(json.dumps({"batch": tag, "state": "quota", "session": sid, "profile": s.get("profile_id")}), flush=True)
        return run(batch_path, judge)
    out = fetch_output(sid)
    problems = check(batch_path, out)
    if any(problems.values()):
        save(V / "out" / f"invalid-{tag}.json", {"session_id": sid, "problems": problems})
        raise RuntimeError(f"batch {tag} output does not cover its items exactly once: "
                           f"{ {k: len(v) for k, v in problems.items()} }")
    save(out_path, out)
    mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900)
    return tag, "done"


batches = sorted(V.glob("batch-*.json"), key=lambda f: int(f.stem.split("-")[1]))
units = [(b, j) for b in batches for j in (["A", "B"] if a.kind == "judge" else [None])]
failed = 0
with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
    futures = {pool.submit(run, b, j): (b, j) for b, j in units}
    for f in concurrent.futures.as_completed(futures):
        try:
            tag, state = f.result()
            print(json.dumps({"batch": tag, "state": state}), flush=True)
        except Exception as e:  # report every failure with its batch
            failed += 1
            b, j = futures[f]
            print(json.dumps({"batch": b.stem + (f"-{j}" if j else ""), "state": "failed", "error": str(e)}),
                  flush=True)
if a.kind == "judge" and not failed:
    # The input brief_score.py collect reads (it was written by brief/workflow-judge.js before).
    verdicts = [{**v, "judge": j} for b, j in units
                for v in json.loads((V / "out" / f"verdicts-{b.stem.split('-')[1]}-{j}.json").read_text())["verdicts"]]
    save(V / "workflow-out.json", {"verdicts": verdicts, "missing": []})
raise SystemExit(1 if failed else 0)
