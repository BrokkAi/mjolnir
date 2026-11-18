"""Runs verifier sessions (Opus/high by default) for every unverified batch in a verify dir.

Usage: python3 verify_driver.py <verify-dir> [--concurrency 7]
       python3 verify_driver.py <judge-dir> --kind judge [--concurrency 7]
       python3 verify_driver.py <tasks-dir> --kind task --output siblings.jsonl [--concurrency 7]

--kind task runs one session per unit: every subdirectory of <tasks-dir> that
holds an INSTRUCTIONS.md. All files of the unit are uploaded to review-input/<unit>/,
the session follows INSTRUCTIONS.md and writes --output in its working
directory, and the driver fetches it into the unit directory (a .jsonl output
must parse line by line). Used by brief_dilution.py's editor and verifier units.

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
import hashlib
import json
import os
import pathlib
import subprocess
import threading
import time

ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
MJX = pathlib.Path(__file__).resolve().parent / "mjx"
# Subscription Claude Code profiles that offer opus; claude-aws bills Bedrock.
# claude6 left out: its login failed with ACP auth_required on 2026-11-14.
PROFILES = ["claude", "claude2", "claude3", "claude4", "claude5", "claude7"]
p = argparse.ArgumentParser()
p.add_argument("verify")
p.add_argument("--concurrency", type=int, default=7)
p.add_argument("--kind", choices=["verify", "judge", "task"], default="verify")
# Model and profiles of the sessions; rounds A to F used Opus on the Claude profiles.
p.add_argument("--output", help="task: the file each unit's session writes, fetched into the unit directory")
p.add_argument("--model", default="opus")
p.add_argument("--profiles", default=",".join(PROFILES), help="comma-separated mj profiles, used in turn")
# Judge rounds normally run both judges with one model; --judges A (or B) runs one side, so the
# two judges of a batch can use different models in two invocations (brief/PLAN-bifrost-oracle-strict.md).
p.add_argument("--judges", default="A,B", help="judge: which of judges A and B this invocation runs")
p.add_argument("--quota-scope", help="model pool for REVIEW_ARMS_QUOTA_FLOOR (for example gemini)")
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
instance = os.environ.get("REVIEW_ARMS_INSTANCE", "default")
if instance != "default":
    p.error("verify_driver supports only REVIEW_ARMS_INSTANCE=default; the private instance is retired and existing verifier IDs belong to the default daemon")
if a.kind == "task" and not a.output:
    p.error("--kind task needs --output")
PROFILES = a.profiles.split(",")
REPO_NAME = a.bundle or os.environ.get("REVIEW_ARMS_REPO_DIR", "mjolnir")
V = ROOT / a.verify
REPO = ROOT / "verify/repo"
SOURCE = subprocess.check_output(["git", "-C", str(pathlib.Path(__file__).resolve().parent),
                                  "rev-parse", "--show-toplevel"], text=True).strip()
def ensure_checkout():
    """Builds the blinded verifier checkout once. Concurrent drivers (for example the A and B judge
    sides) serialize on a lock, and the checkout is built in a temporary directory and renamed into
    place only after the blinding check, so no driver can use a partial or unblinded checkout."""
    import fcntl
    import shutil
    REPO.parent.mkdir(parents=True, exist_ok=True)
    with open(REPO.parent / ".repo-setup.lock", "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        if REPO.exists():
            return
        tmp = REPO.with_name(REPO.name + f".building.{os.getpid()}")
        shutil.rmtree(tmp, ignore_errors=True)
        subprocess.run(["git", "clone", "-q", "--no-local", SOURCE, str(tmp)], check=True)
        for cmd in (["checkout", "-q", "--detach", a.repo_commit], ["branch", "-f", "master", a.repo_commit],
                    ["checkout", "-q", "master"], ["remote", "remove", "origin"],
                    ["reflog", "expire", "--expire=now", "--all"], ["gc", "-q", "--prune=now"]):
            subprocess.run(["git", "-C", str(tmp), *cmd], check=True)
        if subprocess.run(["git", "-C", str(tmp), "cat-file", "-e", "HEAD:.agents/evals"],
                          capture_output=True).returncode == 0:
            raise SystemExit("verifier checkout contains the eval results")
        tmp.rename(REPO)


ensure_checkout()
for sub in ("out", "sessions", "prompts"):
    (V / sub).mkdir(parents=True, exist_ok=True)
if a.kind == "verify" and not (V / "SPEC.md").exists():  # verifiers read the spec from the verify dir
    (V / "SPEC.md").write_text((pathlib.Path(__file__).resolve().parent / "verify/SPEC.md").read_text())
WORKSPACE_RECORD = ROOT / "default-instance/verify/workspace.json"


def ensure_workspace():
    """Creates or selects the verifier workspace once, under the same lock as the checkout, and
    publishes its record atomically so a concurrent driver never reads a partial file."""
    import fcntl
    REPO.parent.mkdir(parents=True, exist_ok=True)
    with open(REPO.parent / ".repo-setup.lock", "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        if WORKSPACE_RECORD.exists():
            return
        WORKSPACE_RECORD.parent.mkdir(parents=True, exist_ok=True)
        r = subprocess.run([str(MJX), "workspaces", "create", "Review arms verify", "--json"],
                           capture_output=True, check=True)
        tmp = WORKSPACE_RECORD.with_name(f"workspace.{os.getpid()}.tmp")
        tmp.write_bytes(r.stdout)
        os.replace(tmp, WORKSPACE_RECORD)


ensure_workspace()
WORKSPACE = json.loads(WORKSPACE_RECORD.read_text())["workspace"]["id"]


def mj(*args, timeout=300):
    r = subprocess.run([str(MJX), *map(str, args), "--json"], capture_output=True, timeout=timeout)
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
    if a.kind == "task":
        first = (f"Before you read anything, run `git checkout -q --detach {a.repo_commit}` in your working "
                 "directory, and do not open .agents/evals/ at any point.\n\n") if a.bundle else ""
        return first + f"""Read {INPUT}/{batch_path.name}/INSTRUCTIONS.md and follow it exactly. Your unit's files are in {INPUT}/{batch_path.name}/.

Write {a.output} in your current working directory when you finish.

The {REPO_NAME} checkout with full history is your current working directory. Read only {INPUT}/{batch_path.name}/ and that repository; do not open anything outside your working directory. Do not build, run tests, or edit any file other than {a.output}.
"""
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

The {REPO_NAME} checkout with full history is your current working directory. Read only {spec}, your batch file, and that repository; do not open anything outside your working directory. Do not build, run tests, or edit any file other than verdicts.jsonl.
"""


def upload_inputs(sid, batch_path):
    if a.kind == "task":
        for src in sorted(f for f in batch_path.rglob("*") if f.is_file() and f.name != a.output):
            rel = src.relative_to(batch_path)
            r = subprocess.run([str(MJX), "put-file", "--session", sid, "--path", f"{INPUT}/{batch_path.name}/{rel}",
                                "--overwrite", str(src)], capture_output=True, timeout=300)
            if r.returncode:
                raise RuntimeError(f"put-file {rel} into {sid}: {r.stderr.decode()[-400:]}")
        return
    for src in (V / "SPEC.md", batch_path):
        r = subprocess.run([str(MJX), "put-file", "--session", sid, "--path", f"{INPUT}/{src.name}", "--overwrite",
                            str(src)], capture_output=True, timeout=300)
        if r.returncode:
            raise RuntimeError(f"put-file {src.name} into {sid}: {r.stderr.decode()[-400:]}")


def fetch_output(sid):
    name = a.output if a.kind == "task" else "verdicts.jsonl" if a.kind == "judge" else "verify-out.json"
    r = subprocess.run([str(MJX), "fetch", "--session", sid, "--path", name, "--stdout"],
                       capture_output=True, timeout=300)
    if r.returncode:
        raise RuntimeError(f"session {sid} left no {name}: {r.stderr.decode()[-400:]}")
    if a.kind == "task":
        text = r.stdout.decode()
        if a.output.endswith(".jsonl"):
            for line in text.splitlines():
                if line.strip():
                    json.loads(line)  # a malformed line fails the unit visibly
        return {"text": text}
    if a.kind == "judge":
        return {"verdicts": [json.loads(line) for line in r.stdout.decode().splitlines() if line.strip()]}
    return json.loads(r.stdout)


def run(batch_path, judge=None):
    """One unit (a task, or one judge side of a batch), owned by exactly one invocation at a time."""
    import fcntl
    n = batch_path.name if a.kind == "task" else batch_path.stem.split("-")[1]
    tag = n if judge is None else f"{n}-{judge}"
    with open(V / "sessions" / f"unit-{tag}.lock", "w") as held:
        try:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError(f"unit {tag} is being run by another invocation") from None
        return run_unit(batch_path, judge)


def run_unit(batch_path, judge=None):
    n = batch_path.name if a.kind == "task" else batch_path.stem.split("-")[1]
    tag = n if judge is None else f"{n}-{judge}"
    out_path = V / "out" / (f"task-{n}.json" if a.kind == "task" else
                            f"verify-out-{n}.json" if judge is None else f"verdicts-{tag}.json")
    rec_path = V / "sessions" / f"batch-{tag}.json"
    if out_path.exists():
        # An existing output from another model is not this invocation's result: say so rather than
        # republish it (for example judge B first run on Opus, then rerun with --model gpt-6.1-sol).
        producer = json.loads(out_path.read_text()).get("producer") or {}
        made_by = (producer.get("config") or {}).get("model")
        made_on = producer.get("profile")
        # A split-judge invocation pins one side's model, so an output that predates producer records
        # cannot show it matches. Unknown and mismatched producers are both ineligible.
        unknown = not producer and a.kind == "judge" and len(JUDGES) == 1
        if unknown or (made_by is not None and made_by != a.model) or (made_on is not None and made_on not in PROFILES):
            # Mark the unit ineligible and retire any aggregate in one serialized step, so no collector
            # reads verdicts this check rejects.
            import fcntl
            with open(V / "workflow-out.lock", "w") as held:
                fcntl.flock(held, fcntl.LOCK_EX)
                save(V / "out" / f"invalid-{tag}.json", {"problems": {"producer": [made_on, made_by] if producer else None,
                                                                      "requested": [PROFILES, a.model]}})
                (V / "workflow-out.json").unlink(missing_ok=True)
            raise RuntimeError(f"unit {tag} already has output from "
                               f"{f'{made_on}/{made_by}' if producer else 'an unrecorded producer'}, not "
                               f"{','.join(PROFILES)}/{a.model}; move {out_path.name} and "
                               f"sessions/batch-{tag}.json aside to rerun it")
        # The cached output matches this invocation, so any earlier invalid marker for it is stale.
        (V / "out" / f"invalid-{tag}.json").unlink(missing_ok=True)
        return tag, "done"
    if rec_path.exists():
        rec = json.loads(rec_path.read_text())
        sid = rec["session_id"]
        # A session set up for another model or profile is not this invocation's unit to resume.
        recorded_model = (rec.get("config") or {}).get("model")
        if (recorded_model is not None and recorded_model != a.model) or rec.get("profile") not in PROFILES:
            raise RuntimeError(f"unit {tag} has a session for {rec.get('profile')}/{recorded_model}, not "
                               f"{','.join(PROFILES)}/{a.model}; move sessions/batch-{tag}.json aside to rerun it")
    else:
        profile = pick_profile()
        if os.environ.get("REVIEW_ARMS_QUOTA_FLOOR"):
            # Share the experiment drivers' floor interpretation. Import only
            # for guarded runs; older verifier data has no driver workspace.
            from driver import quota_hold
            hold = quota_hold(a.quota_scope)
            if hold:
                raise RuntimeError("held before verifier session start: " + hold)
        source = ["--bundle", a.bundle] if a.bundle else ["--project-directory", REPO]
        sid = mj("new", "--workspace-id", WORKSPACE, "--profile", profile, "--target", a.target,
                 *source, "--effort", "high", "--review", "off", "--no-project-memory",
                 "--no-github-events", "--no-github-credentials", "--subagents", "none", "--title", f"review-arms {V.name} {tag}",
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
        # Record the model before sending the prompt: a turn accepted before an interruption keeps
        # its true model, and the resume guard above then compares against it.
        rec = {**rec, "config": cfg}
        save(rec_path, rec)
        upload_inputs(sid, batch_path)
        # A stable command ID makes a resend after an interrupted run the same request,
        # which mj accepts once, instead of a second verifier turn.
        cid = f"review-arms-{V.name}-batch-{tag}-{sid[:12]}"
        if len(cid) > 64:  # mj accepts 1-64 characters (mj-core config::loading::validate_id)
            cid = f"review-arms-{hashlib.sha256(cid.encode()).hexdigest()[:24]}-{sid[:12]}"
        mj("message", "--session", sid, "--command-id", cid, "--prompt-file", prompt)
        rec = {**rec, "prompted": time.time_ns()}
        save(rec_path, rec)
        time.sleep(30)
    while True:
        s = session(sid)
        # A finished turn is done even if the end-of-turn attention classifier marks it needs_input
        # (it does for a closing summary); only an open turn waiting for input blocks.
        if s.get("is_idle") and s.get("last_turn_outcome"):
            break
        if s.get("attention") == "needs_input":
            raise RuntimeError(f"verifier {sid} is waiting for input (permission request?)")
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
        return run_unit(batch_path, judge)
    out = fetch_output(sid)
    problems = {} if a.kind == "task" else check(batch_path, out)
    if any(problems.values()):
        save(V / "out" / f"invalid-{tag}.json", {"session_id": sid, "problems": problems})
        raise RuntimeError(f"batch {tag} output does not cover its items exactly once: "
                           f"{ {k: len(v) for k, v in problems.items()} }")
    if a.kind == "task":
        (batch_path / a.output).write_text(out["text"])
        out = {"output": str(batch_path / a.output), "session_id": sid}
    # Bind the output to the session that produced it, so a consumer can check the model and
    # profile of the exact verdicts it reads rather than a session record that a rerun replaces.
    # The profile recorded is the one the session was on when its turn completed: quota recovery can
    # move a session to another profile after launch.
    out = {**out, "producer": {"session_id": sid, "profile": s.get("profile_id") or rec.get("profile"),
                               "launch_profile": rec.get("profile"), "config": rec.get("config")}}
    made_on, made_by = out["producer"]["profile"], (rec.get("config") or {}).get("model")
    if made_on not in PROFILES or made_by != a.model:
        # The turn finished outside the requested profiles (quota recovery moved it) or without a
        # recorded model: never publish it as this invocation's result.
        import fcntl
        with open(V / "workflow-out.lock", "w") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            save(V / "out" / f"invalid-{tag}.json", {"problems": {"producer": [made_on, made_by],
                                                                  "requested": [PROFILES, a.model]},
                                                     "session_id": sid})
            (V / "workflow-out.json").unlink(missing_ok=True)
        raise RuntimeError(f"unit {tag} finished on {made_on}/{made_by}, not {','.join(PROFILES)}/{a.model}")
    save(out_path, out)
    # A valid replacement makes the unit eligible again.
    (V / "out" / f"invalid-{tag}.json").unlink(missing_ok=True)
    mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900)
    return tag, "done"


if a.kind == "task":
    batches = sorted(d for d in V.iterdir() if d.is_dir() and (d / "INSTRUCTIONS.md").exists())
else:
    batches = sorted(V.glob("batch-*.json"), key=lambda f: int(f.stem.split("-")[1]))
JUDGES = a.judges.split(",")
if a.kind == "judge" and (not JUDGES or set(JUDGES) - {"A", "B"} or len(set(JUDGES)) != len(JUDGES)):
    p.error("--judges must name A, B or both")
units = [(b, j) for b in batches for j in (JUDGES if a.kind == "judge" else [None])]
if a.kind == "judge":
    import fcntl
    # A judge output that is absent (never run, or moved aside for a rejudge) makes the combined
    # aggregate stale: retire it under the aggregate lock so no collector reads the old round.
    with open(V / "workflow-out.lock", "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        if not all((V / "out" / f"verdicts-{b.stem.split('-')[1]}-{j}.json").exists()
                   and not (V / "out" / f"invalid-{b.stem.split('-')[1]}-{j}.json").exists()
                   for b in batches for j in ("A", "B")):
            (V / "workflow-out.json").unlink(missing_ok=True)
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
            import traceback
            print(json.dumps({"batch": b.stem + (f"-{j}" if j else ""), "state": "failed", "error": str(e),
                              "traceback": "".join(traceback.format_exception(e))[-2000:]}), flush=True)
both = [(b, j) for b in batches for j in ("A", "B")]
if a.kind == "judge" and not failed:
    import fcntl
    # One serialized decision point: under an exclusive lock, check that both judges' verdicts are
    # present, read them, and replace the aggregate. A concurrent A or B invocation (or a rejudge)
    # therefore publishes from the verdict files as they are when it holds the lock, never a stale
    # read taken before another invocation's publication.
    with open(V / "workflow-out.lock", "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        # Publish only when both sides have output and neither side is marked ineligible.
        if all((V / "out" / f"verdicts-{b.stem.split('-')[1]}-{j}.json").exists()
               and not (V / "out" / f"invalid-{b.stem.split('-')[1]}-{j}.json").exists() for b, j in both):
            verdicts = [{**v, "judge": j} for b, j in both
                        for v in json.loads((V / "out" / f"verdicts-{b.stem.split('-')[1]}-{j}.json")
                                            .read_text())["verdicts"]]
            target = V / "workflow-out.json"
            tmp = target.with_name(f"workflow-out.{os.getpid()}.tmp")
            tmp.write_text(json.dumps({"verdicts": verdicts, "missing": []}, indent=2) + "\n")
            os.replace(tmp, target)
raise SystemExit(1 if failed else 0)
