#!/usr/bin/env python3
"""Go port acceptance scenarios: eight session flows through a real daemon,
worker and OpenCode harness.

The suite drives `mj` as a user does, in a named test instance, and checks
what the user sees and what is left on disk: a session starts; a prompt that
needs a tool writes a file into the session's checkout; a second prompt reads
it back; an interrupted turn stops its tool and the session takes the next
prompt; a question and a permission request reach the user and their answers
reach the agent; a turn survives a daemon restart; a suspended session comes
back with its checkout and conversation; and destroying sessions leaves no
process or file behind.

It is the acceptance suite for the Go port's milestones (see
.agents/plans/go-port.md). Run it against a Rust daemon with the Rust worker
(the control) and with `--worker-binary <go worker>`, which the daemon this
script starts runs instead of its own worker:

    tests/e2e/go_port_scenarios.py --mj target/debug/mj --instance go-e2e-rust
    tests/e2e/go_port_scenarios.py --mj target/debug/mj --instance go-e2e \\
        --worker-binary go/bin/mj-worker

`--daemon-mj <binary>` starts every daemon the suite starts (at the beginning
and after the daemon-restart scenario stops it) with that `mj` and drives
every other command with `--mj`, so a Go `mj` can be judged against a Rust
daemon:

    tests/e2e/go_port_scenarios.py --mj go/bin/mj --daemon-mj target/debug/mj \\
        --instance go-e2e --worker-binary go/bin/mj-worker

The two must then be the same build (go/scripts/build.sh with the Rust
build's revision, commit time, worker inputs and MJ_BUILD_EXECUTABLE_TIME_FROM
naming its `mj`): a client that orders itself newer replaces the daemon, and
the suite fails when `--mj` does not report that the daemon runs its build.

The instance's config.toml must define a bare target `localhost` and an
OpenCode profile `oc` (`kind = "open-code"`, its `home` an OpenCode
configuration directory selecting a free `opencode/*` model, and a placeholder
provider key under `[profiles.oc.environment]` so the login preflight passes).
OpenCode's free models need no login. The permission scenario needs a profile
whose permission requests reach the user instead of Mjolnir's guardian; when
the config has no `oc-ask` profile the script adds one, a copy of `oc` with
`mjolnir_guardian = false`, and says so. Prompts are synthetic: they go to
OpenCode's hosted service.

The script stops the instance's daemon first, so the daemon it starts runs
with this environment (and pins the worker binary it names), and stops it
again at the end. Sessions it creates are destroyed at the end.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib

# Inside a Mjolnir session these point at the live default instance and
# would override --instance; MJ_WORKER_BINARY is set only from the flag.
INHERITED_SESSION_VARIABLES = (
    "MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_SESSION_ID", "MJ_SESSION_BASH_ENV",
    "MJ_INSTANCE", "MJ_NO_SPLASH", "MJ_WORKER_BINARY",
)
# The worker's own allowance for a harness to open its native session
# (NativeSessionStartupTimeout in mj-core/src/relay.rs), plus time to provision.
SESSION_START_SECONDS = 330
# CLI calls that do not wait for a turn answer within seconds.
CLI_SECONDS = 120
WORKSPACE = "go-port-scenarios"
ASK_PROFILE = "oc-ask"


class ScenarioFailure(AssertionError):
    pass


def product_directory(kind: str) -> Path:
    """Mjolnir's configuration or data directory, as mj-core's loading.rs
    resolves it through the `dirs` crate on Linux and macOS."""
    home = Path.home()
    if sys.platform == "darwin":
        return home / "Library" / "Application Support" / "mjolnir"
    if kind == "config":
        base = os.environ.get("XDG_CONFIG_HOME") or str(home / ".config")
    else:
        base = os.environ.get("XDG_DATA_HOME") or str(home / ".local" / "share")
    return Path(base) / "mjolnir"


class Suite:
    def __init__(self, args: argparse.Namespace):
        # Tool processes are found by their command line, which is visible
        # host-wide, so each run gives its markers a number of its own:
        # another run's `sleep` (a concurrent suite, the Rust control) can
        # then never satisfy this run's checks.
        self.nonce = str(int.from_bytes(os.urandom(4), "big") % 1_000_000).zfill(6)
        self.args = args
        self.mj = args.mj.resolve()
        self.daemon_mj = (args.daemon_mj or args.mj).resolve()
        self.instance = args.instance
        self.config_path = product_directory("config") / "instances" / self.instance / "config.toml"
        self.data = product_directory("data") / "instances" / self.instance
        self.artifacts = Path(tempfile.mkdtemp(prefix="mj-go-port-scenarios-"))
        self.trace = (self.artifacts / "trace.jsonl").open("w", buffering=1)
        self.project = self.artifacts / "project"
        self.sessions: list[str] = []
        self.env = {key: value for key, value in os.environ.items()
                    if key not in INHERITED_SESSION_VARIABLES}
        if args.worker_binary:
            worker = args.worker_binary.resolve()
            if not worker.is_file():
                raise ScenarioFailure(f"worker binary {worker} does not exist")
            self.env["MJ_WORKER_BINARY"] = str(worker)
            with worker.open("rb") as binary:
                digest = hashlib.file_digest(binary, "sha256").hexdigest()
            self.record("setup", "worker-binary", {"path": str(worker), "sha256": digest})

    def record(self, scenario: str, event: str, value: object) -> None:
        self.trace.write(json.dumps({"time": time.time(), "scenario": scenario,
                                     "event": event, "value": value}) + "\n")

    def run(self, *args: str, timeout: float = CLI_SECONDS,
            mj: Path | None = None) -> subprocess.CompletedProcess:
        mj = mj or self.mj
        result = subprocess.run([str(mj), "--instance", self.instance, *args],
                                env=self.env, capture_output=True, text=True, timeout=timeout)
        self.record("cli", "run", {"mj": str(mj), "args": args, "status": result.returncode,
                                   "stdout": result.stdout[-4000:], "stderr": result.stderr[-4000:]})
        return result

    def cli(self, *args: str, timeout: float = CLI_SECONDS, mj: Path | None = None) -> str:
        result = self.run(*args, timeout=timeout, mj=mj)
        if result.returncode != 0:
            raise ScenarioFailure(f"mj {' '.join(args)} exited {result.returncode}: "
                                  f"{result.stderr.strip()} {result.stdout.strip()}")
        return result.stdout

    def cli_json(self, *args: str, timeout: float = CLI_SECONDS):
        return json.loads(self.cli(*args, "--json", timeout=timeout))

    def turn(self, *args: str) -> dict:
        """Run a command that ends with a turn outcome (prompt, wait). It exits
        non-zero for every outcome but `finished`, so the outcome is read from
        its JSON whatever the status."""
        result = self.run(*args, "--json", "--timeout", str(self.args.turn_seconds),
                          timeout=self.args.turn_seconds + CLI_SECONDS)
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise ScenarioFailure(f"mj {' '.join(args)} printed no outcome "
                                  f"({result.returncode}): {result.stderr.strip()}") from error

    def start_daemon(self) -> None:
        """Start the instance's daemon with --daemon-mj: any command that needs
        the daemon starts it, and the daemon inherits this environment."""
        self.cli("sessions", mj=self.daemon_mj)
        status = self.cli("daemon", "status")
        self.record("daemon", "status", status)
        if self.daemon_mj != self.mj and "This daemon runs this build." not in status:
            raise ScenarioFailure(f"{self.mj} is not the build of the daemon {self.daemon_mj} "
                                  f"started, so it would replace that daemon: {status.strip()}")

    def session(self, session: str) -> dict:
        return self.cli_json("sessions", "--session", session)

    def wait_for(self, description: str, predicate, seconds: float):
        deadline = time.monotonic() + seconds
        while True:
            value = predicate()
            if value:
                return value
            if time.monotonic() > deadline:
                raise ScenarioFailure(f"timed out after {seconds:.0f}s waiting for {description}")
            time.sleep(0.5)

    def wait_until_running(self, session: str) -> dict:
        def settled():
            record = self.session(session)
            if record["state"] == "error" or record.get("has_error"):
                raise ScenarioFailure(f"session {session} failed: {json.dumps(record)[:2000]}")
            return record if record["state"] == "running" else None
        return self.wait_for(f"session {session} to run", settled, SESSION_START_SECONDS)

    def transcript(self, session: str, *roles: str, after: int | None = None) -> list[dict]:
        args = ["transcript", "--session", session]
        if after is not None:
            args += ["--after-seq", str(after)]
        for role in roles:
            args += ["--role", role]
        return self.cli_json(*args)["items"]

    def latest_seq(self, session: str) -> int:
        return self.cli_json("transcript", "--session", session, "--limit", "1")["latest_seq"]

    def worker_directory(self, session: str) -> Path:
        return self.data / "workers" / session

    def checkout(self, session: str) -> Path:
        # A local bare target clones a Git project into the project's
        # .mj/clones directory.
        return self.project / ".mj" / "clones" / session

    def processes_under(self, path: Path) -> list[str]:
        listing = subprocess.run(["ps", "-axww", "-o", "pid=,command="], capture_output=True,
                                 text=True, check=True).stdout
        return [line.strip() for line in listing.splitlines() if str(path) in line]

    def process_running(self, pattern: str) -> bool:
        listing = subprocess.run(["ps", "-axww", "-o", "command="], capture_output=True,
                                 text=True, check=True).stdout
        return any(re.search(pattern, line) for line in listing.splitlines())

    def passed(self, number: int, name: str, evidence: str) -> None:
        self.record(name, "passed", evidence)
        print(f"PASS {number} {name}: {evidence}", flush=True)

    # --- setup -----------------------------------------------------------

    def prepare(self) -> None:
        if not self.config_path.is_file():
            raise ScenarioFailure(f"instance {self.instance} has no {self.config_path}; "
                                  "create it with the `oc` profile described in --help")
        config = tomllib.loads(self.config_path.read_text())
        profiles = config.get("profiles", {})
        base = profiles.get(self.args.profile)
        if not base or base.get("kind") != "open-code":
            raise ScenarioFailure(f"{self.config_path} needs an OpenCode profile "
                                  f"`{self.args.profile}` (see --help)")
        if config.get("targets", {}).get(self.args.target, {}).get("kind") != "bare":
            raise ScenarioFailure(f"{self.config_path} needs a bare target `{self.args.target}`")
        if ASK_PROFILE not in profiles:
            lines = [f"\n[profiles.{ASK_PROFILE}]", 'kind = "open-code"',
                     f"home = {json.dumps(base['home'])}", "mjolnir_guardian = false"]
            environment = base.get("environment", {})
            if environment:
                lines.append(f"\n[profiles.{ASK_PROFILE}.environment]")
                lines += [f"{json.dumps(key)} = {json.dumps(value)}" for key, value in environment.items()]
            with self.config_path.open("a") as handle:
                handle.write("\n".join(lines) + "\n")
            print(f"added profile `{ASK_PROFILE}` to {self.config_path}: `{self.args.profile}` "
                  "with mjolnir_guardian = false, so permission requests reach the user", flush=True)

        # The daemon inherits the environment of the command that starts it.
        self.cli("daemon", "stop")
        self.project.mkdir()
        (self.project / "README.md").write_text("hello\n")
        git = ["git", "-C", str(self.project), "-c", "user.name=Scenario", "-c",
               "user.email=scenario@example.invalid"]
        subprocess.run([*git, "init", "-q", "-b", "main"], check=True)
        subprocess.run([*git, "add", "README.md"], check=True)
        subprocess.run([*git, "commit", "-q", "-m", "scenario project"], check=True)
        self.start_daemon()
        self.cli("workspaces", "create", WORKSPACE)
        status = self.cli("daemon", "status")
        self.record("setup", "daemon", status)

    def new_session(self, profile: str, title: str) -> str:
        created = self.cli_json("--workspace", WORKSPACE, "new", "--profile", profile,
                                "--target", self.args.target, "--project-directory",
                                str(self.project), "--title", title)
        session = created["session_id"]
        self.sessions.append(session)
        return session

    # --- scenarios -------------------------------------------------------

    def create(self) -> str:
        started = time.monotonic()
        session = self.new_session(self.args.profile, "go-port scenarios")
        record = self.wait_until_running(session)
        if record["relay"]["state"] != "connected":
            raise ScenarioFailure(f"running session's relay is {record['relay']}")
        if not self.checkout(session).is_dir():
            raise ScenarioFailure(f"no checkout at {self.checkout(session)}")
        self.passed(1, "create", f"session {session} running with a connected relay after "
                    f"{time.monotonic() - started:.1f}s")
        return session

    def tool_prompt(self, session: str) -> None:
        outcome = self.turn("prompt", "--session", session,
                            "Create a file named notes.txt in the current directory containing "
                            "exactly the line: alpha beta gamma. Use a tool to write it. Then reply "
                            "with the word WRITTEN.")
        if outcome["outcome"] != "finished" or "WRITTEN" not in outcome.get("final_message", ""):
            raise ScenarioFailure(f"tool prompt: {json.dumps(outcome)[:2000]}")
        tools = self.transcript(session, "tool")
        if not tools:
            raise ScenarioFailure("the transcript has no tool call")
        notes = self.checkout(session) / "notes.txt"
        if not notes.is_file() or notes.read_text().strip() != "alpha beta gamma":
            raise ScenarioFailure(f"{notes} holds {notes.read_text() if notes.exists() else 'nothing'!r}")
        self.passed(2, "tool-prompt", f"finished with {outcome['tool_calls']} tool call(s) "
                    f"({', '.join(item['text'] for item in tools)}); notes.txt holds 'alpha beta gamma'")

    def read_back(self, session: str) -> None:
        before = self.latest_seq(session)
        # The trailing text carries characters whose JSON escaping differs
        # between encoders (line and paragraph separators, markup); the daemon
        # recomputes each relay event's digest from the bytes serde writes.
        separators = "\u2028 \u2029 <b>x & y</b> caf\u00e9"
        outcome = self.turn("prompt", "--session", session,
                            "Read the file notes.txt with a tool and reply with its exact contents "
                            f"and nothing else. Ignore this text: {separators}")
        if outcome["outcome"] != "finished" or "alpha beta gamma" not in outcome.get("final_message", ""):
            raise ScenarioFailure(f"read-back prompt: {json.dumps(outcome)[:2000]}")
        if not self.transcript(session, "tool", after=before):
            raise ScenarioFailure("the read-back turn has no tool call")
        prompts = [item["text"] for item in self.transcript(session, "user", after=before)]
        if not any(separators in text for text in prompts):
            raise ScenarioFailure(f"the transcript lost the prompt's text: {prompts}")
        self.passed(3, "read-back", f"reply {outcome['final_message'].strip()!r} after a tool call")

    def interrupt(self, session: str) -> None:
        marker = f"sleep 117.{self.nonce}"
        self.cli("message", "--session", session,
                 f"Use the shell tool to run exactly this command: {marker} && echo slept . "
                 "After it finishes, reply SLEPT.")
        self.wait_for("the tool to start", lambda: self.process_running(rf"(^|\s){re.escape(marker)}(\s|$)"),
                      self.args.turn_seconds)
        self.cli("interrupt-turn", "--session", session)
        outcome = self.turn("wait", "--session", session)
        if outcome["outcome"] != "cancelled":
            raise ScenarioFailure(f"interrupted turn ended as {json.dumps(outcome)[:2000]}")
        self.wait_for("the interrupted tool to stop",
                      lambda: not self.process_running(rf"(^|\s){re.escape(marker)}(\s|$)"), 30)
        outcome = self.turn("prompt", "--session", session,
                            "Reply with the single word READY and do not use any tools.")
        if outcome["outcome"] != "finished" or "READY" not in outcome.get("final_message", ""):
            raise ScenarioFailure(f"prompt after the interrupt: {json.dumps(outcome)[:2000]}")
        self.passed(4, "interrupt", "turn cancelled, its tool process gone, next prompt answered READY")

    def question_and_permission(self, session: str) -> str:
        outcome = self.turn("prompt", "--session", session, "--return-on-input",
                            "Use your question tool (the tool that asks the user a multiple-choice "
                            "question) to ask me which color I prefer, with the two options red and "
                            "blue. After I answer, reply with only the chosen color.")
        if outcome["outcome"] != "input_required":
            raise ScenarioFailure(f"no question reached the user: {json.dumps(outcome)[:2000]}")
        question, = self.cli_json("elicitations", "--session", session)
        # The model words the options; it may write "Blue" or "blue".
        choices = [(field["id"], option["value"]) for field in question["fields"]
                   for option in field.get("options", [])]
        if sorted(value.lower() for _, value in choices) != ["blue", "red"]:
            raise ScenarioFailure(f"the question does not offer red and blue: {json.dumps(question)[:2000]}")
        field, blue = next((field, value) for field, value in choices if value.lower() == "blue")
        self.cli("respond", "--session", session, "--elicitation", question["id"],
                 json.dumps({"action": "accept", "content": {field: blue}}))
        outcome = self.turn("wait", "--session", session)
        if outcome["outcome"] != "finished" or "blue" not in outcome.get("final_message", "").lower():
            raise ScenarioFailure(f"after answering the question: {json.dumps(outcome)[:2000]}")
        reply = outcome["final_message"].strip()

        asking = self.new_session(ASK_PROFILE, "go-port scenarios permission")
        self.wait_until_running(asking)
        answers = []
        for command, choice, expected in (("ls -la .", "once", "finished"),
                                          ("pwd", "reject", "cancelled")):
            outcome = self.turn("prompt", "--session", asking, "--return-on-input",
                                f"Use the shell tool to run exactly this command: {command} . "
                                "If the command is rejected, reply REFUSED and do not retry. "
                                "Otherwise reply DONE.")
            if outcome["outcome"] != "input_required":
                raise ScenarioFailure(f"no permission request for {command}: {json.dumps(outcome)[:2000]}")
            request, = self.cli_json("elicitations", "--session", asking)
            if not request["id"].startswith("tool-permission-") or command not in request["message"]:
                raise ScenarioFailure(f"unexpected input request: {json.dumps(request)[:2000]}")
            self.cli("respond", "--session", asking, "--elicitation", request["id"],
                     json.dumps({"action": "accept", "content": {"choice": choice}}))
            outcome = self.turn("wait", "--session", asking)
            if outcome["outcome"] != expected:
                raise ScenarioFailure(f"after answering {choice}: {json.dumps(outcome)[:2000]}")
            answers.append(f"{choice} -> {expected}")
        self.passed(5, "question-and-permission", f"question answered 'blue', agent replied {reply!r}; "
                    f"permission requests: {', '.join(answers)}")
        return asking

    def daemon_restart(self, session: str) -> None:
        before = self.latest_seq(session)
        marker = f"sleep 31.{self.nonce}"
        self.cli("message", "--session", session,
                 f"Use the shell tool to run exactly this command: {marker} && echo done-thirty-one . "
                 "When it finishes, reply with FINISHED followed by the command output.")
        self.wait_for("the tool to start", lambda: self.process_running(rf"(^|\s){re.escape(marker)}(\s|$)"),
                      self.args.turn_seconds)
        self.cli("daemon", "stop")
        worker = self.worker_directory(session)
        if not any(" worker run " in line for line in self.processes_under(worker)):
            raise ScenarioFailure("the worker stopped with the daemon")
        if not self.process_running(rf"(^|\s){re.escape(marker)}(\s|$)"):
            raise ScenarioFailure("the turn's tool stopped with the daemon")
        self.start_daemon()
        outcome = self.turn("wait", "--session", session)
        if outcome["outcome"] != "finished" or "done-thirty-one" not in outcome.get("final_message", ""):
            raise ScenarioFailure(f"turn across the restart: {json.dumps(outcome)[:2000]}")
        items = self.transcript(session, "user", "tool", "agent", after=before)
        roles = [item["role"] for item in items]
        user = next(i for i, item in enumerate(items) if item["role"] == "user")
        tool = next((i for i, item in enumerate(items) if item["role"] == "tool"), None)
        reply = max((i for i, item in enumerate(items) if item["role"] == "agent"), default=None)
        if tool is None or reply is None or not user < tool < reply \
                or "done-thirty-one" not in items[reply]["text"]:
            raise ScenarioFailure(f"incomplete transcript across the restart: {roles}")
        self.passed(6, "daemon-restart", "worker and tool kept running with the daemon stopped; "
                    f"the turn finished under the next daemon; transcript roles {roles}")

    def suspend_resume(self, session: str) -> None:
        checkout = self.checkout(session)
        # Suspension keeps tracked work and refuses to drop untracked files.
        subprocess.run(["git", "-C", str(checkout), "add", "notes.txt"], check=True)
        self.cli("suspend", "--session", session)
        self.wait_for("the session to be suspended",
                      lambda: self.session(session)["state"] == "suspended", CLI_SECONDS)
        left = self.processes_under(self.worker_directory(session))
        if left:
            raise ScenarioFailure(f"processes outlived the suspension: {left}")
        if checkout.exists():
            raise ScenarioFailure(f"the checkout {checkout} outlived the suspension")
        self.cli("resume", "--session", session)
        self.wait_until_running(session)
        notes = checkout / "notes.txt"
        if not notes.is_file() or notes.read_text().strip() != "alpha beta gamma":
            raise ScenarioFailure(f"the restored checkout lacks notes.txt: {notes}")
        outcome = self.turn("prompt", "--session", session,
                            "Without using any tools: which color did I choose earlier in this "
                            "conversation? Reply with only the color.")
        if outcome["outcome"] != "finished" or "blue" not in outcome.get("final_message", "").lower():
            raise ScenarioFailure(f"prompt after resume: {json.dumps(outcome)[:2000]}")
        self.passed(7, "suspend-resume", "suspended with no process or checkout left; resumed with "
                    f"notes.txt restored and the conversation intact (reply {outcome['final_message'].strip()!r})")

    def destroy(self) -> None:
        for session in list(self.sessions):
            self.cli("destroy", "--session", session)

        def gone(session):
            # A destroyed session stays listed as an archived record, which
            # has a `status` and no live `state`.
            result = self.run("sessions", "--session", session, "--json")
            if result.returncode == 0:
                record = json.loads(result.stdout)
                if "state" in record or record.get("status") != "archived":
                    return False
            return not (self.processes_under(self.worker_directory(session))
                        or self.worker_directory(session).exists()
                        or self.checkout(session).exists()
                        or (self.data / "profiles" / session).exists())

        for session in list(self.sessions):
            self.wait_for(f"session {session} to be destroyed", lambda: gone(session), CLI_SECONDS)
            self.sessions.remove(session)
        self.passed(8, "destroy", "sessions archived; no worker process, worker directory, "
                    "checkout or profile directory left")

    def cleanup(self) -> None:
        for session in self.sessions:
            self.run("destroy", "--session", session)
        self.run("daemon", "stop")
        self.trace.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--mj", type=Path, required=True, help="the mj binary to drive")
    parser.add_argument("--instance", required=True, help="named test instance; never default")
    parser.add_argument("--daemon-mj", type=Path,
                        help="the mj binary that starts every daemon (default: --mj)")
    parser.add_argument("--worker-binary", type=Path,
                        help="worker the daemon runs instead of its own (sets MJ_WORKER_BINARY)")
    parser.add_argument("--profile", default="oc", help="OpenCode profile (default oc)")
    parser.add_argument("--target", default="localhost", help="bare target (default localhost)")
    parser.add_argument("--turn-seconds", type=int, default=300,
                        help="how long one turn may take before the scenario fails (default 300)")
    args = parser.parse_args()
    if args.instance == "default" or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*", args.instance):
        parser.error("--instance must name a separate test instance (letters, digits, hyphens)")

    suite = Suite(args)
    print(f"artifacts: {suite.artifacts}", flush=True)
    try:
        suite.prepare()
        session = suite.create()
        suite.tool_prompt(session)
        suite.read_back(session)
        suite.interrupt(session)
        suite.question_and_permission(session)
        suite.daemon_restart(session)
        suite.suspend_resume(session)
        suite.destroy()
    except ScenarioFailure as failure:
        print(f"FAIL: {failure}", file=sys.stderr, flush=True)
        return 1
    finally:
        suite.cleanup()
    shutil.rmtree(suite.project, ignore_errors=True)
    print("all scenarios passed", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
