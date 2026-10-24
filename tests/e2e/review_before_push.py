#!/usr/bin/env python3
"""Opt-in live regression for push-triggered review and the pre-push gate."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import socket
import sqlite3
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--harness", choices=("codex", "claude"), default="codex")
    parser.add_argument("--profile-home", type=Path,
                        help="source harness home; defaults to ~/.codex or ~/.claude")
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--child-model",
                        help="model for straightforward sub-agents; defaults to --model")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("a named isolated instance is required")
    root = args.root.resolve()
    required_root = Path("/home/jonathan/.cache/mjolnir/build").resolve()
    if not root.is_relative_to(required_root):
        parser.error(f"--root must be under {required_root}")
    source_home = args.profile_home or (Path.home() / (".codex" if args.harness == "codex" else ".claude"))
    source_home = source_home.expanduser().resolve()
    credential = "auth.json" if args.harness == "codex" else ".credentials.json"
    if not (source_home / credential).is_file():
        parser.error(f"{args.harness} credentials not found: {source_home / credential}")

    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    config, data, profile, project, home = [root / name for name in
                                            ("config", "data", "profile", "project", "home")]
    for directory in (config, data, profile, project):
        directory.mkdir()
    home.mkdir(mode=0o700)
    shutil.copy2(source_home / credential, profile / credential)
    (profile / credential).chmod(0o600)
    if args.harness == "codex":
        (profile / "config.toml").write_text("model = " + json.dumps(args.model) + "\n")

    bare = root / "origin.git"
    seed = root / "seed"
    subprocess.run(["git", "init", "--bare", "--initial-branch=main", str(bare)], check=True,
                   capture_output=True, text=True)
    subprocess.run(["git", "init", "-q", "--initial-branch=main", str(seed)], check=True)
    (seed / "README.md").write_text("Review before push regression fixture.\n")
    git(seed, "add", "README.md")
    git(seed, "-c", "user.name=Mj test", "-c", "user.email=mj-test@example.invalid",
        "commit", "-qm", "Fixture on main")
    git(seed, "remote", "add", "origin", str(bare))
    git(seed, "push", "-u", "origin", "main")
    subprocess.run(["git", "clone", str(bare), str(project)], check=True, capture_output=True, text=True)
    git(project, "checkout", "-qb", "rbp-test")
    git(project, "push", "-u", "origin", "rbp-test")
    git(project, "remote", "set-head", "origin", "main")
    assert git(project, "symbolic-ref", "refs/remotes/origin/HEAD").strip() == "refs/remotes/origin/main"

    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    (config / "config.toml").write_text(f'''version = 18
[phone]
enabled = true
bind = "127.0.0.1:{port}"
tailscale_detect = false
[github.watch]
enabled = true
[profiles.live]
kind = "{args.harness}"
home = {json.dumps(str(profile))}
[targets.localhost]
kind = "bare"
[targets.podman]
kind = "podman"
pull_policy = "never"
[targets.docker]
kind = "docker"
pull_policy = "never"
[continuation]
enabled = true
[subagents]
max_concurrent = 2
[subagents.straightforward]
model = {json.dumps(args.child_model or args.model)}
effort = "high"
[subagents.subtle]
model = {json.dumps(args.model)}
effort = "medium"
[review]
trigger = "push"
profile = "live"
''')
    isolated_config = (config / "config.toml").read_text()
    assert str(source_home) not in isolated_config, "isolated config contains the real harness home"
    assert str(profile) in isolated_config, "isolated config does not point at the copied profile"

    env = os.environ.copy()
    env.update(MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data), MJ_INSTANCE=args.instance,
               MJ_WORKER_BINARY=str(args.worker.resolve()), MJOLNIR_NO_UPDATE_CHECK="1", HOME=str(home))
    for key in ("MJ_SESSION_ID", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "OPENAI_API_KEY",
                "CODEX_API_KEY", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN", "MJ_DAEMON_OWNER_PID", "MJ_DAEMON_EXIT_WHEN_IDLE"):
        env.pop(key, None)
    env["CODEX_HOME" if args.harness == "codex" else "CLAUDE_CONFIG_DIR"] = str(profile)
    mj = [str(args.mj.resolve()), "--instance", args.instance]

    def cli(*command, check=True):
        result = subprocess.run(mj + list(command), env=env, text=True, capture_output=True, timeout=120)
        with (root / "cli.jsonl").open("a") as log:
            log.write(json.dumps({"command": command, "status": result.returncode,
                                  "stdout": result.stdout, "stderr": result.stderr}) + "\n")
        if check and result.returncode:
            raise RuntimeError(f"{command}: {result.stderr or result.stdout}")
        if not check and result.returncode:
            print(f"cleanup/diagnostic {command} failed: {result.stderr or result.stdout}", flush=True)
        return result.stdout

    def eventually(label, predicate, timeout=240):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.5)
        tail = transcript_tail() if session else "(session not created)"
        raise RuntimeError(f"timed out: {label}\nTranscript tail:\n{tail}")

    sockets = {}
    session = None
    child_ids = []

    def status(session_id):
        if session_id not in sockets:
            found = [path for path in data.rglob("control.sock") if session_id in path.parts]
            if not found:
                return None
            sockets[session_id] = found[0]
        hello = {"request_id": uuid.uuid4().hex, "protocol_version": 1,
                 "request": {"method": "hello", "params": {
                     "controller_version": "review-before-push-e2e",
                     "supported": {"min": 1, "max": 4294967295}}}}
        try:
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(5)
                previous = Path.cwd()
                try:
                    os.chdir(sockets[session_id].parent)
                    connection.connect(sockets[session_id].name)
                finally:
                    os.chdir(previous)
                with connection.makefile("rb") as reader:
                    connection.sendall(json.dumps(hello).encode() + b"\n")
                    greeting = json.loads(reader.readline())
                    assert greeting["result"] == "ok", greeting
                    request = {"request_id": uuid.uuid4().hex,
                               "protocol_version": greeting["protocol_version"],
                               "request": {"method": "status"}}
                    connection.sendall(json.dumps(request).encode() + b"\n")
                    reply = json.loads(reader.readline())
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        assert reply["result"] == "ok", reply
        return reply["payload"]["data"]

    def idle(session_id):
        state = status(session_id)
        return state if state and state["execution"] == "idle" and state.get("active_prompt") is None \
            and state.get("harness_turn") is None else None

    def transcript():
        if not session:
            return {"items": []}
        raw = cli("transcript", "--session", session, "--limit", "1000", "--json", check=False)
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            return {"items": [], "raw": raw}
        (root / "session-transcript.json").write_text(json.dumps(value, indent=2) + "\n")
        return value

    def delivered_messages():
        # Mailbox deliveries, including review verdicts, are system items.
        raw = cli("transcript", "--session", session, "--role", "system", "--limit", "1000",
                  "--json", check=False)
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            return ""
        return "\n".join(item.get("text", "") for item in value.get("items", []))

    def transcript_tail():
        value = transcript()
        items = value.get("items", []) if isinstance(value, dict) else []
        return "\n".join(json.dumps(item, ensure_ascii=False) for item in items[-12:]) or str(value.get("raw", ""))

    def has_request_review_tool_call():
        raw = cli("transcript", "--session", session, "--role", "tool", "--limit", "1000",
                  "--json", check=False)
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            return False
        # The tool name appears in the call's title or raw input, depending on
        # the harness, so search the whole item.
        return any("request_review" in json.dumps(item, ensure_ascii=False)
                   for item in value.get("items", []) if item.get("role") == "tool")

    def get_checkout():
        candidates = [project / ".mj" / "clones" / session]
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            row = database.execute(
                "SELECT project_directory FROM sessions WHERE session_id=?", (session,)).fetchone()
        if row and row[0]:
            candidates.insert(0, Path(bytes(row[0]).decode()))
        for candidate in candidates:
            if candidate.is_dir() and (candidate / ".git").exists():
                return candidate.resolve()
        raise RuntimeError(f"could not locate session checkout; candidates={candidates}; transcript tail:\n{transcript_tail()}")

    def relations():
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            return [json.loads(row[0]) for row in database.execute(
                "SELECT record_json FROM subagent_sessions WHERE parent_session_id=? ORDER BY child_session_id",
                (session,))]

    start = time.monotonic()
    try:
        cli("workspaces", "create", "Review before push regression")
        prompt = '''This is an isolated live regression. Append exactly the line `reviewed change` to README.md and commit it on the current branch. Follow Mjolnir's instructions about review before pushing. If a review finds issues, fix them, commit the fixes, and call request_review again; continue until you receive a clean verdict. Then push the branch to origin. When the push has succeeded, reply DONE. Do not edit any other files.'''
        created = json.loads(cli("new", "--workspace", "Review before push regression",
                                 "--target", "localhost", "--project-directory", str(project),
                                 "--model", args.model, "--effort", "medium", "--subagents", "mjolnir",
                                 "--review", "push", "--no-project-memory", "--json", prompt))
        session = created["session_id"]
        print("session:", session, flush=True)

        seen_rounds = set()
        verdict_kinds = []

        conditions = {}

        def reviewed_state():
            value = transcript()
            messages = delivered_messages()
            tool_called = has_request_review_tool_call()
            verdict_texts = [match.group(0) for match in re.finditer(
                r"Review (?:clean for commits|of commits)[^\n]*", messages)]
            for verdict in verdict_texts:
                if "Review clean for commits" in verdict:
                    kind = "clean"
                elif "Review of commits" in verdict:
                    kind = "findings"
                else:
                    continue
                key = (kind, verdict[:240])
                if key not in seen_rounds:
                    seen_rounds.add(key)
                    verdict_kinds.append(kind)
            clean = next((verdict for verdict in verdict_texts if "Review clean for commits" in verdict), None)
            checkout = None
            tree = None
            if clean:
                checkout = get_checkout()
                tree = git(checkout, "rev-parse", "HEAD^{tree}").strip()
                if not (checkout / ".git").is_file():
                    # Ordinary clones use a .git directory; worktrees use a .git file.
                    if not (checkout / ".git").is_dir():
                        raise RuntimeError(f"checkout has no git metadata: {checkout}")
                ref = f"refs/mj/reviewed/{tree}"
                recorded = subprocess.run(["git", "-C", str(checkout), "show-ref", "--verify", ref],
                                          capture_output=True, text=True)
                remote_trees = {
                    git(bare, "rev-parse", f"{name}^{{tree}}", check=False).strip()
                    for name in git(bare, "for-each-ref", "--format=%(refname)", "refs/heads/").split()
                }
                hook_path = git(checkout, "rev-parse", "--git-path", "hooks").strip()
                hook = Path(hook_path)
                if not hook.is_absolute():
                    hook = checkout / hook
                hook_exists = (hook / "pre-push").is_file()
                conditions.update(tool_called=tool_called, reviewed_ref=recorded.returncode == 0,
                                  remote_has_tree=tree in remote_trees, hook_exists=hook_exists,
                                  idle=idle(session), checkout=str(checkout), tree=tree)
                if (tool_called and recorded.returncode == 0 and tree in remote_trees and hook_exists and
                        "Review clean for commits" in delivered_messages() and idle(session)):
                    return {"checkout": checkout, "tree": tree,
                            "clean": clean, "hook": hook / "pre-push"}
            return None

        try:
            state = eventually("request_review tool call, mailbox verdict, reviewed tree, agent push and hook",
                               reviewed_state, timeout=1200)
        except RuntimeError as error:
            raise RuntimeError(f"{error}\nLast observed conditions: {json.dumps(conditions)}") from None
        value = transcript()
        assert has_request_review_tool_call(), \
            f"transcript did not contain a request_review tool call:\n{transcript_tail()}"
        assert any(item.get("role") == "agent" and "DONE" in item.get("text", "")
                   for item in value.get("items", [])), \
            f"session did not confirm completion after pushing:\n{transcript_tail()}"
        assert "Review clean for commits" in delivered_messages(), \
            f"clean verdict did not reach the session mailbox:\n{transcript_tail()}"
        assert "reviewed change" in (state["checkout"] / "README.md").read_text(), \
            f"agent did not append the requested README line: {state['checkout']}"
        assert "# mj-review-gate" in state["hook"].read_text(), \
            f"pre-push hook lacks # mj-review-gate: {state['hook']}"
        branch = git(state["checkout"], "symbolic-ref", "--short", "HEAD").strip()
        assert git(bare, "rev-parse", f"refs/heads/{branch}^{{tree}}").strip() == state["tree"], \
            f"agent push did not publish the reviewed tree to origin's {branch}"

        checkout = state["checkout"]
        (checkout / "gate-regression.txt").write_text("unreviewed gate probe\n")
        git(checkout, "add", "gate-regression.txt")
        git(checkout, "-c", "user.name=Mj test", "-c", "user.email=mj-test@example.invalid",
            "commit", "-qm", "Unreviewed gate probe")
        gate_started = time.monotonic()
        gated = subprocess.run(["git", "push", "origin", f"HEAD:refs/heads/{branch}"], cwd=checkout,
                               env={**env, "MJ_REVIEW_GATE": "1"}, text=True, capture_output=True)
        gate_refusal_seconds = round(time.monotonic() - gate_started, 2)
        gate_output = gated.stdout + gated.stderr
        assert gated.returncode != 0, f"gated push unexpectedly succeeded:\n{gate_output}"
        assert "has not been reviewed" in gate_output, \
            f"gated push failed without expected refusal text:\n{gate_output}"
        ungated_started = time.monotonic()
        ungated = subprocess.run(["git", "push", "origin", f"HEAD:refs/heads/{branch}"], cwd=checkout,
                                 env={key: value for key, value in env.items() if key != "MJ_REVIEW_GATE"},
                                 text=True, capture_output=True)
        ungated_push_seconds = round(time.monotonic() - ungated_started, 2)
        assert ungated.returncode == 0, f"hook was not inert without MJ_REVIEW_GATE=1:\n{ungated.stdout}{ungated.stderr}"

        rounds = len(seen_rounds)
        final_tree = git(checkout, "rev-parse", "HEAD^{tree}").strip()
        summary = {"session_id": session, "harness": args.harness, "review_rounds": rounds,
                   "verdict_kinds": verdict_kinds,
                   "tree": state["tree"], "remote_tree_after_agent_push": state["tree"],
                   "tree_after_ungated_push": final_tree,
                   "checkout": str(checkout), "elapsed_seconds": round(time.monotonic() - start, 2),
                   "gate_refusal_seconds": gate_refusal_seconds,
                   "ungated_push_seconds": ungated_push_seconds}
        (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary, indent=2), flush=True)
        print("PASS: clean review recorded, agent pushed, gated push refused, ungated push succeeded", flush=True)
    finally:
        try:
            if session:
                (root / "session-transcript-final.json").write_text(
                    cli("transcript", "--session", session, "--limit", "1000", "--json", check=False))
                cli("suspend", "--session", session, "--acknowledge-unpublished-work", check=False)
                child_ids = [relation["child_session_id"] for relation in relations()]
                for child in child_ids:
                    cli("wait", "--session", child, "--timeout", "90", "--json", check=False)
                    cli("destroy", "--session", child, check=False)
                cli("destroy", "--session", session, check=False)

                def sessions_destroyed():
                    with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
                        owned = [session, *child_ids]
                        placeholders = ",".join("?" for _ in owned)
                        remaining = database.execute(
                            f"SELECT session_id FROM sessions WHERE session_id IN ({placeholders})", owned).fetchall()
                        return not remaining

                eventually("test session and reviewer children destroyed", sessions_destroyed, timeout=120)
        finally:
            cli("daemon", "stop", check=False)


def git(repository, *arguments, check=True):
    result = subprocess.run(["git", "-C", str(repository), *arguments], text=True,
                            capture_output=True)
    if check and result.returncode:
        raise RuntimeError(f"git -C {repository} {' '.join(arguments)} failed: {result.stderr or result.stdout}")
    return result.stdout


if __name__ == "__main__":
    main()
