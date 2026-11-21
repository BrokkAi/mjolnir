"""Host adapter for the writer's sealed Podman launcher and distinct engine turns."""
import json
from pathlib import PurePosixPath
import re
import subprocess
import time
from uuid import uuid4
from driver import PILOT_CLAUDE_EFFORT as EFFORT, PILOT_CLAUDE_MODEL as MODEL
from driver import provider_model, verify_session_selection

from .evidence import file_digests, save_bytes, save_json
from .prompt import assert_blind, semantics_only, turn_prompt
from .snapshot import PRIVATE_PATHS


SESSION_ID = re.compile(r"[a-f0-9]{32}")
QUERY_SESSION_PROTOCOL = "sealed-session-per-query-v1"
RUN_ERRORS = (RuntimeError, ValueError, KeyError, OSError, subprocess.SubprocessError)

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


# Verify the frozen checkout outside the reserved scratch directory.
CHECKOUT_CLEAN = r"""
from pathlib import Path
import os, shutil, subprocess, sys

scratch = Path(sys.argv[1])
orphan, action = sys.argv[2:]

def git(*args):
    return subprocess.check_output(["git", "--no-optional-locks", "-c", "safe.directory=*",
                                    "-c", "core.fsmonitor=false", "-c", "core.filemode=true", *args])

if orphan and git("rev-parse", "HEAD").decode().strip() != orphan:
    raise SystemExit("engine checkout is no longer at its frozen orphan")
if git("ls-files", "--", str(scratch)):
    raise SystemExit("engine scratch path overlaps tracked source")
indexed = git("ls-files", "-v", "-z").rstrip(b"\0").split(b"\0")
if any(not item.startswith(b"H ") for item in indexed if item):
    raise SystemExit("engine checkout has unexpected index flags")
if git("status", "--porcelain", "--untracked-files=all", "--ignored=matching", "--ignore-submodules=none",
       "--", ".", f":(exclude){scratch}"):
    raise SystemExit("engine workspace has unexpected changes outside its scratch path")
tracked = {Path(os.fsdecode(item[2:])) for item in indexed if item}
for directory, children, files in os.walk("."):
    if directory == ".":
        children[:] = [child for child in children if child not in {".git", str(scratch)}]
    elif not children and not files and Path(directory) not in tracked:
        # Git status omits empty directories, except as tracked gitlinks.
        raise SystemExit("engine workspace has an untracked empty directory")
if scratch.is_symlink():
    raise SystemExit("engine scratch path is a symlink")
if scratch.is_dir() and action == "clear":
    shutil.rmtree(scratch)
elif scratch.exists() and not scratch.is_dir():
    raise SystemExit("engine scratch path is occupied by an unexpected file")
"""


class TurnFailure(RuntimeError):
    def __init__(self, message, record):
        super().__init__(message)
        self.record = record


def reported_model_usage(turn):
    usage = turn.get("usage") or {}
    return (usage.get("provider_details") or {}).get("model_usage", {})


def usage_delta(before, after, command, turn, model_version):
    try:
        provider_model(after, command, turn, model_version)
    except ValueError as error:
        raise RuntimeError(str(error)) from error
    old, new = before["totals"], after["totals"]
    result = {key: entry["tokens"] - old.get(key, {}).get("tokens", 0)
              for key, entry in new.items()
              if entry["reported_turns"] > old.get(key, {}).get("reported_turns", 0)}
    if "total_tokens" not in result or any(value < 0 for value in result.values()):
        raise RuntimeError("mj usage did not report full-turn token deltas")
    return result


class IsolationFailure(TurnFailure):
    """The evidence on disk belongs to a session that must be retired."""


class QuotaTurnFailure(TurnFailure):
    """The provider refused an accepted engine turn for quota or rate limits."""


class DestroyUncertain(RuntimeError):
    pass


class LaunchHeld(RuntimeError):
    """Quota admission refused before a session launch or paid turn was attempted."""


class SealedRunner:
    def __init__(self, config, manifest=None):
        import driver
        import brief_driver

        self.driver = driver
        self.launch = brief_driver.sealed_launch
        self.config = config
        self.manifest = {"diffs": manifest["eligible_diffs"]} if manifest is not None else None
        self.served_steps = manifest["served_steps"] if manifest is not None else None
        self.instance = config["instance"]
        self.driver.use_named_instance(self.instance)

    def command(self, *args, data=None, timeout=300):
        argv = self.driver.mj_invocation(*args)
        if any(part.split("=", 1)[0] == "--acknowledge-unpublished-work" for part in argv):
            raise ValueError("the engine cannot acknowledge unpublished work")
        if args[0] == "destroy" and argv[-len(args) - 2:] != ["--instance", self.config["instance"], *args]:
            raise RuntimeError("engine destruction must address its named study instance")
        proc = subprocess.run(argv, input=data, capture_output=True, timeout=timeout)
        if proc.returncode:
            detail = proc.stderr.decode(errors="replace").strip()[-1200:]
            raise RuntimeError(f"mj {args[0]} failed: {detail}")
        return proc.stdout

    def verify(self, sid):
        s = self.driver.wait_idle(sid)
        verify_session_selection(s, MODEL, self.config["profile"], self.config["model_display"])
        return s

    def attest(self, sid, snapshot, directory):
        """Verify the sealed checkout, mounted cutoff and target egress."""
        s = self.driver.wait_idle(sid)
        if (s.get("no_later_history") is not True or s.get("at") != snapshot["orphan"]
                or s.get("base") != snapshot["orphan"]):
            raise RuntimeError("engine session does not use the frozen cutoff and base")
        place = self.driver.container(sid)
        cwd = self.checkout_root(sid)
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
        before = self.driver.inventory(place, "audit", snapshot["orphan"])
        if before["head"] != snapshot["orphan"]:
            raise RuntimeError("engine pre-inventory does not match the pinned checkout")
        save_json(directory / "pre-inventory.json", {**before, "cwd": cwd})
        return s

    def prepare(self, sid, snapshot, directory):
        s = self.attest(sid, snapshot, directory)
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
        self.check_quota()
        sid = self.launch(self.config["bundle"], snapshot["orphan"], self.config["profile"],
                          f"RQL pilot engine {record_id[:12]}", base=snapshot["orphan"], effort=EFFORT)
        if not SESSION_ID.fullmatch(sid):
            raise RuntimeError("unexpected engine session id")
        return sid

    def check_quota(self):
        if hold := self.driver.pilot_claude_hold(self.config["profile"]):
            raise LaunchHeld(hold)

    def message(self, sid, text, command):
        accepted = json.loads(self.command(
            "message", "--session", sid, "--command-id", command, "--prompt-file", "-", "--json",
            data=text.encode("utf-8"),
        ))
        if accepted.get("session_id") != sid or accepted.get("via") != "turn":
            raise RuntimeError("engine request did not reach the user prompt path")
        turn = accepted.get("turn_id")
        if type(turn) is not int:
            raise RuntimeError("engine request did not receive an accepted turn id")
        return turn

    def checkout_root(self, sid):
        """The session's checkout inside its container, which is also its starting directory."""
        return f"/workspace/{sid}/{self.config['destination']}"

    def clean_checkout(self, sid, orphan="", *, clear=True):
        scratch = f".rql-engine-{sid}"
        place = self.driver.container(sid)
        cwd = self.checkout_root(sid)
        proc = subprocess.run(
            ["podman", "exec", "--workdir", cwd, place["container"], "python3", "-c",
             CHECKOUT_CLEAN, scratch, orphan, "clear" if clear else "check"], capture_output=True, timeout=300,
        )
        if proc.returncode:
            raise RuntimeError(f"engine workspace check failed: {proc.stderr.decode(errors='replace').strip()}")
        return scratch

    def prompt(self, sid, text, context_id=None, *, before_send=None):
        before = self.driver.mj("usage", "--session", sid)
        command = f"engine-{uuid4().hex}"
        self.check_quota()
        if before_send is not None:
            before_send()
        turn = self.message(sid, text, command)
        record = {"session": sid, "turn": turn, "command_id": command, "tokens": None,
                  "context_id": context_id}
        try:
            waited = self.driver.mj("wait", "--session", sid, "--turn", turn,
                                    "--return-on-input", "--timeout", 1800, timeout=1830)
            if waited.get("turn_id") != turn:
                raise RuntimeError(f"engine turn {turn} ended with {waited['outcome']}")
            if waited["outcome"] != "finished":
                record["outcome"] = waited["outcome"]
                record["stop_reason"] = waited.get("stop_reason")
                diagnostic = waited.get("diagnostic") or {}
                record["diagnostic"] = diagnostic
                code = diagnostic.get("code")
                if (waited["outcome"] == "quota_limit" or record["stop_reason"] == "QuotaLimit"
                        or diagnostic.get("http_status") == 429
                        or code in {"rate_limit_error", "provider.rate_limit_error", "usage_limit", "quota_exceeded",
                                    "provider.quota_exceeded", "usageLimitExceeded", "usage_limit_exceeded"}
                        # Older bridges can carry the Claude API's documented error body as text.
                        # https://platform.claude.com/docs/en/api/errors
                        or (not code or code == "limit" or str(code).lstrip("-").isdigit())
                        and re.search(r'"type"\s*:\s*"rate_limit_error"',
                                      diagnostic.get("message") or waited.get("message") or "")):
                    raise QuotaTurnFailure("The engine provider temporarily refused the turn for quota.", record)
                raise RuntimeError(f"engine turn {turn} ended with {waited['outcome']}")
            after = self.driver.mj("usage", "--session", sid, "--after-seq", before["latest_seq"])
            record["tokens"] = usage_delta(before, after, command, turn, self.config["model_version"])
        except QuotaTurnFailure:
            raise
        except RUN_ERRORS as error:
            raise TurnFailure(str(error), record) from error
        return (waited.get("final_message") or "").strip(), record

    def capture_answer(self, sid, workspace_path, evidence):
        """Record exact bytes or a verified absence before scratch can be removed."""
        try:
            output = self.command("fetch", "--session", sid, "--path", workspace_path, "--stdout")
        except (RuntimeError, OSError, subprocess.SubprocessError) as error:
            path = PurePosixPath(workspace_path)
            listed = json.loads(self.command(
                "fetch", "--session", sid, "--path", str(path.parent), "--ls", "--json"))
            if (PurePosixPath(listed["path"]).name != path.parent.name
                    or any(entry["name"] == path.name for entry in listed["entries"])):
                raise error
            save_json(evidence / "absent.json", {"path": workspace_path, "listing": listed})
            return None, [str(error)]
        save_bytes(evidence / "answer.raw", output)
        return output, []

    def capture_pending(self, sid, directory):
        pending = []
        for path in (directory / "answers").glob("*/pending.json"):
            record = json.loads(path.read_bytes())
            captured = (path.parent / "answer.raw").is_file() or (path.parent / "absent.json").is_file()
            if not captured and not (path.parent / "checkout-invalid.json").is_file():
                pending.append((path, record))
        if len(pending) > 1:
            raise RuntimeError("more than one engine answer is pending capture")
        if pending:
            path, record = pending[0]
            expected = f".rql-engine-{sid}/answer.json"
            if record["path"] != expected:
                raise RuntimeError("pending engine answer path does not match its session")
            self.capture_answer(sid, expected, path.parent)

    def capture_usage(self, sid, expected_model):
        pages, cursor = [], 0
        while True:
            page = self.driver.mj("usage", "--session", sid, "--after-seq", cursor)
            if page["session_id"] != sid:
                raise RuntimeError("usage response does not match the recorded engine session")
            pages.append(page)
            next_cursor = page["next_after_seq"]
            if next_cursor == page["latest_seq"]:
                break
            if not cursor < next_cursor < page["latest_seq"] or not page["turns"]:
                raise RuntimeError("incomplete engine usage capture")
            cursor = next_cursor
        models = sorted({model for page in pages for turn in page["turns"]
                         for model in reported_model_usage(turn)})
        return {"expected_provider_model": expected_model,
                "reported_provider_models": models, "pages": pages}

    def retire(self, directory, record, *, require_clean_audit=False):
        from audit import audit_pilot

        sid = record["session_id"]
        if not SESSION_ID.fullmatch(sid):
            raise RuntimeError("invalid session id in engine record")
        if record.get("instance", self.config["instance"]) != self.config["instance"]:
            raise RuntimeError("engine session was recorded for a different study instance")
        sent = directory / "destroy-request.json"
        receipt = directory / "destroy-receipt.json"
        if receipt.exists():
            return
        if sent.exists():
            raise DestroyUncertain(f"destroy status for {sid} is uncertain; inspect {sent}")

        try:
            expected_model = record.get("model_version") or self.config.get("model_version")
            if not expected_model:
                raise RuntimeError("the engine record has no pinned provider model")
            s = self.driver.wait_idle(sid)
            if (s.get("id") != sid or s.get("at") != record["orphan"] or s.get("base") != record["orphan"]
                    or s.get("no_later_history") is not True):
                raise RuntimeError("live engine session does not match the recorded identity")
            self.capture_pending(sid, directory)
            pages = self.driver.transcript(sid)
            if not pages or any(page["session_id"] != sid for page in pages):
                raise RuntimeError("transcript capture does not match the recorded engine session")
            if pages[-1]["next_after_seq"] < pages[-1]["latest_seq"]:
                raise RuntimeError("incomplete engine transcript capture")
            transcript = directory / "transcript.json"
            save_json(transcript, pages)
            save_json(directory / "usage.json", self.capture_usage(sid, expected_model))
            result = audit_pilot(transcript, sid, commits=(record["orphan"],), single_commit=True)
            save_json(directory / "audit.json", result)
            if any(hit["kind"] == "missing-or-unreadable-transcript" for hit in result["hits"]):
                raise RuntimeError("the audit could not read the captured transcript")
            if require_clean_audit and not result["passed"]:
                raise RuntimeError("the stored transcript did not pass the audit")
            hashes = file_digests(directory)
            hashes.pop("capture.json", None)
            save_json(directory / "capture.json", {
                "session": sid, "instance": self.config["instance"], "commit": record["commit"],
                "files_sha256": hashes, "audit_passed": result["passed"],
            })
        except (*RUN_ERRORS, TypeError) as error:
            raise RuntimeError(f"could not capture engine {sid}; session preserved: {error}") from error

        save_json(sent, {"session": sid, "instance": self.config["instance"]})
        try:
            accepted = json.loads(self.command("destroy", "--session", sid, "--json"))
            if (not isinstance(accepted, dict) or accepted.get("session_id") != sid
                    or accepted.get("accepted") is not True):
                raise RuntimeError("missing matching acceptance receipt")
            save_json(receipt, accepted)
        except RUN_ERRORS as error:
            raise DestroyUncertain(f"destroy was not confirmed for {sid}; inspect {sent}: {error}") from error

    def turn(self, sid, query, catalogue, menu, errors, directory_on_host):
        self.verify(sid)
        orphan = json.loads((directory_on_host / "session.json").read_bytes())["orphan"]
        directory = self.clean_checkout(sid, orphan)
        context_id = f"engine-answer-{uuid4().hex}"
        catalogue_path = f"{directory}/catalogue.md"
        menu_path = f"{directory}/menu.md"
        output_path = f"{directory}/answer.json"
        menu_doc = semantics_only(menu, self.served_steps)
        # The prompt names absolute paths, so no command depends on the shell's directory.
        root = self.checkout_root(sid)
        prompt = turn_prompt(query, f"{root}/{output_path}", f"{root}/{catalogue_path}",
                             f"{root}/{menu_path}", errors)
        assert_blind((prompt, catalogue, menu_doc), self.manifest)
        for path, contents in ((catalogue_path, catalogue), (menu_path, menu_doc)):
            self.command("put-file", "--session", sid, "--path", path, "-",
                         data=contents.encode("utf-8"))
        evidence = directory_on_host / "answers" / context_id

        def record_pending():
            save_json(evidence / "pending.json", {"path": output_path, "query": query,
                                                 "prior_validation_errors": errors})

        _, record = self.prompt(sid, prompt, context_id, before_send=record_pending)
        try:
            save_json(evidence / "turn.json", record)
            try:
                self.clean_checkout(sid, orphan, clear=False)
            except RUN_ERRORS as error:
                save_json(evidence / "checkout-invalid.json", {"reason": str(error)})
                raise IsolationFailure(str(error), record) from error
            try:
                passed = self.audit_turn(sid, record["turn"], directory_on_host)
            except RUN_ERRORS as error:
                raise IsolationFailure(f"could not audit the engine session: {error}", record) from error
            if not passed:
                raise IsolationFailure("engine session failed transcript isolation audit", record)
            output, errors = self.capture_answer(sid, output_path, evidence)
        except IsolationFailure:
            raise
        except RUN_ERRORS as error:
            raise TurnFailure(str(error), record) from error
        return output, record, errors

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
        return result["passed"]
