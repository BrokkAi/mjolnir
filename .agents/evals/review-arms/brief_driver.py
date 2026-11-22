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
brief/PLAN-writer-terra.md; blind-sonnet: Sonnet/high,
brief/PLAN-writer-sonnet.md); the transcript is stored under
reviewers/ so that audit.py can scan it. Every step writes a file; a writer
with a session is resumed, never started twice.
"""
import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import re
import subprocess
import sys
import time
import threading
import time
import traceback

import driver
from driver import ROOT, SCRIPTS, mj, save, session

# Brief kind -> writer model and effort. Everything else (prompt, tools, blinding, lint) is shared.
KINDS = {"blind": ("gpt-6.1-sol", "high"), "blind-terra": ("gpt-6.1-terra", "high"),
         "blind-sonnet": ("sonnet", "high")}
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


class WriterTurnFailed(RuntimeError):
    """The requested command reached a terminal failure."""


class WriterBriefMissing(RuntimeError):
    """The completed writer left no brief.md in its checkout."""


class PromptUncertain(RuntimeError):
    """Keep the recorded session and reconcile this command before sending another."""


class WriterNeedsInput(RuntimeError):
    def __init__(self, command, questions):
        self.command = command
        self.questions = questions
        summary = "\n".join(question.get("message") or question.get("title") or question["id"]
                            for question in questions)
        super().__init__(f"writer needs input. {summary}")


def wait_command(sid, command, limit=1800, *, awaiting_receipt=False):
    """limit=None waits for the session's own outcome, with no observer deadline."""
    end = None if limit is None else time.monotonic() + limit
    while end is None or time.monotonic() < end:
        s = session(sid)
        outcome = s.get("last_turn_outcome") or {}
        if outcome.get("command_id") == command and s.get("is_idle"):
            if (outcome.get("outcome") or {}).get("stop_reason") == "QuotaLimit":
                raise WriterTurnFailed(f"writer turn hit QuotaLimit: {json.dumps(outcome)[:600]}")
            if outcome["outcome"]["kind"] != "completed":
                raise WriterTurnFailed(f"writer turn did not complete: {json.dumps(outcome)[:600]}")
            return outcome
        if questions := s.get("pending_elicitations"):
            raise WriterNeedsInput(command, questions)
        if s.get("lifecycle") == "failed":
            raise WriterTurnFailed(f"session {sid} failed: {json.dumps(s)[:600]}")
        if awaiting_receipt and s.get("is_idle") and s.get("lifecycle") == "live":
            raise PromptUncertain(f"{sid}: no recorded outcome for {command}; submission remains uncertain")
        time.sleep(5)
    raise TimeoutError(f"writer turn {command} did not finish")


def fetch_brief(sid):
    r = subprocess.run(driver.mj_invocation("fetch", "--session", sid, "--path", "brief.md", "--stdout"),
                       capture_output=True, timeout=300)
    if r.returncode:
        detail = r.stderr.decode(errors="replace")
        failure = f"writer {sid} left no brief.md: {detail[-400:]}"
        if "does not exist on the session's target" in detail:
            raise WriterBriefMissing(failure)
        raise RuntimeError(failure)
    return r.stdout.decode()


def send(d, sid, name, command, text, limit=1800, *, reconcile=False):
    prompt = d / f"{name}.txt"
    intent = d / f"{name}-intent.json"
    request = d / f"{name}-request.json"
    completion = d / f"{name}-completion.json"
    if completion.exists():
        return
    identity = {"session_id": sid, "command_id": command, "prompt_sha256": hashlib.sha256(text.encode()).hexdigest()}
    try:
        if intent.exists() and json.loads(intent.read_text()) != identity:
            raise PromptUncertain(f"{sid}: inputs changed for pending command {command}")
        if not (request.exists() or intent.exists()):
            if reconcile:
                raise PromptUncertain(f"{sid}: missing command receipt for {command}")
            if prompt.exists():
                # Older attempts recorded the prompt before submission but had no intent receipt.
                if prompt.read_text() != text:
                    raise PromptUncertain(f"{sid}: inputs changed for pending command {command}")
                save(intent, identity)
            else:
                prompt.write_text(text)
                save(intent, identity)
                try:
                    receipt = mj("message", "--session", sid, "--command-id", command, "--prompt-file", prompt)
                except RuntimeError as error:
                    if "session initialization is still running" in str(error):
                        # The daemon rejected the command before handing it to the worker.
                        prompt.unlink()
                        intent.unlink()
                    raise
                if (not isinstance(receipt, dict) or receipt.get("session_id") != sid or receipt.get("via") != "turn"
                        or type(receipt.get("turn_id")) is not int):
                    raise PromptUncertain(f"{sid}: missing acceptance receipt for {command}")
                save(request, receipt)
        save(completion, wait_command(sid, command, limit, awaiting_receipt=reconcile or not request.exists()))
    except (WriterTurnFailed, WriterNeedsInput, PromptUncertain):
        raise
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
        raise PromptUncertain(f"{sid}: could not confirm {command}: {error}") from error


def resume_input_required(d, sid, failure):
    if failure["retained_session"] != sid:
        raise PromptUncertain(f"{sid}: the pending question belongs to another session")
    command = failure.get("command_id")
    candidates = []
    for path in sorted(d.glob("*-intent.json")):
        intent = json.loads(path.read_text())
        stage = path.name.removesuffix("-intent.json")
        recorded = intent.get("command_id")
        if intent.get("session_id") != sid or not recorded:
            continue
        if recorded == command or (command is None and not (d / f"{stage}-completion.json").exists()):
            candidates.append((stage, recorded))
    if len(candidates) != 1:
        raise PromptUncertain(f"{sid}: unable to identify the command that requested input")
    stage, recorded = candidates[0]
    if command is None:
        # Bind older input receipts before saving completion so a restart can still find it.
        save(d / "failed.json", {**failure, "command_id": recorded})
    send(d, sid, stage, recorded, (d / f"{stage}.txt").read_text(), limit=None, reconcile=True)


def wait_pilot_ready(sid):
    """Use the daemon's settled wait, which includes its startup gate."""
    try:
        result = mj("wait", "--session", sid, "--timeout", 1200, timeout=1230)
        state = result["session"]
        outcome = result["outcome"]
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, subprocess.SubprocessError) as error:
        raise PromptUncertain(f"{sid}: could not confirm startup: {error}") from error
    if outcome == "timeout":
        raise TimeoutError(f"session {sid} did not finish startup")
    if outcome != "finished" or state["lifecycle"] != "live" or not state["is_idle"]:
        raise RuntimeError(f"session {sid} is not ready: {result.get('message') or outcome}")
    return state


def sealed_launch(bundle, commit, profile, title, *, base=None, effort=None):
    """Start a pinned pilot session, leaving the caller to check readiness."""
    if not profile or not driver.MJ_INSTANCE_ARGS:
        raise ValueError("sealed launch requires an explicit profile and named instance")
    launch = mj("new", "--workspace-id", driver.workspace_id(), "--target", "podman",
                "--bundle", bundle, "--at", commit, "--base", base or commit, "--no-later-history",
                "--profile", profile, *(["--effort", effort] if effort else ()), "--review", "off",
                "--no-project-memory", "--no-github-events", "--no-github-credentials",
                "--subagents", "none", "--oracle", "disabled", "--title", title, timeout=900)
    return launch["session_id"]


def configure_pilot_model(sid, model, effort):
    current = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
    for key in ("web_fetch", "web_search"):
        if key in current:
            mj("set-config", "--session", sid, "--key", key, "--value", "disabled")
    for key, value in (("model", model), ("effort", effort)):
        current = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
        if current.get(key) != value:
            mj("set-config", "--session", sid, "--key", key, "--value", value)
    settings = {c["key"]: c.get("current") for c in session(sid).get("config_options", [])}
    assert settings.get("model") == model and settings.get("effort") == effort, settings
    return settings


def configure_pilot_model_ready(sid, model, effort, *, wait_idle=None, delay=5):
    """Retry only the daemon's explicit startup refusal on this same session."""
    while True:
        try:
            return configure_pilot_model(sid, model, effort)
        except RuntimeError as error:
            if "initialization is still running" not in str(error):
                raise
            (wait_idle or driver.wait_idle)(sid)
            time.sleep(delay)


def final_messages(pages, completion):
    """Find the completed assistant messages belonging to just this turn."""
    start = completion.get("turn_start_position")
    if start is None:
        return []
    return [item["text"].strip() for page in pages for item in page["items"]
            if item["position"] >= start and item["role"] == "agent"
            and not item["body"]["streaming"] and item["text"].strip()]


def live_rql_count(response):
    """Codex ACP's local MCP command reports a count per connected server."""
    connected = False
    for line in response.splitlines():
        if line.startswith("**"):
            connected = line == "**Connected**"
        if connected and (match := re.fullmatch(r"- `rql`: (\d+) tools?", line)):
            return int(match[1])
    return None


def check_pilot_tool(pilot, sid):
    """Prove the pinned Codex thread sees the endpoint's registered tools."""
    d = pilot["path"]
    proof = d / "mcp-reconnect-result.json"
    if not proof.exists():
        command = "pilot-mcp-" + hashlib.sha256(pilot["tag"].encode()).hexdigest()[:24]
        error = None
        completion = {}
        messages = []
        try:
            send(d, sid, "mcp-reconnect", command, "/mcp reconnect\n")
            completion = json.loads((d / "mcp-reconnect-completion.json").read_text())
            messages = final_messages(driver.transcript(sid), completion)
        except WriterTurnFailed as exc:
            error = str(exc)
        response = "\n".join(messages)
        endpoint = json.loads((d / "tool-endpoint.json").read_text())
        tools = pilot["inputs"]["rql_tools"]
        count = live_rql_count(response)
        passed = (not error and "Reloaded the MCP configuration of all open sessions." in response
                  and count == len(tools) and endpoint == {"session_id": sid, "tools": tools})
        save(proof, {"session_id": sid, "expected_tools": tools, "endpoint": endpoint,
                     "live_count": count, "response": response, "usage": completion.get("usage"),
                     "error": error, "passed": passed})
    passed = json.loads(proof.read_text())["passed"]
    if not passed:
        save(d / "transcript.json", driver.transcript(sid))
        save(d / "usage.json", mj("usage", "--session", sid))
        driver.retain_pilot_session(d, sid, "Codex's live RQL catalog did not match the assignment")
    return passed


def run(bug_id, gate, *, pilot=None):
    bug = pilot["bug"] if pilot else driver.CORPUS[bug_id]
    d = pilot["path"] if pilot else ROOT / "briefs" / KIND / bug_id
    model, effort = pilot["model"] if pilot else (WRITER_MODEL, WRITER_EFFORT)
    profile = pilot["profile"] if pilot else WRITER_PROFILE
    if (d / "done.json").exists():
        return "done"
    # Stage B's tag is kept for its kind, so its command ids and titles stay the same.
    tag_key = pilot["tag"] if pilot else "brief" + ("" if KIND == "blind" else KIND) + bug_id
    tag = hashlib.sha256(tag_key.encode()).hexdigest()
    if (d / "launch.json").exists():
        sid = json.loads((d / "launch.json").read_text())["session_id"]
        if pilot:
            profile = json.loads((d / "launch-intent.json").read_text())["profile"]
            if (d / "failed.json").exists():
                return "retained-failure"
            if (d / "accepted.json").exists():
                return finish_pilot_writer(pilot, sid, bug_id, profile)
    elif (d / "launch-intent.json").exists():
        return "needs-recovery"  # a session may exist without a record; find it before rerunning
    else:
        with gate:
            hold = None if pilot else driver.quota_hold()
        if hold:
            return "held: " + hold  # nothing recorded; the writer starts on a later run
        workspace = None if pilot else driver.workspace_id()
        save(d / "launch-intent.json", {"bug": bug_id, "model": model, "effort": effort,
                                        "profile": profile, "time_ns": time.time_ns()})
        with gate:
            title = "brief " + tag[:8]
            if pilot:
                sid = sealed_launch(driver.BUNDLE, bug["intro_commit"], profile, title,
                                    base=bug["base_commit"], effort=effort)
                launch = {"session_id": sid, "instance": driver.MJ_INSTANCE_ARGS[-1]}
            else:
                launch = mj("new", "--workspace-id", workspace, "--target", "podman", "--bundle", driver.BUNDLE,
                            "--at", bug["intro_commit"], "--base", bug["base_commit"], "--no-later-history",
                            # A pinned writer selects its model below, before its first prompt.
                            *(["--profile", profile] if profile else ["--model", model]),
                            "--effort", effort, "--review", "off",
                            "--no-project-memory", "--no-github-events", "--no-github-credentials",
                            "--subagents", "none", "--title", title, timeout=900)
            save(d / "launch.json", launch)
            sid = launch["session_id"]
            if not pilot:
                driver.wait_idle(sid)
    if not (d / "settings.json").exists():
        s = wait_pilot_ready(sid) if pilot else session(sid)
        assert s["no_later_history"] and s["at"] == bug["intro_commit"] and s["base"] == bug["base_commit"], s
        settings = configure_pilot_model_ready(sid, model, effort)
        if profile:
            assert s.get("profile_id") == profile, s.get("profile_id")
        if not pilot:
            save(d / "settings.json", settings)
        place = driver.container(sid)
        save(d / "placement.json", place)
        if pilot and driver.image_digest(place["image"]) != pilot["inputs"]["image_digest"]:
            return driver.retain_pilot_session(d, sid, "writer image differs from the pinned image",
                                               image=place["image"], pinned=pilot["inputs"]["image_digest"])
        before = driver.inventory(place, "audit", bug["base_commit"])
        assert not before["remotes"] and not before["alternates"] and not before["status"], before
        if pilot and pilot["attest_cache"]:
            driver.attest_pilot_cache(d, place, bug["intro_commit"], bug["base_commit"])
        save(d / "pre-inventory.json", before)
        if pilot:
            save(d / "settings.json", settings)

    if pilot:
        pilot["publish_tool_assignment"](sid)
    if pilot and not check_pilot_tool(pilot, sid):
        return "retained-failure"
    prompt = pilot["prompt"] if pilot else ((SCRIPTS / "brief/WRITER.md").read_text()
              .replace("{base}", bug["base_commit"]).replace("{message}", driver.commit_message(bug)))
    try:
        # A pilot writer's turn contains its RQL engine calls, each a whole Fable turn, so the
        # session's own outcome ends the wait; 1800 s was sized for writers without RQL.
        text, problems, rounds = write_brief(d, sid, tag, prompt, bug, limit=None if pilot else 1800)
    except (WriterTurnFailed, WriterBriefMissing) as error:
        if not pilot:
            raise
        return retain_pilot_writer_failure(pilot, sid, error)
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
    cd = d if pilot else d / "reviewers" / sid[:12]
    save(cd / "transcript.json", driver.transcript(sid))
    save(cd / "usage.json", mj("usage", "--session", sid))
    if pilot:
        actual_profile = session(sid).get("profile_id")
        if actual_profile != profile:
            return driver.retain_pilot_session(d, sid, f"writer profile changed from {profile} to {actual_profile}")
    inv = json.loads((d / "inventory-check.json").read_text())["problems"]
    if inv or problems:
        # No done.json: a rerun fails the same way instead of passing an invalid brief on.
        # The session stays live for inspection.
        (d / "brief-rejected.md").write_text(text)
        if pilot:
            return driver.retain_pilot_session(d, sid, "writer failed inventory or lint checks",
                                               fix_rounds=rounds, lint_problems=problems, inventory_problems=inv)
        save(d / "failed.json", {"bug": bug_id, "session_id": sid, "fix_rounds": rounds, "lint_problems": problems,
                                 "inventory_problems": inv, "time_ns": time.time_ns()})
        raise RuntimeError(f"brief not accepted: inventory problems {inv}; "
                           f"{len(problems)} rule violations after {rounds} fix rounds (lint-{rounds}.json)")
    if pilot:
        save(d / "accepted.json", {"fix_rounds": rounds, "brief_sha256": hashlib.sha256(text.encode()).hexdigest()})
        return finish_pilot_writer(pilot, sid, bug_id, profile)
    save(d / "suspend.json", mj("suspend", "--session", sid, "--acknowledge-unpublished-work", timeout=900))
    (d / "brief.md").write_text(text)
    save(d / "done.json", {"bug": bug_id, "session_id": sid, "fix_rounds": rounds, "finished_ns": time.time_ns()})
    return "done"


def pilot_audit(d, sid, allow_rql):
    before = json.loads((d / "pre-inventory.json").read_text())
    commits = [*before["commits"], before["head_tree"], before["base_tree"]]
    import audit  # pilot-only; legacy brief_driver commands run without audit.py beside them

    verdict = audit.audit_pilot(d / "transcript.json", sid, commits=commits, allow_rql=allow_rql,
                                allow_in_place_edits=True)
    save(d / "audit.json", verdict)
    return verdict


def retain_pilot_writer_failure(pilot, sid, error):
    d = pilot["path"]
    save(d / "transcript.json", driver.transcript(sid))
    save(d / "usage.json", mj("usage", "--session", sid))
    driver.capture_pilot_rollouts(d, sid, reuse_existing=True)
    verdict = pilot_audit(d, sid, bool(pilot["inputs"]["rql_tools"]))
    return driver.retain_pilot_session(d, sid, str(error),
                                       **({"void_reason": verdict["hits"]} if not verdict["passed"] else {}))


def write_brief(d, sid, tag, prompt, bug, limit=1800):
    send(d, sid, "prompt", "brief-" + tag[:32], prompt, limit)
    for rounds in range(FIX_ROUNDS + 1):
        # The workspace is already past this lint result after a resolved repair.
        if (d / f"fix-{rounds + 1}-completion.json").exists():
            continue
        text = fetch_brief(sid)
        problems = lint(text, bug)
        save(d / f"lint-{rounds}.json", problems)
        if not problems or rounds == FIX_ROUNDS:
            return text, problems, rounds
        fix = ("Your brief.md breaks these rules from the instructions:\n\n"
               + "\n".join(f"- {p}" for p in problems)
               + "\n\nFix brief.md so that it follows every rule, then reply `brief written`.\n")
        repair = rounds + 1
        send(d, sid, f"fix-{repair}", f"brief-fix{repair}-" + tag[:28], fix, limit)


def interrupt_on_rql_quota(pilot, sid):
    """Classify a terminal writer's attempt from the exact session's delivery log."""
    if not pilot["inputs"].get("rql_tools"):
        return None
    d = pilot["path"]
    try:
        log = (d / f"queries-{sid}.jsonl").open()
    except FileNotFoundError:
        return None
    with log:
        fcntl.flock(log, fcntl.LOCK_SH)
        held = any(delivery.get("session") == sid and delivery.get("delivered") is True
                   and delivery.get("status") == "quota_hold"
                   for delivery in map(json.loads, log))
    if held:
        reason = "RQL engine quota held a writer query"
        path = d / "failed.json"
        failure = json.loads(path.read_text()) if path.exists() else {}
        if "reason" in failure and failure["reason"] != reason:
            failure["terminal_reason"] = failure["reason"]
        save(path, {**failure, "session_id": sid, "status": "quota_hold", "reason": reason})
        return f"interrupted: {reason}"
    return None


def finish_pilot_writer(pilot, sid, bug_id, profile):
    d, inputs = pilot["path"], pilot["inputs"]
    accepted = json.loads((d / "accepted.json").read_text())
    text = (d / "brief-candidate.md").read_bytes()
    if hashlib.sha256(text).hexdigest() != accepted["brief_sha256"]:
        raise RuntimeError(f"accepted brief changed under {d}")
    output = d / "brief.md"
    if output.exists() and output.read_bytes() != text:
        raise RuntimeError(f"stored brief changed under {d}")
    if not output.exists():
        output.write_bytes(text)
    state = driver.destroy_captured_pilot(pilot["run_root"], d, sid, profile, *pilot["model"], output.name,
                                          allow_rql=bool(inputs["rql_tools"]), allow_in_place_edits=True)
    if state != "done":
        return state
    if inputs["rql_tools"]:
        save(d / "pending-revoke.json", {"session_id": sid})
    if interruption := interrupt_on_rql_quota(pilot, sid):
        return interruption
    save(d / "done.json", dict(accepted, bug=bug_id, session_id=sid,
                               profile=profile, inputs=inputs, finished_ns=time.time_ns(),
                               mcp_reconnect_sha256=hashlib.sha256((d / "mcp-reconnect-result.json").read_bytes()).hexdigest()))
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
