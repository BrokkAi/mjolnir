"""Host adapter for the writer's sealed Podman launcher and distinct engine turns."""
import json
from pathlib import PurePosixPath
import re
import subprocess
import time
from uuid import uuid4

from .evidence import file_digests, save_bytes, save_json
from .prompt import CONTEXT_PROTOCOL, CONTEXT_RECALL, CONTEXT_SEED, assert_blind, semantics_only, turn_prompt
from .snapshot import PRIVATE_PATHS


MODEL = "fable"
EFFORT = "high"
CONTEXT_BOUNDARY = "context-cleared:"
SESSION_ID = re.compile(r"[a-f0-9]{32}")
# Use the same readiness window as the shared driver's wait_idle.
CONTEXT_TIMEOUT = 1200
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


# The only workspace writes the host owns are under a reserved scratch path.
# Clear it while the worker is idle, then check for unexpected tree changes.
CHECKOUT_CLEAN = """
from pathlib import Path
import shutil, subprocess, sys

scratch = Path(sys.argv[1])

def git(*args):
    return subprocess.check_output(["git", "-c", "safe.directory=*", *args])

if git("ls-files", "--", str(scratch)):
    raise SystemExit("engine scratch path overlaps tracked source")
if git("status", "--porcelain", "--untracked-files=all", "--ignored=matching",
       "--", ".", f":(exclude){scratch}"):
    raise SystemExit("engine workspace has unexpected changes outside its scratch path")
if scratch.is_symlink():
    scratch.unlink()
elif scratch.is_dir():
    shutil.rmtree(scratch)
elif scratch.exists():
    raise SystemExit("engine scratch path is occupied by an unexpected file")
"""

TMP_ARTIFACTS = """
import json, shutil, sys
from pathlib import Path

root = Path("/tmp")
if len(sys.argv) > 1:
    for name in json.loads(sys.argv[1]):
        if not name or "/" in name or name in {".", ".."}:
            raise ValueError("expected a top-level temporary entry")
        path = root / name
        if not path.is_symlink() and path.is_dir():
            shutil.rmtree(path)
        else:
            path.unlink(missing_ok=True)
else:
    print(json.dumps(sorted(path.name for path in root.iterdir())))
"""


def mentioned_tmp_entries(pages, after_seq, names):
    from audit import call_parts

    calls = [call_parts(item)[0] for page in pages for item in page["items"]
             if item["seq"] > after_seq and item["role"] in {"tool", "terminal"}
             and (item.get("body") or {}).get("call")]
    mentioned = set()
    for name in names:
        literal = json.dumps(f"/tmp/{name}")[1:-1]
        pattern = re.compile(r"(?<![\w./-])" + re.escape(literal) + r"""(?=$|[/\s'"\`;&|()<>{},\\])""")
        if any(pattern.search(call) for call in calls):
            mentioned.add(name)
    return mentioned


class TurnFailure(RuntimeError):
    def __init__(self, message, record):
        super().__init__(message)
        self.record = record


def reported_model_usage(turn):
    usage = turn.get("usage") or {}
    return (usage.get("provider_details") or {}).get("model_usage", {})


def usage_delta(before, after, command, turn, model_version):
    turns = after["turns"]
    if len(turns) != 1 or turns[0].get("command_id") != command or turns[0].get("accepted_ordinal") != turn:
        raise RuntimeError("mj usage did not identify exactly the expected engine turn")
    if after["turn_selections"].get(command) != {"model": MODEL, "effort": EFFORT}:
        raise RuntimeError("mj usage did not attribute the engine turn to Fable/high")
    models = reported_model_usage(turns[0])
    fable_models = {model for model in models if model.startswith("claude-fable-")}
    if fable_models != {model_version}:
        raise RuntimeError(f"expected provider usage for {model_version}, got {sorted(fable_models)}")
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
        settings = {row["key"]: row.get("current") for row in s.get("config_options", [])}
        if (settings.get("model"), settings.get("effort")) != (MODEL, EFFORT):
            raise RuntimeError(f"engine session {sid} is not configured for {MODEL}/{EFFORT}")
        if s.get("profile_id") != self.config["profile"]:
            raise RuntimeError(f"engine session {sid} is on the wrong Claude profile")
        return s

    def attest(self, sid, snapshot, directory):
        """Verify the sealed checkout, mounted cutoff and target egress."""
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
        if hold := self.driver.pilot_weekly_hold(self.config["profile"], scope=MODEL):
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

    def clear_context(self, sid, timeout=CONTEXT_TIMEOUT):
        self.verify(sid)
        page = self.driver.mj("transcript", "--session", sid, "--role", "system", "--limit", 1)
        cursor = page["latest_seq"]
        command = f"engine-clear-{uuid4().hex}"
        self.message(sid, "/clear", command)
        deadline = time.monotonic() + timeout
        while True:
            page = self.driver.mj("transcript", "--session", sid, "--role", "system",
                                  "--after-seq", cursor)
            if any(item["stable_id"] == CONTEXT_BOUNDARY + command for item in page["items"]):
                # Completion can replace the native ACP session and restore
                # the selectors. Recheck both settings before the next prompt.
                self.verify(sid)
                return command
            cursor = max(cursor, page["next_after_seq"])
            if time.monotonic() >= deadline:
                raise RuntimeError(f"engine did not confirm /clear ({command})")
            time.sleep(1)

    def context_probe(self, sid, directory):
        path = directory / "context-probe.json"
        identity = {"session": sid, "protocol": CONTEXT_PROTOCOL,
                    "model_version": self.config["model_version"]}
        if path.is_file():
            old = json.loads(path.read_text(encoding="utf-8"))
            if all(old.get(key) == value for key, value in identity.items()) and old["status"] != "error":
                return old
        result = {**identity, "status": "error", "context_id": None,
                  "primer": None, "recall": None}
        try:
            self.verify(sid)
            self.capture_pending(sid, directory)
            self.clean_checkout(sid)
            query = f'(function :name "context_probe_{uuid4().hex}")'
            primer = CONTEXT_SEED.format(query=query)
            assert_blind((primer, CONTEXT_RECALL), self.manifest)
            response, result["primer"] = self.prompt(sid, primer)
            if response != "ACK":
                raise RuntimeError("engine did not acknowledge the synthetic context primer")
            result["context_id"] = self.clear_context(sid)
            response, result["recall"] = self.prompt(sid, CONTEXT_RECALL, result["context_id"])
            try:
                empty = json.loads(response or "null") == ""
            except ValueError:
                empty = False
            result["status"] = "passed" if empty else "failed"
            if not empty:
                result["message"] = "engine recalled prior query text or failed to report empty context after /clear"
        except LaunchHeld:
            raise
        except TurnFailure as error:
            key = "recall" if result["context_id"] else "primer"
            result[key] = error.record
            result["message"] = str(error)
            if isinstance(error, QuotaTurnFailure):
                self.driver.save(path, result)
                raise
        except RUN_ERRORS as error:
            result["message"] = str(error)
        self.driver.save(path, result)
        return result

    def clean_checkout(self, sid):
        scratch = f".rql-engine-{sid}"
        place = self.driver.container(sid)
        cwd = f"/workspace/{sid}/{self.config['destination']}"
        proc = subprocess.run(
            ["podman", "exec", "--workdir", cwd, place["container"], "python3", "-c",
             CHECKOUT_CLEAN, scratch], capture_output=True, timeout=300,
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

    def tmp_admin(self, sid, remove=None):
        container = self.driver.container(sid)["container"]
        args = [] if remove is None else [json.dumps(remove)]
        proc = subprocess.run(["podman", "exec", "--user", "0", container,
                               "python3", "-c", TMP_ARTIFACTS, *args], capture_output=True, timeout=300)
        if proc.returncode:
            raise RuntimeError(f"engine temporary file reset failed: {proc.stderr.decode(errors='replace').strip()}")
        return json.loads(proc.stdout) if remove is None else None

    def tmp_query_start(self, sid, directory):
        before = self.tmp_admin(sid)
        baseline = directory / "tmp-baseline.json"
        if not baseline.exists():
            save_json(baseline, before)
        cursor = self.driver.mj("transcript", "--session", sid, "--limit", 1)["latest_seq"]
        return {"before": before, "after_seq": cursor}

    def reset_query_tmp(self, sid, directory, evidence, scope, transcript_path=None):
        reset = evidence / "tmp-reset.json"
        if reset.exists():
            return json.loads(reset.read_bytes())["cross_query_reuse"]
        history_path = directory / "residual-artifacts.json"
        history = json.loads(history_path.read_bytes()) if history_path.exists() else []
        event = next((row for row in history if row["context_id"] == evidence.name), None)
        if event is None:
            if transcript_path is None:
                transcript_path = evidence / "reset-transcript.json"
                if not transcript_path.exists():
                    save_json(transcript_path, self.driver.transcript(sid))
            pages = json.loads(transcript_path.read_bytes())
            before, after = set(scope["before"]), set(self.tmp_admin(sid))
            baseline = set(json.loads((directory / "tmp-baseline.json").read_bytes()))
            prior = history[-1]["survivors"] if history else {}
            candidates = after - baseline
            mentions = mentioned_tmp_entries(pages, scope["after_seq"], candidates | (before & prior.keys()))
            kept = candidates - mentions
            survivors = {name: prior[name] for name in kept & before & prior.keys()}
            survivors.update({name: evidence.name for name in kept - before})
            event = {"context_id": evidence.name, "remove": sorted(candidates & mentions), "kept": sorted(kept),
                     "survivors": survivors, "cleanup_complete": False,
                     "cross_query_reuse": [{"path": f"/tmp/{name}", "prior_context_id": prior[name]}
                                           for name in sorted(before & prior.keys() & mentions)]}
            history.append(event)
            save_json(history_path, history)
        if not event["cleanup_complete"]:
            if event["remove"]:
                self.tmp_admin(sid, event["remove"])
            event["cleanup_complete"] = True
            save_json(history_path, history)
        save_json(reset, event)
        return event["cross_query_reuse"]

    def capture_pending(self, sid, directory):
        pending = []
        for path in (directory / "answers").glob("*/pending.json"):
            record = json.loads(path.read_bytes())
            captured = (path.parent / "answer.raw").is_file() or (path.parent / "absent.json").is_file()
            reset_due = "tmp_scope" in record and not (path.parent / "tmp-reset.json").is_file()
            if not captured or reset_due:
                pending.append((path, record, captured))
        if len(pending) > 1:
            raise RuntimeError("more than one engine answer is pending capture")
        if pending:
            path, record, captured = pending[0]
            expected = f".rql-engine-{sid}/answer.json"
            if record["path"] != expected:
                raise RuntimeError("pending engine answer path does not match its session")
            if not captured:
                self.capture_answer(sid, expected, path.parent)
            if "tmp_scope" in record:
                self.reset_query_tmp(sid, directory, path.parent, record["tmp_scope"])

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
            raise RuntimeError(f"destroy status for {sid} is uncertain; inspect {sent}")

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
                raise RuntimeError("the stored fidelity transcript did not pass the audit")
            hashes = file_digests(directory)
            hashes.pop("capture.json", None)
            save_json(directory / "capture.json", {
                "session": sid, "instance": self.config["instance"], "commit": record["commit"],
                "files_sha256": hashes, "audit_passed": result["passed"],
            })
        except (*RUN_ERRORS, TypeError) as error:
            raise RuntimeError(f"could not capture engine {sid}; session preserved: {error}") from error

        save_json(sent, {"session": sid, "instance": self.config["instance"]})
        accepted = json.loads(self.command("destroy", "--session", sid, "--json"))
        if accepted.get("session_id") != sid or accepted.get("accepted") is not True:
            raise RuntimeError(f"destroy was not confirmed for {sid}; inspect {sent}")
        save_json(receipt, accepted)

    def turn(self, sid, query, catalogue, menu, errors, directory_on_host):
        self.verify(sid)
        self.capture_pending(sid, directory_on_host)
        directory = self.clean_checkout(sid)
        context_id = self.clear_context(sid)
        catalogue_path = f"{directory}/catalogue.md"
        menu_path = f"{directory}/menu.md"
        output_path = f"{directory}/answer.json"
        menu_doc = semantics_only(menu, self.served_steps)
        prompt = turn_prompt(query, output_path, catalogue_path, menu_path, errors)
        assert_blind((prompt, catalogue, menu_doc), self.manifest)
        for path, contents in ((catalogue_path, catalogue), (menu_path, menu_doc)):
            self.command("put-file", "--session", sid, "--path", path, "-",
                         data=contents.encode("utf-8"))
        evidence = directory_on_host / "answers" / context_id

        def record_pending():
            save_json(evidence / "pending.json", {"path": output_path, "query": query,
                                                 "prior_validation_errors": errors,
                                                 "tmp_scope": self.tmp_query_start(sid, directory_on_host)})

        _, record = self.prompt(sid, prompt, context_id, before_send=record_pending)
        try:
            save_json(evidence / "turn.json", record)
            try:
                passed = self.audit_turn(sid, record["turn"], directory_on_host)
            except RUN_ERRORS as error:
                raise IsolationFailure(f"could not audit the engine session: {error}", record) from error
            if not passed:
                raise IsolationFailure("engine session failed transcript isolation audit", record)
            output, errors = self.capture_answer(sid, output_path, evidence)
            scope = json.loads((evidence / "pending.json").read_bytes())["tmp_scope"]
            record["cross_query_reuse"] = self.reset_query_tmp(
                sid, directory_on_host, evidence, scope, directory_on_host / f"turn-{record['turn']}-transcript.json")
            save_json(evidence / "turn.json", record)
            self.clean_checkout(sid)
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
