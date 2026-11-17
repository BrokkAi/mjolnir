"""Writes a blind relation brief for each diff with one isolated writer session.

Usage: python3 brief_driver.py <diff id> ... [--concurrency N]
       python3 brief_driver.py --lint <diff id> ...   # lint existing briefs only

Each writer is a session with the reviewers'
isolation: carrier-style checkout at the introducing commit with
--no-later-history on the Podman target, no project memory, GitHub or helper
agents. It gets brief/WRITER.md with the base commit and the commit message
(trailers removed), writes brief.md in its checkout, and the driver fetches
it, checks the rules a script can check (lint), and asks the same writer to
fix a brief that breaks them, at most FIX_ROUNDS times. Output goes to
$REVIEW_ARMS_DIR/briefs/<kind>/<diff>/ (--kind, default blind: the Sol/high
writer of stage B; blind-terra: the same writer run on Terra/high,
brief/PLAN-writer-terra.md); the transcript is stored under
reviewers/ so that audit.py can scan it. Every step writes a file; a writer
with a session is resumed, never started twice.
"""
import argparse
import concurrent.futures
import hashlib
import json
import re
import subprocess
import sys
import threading
import time
import traceback

import driver
from driver import ROOT, SCRIPTS, MJX, mj, save, session

# Brief kind -> writer model and effort. Everything else (prompt, tools, blinding, lint) is shared.
KINDS = {"blind": ("gpt-6.1-sol", "high"), "blind-terra": ("gpt-6.1-terra", "high")}
KIND = "blind"
WRITER_MODEL, WRITER_EFFORT = KINDS[KIND]
# --profile pins the writer to one mj profile (None: load balancing picks one).
WRITER_PROFILE = None
FIX_ROUNDS = 2
MAX_ITEMS = 25
# "About 1,500 tokens" (brief/PLAN.md); at about 4 characters a token, with slack.
MAX_CHARS = 7000
SECTIONS = ["## Affected callers", "## Paths", "## Value flow"]
JUDGMENT = re.compile(r"\b(bugs?|wrong|missing|should|risks?|risky|unsafe|issues?|problems?|incorrect(ly)?|"
                      r"broken|fails? to|forgets?|forgot|forgotten|flaw|mistake|careful|beware|ensure)\b", re.I)
ANCHOR = re.compile(r"([A-Za-z0-9_./-]+\.[A-Za-z0-9]+):(\d+)(?:-(\d+))?")


def changed_lines(bug):
    """New-side line numbers each file's hunks touch at the introducing commit."""
    out = subprocess.check_output(["git", "-C", driver.REPO, "diff", "-U0", "--no-color",
                                   bug["base_commit"], bug["intro_commit"]]).decode(errors="replace")
    lines, path = {}, None
    for line in out.splitlines():
        if line.startswith("+++ "):
            path = None if line[4:] == "/dev/null" else line[6:]
        elif line.startswith("@@") and path:
            m = re.match(r"@@ -\S+ \+(\d+)(?:,(\d+))? @@", line)
            start, count = int(m.group(1)), int(m.group(2) or 1)
            lines.setdefault(path, set()).update(range(start, start + count))
    return lines


def lint(text, bug, in_diff_items=()):
    """Rule violations a script can see. Items in in_diff_items (a secondary oracle item,
    brief/PLAN.md) may relate two changed locations."""
    changed = changed_lines(bug)
    exempt = {i.strip().removeprefix("- ").strip() for i in in_diff_items if i}
    problems = []
    for s in SECTIONS:
        if s not in text:
            problems.append(f"section `{s}` is absent")
    items = [l.strip()[2:] for l in text.splitlines() if l.strip().startswith("- ")]
    items = [i for i in items if i.strip().lower() != "none"]
    if len(items) > MAX_ITEMS:
        problems.append(f"{len(items)} items; the limit is {MAX_ITEMS}")
    if len(text) > MAX_CHARS:
        problems.append(f"{len(text)} characters; the limit is about 1,500 tokens ({MAX_CHARS} characters)")
    for n, item in enumerate(items, 1):
        prose = re.sub(r"`[^`]*`", "", item)  # identifiers such as `missing_ok` are not judgments
        words = sorted({m.group(0).lower() for m in JUDGMENT.finditer(prose)})
        if words:
            problems.append(f"item {n} uses judgment words {words}: {item[:120]}")
        anchors = [(m.group(1), int(m.group(2)), int(m.group(3) or m.group(2))) for m in ANCHOR.finditer(item)]
        if not anchors:
            problems.append(f"item {n} has no file:line anchor: {item[:120]}")
        elif item.strip() not in exempt and all(p in changed and set(range(a, b + 1)) <= changed[p]
                                                for p, a, b in anchors):
            problems.append(f"item {n} has no anchor outside the changed lines: {item[:120]}")
    return problems


def wait_command(sid, command, limit=1800):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        s = session(sid)
        outcome = s.get("last_turn_outcome") or {}
        if outcome.get("command_id") == command and s.get("is_idle"):
            if (outcome.get("outcome") or {}).get("stop_reason") == "QuotaLimit":
                raise RuntimeError(f"writer turn hit QuotaLimit: {json.dumps(outcome)[:600]}")
            if outcome["outcome"]["kind"] != "completed":
                raise RuntimeError(f"writer turn did not complete: {json.dumps(outcome)[:600]}")
            return outcome
        if s.get("lifecycle") == "failed" or s.get("has_error"):
            raise RuntimeError(f"writer session {sid} failed: {json.dumps(s)[:600]}")
        time.sleep(5)
    raise TimeoutError(f"writer turn {command} did not finish")


def fetch_brief(sid):
    r = subprocess.run([str(MJX), "fetch", "--session", sid, "--path", "brief.md", "--stdout"],
                       capture_output=True, timeout=300)
    if r.returncode:
        raise RuntimeError(f"writer {sid} left no brief.md: {r.stderr.decode()[-400:]}")
    return r.stdout.decode()


def send(d, sid, name, command, text):
    (d / f"{name}.txt").write_text(text)
    if not (d / f"{name}-request.json").exists():
        save(d / f"{name}-request.json", mj("message", "--session", sid, "--command-id", command,
                                            "--prompt-file", d / f"{name}.txt"))
    if not (d / f"{name}-completion.json").exists():
        save(d / f"{name}-completion.json", wait_command(sid, command))


def run(bug_id, gate):
    bug = driver.CORPUS[bug_id]
    d = ROOT / "briefs" / KIND / bug_id
    if (d / "done.json").exists():
        return "done"
    # Stage B's tag is kept for its kind, so its command ids and titles stay the same.
    tag = hashlib.sha256(("brief" + ("" if KIND == "blind" else KIND) + bug_id).encode()).hexdigest()
    if (d / "launch.json").exists():
        sid = json.loads((d / "launch.json").read_text())["session_id"]
    elif (d / "launch-intent.json").exists():
        return "needs-recovery"  # a session may exist without a record; find it before rerunning
    else:
        with gate:
            hold = driver.quota_hold()
        if hold:
            return "held: " + hold  # nothing recorded; the writer starts on a later run
        workspace = driver.workspace_id()
        save(d / "launch-intent.json", {"bug": bug_id, "model": WRITER_MODEL, "effort": WRITER_EFFORT,
                                        "profile": WRITER_PROFILE, "time_ns": time.time_ns()})
        with gate:
            launch = mj("new", "--workspace-id", workspace, "--target", "podman", "--bundle", driver.BUNDLE,
                        "--at", bug["intro_commit"], "--base", bug["base_commit"], "--no-later-history",
                        # mj new takes a profile or a model, not both; a pinned writer gets its
                        # model below, before its first prompt, as verify_driver.py does.
                        *(["--profile", WRITER_PROFILE] if WRITER_PROFILE else ["--model", WRITER_MODEL]),
                        "--effort", WRITER_EFFORT, "--review", "off",
                        "--no-project-memory", "--no-github-events", "--no-github-credentials",
                        "--subagents", "none", "--title", "brief " + tag[:8], timeout=900)
            save(d / "launch.json", launch)
            sid = launch["session_id"]
            driver.wait_idle(sid)
    if not (d / "settings.json").exists():
        s = session(sid)
        assert s["no_later_history"] and s["at"] == bug["intro_commit"] and s["base"] == bug["base_commit"], s
        for key in ("web_fetch", "web_search"):
            if any(c["key"] == key for c in s.get("config_options", [])):
                mj("set-config", "--session", sid, "--key", key, "--value", "disabled")
        for key, value in (("model", WRITER_MODEL), ("effort", WRITER_EFFORT)):
            current = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
            if current.get(key) != value:
                mj("set-config", "--session", sid, "--key", key, "--value", value)
        settings = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
        assert settings.get("model") == WRITER_MODEL and settings.get("effort") == WRITER_EFFORT, settings
        if WRITER_PROFILE:
            assert s.get("profile_id") == WRITER_PROFILE, s.get("profile_id")
        save(d / "settings.json", settings)
        place = driver.container(sid)
        save(d / "placement.json", place)
        before = driver.inventory(place, "audit", bug["base_commit"])
        assert not before["remotes"] and not before["alternates"] and not before["status"], before
        save(d / "pre-inventory.json", before)

    prompt = ((SCRIPTS / "brief/WRITER.md").read_text()
              .replace("{base}", bug["base_commit"]).replace("{message}", driver.commit_message(bug)))
    send(d, sid, "prompt", "brief-" + tag[:32], prompt)
    text = fetch_brief(sid)
    rounds = 0
    while True:
        problems = lint(text, bug)
        save(d / f"lint-{rounds}.json", problems)
        if not problems or rounds == FIX_ROUNDS:
            break
        rounds += 1
        fix = ("Your brief.md breaks these rules from the instructions:\n\n"
               + "\n".join(f"- {p}" for p in problems)
               + "\n\nFix brief.md so that it follows every rule, then reply `brief written`.\n")
        send(d, sid, f"fix-{rounds}", f"brief-fix{rounds}-" + tag[:28], fix)
        text = fetch_brief(sid)
    # brief.md is where the oracle builder and the blind arms read; it is written only on success.
    (d / "brief-candidate.md").write_text(text)
    if not (d / "inventory-check.json").exists():
        # The writer may add brief.md and nothing else; audit.py reads this file.
        place = json.loads((d / "placement.json").read_text())
        after = driver.inventory(place, "audit", bug["base_commit"])
        save(d / "post-inventory.json", after)
        only_brief = {k: "?? brief.md" for k in ("status", "ignored")}
        problems_inv, new_refs = driver.compare_inventories(
            json.loads((d / "pre-inventory.json").read_text()),
            {**after, **{k: "" for k, v in only_brief.items() if after.get(k, "").strip() == v}})
        save(d / "inventory-check.json", {"problems": problems_inv, "new_refs": new_refs})
    cd = d / "reviewers" / sid[:12]
    save(cd / "transcript.json", driver.transcript(sid))
    save(cd / "usage.json", mj("usage", "--session", sid))
    inv = json.loads((d / "inventory-check.json").read_text())["problems"]
    if inv or problems:
        # No done.json: a rerun fails the same way instead of passing an invalid brief on.
        # The session stays live for inspection.
        (d / "brief-rejected.md").write_text(text)
        save(d / "failed.json", {"bug": bug_id, "session_id": sid, "fix_rounds": rounds, "lint_problems": problems,
                                 "inventory_problems": inv, "time_ns": time.time_ns()})
        raise RuntimeError(f"brief not accepted: inventory problems {inv}; "
                           f"{len(problems)} rule violations after {rounds} fix rounds (lint-{rounds}.json)")
    save(d / "suspend.json", mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900))
    (d / "brief.md").write_text(text)
    save(d / "done.json", {"bug": bug_id, "session_id": sid, "fix_rounds": rounds, "finished_ns": time.time_ns()})
    return "done"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("bugs", nargs="+")
    p.add_argument("--concurrency", type=int, default=1)
    p.add_argument("--lint", action="store_true")
    p.add_argument("--kind", choices=sorted(KINDS), default="blind")
    p.add_argument("--profile", help="mj profile for the writers (default: load balancing)")
    p.add_argument("--stop-on-failure", action="store_true",
                   help="stop admitting new bugs after a failure, held job, or recovery-needed job")
    a = p.parse_args()
    global KIND, WRITER_MODEL, WRITER_EFFORT, WRITER_PROFILE
    KIND, WRITER_PROFILE = a.kind, a.profile
    WRITER_MODEL, WRITER_EFFORT = KINDS[KIND]
    if a.lint:
        for b in a.bugs:
            d = ROOT / "briefs" / KIND / b
            text = (d / "brief.md" if (d / "brief.md").exists() else d / "brief-candidate.md").read_text()
            print(json.dumps({"bug": b, "problems": lint(text, driver.CORPUS[b])}))
        return 0
    gate = threading.Semaphore(2)  # serialize session startup as driver.py does
    admission_lock = threading.Lock()
    stopped = False

    def one(b):
        nonlocal stopped
        with admission_lock:
            if a.stop_on_failure and stopped:
                return b, "not-started", None
        try:
            state = run(b, gate)
            error = None
        except Exception as e:  # report every failure with its diff
            save(ROOT / "briefs" / KIND / b / "error.json",
                 {"error": str(e), "traceback": traceback.format_exc(), "time_ns": time.time_ns()})
            state, error = "failed", str(e)
        if a.stop_on_failure and (state != "done"):
            with admission_lock:
                stopped = True
        return b, state, error

    unfinished = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
        for f in concurrent.futures.as_completed([pool.submit(one, b) for b in a.bugs]):
            b, state, error = f.result()
            print(json.dumps({"bug": b, "state": state, "error": error}), flush=True)
            unfinished += state != "done"
    return 1 if unfinished else 0


if __name__ == "__main__":
    sys.exit(main())
