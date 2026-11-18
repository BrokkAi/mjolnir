"""Runs native mj reviews of historical mjolnir commits for the review-arms experiment.

One job = one bug x one arm. Each job creates a carrier session at the bug's
introducing commit with --no-later-history, pins the review start to the base
commit, sends the historical commit message as the task, runs `mj review start`
with the arm's reviewers, and captures every reviewer's output, transcript and
usage. Every step writes a durable file; a job with launch-intent.json but no
done.json is never rerun automatically.

Usage: python3 driver.py --jobs jobs.json [--concurrency N]
jobs.json: [{"bug": "<corpus id>", "arm": "A"|"B"}, ...]
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
import pathlib
import re
import sqlite3
import subprocess
import sys
import threading
import time
import traceback

SCRIPTS = pathlib.Path(__file__).resolve().parent
# Experiment data (instance, corpus, runs) lives outside the repository.
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
MJX = SCRIPTS / "mjx"
# The mj instance mjx reaches (see mjx): the default instance, or the private one of the
# earlier stages. Each keeps its workspace record in its own directory under ROOT.
if os.environ.get("REVIEW_ARMS_INSTANCE", "default") == "private":
    INSTANCE, STORE = ROOT / "instance", ROOT / "instance/data/mj.sqlite3"
else:
    INSTANCE = ROOT / "default-instance"
    STORE = pathlib.Path.home() / ".local/share/mjolnir/mj.sqlite3"
# The host checkout that supplies commit messages; defaults to the one holding these scripts.
REPO = os.environ.get("REVIEW_ARMS_REPO") or subprocess.check_output(
    ["git", "-C", str(SCRIPTS), "rev-parse", "--show-toplevel"]).decode().strip()
# Source settings shared by writers, reviewers and scoring preparation.
CORPUS_PATH = pathlib.Path(os.environ.get("REVIEW_ARMS_CORPUS", SCRIPTS / "corpus/candidates-all.jsonl")).resolve()
DIFFS_PATH = pathlib.Path(os.environ.get("REVIEW_ARMS_DIFFS", SCRIPTS / "corpus/diffs.jsonl")).resolve()
BUNDLE = os.environ.get("REVIEW_ARMS_BUNDLE", "mjolnir")
REPO_DIR = os.environ.get("REVIEW_ARMS_REPO_DIR", "mjolnir")
# Reviewer sessions are independent, so setups are composed afterwards from
# sessions; a review only groups up to three sessions of one launch.
ARMS = {
    "A": ["gpt-6.1-terra:high"] * 3,
    "B": ["gpt-6.1-terra:high", "gpt-6.1-sol:high"],
    "C": ["muse-ember-1.0:high"] * 3,
    "D": ["gpt-6.1-sol:high"] * 3,
    "D2": ["gpt-6.1-sol:high"] * 2,  # stage 1 diffs already have one Sol session
    "E": ["gpt-6.1-sol:medium"] * 2,
    # deepseek-flash runs on the dsh-review profile (see README). F (DSF/high) was
    # stopped as too slow after 5 diffs; its runs are kept in runs-dsf-high/.
    "F": ["deepseek-flash:high", "gpt-6.1-terra:xhigh", "gpt-6.1-sol:xhigh"],
    "I": ["deepseek-flash:low"],  # 3-diff pilot
    # H and J give each diff 2 Terra/xhigh, 2 DSF/low and 2 Sol/xhigh sessions.
    "H": ["gpt-6.1-terra:xhigh"] * 2 + ["gpt-6.1-sol:xhigh"],
    "J": ["deepseek-flash:low"] * 2 + ["gpt-6.1-sol:xhigh"],
    # DSF/low proved as slow as DSF/high (126-561 s) and never handed back, so the J
    # jobs not yet started when it was dropped run as K: the Sol/xhigh session only.
    "K": ["gpt-6.1-sol:xhigh"],
    # Non-GPT reviewers, one session each. Muse's highest effort is xhigh.
    "L": ["muse-ember-1.0:xhigh", "gemini-4-hydrogen:high", "kimi-code/k3.1-pro:high"],
    "M": ["glm-5.3:high", "glm-5.3-flash:high"],  # GLM, one session each
}
# Hard-won candidates and the sampled ordinary diffs share one id space.
CORPUS = {c["id"]: c for path, setting in ((CORPUS_PATH, "REVIEW_ARMS_CORPUS"),
                                        (DIFFS_PATH, "REVIEW_ARMS_DIFFS"))
          if setting in os.environ or path.exists()
          for c in map(json.loads, path.read_text().splitlines())}
# Relation-brief arms (brief/PLAN.md), kept apart from ARMS above.
ARMS.update({"OT": ["gpt-6.1-terra:high"] * 3, "OS": ["gpt-6.1-sol:high"] * 3,
             "BT": ["gpt-6.1-terra:high"] * 3, "BS": ["gpt-6.1-sol:high"] * 3,
             # Stage C (brief/PLAN-bifrost.md): the Bifrost relation_brief tool's brief.
             "FT": ["gpt-6.1-terra:high"] * 3, "FS": ["gpt-6.1-sol:high"] * 3,
             # Ablation (brief/PLAN-ablation.md): Bifrost plus the decisive item; blind minus it.
             "DT": ["gpt-6.1-terra:high"] * 3, "DS": ["gpt-6.1-sol:high"] * 3,
             "XT": ["gpt-6.1-terra:high"] * 3, "XS": ["gpt-6.1-sol:high"] * 3,
             # Dilution (brief/PLAN-dilution.md): arm D's brief plus every sibling fact.
             "WT": ["gpt-6.1-terra:high"] * 3, "WS": ["gpt-6.1-sol:high"] * 3,
             # Wizard-of-Oz query arms (brief/PLAN-oz.md): no brief, a site_context tool.
             "QT": ["gpt-6.1-terra:high"] * 3, "QS": ["gpt-6.1-sol:high"] * 3,
             # Writer model (brief/PLAN-writer-terra.md): stage B's blind brief written by Terra/high.
             "TT": ["gpt-6.1-terra:high"] * 3,
             # Sonnet writer (brief/PLAN-writer-sonnet.md), same Terra reviewers.
             "TS": ["gpt-6.1-terra:high"] * 3,
             # Non-leaking oracle brief (brief/PLAN-bifrost-oracle-noleak.md), Terra reviewers.
             "ON": ["gpt-6.1-terra:high"] * 3,
             # Fresh no-brief control in the same window and image as ON (its 2026-11-18 amendment).
             "AF": ["gpt-6.1-terra:high"] * 3})


def workspace_id():
    """Read the selected instance's workspace only when a new session is launched."""
    path = INSTANCE / "workspace.json"
    try:
        return json.loads(path.read_text())["workspace"]["id"]
    except (OSError, KeyError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"cannot read workspace id from {path}; create the workspace with mjx and save its JSON there") from exc


def save(path, value):
    path = pathlib.Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    tmp.replace(path)


def mj(*args, timeout=300):
    result = subprocess.run([str(MJX), *map(str, args), "--json"], capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"mj {' '.join(map(str, args[:2]))}: {result.stderr.decode()[-1500:]}")
    return json.loads(result.stdout)


def session(sid):
    value = mj("sessions", "--session", sid)
    return value["sessions"][0] if "sessions" in value else value


def quota_hold(scope=None):
    """Why a new job must not start, or None. REVIEW_ARMS_QUOTA_FLOOR="codex4=25,..." names
    shared profiles whose least window must keep that many percent (a coordinator's limit,
    2026-11-16: codex4 is shared with other sessions). Checked once per launch, from the
    daemon's stored report; a missing or unreadable report holds the job, since unknown is
    not "enough"."""
    floor = os.environ.get("REVIEW_ARMS_QUOTA_FLOOR")
    if not floor:
        return None
    for spec in floor.split(","):
        profile, _, percent = spec.partition("=")
        rows = mj("quota", "--profile", profile).get("profiles") or []
        report = (rows[0].get("report") or {}) if rows else {}
        windows = report.get("windows") or []
        # Antigravity reports separate Gemini and Claude/GPT pools. Unscoped
        # windows apply to every model; an absent matching pool stays unknown.
        if scope:
            windows = [w for w in windows if not w.get("model_scope")
                       or scope in w["model_scope"].split(",")]
        left = [w.get("remaining_percent") for w in windows]
        if not left or None in left or report.get("error"):
            return f"{profile}: no usable quota report ({json.dumps(report)[:300]})"
        if min(left) < float(percent):
            return f"{profile}: {min(left)}% left, below the floor of {percent}%"
    return None


def wait_idle(sid, limit=1200):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        s = session(sid)
        if s.get("lifecycle") == "failed" or s.get("has_error"):
            raise RuntimeError(f"session {sid} failed: {json.dumps(s)[:800]}")
        if s.get("is_idle") and s.get("lifecycle") == "live":
            return s
        time.sleep(4)
    raise TimeoutError(f"session {sid} did not become idle")


def container(sid):
    ids = subprocess.check_output(["podman", "ps", "--filter", f"label=dev.mj.session={sid}",
                                   "--format", "{{.ID}}"]).decode().split()
    assert len(ids) == 1, ids
    mounts = json.loads(subprocess.check_output(["podman", "inspect", ids[0], "--format", "{{json .Mounts}}"]))
    # The default instance's Podman target follows agent-dev:latest, so record the image per job.
    image = subprocess.check_output(["podman", "inspect", ids[0], "--format", "{{.Image}}"]).decode().strip()
    return {"container": ids[0], "image": image, "cwd": f"/workspace/{sid}/{REPO_DIR}",
            "mounts": [{"source": m.get("Source"), "destination": m.get("Destination"), "rw": m.get("RW")}
                       for m in mounts]}


def reviewer_runtime(child, carrier):
    """The reviewer's container image and runtime, read while the reviewer is alive (reviewers are
    destroyed at role end). Podman reviewers run inside the carrier's container, which carries the
    carrier's session label; a reviewer with a container of its own is read from that instead.
    Harness binaries come from mj, so the mj version and mounts name them."""
    # Each probe command gets the 60 s the process listing below already had: a stalled podman call
    # ends this attempt (logged, retried) instead of holding collection.
    ids = subprocess.check_output(["podman", "ps", "--filter", f"label=dev.mj.session={child}",
                                   "--format", "{{.ID}}"], timeout=60).decode().split()
    source = "reviewer"
    child_state = {k: session(child).get(k) for k in ("lifecycle", "is_idle", "activity_state")}
    if not ids:
        # Only a live reviewer without a container of its own is known to run in the carrier's; a
        # finished one may have had its own, now gone, so it stays unobserved rather than inferred.
        if child_state.get("lifecycle") != "live":
            return None
        ids, source = [carrier], "carrier"
    if len(ids) != 1:
        return None
    info = json.loads(subprocess.check_output(["podman", "inspect", ids[0]], timeout=60))[0]
    image = json.loads(subprocess.check_output(["podman", "image", "inspect", info["Image"]], timeout=60))[0]
    version = subprocess.check_output([str(MJX), "--version"], timeout=60).decode().strip()
    # The installed CLI can change during a run; the container's live processes name the worker and
    # harness binaries actually serving this review.
    # check=True: a failed probe raises, is logged by record_runtimes and retried at the next poll.
    processes = subprocess.run(["podman", "exec", ids[0], "ps", "-eo", "args"], capture_output=True,
                               text=True, timeout=60, check=True).stdout.splitlines()
    # The process list is evidence of the reviewer's worker and harness only while its session is
    # live, so its state at the probe is recorded beside it.
    return {"container": ids[0], "container_source": source, "image": info["Image"],
            "reviewer_state_at_probe": child_state,
            "image_digest": image.get("Digest"),
            "repo_digests": image.get("RepoDigests"), "image_labels": (info.get("Config") or {}).get("Labels"),
            "mj_cli_version_at_observation": version, "processes": processes, "observed_ns": time.time_ns(),
            "mounts": [{"source": m.get("Source"), "destination": m.get("Destination"), "rw": m.get("RW")}
                       for m in info.get("Mounts") or []]}


# --same-image: every job of this run must use the image the first job recorded, checked before
# its review starts, so a moving image tag cannot split a paired comparison (ON/AF amendment).
IMAGE_PIN = None
IMAGE_PIN_LOCK = threading.Lock()


def image_pin_mismatch(place):
    """None when the carrier's image is the run's pinned image (pinning it if this is the first)."""
    if IMAGE_PIN is None:
        return None
    with IMAGE_PIN_LOCK:
        # Write a complete temporary file, then hard-link it into place: link fails if a pin exists,
        # so the first writer wins across processes and a visible pin is never partial.
        tmp = IMAGE_PIN.with_name(f"{IMAGE_PIN.name}.{os.getpid()}.{threading.get_ident()}.tmp")
        tmp.write_text(json.dumps({"image": place["image"], "time_ns": time.time_ns()}) + "\n")
        try:
            os.link(tmp, IMAGE_PIN)
        except FileExistsError:
            pinned = json.loads(IMAGE_PIN.read_text())["image"]
            return None if place["image"] == pinned else pinned
        finally:
            tmp.unlink()
        return None


def set_aside_carrier(d, sid, place, pinned):
    """A carrier on another image never reviews. The marker records the decision first; finishing it
    (suspend, move the launch records aside) is idempotent and resumes at the job's next start, so a
    later run starts a fresh carrier once the pinned image is current again."""
    save(d / "rejected-pending.json", {"tag": f"rejected-{time.time_ns()}", "session_id": sid,
                                       "image": place["image"], "pinned": pinned})
    finish_set_aside(d)


def finish_set_aside(d):
    marker = d / "rejected-pending.json"
    rec = json.loads(marker.read_text())
    tag, sid = rec["tag"], rec["session_id"]
    # Suspend is accepted asynchronously and can fail after acceptance; request it again whenever the
    # carrier is not yet suspended, and retire the launch records only once it has taken effect.
    # Otherwise the marker stays and the next start retries.
    state = session(sid).get("lifecycle")
    if state not in ("suspended", "closed"):
        with (d / f"{tag}-suspend.jsonl").open("a") as f:
            f.write(json.dumps({"time_ns": time.time_ns(), "state": state,
                                "response": mj("suspend", "--session", sid, "--acknowledge-unpublished-work",
                                               timeout=900)}) + "\n")
        # The request is accepted before checkpoint and teardown finish; wait for them within the
        # same bound the suspend call itself allows.
        end = time.monotonic() + 900
        while (state := session(sid).get("lifecycle")) not in ("suspended", "closed") and time.monotonic() < end:
            time.sleep(5)
    if state not in ("suspended", "closed"):
        raise RuntimeError(f"rejected carrier {sid} is {state}, not suspended yet; set-aside will be retried")
    for name in ("launch-intent.json", "launch.json", "placement.json", "pre-inventory.json"):
        if (d / name).exists():
            (d / name).rename(d / f"{tag}-{name}")
    marker.rename(d / f"{tag}-image.json")


RUNTIME_PROBES = {}  # (job dir, child) -> its probe thread
RUNTIME_STOP = {}  # job dir -> Event set when the review settles; probes then stop retrying
RUNTIME_PROBES_LOCK = threading.Lock()


def probe_runtime(d, child, carrier):
    """Samples one reviewer's runtime on its own thread, so a slow podman or mj call never delays the
    status polls that observe the other reviewers. Retries until it records the runtime or the
    reviewer is gone; every failed attempt is written to runtime-errors.jsonl."""
    path = d / "reviewers" / child[:12] / "runtime.json"
    stop = RUNTIME_STOP[d]
    while not path.exists() and not stop.is_set():
        try:
            runtime = reviewer_runtime(child, carrier)
        except Exception as e:  # noqa: BLE001 - a probe failure loses evidence, never fails a review
            with (d / "runtime-errors.jsonl").open("a") as f:
                f.write(json.dumps({"child": child, "error": repr(e), "time_ns": time.time_ns()}) + "\n")
            runtime = None
        if runtime:
            path.parent.mkdir(parents=True, exist_ok=True)
            save(path, runtime)
            return
        try:
            if session(child).get("lifecycle") in ("suspended", "closed", "failed", "destroyed"):
                return  # ended: collect() records it as unobserved; starting reviewers keep being probed
        except Exception:  # noqa: BLE001 - a destroyed reviewer has no session to read
            return
        stop.wait(5)


def record_runtimes(d, children):
    carrier = json.loads((d / "placement.json").read_text())["container"]
    with RUNTIME_PROBES_LOCK:
        RUNTIME_STOP.setdefault(d, threading.Event())
        for child in children:
            if (d, child) not in RUNTIME_PROBES and not (d / "reviewers" / child[:12] / "runtime.json").exists():
                t = threading.Thread(target=probe_runtime, args=(d, child, carrier), daemon=True)
                RUNTIME_PROBES[(d, child)] = t
                t.start()


def join_runtime_probes(d):
    """Once the review has settled, probes stop retrying; collect() waits for any attempt in flight
    (each command is bounded) so runtime.json is final before it reads them."""
    with RUNTIME_PROBES_LOCK:
        RUNTIME_STOP.setdefault(d, threading.Event()).set()
        threads = [t for (jd, _), t in RUNTIME_PROBES.items() if jd == d]
    for t in threads:
        t.join()


def inventory(place, mode, base):
    request = json.dumps({"mode": mode, "cwd": place["cwd"], "base": base}).encode()
    out = subprocess.run(["podman", "exec", "-i", place["container"], "python3", "-c",
                          (SCRIPTS / "inventory.py").read_text()], input=request, capture_output=True, timeout=300)
    if out.returncode:
        raise RuntimeError("inventory: " + out.stderr.decode()[-1500:])
    return json.loads(out.stdout)


def transcript(sid):
    pages, cursor = [], 0
    roles = [v for role in ["user", "message", "agent", "tool", "terminal", "thought", "system"]
             for v in ["--role", role]]
    while True:
        page = mj("transcript", "--session", sid, "--after-seq", cursor, "--limit", 500, *roles)
        pages.append(page)
        nxt = page.get("next_after_seq")
        if not page.get("items") or not nxt or nxt <= cursor or nxt >= page.get("latest_seq", nxt):
            return pages
        cursor = nxt


def handback_messages(items):
    """Completed handback tool calls; either layer may be a JSON string."""
    messages = []
    for item in items:
        call = item.get("body", {}).get("call", {})
        if call.get("status") != "completed":
            continue
        raw = call.get("rawInput", {})
        if isinstance(raw, str):
            try:
                raw = json.loads(raw)
            except json.JSONDecodeError:
                continue
        if not isinstance(raw, dict):
            continue
        args = raw.get("arguments", raw)
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except json.JSONDecodeError:
                continue
        message = args.get("message") if isinstance(args, dict) else None
        if isinstance(message, str) and message.strip():
            messages.append(message)
    return messages


def stored_reviewer(child):
    """A finished review's reviewer, read from the instance store.

    mj destroys a reviewer session when its role ends (8fe3c40bd), so `mj sessions` and
    `mj transcript` no longer know it. The handback text (subagent_handbacks) and the
    turn selection (session_turn_selections) stay in the store; usage comes from `mj usage`.
    The store is opened read-only; WAL mode lets that coexist with the running daemon.
    """
    db = sqlite3.connect(f"file:{STORE}?mode=ro", uri=True, timeout=30)
    try:
        report = db.execute("SELECT handback_message, handback_command_id FROM subagent_handbacks "
                            "WHERE child_session_id = ?", (child,)).fetchone()
        picks = db.execute("SELECT model, effort, profile FROM session_turn_selections "
                           "WHERE session_id = ? ORDER BY rowid", (child,)).fetchall()
    finally:
        db.close()
    model, effort, profile = picks[-1] if picks else (None, None, None)
    return {"handback_message": report[0] if report else None,
            "handback_command_id": report[1] if report else None,
            "selection": {"model": model, "effort": effort, "profile": profile}}


def status_efforts(d):
    """Per-slot "model / effort" from the review status line ("codex: m / high; codex: m / high · ...").
    The turn selection stores no effort for these reviewers, so the status text is its record."""
    log = d / "review-status.jsonl"
    found = []
    if log.exists():
        for line in log.read_text().splitlines():
            text = ((json.loads(line)["status"].get("review") or {}).get("status")) or ""
            m = re.match(r"((?:[^;·]+/ *\S+)(?:; *[^;·]+/ *\S+)*) *·", text)
            if m:
                found = [tuple(p.strip() for p in part.split(":", 1)[-1].split("/")) for part in m.group(1).split(";")]
    return found  # [(model, effort), ...] in slot order; the last status that listed them


def commit_message(bug):
    message = subprocess.check_output(["git", "-C", REPO, "log", "-1", "--format=%B", bug["intro_commit"]]).decode()
    # Trailers point at external sessions and other branches; the task intent is the rest.
    return "\n".join(line for line in message.splitlines()
                     if not re.match(r"(Co-Authored-By|Claude-Session|Signed-off-by):|\(cherry picked from commit ", line.strip(), re.I)).strip()


# Arms whose reviewers also get a relation brief (brief/PLAN.md), by brief kind.
BRIEF_ARMS = {"OT": "oracle", "OS": "oracle", "BT": "blind", "BS": "blind", "FT": "bifrost", "FS": "bifrost",
              "DT": "bifrost-plus", "DS": "bifrost-plus", "XT": "blind-minus", "XS": "blind-minus",
              "WT": "bifrost-diluted", "WS": "bifrost-diluted", "TT": "blind-terra", "TS": "blind-sonnet",
              "ON": "oracle-noleak"}
BRIEF_NOTE = ("These are relations computed from the repository at this commit, not findings. "
              "They may be incomplete.")


def brief_section(bug, arm):
    if arm not in BRIEF_ARMS:
        return ""
    brief = (ROOT / "briefs" / BRIEF_ARMS[arm] / bug["id"] / "brief.md").read_text().strip()
    return f"\n## Relations computed from the repository\n\n{BRIEF_NOTE}\n\n{brief}\n"


QUERY_ARMS = {"QT", "QS"}
QUERY_TOOL_NOTE = ("A `site_context` tool (MCP server `bifrost`) is available, and it is the one exception "
                   "to the network rule below. Give it a qualified function name or a `path:line` in this "
                   "commit. It returns the facts a static analyzer recorded for that site and says whether "
                   "the answer is complete.")


def context_text(bug, arm=None):
    message = commit_message(bug)
    if arm in QUERY_ARMS:
        tool_note = f"\n{QUERY_TOOL_NOTE}\n"
        network = "The reviewer does not fetch or use the network (except the `site_context` tool), GitHub, or other repositories"
    else:
        tool_note = ""
        network = "The reviewer does not fetch or use the network, GitHub, or other repositories"
    return f"""# Historical change for code review

This message is context for a separate code reviewer. The repository is
checked out at commit {bug['intro_commit']}, which holds the change described
below. The checkout contains only this commit's history: no later commits,
branches, tags or remotes. The change is complete and committed.

## Commit message

{message}
{brief_section(bug, arm)}{tool_note}
## Reviewer constraints

The reviewer reviews only this change, using this checkout and its history.
{network}
or checkouts, and does not read user memory, session history, build caches, or
/run/hel/git-cache. The reviewer does not build, run tests, run Cargo, install
tools, or modify files. Helper agents it starts follow the same constraints.
"""


def compare_inventories(before, after):
    """Returns problems; extra objects are reported, not silently accepted."""
    problems = []
    for key in ["head", "head_tree", "base_tree", "diff_sha256", "remotes", "alternates", "status", "session_start", "ignored"]:
        if key in before and before[key] != after.get(key):  # stage 1 inventories predate "ignored"
            problems.append(f"{key} changed: {before[key]!r} -> {after[key]!r}")
    new_commits = sorted(set(after["commits"]) - set(before["commits"]))
    if new_commits:
        problems.append(f"{len(new_commits)} new commit objects: {new_commits[:10]}")
    new_refs = sorted(set(after["refs"]) - set(before["refs"]))
    foreign = [r for r in new_refs if not r.startswith("refs/mj/")]
    if foreign:
        problems.append(f"new non-mj refs: {foreign}")
    return problems, new_refs


def run_job(job, gate):
    bug = CORPUS[job["bug"]]
    arm = job["arm"]
    d = ROOT / "runs" / bug["id"] / arm
    if (d / "done.json").exists():
        return "done"
    if (d / "rejected-pending.json").exists():
        finish_set_aside(d)  # an interrupted set-aside of a wrong-image carrier
    if (d / "review-start.json").exists():
        # The review was started; finishing it only reads state, so it is safe to resume.
        identity = json.loads((d / "launch-intent.json").read_text())
        sid = json.loads((d / "launch.json").read_text())["session_id"]
        return collect(d, bug, identity, sid, json.loads((d / "placement.json").read_text()),
                       json.loads((d / "pre-inventory.json").read_text()))
    recorded = (d / "launch.json").exists()
    intended = (d / "launch-intent.json").exists()
    if recorded != intended or any((d / name).exists() for name in
                                  ("historical-context.txt", "carrier-request.json", "review-start-intent.json")):
        return "needs-recovery"
    identity = {"bug": bug["id"], "arm": arm, "reviewers": ARMS[arm], "intro": bug["intro_commit"],
                "base": bug["base_commit"], "time_ns": time.time_ns()}
    with gate:
        hold = quota_hold()
    if hold:
        return "held: " + hold  # nothing recorded; the job starts on a later run
    workspace = workspace_id()
    if recorded:
        launch = json.loads((d / "launch.json").read_text())
        saved = json.loads((d / "launch-intent.json").read_text())
        if not (d / "pre-inventory.json").exists() or not (d / "placement.json").exists():
            return "needs-recovery"
        if any(saved.get(k) != identity[k] for k in ("bug", "arm", "reviewers", "intro", "base")):
            return "needs-recovery"
        identity = saved
        sid = launch["session_id"]
        s = wait_idle(sid)
        if (s.get("id") != sid or s.get("profile_id") != "carrier"
                or s.get("workspace_id", workspace) != workspace or s.get("last_turn_outcome")):
            return "needs-recovery"
    else:
        save(d / "launch-intent.json", identity)
        # Neutral title: reviewers must not see the bug id.
        title = "review-arms " + hashlib.sha256((bug["id"] + arm).encode()).hexdigest()[:8]
        with gate:
            launch = mj("new", "--workspace-id", workspace, "--profile", "carrier", "--target", "podman",
                        "--bundle", BUNDLE, "--at", bug["intro_commit"], "--base", bug["base_commit"],
                        "--no-later-history", *[v for r in ARMS[arm] for v in ["--reviewer", r]],
                        "--no-project-memory", "--no-github-events", "--no-github-credentials",
                        "--subagents", "none", "--title", title, timeout=900)
            save(d / "launch.json", launch)
            sid = launch["session_id"]
            s = wait_idle(sid)
    assert s["no_later_history"] and s["at"] == bug["intro_commit"] and s["base"] == bug["base_commit"], s
    place = container(sid)
    pinned = image_pin_mismatch(place)
    if pinned:
        set_aside_carrier(d, sid, place, pinned)
        return f"held: carrier image {place['image']} differs from the pinned {pinned}; carrier set aside"
    if recorded:
        if place != json.loads((d / "placement.json").read_text()):
            return "needs-recovery"
        before = json.loads((d / "pre-inventory.json").read_text())
        fresh = inventory(place, "audit", bug["base_commit"])
        problems, _ = compare_inventories(before, fresh)
        if problems:
            return "needs-recovery"
    else:
        save(d / "placement.json", place)
        before = inventory(place, "pin", bug["base_commit"])
        save(d / "pre-inventory.json", before)
    assert list(before["session_start"].values()) == [bug["base_commit"]], before["session_start"]
    assert not before["remotes"] and not before["alternates"] and not before["status"], before
    assert before["base_tree"] != before["head_tree"]

    # Reviewers read the carrier's workspace, and review results reach the carrier
    # while other reviewers still run: make the carrier unable to edit or browse
    # before it sees anything.
    for key, value in [("mode", "plan"), ("web_fetch", "disabled")]:
        current = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
        if current.get(key) != value:
            mj("set-config", "--session", sid, "--key", key, "--value", value)
    settings = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
    assert settings.get("mode") == "plan" and settings.get("web_fetch") == "disabled", settings
    save(d / "carrier-settings.json", settings)

    (d / "historical-context.txt").write_text(context_text(bug, arm))
    command = "carrier-" + hashlib.sha256((bug["id"] + arm).encode()).hexdigest()[:32]
    request = mj("message", "--session", sid, "--command-id", command, "--prompt-file", d / "historical-context.txt")
    save(d / "carrier-request.json", request)
    end = time.monotonic() + 1200
    while time.monotonic() < end:
        s = session(sid)
        outcome = s.get("last_turn_outcome") or {}
        if outcome.get("command_id") == command and s.get("is_idle"):
            assert outcome["outcome"]["kind"] == "completed", outcome
            save(d / "carrier-completion.json", outcome)
            break
        time.sleep(3)
    else:
        raise TimeoutError("carrier turn did not finish")
    carrier_pages = transcript(sid)
    save(d / "carrier-transcript.json", carrier_pages)
    # What reviewers need is an unchanged checkout; a carrier that only looked
    # around (it sometimes lists files) does not change what they review.
    tools = [{"seq": i["seq"], "title": (i["body"].get("call") or {}).get("title")}
             for p in carrier_pages for i in p["items"] if i["role"] in ("tool", "terminal")]
    save(d / "carrier-first-turn-tools.json", tools)
    problems, _ = compare_inventories(before, inventory(place, "audit", bug["base_commit"]))
    assert not problems, f"carrier changed the checkout; review not started: {problems}"

    save(d / "review-start-intent.json", {"time_ns": time.time_ns()})
    save(d / "review-start.json", mj("review", "start", "--session", sid))
    return collect(d, bug, identity, sid, place, before)


def observe(children, review):
    for i, role in enumerate(review.get("roles") or []):
        if role.get("child_session_id"):
            children[role["child_session_id"]] = {"slot": i, "role": role}
    if review.get("child_session_id"):
        children.setdefault(review["child_session_id"], {"slot": None, "role": None})


def await_review(d, sid):
    """Polls until a verdict appears or the review clears after its reviewers were seen.

    A finished review is delivered to the carrier and then cleared from status, so
    the verdict field can come and go between polls. Earlier polls are replayed so
    a resumed job still knows its reviewers.
    """
    children, verdict = {}, None
    log = d / "review-status.jsonl"
    if log.exists():
        for line in log.read_text().splitlines():
            observe(children, json.loads(line)["status"].get("review") or {})
    deadline = time.monotonic() + 3 * 3600
    while time.monotonic() < deadline:
        status = mj("review", "status", "--session", sid)
        with log.open("a") as f:
            f.write(json.dumps({"time_ns": time.time_ns(), "status": status}) + "\n")
        review = status.get("review")
        if review is None and children:
            return children, None
        observe(children, review or {})
        record_runtimes(d, children)
        verdict = (review or {}).get("verdict")
        if verdict:
            return children, verdict
        time.sleep(10)
    raise TimeoutError("review deadline")


def collect(d, bug, identity, sid, place, before):
    """Finishes a started review. Each step is skipped when its file exists, so a
    resumed job keeps evidence captured before the carrier could touch the workspace."""
    if (d / "reviewers.json").exists():
        reviewers = json.loads((d / "reviewers.json").read_text())
    else:
        children, verdict = await_review(d, sid)
        join_runtime_probes(d)
        save(d / "verdict.json", verdict or {"kind": "cleared", "note": "review finished and cleared between polls"})
        if verdict and verdict.get("kind") == "failed":
            raise RuntimeError("review failed: " + json.dumps(verdict)[:800])
        reviewers = []
        for child, info in children.items():
            cd = d / "reviewers" / child[:12]
            stored = stored_reviewer(child)
            save(cd / "stored.json", stored)
            usage = mj("usage", "--session", child)
            save(cd / "usage.json", usage)
            efforts = status_efforts(d)
            slot_pick = efforts[info["slot"]] if info["slot"] is not None and info["slot"] < len(efforts) else (None, None)
            if slot_pick[0] and stored["selection"]["model"] and slot_pick[0] != stored["selection"]["model"]:
                raise RuntimeError(f"reviewer {child}: status line lists {slot_pick}, store has {stored['selection']}")
            settings = {"model": stored["selection"]["model"] or slot_pick[0],
                        "effort": stored["selection"]["effort"] or slot_pick[1]}
            # A reviewer that ends its turn with its findings as a message instead of a
            # handback call (DeepSeek Flash on DSH did) leaves no stored handback and, with
            # the session destroyed, no transcript to recover them from: it counts as no output.
            message = (stored["handback_message"] or "").strip()
            messages = [message] if message else []
            output = message or None
            if output:
                (cd / "output.md").write_text(output + "\n")
            runtime_path = cd / "runtime.json"
            if not runtime_path.exists():
                # Never observed alive (it finished between polls): say so rather than guess.
                save(runtime_path, {"container": None, "note": "reviewer container not observed while alive"})
            reviewers.append({"child": child, "slot": info["slot"], "profile": stored["selection"]["profile"],
                              "image": json.loads(runtime_path.read_text()).get("image"),
                              "model": settings.get("model"), "effort": settings.get("effort"),
                              "has_output": bool(output), "handback": bool(messages)})
        # A review can clear before every reviewer was observed, or with a reviewer
        # that failed. Only a complete capture is saved as reviewers.json, so a
        # resumed job captures again instead of reloading an incomplete one.
        expected = sorted(ARMS[identity["arm"]])
        got = sorted(f"{r['model']}:{r['effort']}" for r in reviewers)
        incomplete = [r["child"] for r in reviewers if not r["has_output"]]
        if got != expected or incomplete:
            save(d / "reviewers-incomplete.json", reviewers)
            raise RuntimeError(f"incomplete review: reviewers {got}, expected {expected}; "
                               f"without output: {incomplete}")
        save(d / "reviewers.json", reviewers)

    if (d / "inventory-check.json").exists():
        problems = json.loads((d / "inventory-check.json").read_text())["problems"]
    else:
        after = inventory(place, "audit", bug["base_commit"])
        save(d / "post-inventory.json", after)
        problems, new_refs = compare_inventories(before, after)
        save(d / "inventory-check.json", {"problems": problems, "new_refs": new_refs})

    # The carrier's own turns after the review are irrelevant; stop them. Background
    # processes it started are ended by the suspend below.
    end = time.monotonic() + 600
    while (session(sid).get("activity_state") or {}).get("state") == "turn":
        try:
            save(d / "carrier-interrupt.json", mj("interrupt-turn", "--session", sid))
        except RuntimeError as e:
            if "no turn to cancel" not in str(e):  # the turn ended on its own meanwhile
                raise
        if time.monotonic() > end:
            raise TimeoutError("carrier turn did not stop")
        time.sleep(5)
    pages = transcript(sid)
    save(d / "carrier-transcript-after.json", pages)
    items = [i for p in pages for i in p["items"]]
    finals = [i["seq"] for i in items if i["role"] == "system" and "Review of commits" in json.dumps(i["body"])]
    early = [{"seq": i["seq"], "kind": (i["body"].get("call") or {}).get("kind"),
              "title": (i["body"].get("call") or {}).get("title")}
             for i in items if i["role"] in ("tool", "terminal") and (not finals or i["seq"] < finals[0])]
    save(d / "carrier-check.json", {"final_delivery_seq": finals[0] if finals else None, "tools_before_final": early})
    save(d / "suspend.json", mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900))
    save(d / "done.json", dict(identity, session_id=sid, reviewers=reviewers, inventory_problems=problems,
                               carrier_tools_before_final=early, finished_ns=time.time_ns()))
    return "done"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--jobs", required=True)
    p.add_argument("--concurrency", type=int, default=1)
    p.add_argument("--same-image", help="path of a pin file: every job must use the first job's image")
    p.add_argument("--stop-on-failure", action="store_true",
                   help="stop admitting new jobs after a failed, held, or recovery-needed job")
    a = p.parse_args()
    global IMAGE_PIN
    IMAGE_PIN = pathlib.Path(a.same_image) if a.same_image else None
    jobs = json.loads(pathlib.Path(a.jobs).read_text())
    # Serialize session creation and startup; reviews themselves run concurrently.
    gate = threading.Semaphore(2)
    admission_lock = threading.Lock()
    stopped = False

    def one(job):
        nonlocal stopped
        with admission_lock:
            if a.stop_on_failure and stopped:
                return job, "not-started", None
        try:
            state, error = run_job(job, gate), None
        except Exception as e:  # report every failure with its job
            d = ROOT / "runs" / job["bug"] / job["arm"]
            save(d / "error.json", {"error": str(e), "traceback": traceback.format_exc(), "time_ns": time.time_ns()})
            state, error = "failed", str(e)
        if a.stop_on_failure and state != "done":
            with admission_lock:
                stopped = True
        return job, state, error

    unfinished = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
        for future in concurrent.futures.as_completed([pool.submit(one, job) for job in jobs]):
            job, state, error = future.result()
            print(json.dumps({"job": job, "state": state, "error": error}), flush=True)
            unfinished += state != "done"
    return 1 if unfinished else 0


if __name__ == "__main__":
    sys.exit(main())
