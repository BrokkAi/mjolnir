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
import subprocess
import sys
import threading
import time
import traceback

SCRIPTS = pathlib.Path(__file__).resolve().parent
# Experiment data (instance, corpus, runs) lives outside the repository.
ROOT = pathlib.Path(os.environ.get("REVIEW_ARMS_DIR", "/mnt/optane/review-arms-20261113"))
MJX = SCRIPTS / "mjx"
# The mjolnir checkout that supplies commit messages; defaults to the one holding these scripts.
REPO = os.environ.get("REVIEW_ARMS_REPO") or subprocess.check_output(
    ["git", "-C", str(SCRIPTS), "rev-parse", "--show-toplevel"]).decode().strip()
ARMS = {
    "A": ["gpt-6.1-terra:high"] * 3,
    "B": ["gpt-6.1-terra:high", "gpt-6.1-sol:high"],
}
WORKSPACE = json.loads((ROOT / "instance/workspace.json").read_text())["workspace"]["id"]
CORPUS = {c["id"]: c for c in map(json.loads, (ROOT / "corpus/candidates-all.jsonl").read_text().splitlines())}


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
    return {"container": ids[0], "cwd": f"/workspace/{sid}/mjolnir",
            "mounts": [{"source": m.get("Source"), "destination": m.get("Destination"), "rw": m.get("RW")}
                       for m in mounts]}


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


def context_text(bug):
    message = subprocess.check_output(["git", "-C", REPO, "log", "-1", "--format=%B", bug["intro_commit"]]).decode()
    # Trailers point at external sessions and other branches; the task intent is the rest.
    message = "\n".join(line for line in message.splitlines()
                        if not re.match(r"(Co-Authored-By|Claude-Session|Signed-off-by):|\(cherry picked from commit ", line.strip(), re.I)).strip()
    return f"""# Historical change for code review

This message is context for a separate code reviewer. The repository is
checked out at commit {bug['intro_commit']}, which holds the change described
below. The checkout contains only this commit's history: no later commits,
branches, tags or remotes. The change is complete and committed.

## Commit message

{message}

## Reviewer constraints

The reviewer reviews only this change, using this checkout and its history.
The reviewer does not fetch or use the network, GitHub, or other repositories
or checkouts, and does not read user memory, session history, build caches, or
/run/hel/git-cache. The reviewer does not build, run tests, run Cargo, install
tools, or modify files. Helper agents it starts follow the same constraints.
"""


def compare_inventories(before, after):
    """Returns problems; extra objects are reported, not silently accepted."""
    problems = []
    for key in ["head", "head_tree", "base_tree", "diff_sha256", "remotes", "alternates", "status", "session_start"]:
        if before[key] != after[key]:
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
    if (d / "review-start.json").exists():
        # The review was started; finishing it only reads state, so it is safe to resume.
        identity = json.loads((d / "launch-intent.json").read_text())
        sid = json.loads((d / "launch.json").read_text())["session_id"]
        return collect(d, bug, identity, sid, json.loads((d / "placement.json").read_text()),
                       json.loads((d / "pre-inventory.json").read_text()))
    if (d / "launch-intent.json").exists():
        return "needs-recovery"
    identity = {"bug": bug["id"], "arm": arm, "reviewers": ARMS[arm], "intro": bug["intro_commit"],
                "base": bug["base_commit"], "time_ns": time.time_ns()}
    save(d / "launch-intent.json", identity)
    # Neutral title: reviewers must not see the bug id.
    title = "review-arms " + hashlib.sha256((bug["id"] + arm).encode()).hexdigest()[:8]
    with gate:
        launch = mj("new", "--workspace-id", WORKSPACE, "--profile", "carrier", "--target", "podman",
                    "--bundle", "mjolnir", "--at", bug["intro_commit"], "--base", bug["base_commit"],
                    "--no-later-history", *[v for r in ARMS[arm] for v in ["--reviewer", r]],
                    "--no-project-memory", "--no-github-events", "--no-github-credentials",
                    "--subagents", "none", "--title", title, timeout=900)
        save(d / "launch.json", launch)
        sid = launch["session_id"]
        s = wait_idle(sid)
    assert s["no_later_history"] and s["at"] == bug["intro_commit"] and s["base"] == bug["base_commit"], s
    place = container(sid)
    save(d / "placement.json", place)
    before = inventory(place, "pin", bug["base_commit"])
    save(d / "pre-inventory.json", before)
    assert list(before["session_start"].values()) == [bug["base_commit"]], before["session_start"]
    assert not before["remotes"] and not before["alternates"] and not before["status"], before
    assert before["base_tree"] != before["head_tree"]

    (d / "historical-context.txt").write_text(context_text(bug))
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
    tools = [i for p in carrier_pages for i in p["items"] if i["role"] in ("tool", "terminal")]
    assert not tools, "carrier used tools; review not started"
    # Reviewers read the carrier's workspace, and review results reach the carrier
    # while other reviewers still run: make the carrier unable to edit or browse.
    for key, value in [("mode", "plan"), ("web_fetch", "disabled")]:
        mj("set-config", "--session", sid, "--key", key, "--value", value)
    settings = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
    assert settings.get("mode") == "plan" and settings.get("web_fetch") == "disabled", settings
    save(d / "carrier-settings.json", settings)

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
        save(d / "verdict.json", verdict or {"kind": "cleared", "note": "review finished and cleared between polls"})
        if verdict and verdict.get("kind") == "failed":
            raise RuntimeError("review failed: " + json.dumps(verdict)[:800])
        reviewers = []
        for child, info in children.items():
            cd = d / "reviewers" / child[:12]
            row = session(child)
            save(cd / "session.json", row)
            settings = {c["key"]: c.get("current") for c in row.get("config_options", [])}
            pages = transcript(child)
            save(cd / "transcript.json", pages)
            save(cd / "usage.json", mj("usage", "--session", child))
            items = [i for p in pages for i in p["items"]]
            messages = handback_messages([i for i in items if i["role"] == "tool" and "handback" in json.dumps(i)])
            if messages:
                (cd / "output.md").write_text(messages[-1] + "\n")
            reviewers.append({"child": child, "slot": info["slot"], "profile": row.get("profile_id"),
                              "model": settings.get("model"), "effort": settings.get("effort"),
                              "has_output": bool(messages)})
        save(d / "reviewers.json", reviewers)
    # A review can clear before every reviewer was observed, or with a reviewer that
    # failed; such a run must not count as finished.
    expected = sorted(r.split(":")[0] for r in ARMS[identity["arm"]])
    got = sorted(r["model"] or "" for r in reviewers)
    incomplete = [r["child"] for r in reviewers if not r["has_output"] or r["effort"] != "high"]
    if got != expected or incomplete:
        raise RuntimeError(f"incomplete review: models {got}, expected {expected}; without output or wrong effort: {incomplete}")

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
    a = p.parse_args()
    jobs = json.loads(pathlib.Path(a.jobs).read_text())
    # Serialize session creation and startup; reviews themselves run concurrently.
    gate = threading.Semaphore(2)

    def one(job):
        try:
            return job, run_job(job, gate), None
        except Exception as e:  # report every failure with its job
            d = ROOT / "runs" / job["bug"] / job["arm"]
            save(d / "error.json", {"error": str(e), "traceback": traceback.format_exc(), "time_ns": time.time_ns()})
            return job, "failed", str(e)

    unfinished = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
        for job, state, error in pool.map(one, jobs):
            print(json.dumps({"job": job, "state": state, "error": error}), flush=True)
            unfinished += state != "done"
    return 1 if unfinished else 0


if __name__ == "__main__":
    sys.exit(main())
