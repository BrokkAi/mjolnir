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
import sys
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
p.add_argument("--text-only", action="store_true",
               help="pilot judgment task: one pinned profile and a sealed empty Podman checkout")
p.add_argument("--instance", help="named instance for sealed pilot judges")
p.add_argument("--seal-probe", type=pathlib.Path, help="trusted isolation probe for sealed pilot judges")
p.add_argument("--provider-model", help="concrete provider-reported Fable model ID")
p.add_argument("--output", help="task: the file each unit's session writes, fetched into the unit directory")
# Model and profiles of the sessions; rounds A to F used Opus on the Claude profiles.
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
if a.text_only:
    import re
    if (a.kind != "task" or a.output != "verdicts.jsonl" or not a.instance or not a.bundle
            or not re.fullmatch(r"[0-9a-f]{40}", a.repo_commit) or not a.seal_probe
            or not os.access(a.seal_probe, os.X_OK) or len(PROFILES) != 1 or not PROFILES[0]
            or a.model != "fable" or not re.fullmatch(r"claude-fable-(?:[0-9]+-)*[0-9]+", a.provider_model or "")):
        p.error("--text-only requires a named instance, pinned profile, Fable provider model, "
                "empty bundle commit, executable seal probe and verdicts.jsonl task")
    import tomllib
    import driver as pilot_driver
    config_path = pilot_driver.use_named_instance(a.instance) / "config.toml"
    config = tomllib.loads(config_path.read_text())
    bundle = config["bundles"][a.bundle]
    repositories = bundle["repositories"]
    if len(repositories) != 1 or repositories[0]["id"] != bundle["primary_repo"]:
        p.error("the sealed judge bundle must contain exactly one repository")
    JUDGE_DESTINATION = repositories[0]["destination"]
    pilot_driver.workspace_id()
elif a.instance or a.seal_probe or a.provider_model:
    p.error("--instance, --seal-probe and --provider-model require --text-only")
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


if not a.text_only:
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


if not a.text_only:
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
        if a.text_only:
            return (f"Read {INPUT}/{batch_path.name}/INSTRUCTIONS.md and follow it exactly. "
                    f"Read only the files in {INPUT}/{batch_path.name}/. "
                    f"Write {a.output} in your current working directory. Do not use the network.")
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


def invocation(*args):
    return pilot_driver.mj_invocation(*args) if a.text_only else [str(MJX), *args]


def upload_inputs(sid, batch_path):
    if a.kind == "task":
        if a.text_only:
            sources = [batch_path / name for name in ("INSTRUCTIONS.md", "SPEC.md", "PILOT.md", "items.json")]
        else:
            sources = sorted(f for f in batch_path.rglob("*") if f.is_file() and f.name != a.output)
        for src in sources:
            rel = src.relative_to(batch_path)
            r = subprocess.run(invocation("put-file", "--session", sid, "--path", f"{INPUT}/{batch_path.name}/{rel}",
                                          "--overwrite", str(src)), capture_output=True, timeout=300)
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
    r = subprocess.run(invocation("fetch", "--session", sid, "--path", name, "--stdout"),
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
        if a.text_only:
            return run_sealed_judge(batch_path)
        return run_unit(batch_path, judge)


def suspend(sid):
    mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900)


EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"


def attest_judge(attempt, sid):
    state = pilot_driver.wait_idle(sid)
    if (state.get("no_later_history") is not True or state.get("at") != a.repo_commit
            or state.get("base") != a.repo_commit or state.get("profile_id") != PROFILES[0]):
        raise RuntimeError(f"judge {sid} did not launch with the pinned profile and checkout")
    place = pilot_driver.container(sid)
    place["cwd"] = f"/workspace/{sid}/{JUDGE_DESTINATION}"
    before = pilot_driver.inventory(place, "audit", a.repo_commit)
    if (before["head"] != a.repo_commit or before["head_tree"] != EMPTY_TREE
            or before["commits"] != [a.repo_commit] or before["object_count"] != 2
            or before["remotes"] or before["alternates"] or before["status"] or before["ignored"]):
        raise RuntimeError(f"judge {sid} checkout is not a clean empty parentless commit")
    evidence = pilot_driver.attest_pilot_cache(attempt, place, a.repo_commit, a.repo_commit)
    if (len(evidence["snapshots"]) != 1
            or next(iter(evidence["snapshots"].values()))["commits"] != [a.repo_commit]):
        raise RuntimeError(f"judge {sid} has an unexpected Git cache")
    process = subprocess.run([str(a.seal_probe.resolve()), place["container"]], capture_output=True, timeout=300)
    save(attempt / "isolation.json", {"placement": place, "checkout": before, "probe_exit": process.returncode,
                                      "probe_stdout": process.stdout.decode(errors="replace"),
                                      "probe_stderr": process.stderr.decode(errors="replace")})
    if process.returncode:
        raise RuntimeError(f"judge {sid} failed its isolation probe")


def durable_files(*paths):
    # A receipt can outlive its worker. Flush the evidence and directory records before teardown.
    directories = {path.parent for path in paths}
    for path in [*paths, *directories, *(path.parent for path in directories)]:
        fd = os.open(path, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


def publish_sealed_judge(attempt, batch_path, output_path):
    from pilot_judge import EVIDENCE_FILES, load, sha, validate_payload_verdicts, verify_capture

    receipt_path = attempt / "receipt.json"
    receipt = load(receipt_path)
    producer = verify_capture(attempt, receipt, a.provider_model)
    sid = producer["session_id"]
    if (producer["profile"], producer["launch_profile"]) != (PROFILES[0], PROFILES[0]):
        raise RuntimeError(f"judge {sid} receipt is for another profile")
    data = (attempt / a.output).read_bytes()
    validate_payload_verdicts(load(batch_path / "items.json"), data)
    if sha(data) != producer["output_sha256"]:
        raise ValueError(f"judge {sid}: fetched verdict differs from receipt")
    durable_files(receipt_path, attempt / "launch.json", *(attempt / name for name in EVIDENCE_FILES))
    cleanup = attempt / "destroy.json"
    if not cleanup.is_file():
        live = pilot_driver.mj("sessions", "--all")["sessions"]
        result = (pilot_driver.mj("destroy", "--session", sid, timeout=900)
                  if any(row["id"] == sid for row in live) else {"already_absent": True})
        save(cleanup, result)
    (batch_path / a.output).write_bytes(data)
    save(output_path, receipt)


def run_sealed_judge(batch_path):
    import audit
    import brief_driver
    from pilot_judge import EVIDENCE_FILES, load, provider_model, sha, validate_payload_verdicts

    tag = batch_path.name
    output_path = V / "out" / f"task-{tag}.json"
    home = V / "sessions" / tag
    attempt = home / "attempt"
    attempt.mkdir(parents=True, exist_ok=True)
    prompt = V / "prompts" / f"prompt-{tag}.txt"
    prompt.write_text(prompt_text(batch_path, None))
    while True:
        launch_path = attempt / "launch.json"
        if (attempt / "void.json").is_file():
            sid = load(launch_path)["session_id"]
            attempt.rename(home / f"void-{sid}")
            attempt.mkdir()
        if not launch_path.is_file():
            launch_intent = attempt / "launch-intent.json"
            if launch_intent.exists():
                raise RuntimeError(f"{attempt}: launch has no recorded session ID; reconcile before retrying")
            profile = pick_profile()
            if hold := pilot_driver.pilot_weekly_hold(profile, scope=a.model):
                raise RuntimeError("held before judge launch: " + hold)
            title = f"Assessment {tag}"
            scope = {"profile": profile, "instance": a.instance, "bundle": a.bundle, "commit": a.repo_commit}
            save(launch_intent, {**scope, "title": title})
            sid = brief_driver.sealed_launch(a.bundle, a.repo_commit, profile, title,
                                             base=a.repo_commit, effort="high")
            save(launch_path, {**scope, "session_id": sid})
        launch = load(launch_path)
        sid = launch["session_id"]
        if (launch["profile"], launch["instance"], launch["bundle"], launch["commit"]) != (
                PROFILES[0], a.instance, a.bundle, a.repo_commit):
            raise RuntimeError(f"judge {sid} has a different recorded launch configuration")
        if (attempt / "receipt.json").is_file():
            publish_sealed_judge(attempt, batch_path, output_path)
            return tag, "done"
        settings_path, request_path = attempt / "settings.json", attempt / "request.json"
        command = f"judge-{tag}-{sid[:12]}"
        if not settings_path.is_file():
            attest_judge(attempt, sid)
            while True:
                try:
                    save(settings_path, brief_driver.configure_pilot_model(sid, a.model, "high"))
                    break
                except RuntimeError as error:
                    if "initialization is still running" not in str(error):
                        raise
                    pilot_driver.wait_idle(sid)
                    time.sleep(5)
        if not request_path.is_file():
            upload_inputs(sid, batch_path)
            save(request_path, pilot_driver.mj("message", "--session", sid, "--command-id", command,
                                                "--prompt-file", prompt))
        turn = load(request_path)["turn_id"]
        waited = pilot_driver.mj("wait", "--session", sid, "--turn", turn, "--return-on-input",
                                 "--timeout", 1800, timeout=1830)
        if waited.get("turn_id") != turn or waited["outcome"] != "finished":
            raise RuntimeError(f"judge {sid} turn {turn} ended with {waited['outcome']}; session retained")
        state = pilot_driver.session(sid)
        if (state.get("profile_id") != PROFILES[0]
                or (state["last_turn_outcome"]["outcome"]).get("kind") != "completed"):
            raise RuntimeError(f"judge {sid} changed profiles or did not complete; session retained")
        try:
            transcript_path = attempt / "transcript.json"
            save(transcript_path, pilot_driver.transcript(sid))
            verdict = audit.audit_pilot(transcript_path, sid, commits=(a.repo_commit,), single_commit=True)
            save(attempt / "audit.json", verdict)
            if not verdict["passed"]:
                if any(hit["kind"] == "missing-or-unreadable-transcript" for hit in verdict["hits"]):
                    raise RuntimeError("the transcript capture could not be audited")
                save(attempt / "void.json", {"session_id": sid, "reason": "transcript audit failed"})
                print(json.dumps({"batch": tag, "state": "void", "retained_session": sid}),
                      file=sys.stderr, flush=True)
                continue
            usage = pilot_driver.mj("usage", "--session", sid)
            save(attempt / "usage.json", usage)
            model = provider_model(usage, command, turn, a.provider_model)
            data = fetch_output(sid)["text"].encode("utf-8")
            (attempt / a.output).write_bytes(data)
            validate_payload_verdicts(load(batch_path / "items.json"), data)
            producer = {"sealed": True, "session_id": sid, "profile": state["profile_id"],
                        "launch_profile": launch["profile"], "config": load(settings_path),
                        "provider_model": model, "command": command, "turn": turn, "output_sha256": sha(data)}
            save(attempt / "receipt.json", {"producer": producer,
                                          "evidence_sha256": {name: sha((attempt / name).read_bytes())
                                                              for name in EVIDENCE_FILES}})
        except (OSError, KeyError, ValueError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
            raise RuntimeError(f"judge {sid}: {error}; session retained") from error


def run_unit(batch_path, judge=None):
    n = batch_path.name if a.kind == "task" else batch_path.stem.split("-")[1]
    tag = n if judge is None else f"{n}-{judge}"
    out_path = V / "out" / (f"task-{n}.json" if a.kind == "task" else
                            f"verify-out-{n}.json" if judge is None else f"verdicts-{tag}.json")
    rec_path = V / "sessions" / f"batch-{tag}.json"
    if out_path.exists():
        # An existing output from another model is not this invocation's result: say so rather than
        # republish it (for example judge B first run on Opus, then rerun with --model gpt-6.1-sol).
        cached = json.loads(out_path.read_text())
        producer = cached.get("producer") or {}
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
                 "--no-github-events", "--no-github-credentials", "--subagents", "none",
                 "--title", f"review-arms {V.name} {tag}",
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
        suspend(sid)
        rec_path.rename(V / "sessions" / f"quota-batch-{tag}-{sid[:12]}.json")
        print(json.dumps({"batch": tag, "state": "quota", "session": sid, "profile": s.get("profile_id")}), flush=True)
        return run_unit(batch_path, judge)
    out = fetch_output(sid)
    problems = {} if a.kind == "task" else check(batch_path, out)
    if any(problems.values()):
        save(V / "out" / f"invalid-{tag}.json", {"session_id": sid, "problems": problems})
        raise RuntimeError(f"batch {tag} output does not cover its items exactly once: "
                           f"{ {k: len(v) for k, v in problems.items()} }")
    # Check who produced the result before exposing it anywhere a collector reads: the profile the
    # session was on when its turn completed (quota recovery can move it) and the recorded model.
    producer = {"session_id": sid, "profile": s.get("profile_id") or rec.get("profile"),
                "launch_profile": rec.get("profile"), "config": rec.get("config")}
    made_on, made_by = producer["profile"], (rec.get("config") or {}).get("model")
    if made_on not in PROFILES or made_by != a.model:
        import fcntl
        with open(V / "workflow-out.lock", "w") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            save(V / "out" / f"invalid-{tag}.json", {"problems": {"producer": [made_on, made_by],
                                                                  "requested": [PROFILES, a.model]},
                                                     "session_id": sid})
            (V / "workflow-out.json").unlink(missing_ok=True)
        raise RuntimeError(f"unit {tag} finished on {made_on}/{made_by}, not {','.join(PROFILES)}/{a.model}")
    if a.kind == "task":
        (batch_path / a.output).write_text(out["text"])
        out = {"output": str(batch_path / a.output), "session_id": sid}
    out = {**out, "producer": producer}
    save(out_path, out)
    # A valid replacement makes the unit eligible again.
    (V / "out" / f"invalid-{tag}.json").unlink(missing_ok=True)
    suspend(sid)
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
