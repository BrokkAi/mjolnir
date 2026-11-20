"""Host adapter for the writer's sealed Podman launcher and distinct engine turns."""
import json
import re
import subprocess
import time
from uuid import uuid4

from .prompt import assert_blind, semantics_only, turn_prompt
from .snapshot import PRIVATE_PATHS


MODEL = "fable"
EFFORT = "high"

# Runs via Podman admin before the first prompt, outside the model's transcript.
PROBE = """
import json, os, socket, subprocess, sys
from pathlib import Path

orphan, expected_tree, paths, git_port = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), int(sys.argv[4])

def git(*args):
    return subprocess.check_output(["git", "-c", "safe.directory=*", "--no-replace-objects", *args]).decode().strip()

if git("rev-list", "--count", "HEAD") != "1" or git("rev-parse", "HEAD") != orphan:
    raise SystemExit("expected singleton orphan history")
if git("rev-parse", "HEAD^{tree}") != expected_tree:
    raise SystemExit("workspace tree differs from the frozen introducing tree")
if set(git("rev-list", "--all").splitlines()) != {orphan} or git("remote"):
    raise SystemExit("unexpected refs or remote in engine checkout")
if (Path(".git/objects/info/alternates").exists() or os.environ.get("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        or git("status", "--porcelain", "--untracked-files=all") or
        any(os.path.lexists(path) for path in paths)):
    raise SystemExit("unexpected alternate objects, workspace changes, or study files")
for host in ("127.0.0.1", "host.containers.internal", "host.docker.internal"):
    try:
        with socket.create_connection((host, git_port), timeout=2):
            raise SystemExit("engine container can connect to the host Git daemon")
    except OSError:
        pass
"""


class TurnFailure(RuntimeError):
    def __init__(self, message, record):
        super().__init__(message)
        self.record = record


def usage_delta(before, after, command, turn, model_version):
    turns = after["turns"]
    if len(turns) != 1 or turns[0].get("command_id") != command or turns[0].get("accepted_ordinal") != turn:
        raise RuntimeError("mj usage did not identify exactly the expected engine turn")
    if after["turn_selections"].get(command) != {"model": MODEL, "effort": EFFORT}:
        raise RuntimeError("mj usage did not attribute the engine turn to Fable/high")
    usage = turns[0].get("usage") or {}
    models = (usage.get("provider_details") or {}).get("model_usage", {})
    fable_models = {model.split("[", 1)[0] for model in models if model.startswith("claude-fable-")}
    if fable_models != {model_version}:
        raise RuntimeError(f"expected provider usage for {model_version}, got {sorted(fable_models)}")
    old, new = before["totals"], after["totals"]
    result = {key: entry["tokens"] - old.get(key, {}).get("tokens", 0)
              for key, entry in new.items()
              if entry["reported_turns"] > old.get(key, {}).get("reported_turns", 0)}
    if "total_tokens" not in result or any(value < 0 for value in result.values()):
        raise RuntimeError("mj usage did not report full-turn token deltas")
    return result


class SealedRunner:
    def __init__(self, config, manifest):
        import driver
        import brief_driver

        self.driver = driver
        self.launch = brief_driver.sealed_launch
        self.config = config
        self.manifest = manifest
        self.driver.use_named_instance(config["instance"])

    def command(self, *args, data=None, timeout=300):
        proc = subprocess.run(self.driver.mj_invocation(*args), input=data, capture_output=True, timeout=timeout)
        if proc.returncode:
            detail = proc.stderr.decode(errors="replace").strip()[-1200:]
            raise RuntimeError(f"mj {args[0]} failed: {detail}")
        return proc.stdout

    def verify(self, sid):
        s = self.driver.wait_idle(sid)
        settings = {row["key"]: row.get("current") for row in s.get("config_options", [])}
        if (settings.get("model"), settings.get("effort")) != (MODEL, EFFORT):
            raise RuntimeError(f"engine session {sid} is not configured for {MODEL}/{EFFORT}")
        if s.get("profile_id") != self.config["profile"]:
            raise RuntimeError(f"engine session {sid} is on the wrong Claude profile")
        return s

    def prepare(self, sid, snapshot, directory):
        s = self.driver.wait_idle(sid)
        if (s.get("no_later_history") is not True or s.get("at") != snapshot["orphan"]
                or s.get("base") != snapshot["orphan"]):
            raise RuntimeError("engine session does not use the frozen cutoff and base")
        place = self.driver.container(sid)
        cwd = f"/workspace/{sid}/{self.config['destination']}"
        proc = subprocess.run(
            ["podman", "exec", "--workdir", cwd, place["container"], "python3", "-c", PROBE,
             snapshot["orphan"], snapshot["tree"], json.dumps(PRIVATE_PATHS), str(self.config["git_port"])],
            capture_output=True, timeout=300,
        )
        if proc.returncode:
            raise RuntimeError(f"sealed engine probe failed: {proc.stderr.decode(errors='replace').strip()}")
        place["cwd"] = cwd
        evidence = self.driver.attest_pilot_cache(directory, place, snapshot["orphan"], snapshot["orphan"])
        snapshots = evidence["snapshots"]
        if len(snapshots) != 1 or next(iter(snapshots.values()))["commits"] != [snapshot["orphan"]]:
            raise RuntimeError("engine container must mount exactly one singleton Git cache bundle")
        # The shared launch path leaves network/mount enforcement to the target.
        # Require the operator's probe to attest this actual container before any prompt.
        probe = self.config["seal_probe"]
        if not isinstance(probe, list) or not probe or not all(isinstance(part, str) for part in probe):
            raise ValueError("engine seal_probe must be a host command array")
        check = subprocess.run([*probe, place["container"]], capture_output=True, timeout=300)
        if check.returncode:
            raise RuntimeError(f"engine isolation probe failed: {check.stderr.decode(errors='replace').strip()}")

        web_tools = {"web_fetch", "web_search"}
        options = {row["key"] for row in s.get("config_options", [])}
        settings = [("model", MODEL), ("effort", EFFORT)]
        settings.extend((key, "disabled") for key in sorted(web_tools & options))
        for key, value in settings:
            # A session can already be idle/live before configuration is ready.
            while True:
                try:
                    self.driver.mj("set-config", "--session", sid, "--key", key, "--value", value)
                    break
                except RuntimeError as error:
                    if "initialization is still running" not in str(error):
                        raise
                    self.driver.wait_idle(sid)
                    time.sleep(5)
        self.verify(sid)

    def start(self, snapshot, record_id):
        sid = self.launch(self.config["bundle"], snapshot["orphan"], self.config["profile"],
                          f"RQL pilot engine {record_id[:12]}", base=snapshot["orphan"], effort=EFFORT)
        if not re.fullmatch(r"[a-f0-9]{32}", sid):
            raise RuntimeError("unexpected engine session id")
        return sid

    def turn(self, sid, query, catalogue, menu, errors, directory_on_host):
        self.verify(sid)
        directory = f".rql-mock-{uuid4().hex}"
        catalogue_path = f"{directory}/catalogue.md"
        menu_path = f"{directory}/menu.md"
        output_path = f"{directory}/answer.json"
        menu_doc = semantics_only(menu)
        prompt = turn_prompt(query, output_path, catalogue_path, menu_path, errors)
        assert_blind((prompt, catalogue, menu_doc), self.manifest)
        for path, contents in ((catalogue_path, catalogue), (menu_path, menu_doc)):
            self.command("put-file", "--session", sid, "--path", path, "--source", "-",
                         data=contents.encode("utf-8"))
        before = self.driver.mj("usage", "--session", sid)
        command = f"engine-{uuid4().hex}"
        accepted = json.loads(self.command(
            "message", "--session", sid, "--command-id", command, "--prompt-file", "-", "--json",
            data=prompt.encode("utf-8"),
        ))
        turn = accepted["turn_id"]
        if type(turn) is not int:
            raise RuntimeError("engine request did not receive an accepted turn id")
        record = {"session": sid, "turn": turn, "command_id": command, "tokens": None}
        try:
            waited = self.driver.mj("wait", "--session", sid, "--turn", turn,
                                    "--return-on-input", "--timeout", 1800, timeout=1830)
            if waited.get("turn_id") != turn or waited["outcome"] != "finished":
                raise RuntimeError(f"engine turn {turn} ended with {waited['outcome']}")
            after = self.driver.mj("usage", "--session", sid, "--after-seq", before["latest_seq"])
            record["tokens"] = usage_delta(before, after, command, turn, self.config["model_version"])
            self.audit_turn(sid, turn, directory_on_host)
        except (RuntimeError, ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
            raise TurnFailure(str(error), record) from error
        try:
            output = self.command("fetch", "--session", sid, "--path", output_path, "--stdout")
        except (RuntimeError, OSError, subprocess.SubprocessError) as error:
            return None, record, [str(error)]
        return output, record, []

    def audit_turn(self, sid, turn, directory):
        from audit import audit_pilot

        # The existing audit considers only tool and terminal events. Retain
        # the host-only collection so a voided answer remains inspectable.
        path = directory / f"turn-{turn}-transcript.json"
        self.driver.save(path, self.driver.transcript(sid))
        s = self.driver.session(sid)
        snapshot = s["at"]
        result = audit_pilot(path, sid, commits=(snapshot,), single_commit=True)
        self.driver.save(directory / f"turn-{turn}-audit.json", result)
        if not result["passed"]:
            raise RuntimeError(f"engine session {sid} failed transcript isolation audit")
