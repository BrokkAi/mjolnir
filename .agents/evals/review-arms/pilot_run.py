"""Run a frozen RQL census pilot matrix, one admitted diff at a time."""
import argparse
import concurrent.futures
from contextlib import nullcontext
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import tomllib

import brief_driver
import driver
from pilot_lock import pilot_lock
import pilot_review

PILOT = Path(__file__).resolve().parent / "brief/rql-census/pilot"
DEFAULT_ROOT = Path("/mnt/optane/review-arms-pilot")
BUNDLES = {"mjolnir": "mjolnir", "bifrost-dev": "bifrost-history"}
WRITER_ARMS = ("G", "R", "R-loc")
ARMS = ("N", *WRITER_ARMS)
TOOL_ARMS = ("R", "R-loc")
# The one sentence of brief/WRITER.md that tells a writer how to read the change. A writer's
# diff-reading configuration replaces it; the default keeps the prompt byte-identical.
WRITER_DIFF_SENTENCE = "Read the change with\n`git diff {base} HEAD` and read the rest of the repository as you need."
# Hundredths of a percentage point. Registered lean-pilot Sol/Terra rates.
COST = {"writer": 43, "reviewer": driver.PILOT_TERRA_QUOTA_COST}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_json(path):
    return json.loads(path.read_text())


def job_path(root, job):
    cell = root / "runs" / job["diff"] / job["arm"] / f"r{job['replicate']}"
    return cell / "writer" if job["kind"] == "writer" else cell / "reviewers" / str(job["slot"])


def load_manifest(raw):
    manifest = json.loads(raw)
    if manifest["version"] != 1 or manifest["page_rows"] != 50:
        raise ValueError("expected pilot manifest version 1 with 50 rows per page")
    expected = {}
    diffs = set()
    for diff in manifest["diffs"]:
        key = diff["key"]
        if (diff["repo"] not in BUNDLES or not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9._-]*", diff["id"])
                or key != f"{diff['repo']}/{diff['id']}" or key in diffs
                or any(not re.fullmatch(r"[0-9a-f]{40}", diff[field]) for field in ("intro", "base"))):
            raise ValueError(f"invalid or repeated pilot diff {key!r}")
        diffs.add(key)
        reps = diff["replicates"]
        if set(reps) != set(WRITER_ARMS) or any(type(n) is not int or n not in (1, 2) for n in reps.values()):
            raise ValueError(f"invalid replicates for {key}")
        if reps["R-loc"] != 1:
            raise ValueError(f"expected one R-loc replicate for {key}")
        for arm, count in {"N": 1, **reps}.items():
            for rep in range(1, count + 1):
                members = [("reviewer", 1), ("reviewer", 2)]
                if arm != "N":
                    members.append(("writer", None))
                for kind, slot in members:
                    suffix = "writer" if slot is None else f"reviewer-{slot}"
                    job = f"{key}/{arm}/r{rep}/{suffix}"
                    expected[job] = dict(job=job, diff=key, arm=arm, replicate=rep, kind=kind, slot=slot)
    for job in manifest["jobs"]:
        key = job["job"]
        identity = expected.pop(key, None)
        if identity is None or any(job.get(field) != value for field, value in identity.items()):
            raise ValueError(f"unexpected or repeated pilot job {key!r}")
    if expected:
        raise ValueError(f"missing pilot job {next(iter(expected))}")
    return manifest


def admission(profile, jobs, quota=driver.quota_hold):
    return driver.pilot_weekly_hold(profile, cost=sum(COST[job["kind"]] for job in jobs), quota=quota)


def retained_attempts(root, job):
    path = job_path(root, job)
    attempts = sorted(path.parent.glob(f"{path.name}.superseded-*")) + [path]
    receipts = []
    for attempt in attempts:
        receipt = attempt / "failed.json"
        if receipt.exists():
            failure = read_json(receipt)
            if "retained_session" in failure:
                receipts.append({"receipt": str(receipt), **failure})
    return receipts


def run_matrix(root, diff_jobs, profile, complete, execute, quota=driver.quota_hold, *, concurrency=1):
    """A persisted admission owns the next slot until all its selected jobs validate."""
    if concurrency < 1:
        raise ValueError("concurrency must be at least 1")
    active = root / "active-diff.json"
    if active.exists():
        previous = read_json(active)
        key = previous["diff"]
        selected = {job["job"] for job in diff_jobs.get(key, [])}
        if not set(previous["jobs"]) <= selected:
            raise RuntimeError(f"{key} was interrupted. Resume with its admitted jobs before selecting another diff")
        diff_jobs = {key: diff_jobs[key], **{k: v for k, v in diff_jobs.items() if k != key}}

    for key, jobs in diff_jobs.items():
        pending = [job for job in jobs if not complete(job)]
        if pending:
            if hold := admission(profile, pending, quota):
                print(json.dumps({"diff": key, "state": "held", "reason": hold}), flush=True)
                return 1
            driver.save(active, {"diff": key, "jobs": [job["job"] for job in jobs]})
            ordered = sorted(jobs, key=lambda job: job["kind"] != "writer")
            phases = (
                [job for job in ordered if job["kind"] == "writer"],
                [job for job in ordered if job["kind"] != "writer"],
            )
            retried = set()
            halted = False
            error = None

            def handle_outcome(job, state, retry_queue):
                nonlocal halted, error
                print(json.dumps({"job": job["job"], "state": state}), flush=True)
                if job["job"] in retried:
                    print(json.dumps({"job": job["job"], "retained_attempts": retained_attempts(root, job)}),
                          flush=True)
                if state == "retained-failure" and job["job"] not in retried:
                    if halted:
                        return
                    remaining = [member for member in jobs if not complete(member)]
                    if hold := admission(profile, remaining, quota):
                        print(json.dumps({"diff": key, "state": "held", "reason": hold,
                                          "retained_attempts": retained_attempts(root, job)}), flush=True)
                        halted = True
                        return
                    retried.add(job["job"])
                    retry_queue.insert(0, job)
                    return
                if state != "done":
                    halted = True
                    return
                if not complete(job):
                    halted = True
                    if error is None:
                        error = RuntimeError(f"{job['job']} completed without valid artifacts")

            def peek_next(phase_jobs, retry_queue, active_ids):
                while retry_queue and (retry_queue[0]["job"] in active_ids or complete(retry_queue[0])):
                    retry_queue.pop(0)
                if retry_queue:
                    return retry_queue[0], True
                for candidate in phase_jobs:
                    if candidate["job"] not in active_ids and not complete(candidate):
                        return candidate, False
                return None, False

            # Finish all writers before admitting any reviewer (PLAN-rql-census.md).
            for phase_jobs in phases:
                if halted or not phase_jobs:
                    break
                retry_queue = []
                if concurrency == 1:
                    while not halted:
                        next_job, from_retry = peek_next(phase_jobs, retry_queue, set())
                        if next_job is None:
                            break
                        if from_retry:
                            retry_queue.pop(0)
                        handle_outcome(next_job, execute(next_job), retry_queue)
                    continue
                in_flight = {}
                with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
                    try:
                        while True:
                            # Drain every already-finished future before refilling slots so a peer
                            # interruption or exception halts submission before spending more quota.
                            while ready_done := [future for future in list(in_flight) if future.done()]:
                                for future in ready_done:
                                    job = in_flight.pop(future)
                                    try:
                                        state = future.result()
                                    except Exception as exc:
                                        halted = True
                                        if error is None:
                                            error = exc
                                        continue
                                    handle_outcome(job, state, retry_queue)
                            if not halted:
                                while len(in_flight) < concurrency:
                                    if any(future.done() for future in in_flight):
                                        break
                                    active_ids = {member["job"] for member in in_flight.values()}
                                    next_job, from_retry = peek_next(phase_jobs, retry_queue, active_ids)
                                    if next_job is None or any(future.done() for future in in_flight):
                                        break
                                    if from_retry:
                                        retry_queue.pop(0)
                                    in_flight[pool.submit(execute, next_job)] = next_job
                            if not in_flight:
                                break
                            concurrent.futures.wait(in_flight, return_when=concurrent.futures.FIRST_COMPLETED)
                    except KeyboardInterrupt:
                        # Keep pilot locks held and daemon up until active worker threads drain.
                        for future in in_flight:
                            future.cancel()
                        raise
            if error is not None:
                raise error
            if halted:
                return 1
        active.unlink(missing_ok=True)
    return 0


def codex_config(profile):
    home = Path(profile["home"]).expanduser()
    base = home / "config.toml"
    result = tomllib.loads(base.read_text()) if base.exists() else {}
    if overlay := profile.get("codex_profile"):
        overrides = tomllib.loads((home / f"{overlay}.config.toml").read_text())
        servers = {**result.get("mcp_servers", {}), **overrides.get("mcp_servers", {})}
        result.update(overrides)
        result["mcp_servers"] = servers
    return result


def rql_servers(config):
    return [name for name, entry in config.get("mcp_servers", {}).items()
            if entry.get("enabled", True)
            and ("rql" in name.lower() or "rql_query" in json.dumps(entry))]


def preflight(config, profile):
    codex = {name: spec for name, spec in config["profiles"].items() if spec["kind"] == "codex"}
    if set(codex) != {profile} or not codex[profile].get("enabled", True):
        raise ValueError("the private instance needs exactly one Codex profile, the enabled allotted login")
    if not codex[profile].get("codex_profile") or rql_servers(codex_config(codex[profile])) != ["rql"]:
        raise ValueError(f"{profile}: configure the private instance profile with the rql MCP overlay")
    target = config["targets"]["podman"]
    image = target["image"]
    if not re.fullmatch(r"(?:sha256:)?[0-9a-f]{64}", image) or target.get("pull_policy") != "never":
        raise ValueError("the private instance's podman target must pin an image ID with pull_policy = 'never'")
    return driver.image_digest(image)


def select_jobs(manifest, diff=None, arms=None, replicate=None):
    groups = {row["key"]: [] for row in manifest["diffs"] if not diff or diff == row["key"]}
    for job in manifest["jobs"]:
        if (job["diff"] in groups and (not arms or job["arm"] in arms)
                and (replicate is None or replicate == job["replicate"])):
            groups[job["diff"]].append(job)
    return {key: jobs for key, jobs in groups.items() if jobs}


def require_fidelity(root, raw, manifest, groups):
    uses_rql = any(tool_arm(job) != "none" for jobs in groups.values() for job in jobs)
    path = root / "fidelity/gate-report.json"
    if not uses_rql and not path.exists():
        return
    try:
        report = read_json(path)
        pooled = report["prescience_pooled"]
        if pooled["pass"] is not True:
            detail = pooled.get("reason", "recall gap exceeds the registered bar")
            raise ValueError(f"pooled prescience failed and blocks the pilot: {detail}")
        if not uses_rql:
            return
        steps = manifest.get("served_steps")
        if not isinstance(steps, list) or not all(isinstance(step, str) for step in steps):
            raise ValueError("R and R-loc require a manifest with served_steps")
        from rql_mock.engine import load_engine
        identity = load_engine().identity
        if (report["manifest_sha256"] != digest(raw)
                or report["manifest_sha256"] != identity["manifest_sha256"]
                or report["engine_inputs_sha256"] != identity["engine_inputs_sha256"]
                or report["served_steps"] != steps):
            raise ValueError("report is for another manifest, engine identity, or step list")
        evidence = report["steps"]
        if not isinstance(evidence, dict):
            raise ValueError("report has no per-step evidence")
        failed = [step for step in steps if evidence.get(step, {}).get("pass") is not True]
        if failed:
            raise ValueError(f"no passing result for {', '.join(failed)}")
        return identity
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        reason = "R and R-loc require a passing fidelity gate" if uses_rql else "pilot fidelity gate blocked launch"
        raise ValueError(f"{reason} at {path}: {error}") from error


def repository(config, row, args):
    repo = row["repo"]
    bundle = BUNDLES[repo]
    spec = config["bundles"][bundle]
    primary = next(r for r in spec["repositories"] if r["id"] == spec["primary_repo"])
    driver.BUNDLE, driver.REPO_DIR = bundle, primary["destination"]
    driver.REPO = args.mjolnir_clone if repo == "mjolnir" else args.bifrost_clone


def served_menu(semantics, served_steps):
    if not served_steps:
        return "The pilot serves the documented steps above."
    header = "## Common answer rules"
    parts = re.split(r"(?m)^## `([a-z][a-z0-9-]*)`\s*$", semantics)
    sections = dict(zip(parts[1::2], parts[2::2], strict=True))
    if header not in parts[0] or any(step not in sections for step in served_steps):
        raise ValueError("frozen menu semantics do not cover the manifest's served steps")
    common = parts[0][parts[0].index(header):].strip()
    common = common.replace("used by the three steps", "used by the added steps")
    return "\n\n".join([common] + [f"## `{step}`\n\n{sections[step].strip()}" for step in served_steps])


def prompt_set(reference, steps, served_steps=()):
    base = (driver.SCRIPTS / "brief/WRITER.md").read_text()
    grep = base + "\n" + (PILOT / "writer-grep.md").read_text()
    if served_steps and steps is None:
        raise ValueError("the writer reference requires the frozen served-step semantics")
    menu = served_menu(steps, served_steps)
    reference_block = f"\n\n### Documented RQL reference\n\n{reference}\n\n### Additional menu steps\n\n{menu}"
    prompts = {"G": grep + "\nYou have no query tool." + reference_block}
    if steps is not None:
        # Put the tool exception inside the existing rule too, so the directions agree.
        with_tool = grep.replace("use the\n  network", "use the\n  network except through the `rql_query` tool")
        tool = (PILOT / "writer-rql.md").read_text()
        prompts.update(dict.fromkeys(TOOL_ARMS, with_tool + "\n" + tool.rstrip() + reference_block))
    return prompts


def result_record(path, kind):
    name, key = ("brief.md", "brief_sha256") if kind == "writer" else ("output.md", "output_sha256")
    try:
        done = read_json(path / "done.json")
        data = (path / name).read_bytes()
        if (done["session_id"] and data.strip() and digest(data) == done[key]
                and digest((path / "prompt.txt").read_bytes()) == done["inputs"]["prompt_sha256"]
                and read_json(path / "audit.json")["passed"] is True
                and driver.valid_usage(path / "usage.json")):
            return done
    except (OSError, ValueError, KeyError, TypeError):
        pass
    return None


def archive(path):
    index = 1
    while (retired := path.with_name(f"{path.name}.superseded-{index}")).exists():
        index += 1
    path.rename(retired)


def tool_arm(job):
    return job["arm"] if job["kind"] == "writer" and job["arm"] in TOOL_ARMS else "none"


def expected_tools(arm):
    return [] if arm == "none" else ["rql_query"]


def publish_tool_assignment(path, args, job, diff, sid, *, engine_identity):
    marker = path / "tool-endpoint.json"
    arm = tool_arm(job)
    if arm != "none" and not engine_identity:
        raise ValueError("an RQL writer assignment requires an admitted engine identity")
    if not marker.exists():
        assignment = path / "tool-assignment.json"
        record = {"session_id": sid, "arm": arm, "manifest": str(args.manifest)}
        if arm != "none":
            log = path / f"queries-{sid}.jsonl"
            record.update(diff=job["diff"], intro=diff["intro"], log=str(log), **engine_identity)
        driver.save(assignment, record)
        # The installer binds the credential to this session and publishes it
        # only after the host assignment is ready. It must be idempotent.
        check = subprocess.run([str(args.rql_hook), str(assignment)], check=True, stdout=subprocess.PIPE)
        listed = json.loads(check.stdout)
        if arm != "none":
            log.touch(exist_ok=True)
        driver.save(marker, listed)


class Pilot:
    def __init__(self, args, config, manifest, raw, image, reference, steps, *, engine_identity=None,
                 writer_diff_reading=pilot_review.SINGLE_COMMAND, reviewer_diff_reading=pilot_review.SINGLE_COMMAND):
        self.args, self.config = args, config
        self.diff_reading = {"writer": writer_diff_reading, "reviewer": reviewer_diff_reading}
        if not set(self.diff_reading.values()) <= set(pilot_review.DIFF_READINGS):
            raise ValueError(f"unknown diff reading {self.diff_reading}")
        self.engine_identity = engine_identity
        self.diffs = {row["key"]: row for row in manifest["diffs"]}
        self.writer_model = tuple(manifest["models"]["writer"].rsplit("/", 1))
        self.reviewer_model = tuple(manifest["models"]["reviewer"].rsplit("/", 1))
        self.common = {"manifest_sha256": digest(raw), "image_digest": image,
                       "rql_reference_sha256": digest(reference),
                       "menu_steps_sha256": digest(steps) if steps is not None else None}
        self.prompts = prompt_set(reference.decode(), steps.decode() if steps is not None else None,
                                  manifest.get("served_steps", []))
        self.rubric = pilot_review.RUBRIC_PATH.read_bytes()
        self.gold = (driver.SCRIPTS / "brief/rql-census/menu-steps-cases.md").read_text().strip()
        self.bug_ids = {row["id"].casefold() for row in manifest["diffs"]}
        self.gate = threading.Semaphore(1)
        self._lock = threading.RLock()
        self.messages = {}
        self.captures = {}
        self.reads = {}
        self.attested = set()

    def diff_reading_inputs(self, kind, row, bug):
        """Prompt reads for `kind` and the inputs that bind its configuration.

        The Stage 2a configuration adds no input, so its recorded inputs still match.
        """
        if self.diff_reading[kind] == pilot_review.SINGLE_COMMAND:
            return None, {}
        if row["key"] not in self.captures:
            self.captures[row["key"]] = pilot_review.capture(bug)
        if row["key"] not in self.reads:
            self.reads[row["key"]] = pilot_review.plan_reads(pilot_review.file_diffs(*self.captures[row["key"]][:2]))
        text = "\n".join([str(pilot_review.READ_BYTES), pilot_review.DIFF_READING_TEXT, WRITER_DIFF_SENTENCE,
                          *pilot_review.PATCH_FLAGS])
        return self.reads[row["key"]], {f"{kind}_diff_reading": self.diff_reading[kind],
                                        f"{kind}_diff_reading_sha256": digest(text.encode())}

    def _state_lock(self):
        return self._lock if hasattr(self, "_lock") else nullcontext()

    def render(self, job):
        with self._state_lock():
            row = self.diffs[job["diff"]]
            repository(self.config, row, self.args)
            bug = {"id": row["id"], "intro_commit": row["intro"], "base_commit": row["base"]}
            params = {"path": job_path(self.args.run_root, job), "bug": bug, "profile": self.args.profile}
            inputs = dict(self.common, rql_tools=expected_tools(tool_arm(job)))
            if tool_arm(job) != "none":
                if not self.engine_identity:
                    raise ValueError("an RQL writer requires an admitted engine identity")
                inputs.update(self.engine_identity)
            if job["kind"] == "writer":
                if row["key"] not in self.messages:
                    self.messages[row["key"]] = driver.commit_message(bug)
                # Replacing both placeholders in one pass leaves source text and
                # RQL reference examples untouched.
                replacements = {"base": row["base"], "message": self.messages[row["key"]]}

                def fill(text):
                    return re.sub(r"\{(base|message)\}", lambda m: replacements[m[1]], text)

                reads, reading = self.diff_reading_inputs("writer", row, bug)
                inputs.update(reading)
                if reads is None:
                    prompt = fill(self.prompts[job["arm"]])
                else:
                    head, found, tail = self.prompts[job["arm"]].partition(WRITER_DIFF_SENTENCE)
                    if not found or WRITER_DIFF_SENTENCE in tail:
                        raise ValueError(f"{job['job']}: the writer prompt must name its diff read exactly once")
                    trees = self.captures[row["key"]][:2]
                    block = ("Read the change with the reads below.\n\n" + pilot_review.reads_text(*trees, reads)
                             + "\n\nRead the rest of the repository as you need.")
                    prompt = fill(head) + block + fill(tail)
            else:
                brief = None
                if job["arm"] != "N":
                    writer = dict(job, kind="writer", job=f"{job['diff']}/{job['arm']}/r{job['replicate']}/writer")
                    accepted = self.complete(writer)
                    if not accepted:
                        return None
                    brief = (job_path(self.args.run_root, writer) / "brief.md").read_text()
                    inputs.update(brief_sha256=accepted["brief_sha256"], writer_session_id=accepted["session_id"])
                if row["key"] not in self.captures:
                    self.captures[row["key"]] = pilot_review.capture(bug)
                task = driver.context_text(bug)
                reads, reading = self.diff_reading_inputs("reviewer", row, bug)
                inputs.update(reading)
                prompt = pilot_review.prompt_with_rubric(self.rubric, self.captures[row["key"]], task, brief,
                                                         reads=reads)
                inputs.update(review_rubric_sha256=digest(self.rubric),
                              review_template_sha256=digest(pilot_review.TEMPLATE.encode()))
            if self.gold and self.gold in prompt:
                raise ValueError(f"{job['job']}: prompt contains the private menu-step cases")
            if any(bug_id in prompt.casefold() for bug_id in self.bug_ids):
                raise ValueError(f"{job['job']}: prompt contains a pilot bug id")
            inputs["prompt_sha256"] = digest(prompt.encode())
            return dict(params, inputs=inputs, prompt=prompt)

    def complete(self, job):
        path = job_path(self.args.run_root, job)
        record = result_record(path, job["kind"])
        if not record:
            return None
        params = self.render(job)
        if params is not None and params["inputs"] == record.get("inputs"):
            if tool_arm(job) != "none" and not (path / f"queries-{record['session_id']}.jsonl").is_file():
                return None
            try:
                proof = (path / "mcp-reconnect-result.json").read_bytes()
                data = json.loads(proof)
                if (digest(proof) != record.get("mcp_reconnect_sha256") or data["passed"] is not True
                        or data["session_id"] != record["session_id"]
                        or data["expected_tools"] != expected_tools(tool_arm(job))):
                    return None
            except (OSError, ValueError, KeyError, TypeError):
                return None
            return record
        return None

    def revoke_assignment(self, path, sid):
        assignment = path / "tool-assignment.json"
        if not assignment.exists():
            return
        record = read_json(assignment)
        if record["session_id"] != sid:
            raise ValueError("the assignment does not belong to the recorded launch")
        if record["arm"] != "none":
            subprocess.run([str(self.args.rql_hook), "--revoke", str(assignment)],
                           check=True, stdout=subprocess.PIPE)

    def retry_pending_revoke(self, marker):
        if not marker.exists():
            return
        try:
            self.revoke_assignment(marker.parent, read_json(marker)["session_id"])
            marker.unlink()
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
            print(f"RQL assignment revocation still pending at {marker}: {error}", file=sys.stderr, flush=True)

    def retry_pending_revokes(self):
        for marker in sorted(self.args.run_root.glob("runs/*/*/*/r*/writer/pending-revoke.json")):
            self.retry_pending_revoke(marker)

    def execute(self, job):
        params = self.render(job)
        if params is None:
            raise RuntimeError(f"{job['job']} requires an accepted brief")
        path = params["path"]
        launch = path / "launch.json"
        failure = path / "failed.json"

        def hold_for_input(sid, error):
            driver.retain_pilot_session(path, sid, str(error), status="input_required",
                                        pending_elicitations=error.questions, command_id=error.command)
            return f"interrupted: {sid} {error}"

        if failure.exists() and (pending := read_json(failure)).get("status") == "input_required":
            if not launch.exists():
                return f"needs-recovery: missing launch for {pending['retained_session']} ({pending['reason']})"
            sid = read_json(launch)["session_id"]
            try:
                brief_driver.resume_input_required(path, sid, pending)
            except brief_driver.WriterNeedsInput as error:
                return hold_for_input(sid, error)
            except brief_driver.WriterTurnFailed as error:
                if job["kind"] != "writer":
                    return driver.retain_pilot_session(path, sid, str(error))
                try:
                    state = brief_driver.retain_pilot_writer_failure(params, sid, error)
                except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as capture:
                    return f"interrupted: {sid} could not capture the failed writer ({capture})"
                return brief_driver.interrupt_on_rql_quota(params, sid) or state
            except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
                return f"interrupted: {sid} {pending['reason']} ({error})"
            else:
                failure.unlink()

        if failure.exists() and job["kind"] == "writer" and launch.exists():
            # A retained attempt made by an earlier build may lack the status.
            if read_json(failure).get("status") != "quota_hold":
                brief_driver.interrupt_on_rql_quota(params, read_json(launch)["session_id"])
        if failure.exists() and read_json(failure).get("status") == "quota_hold":
            from rql_mock.engine import LaunchHeld, load_engine
            try:
                engine = load_engine()
                if engine.identity != self.engine_identity:
                    raise ValueError("the RQL engine differs from the admitted identity")
                engine.runner().select_login()
            except LaunchHeld as hold:
                return f"interrupted: {hold}"
            except (OSError, ValueError, KeyError, TypeError) as error:
                return f"needs-recovery: cannot check the interrupted writer's engine quota ({error})"
        if (path / "done.json").exists() or failure.exists():
            if launch.exists():
                try:
                    self.revoke_assignment(path, read_json(launch)["session_id"])
                except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
                    return f"needs-recovery: could not revoke the superseded RQL assignment ({error})"
            archive(path)
        receipt = path / "inputs.json"
        if receipt.exists() and read_json(receipt) != params["inputs"] and (path / "launch-intent.json").exists():
            return "needs-recovery: inputs changed with a launch in progress"
        driver.save(receipt, params["inputs"])
        attempt = len(list(path.parent.glob(f"{path.name}.superseded-*")))
        params["tag"] = f"{job['job']}/{attempt}"
        params["run_root"] = self.args.run_root
        # Every writer and reviewer is attested before its prompt: the transcript audit is only a
        # partial check, so the mounted cache cutoff is verified structurally in each session.
        params["attest_cache"] = True
        params["publish_tool_assignment"] = lambda sid: publish_tool_assignment(
            path, self.args, job, self.diffs[job["diff"]], sid, engine_identity=self.engine_identity)
        try:
            if job["kind"] == "writer":
                params["model"] = self.writer_model
                state = brief_driver.run(params["bug"]["id"], self.gate, pilot=params)
            else:
                params["model"] = self.reviewer_model
                state = pilot_review.run(params)
        except brief_driver.WriterNeedsInput as error:
            return hold_for_input(read_json(launch)["session_id"], error)
        except (brief_driver.PromptUncertain, TimeoutError, subprocess.TimeoutExpired) as error:
            return f"interrupted: {error}"
        except (OSError, KeyError, TypeError, ValueError, RuntimeError, AssertionError, subprocess.SubprocessError) as error:
            if not launch.exists():
                return f"needs-recovery: launch has no recorded session ID ({error})"
            state = driver.retain_pilot_session(path, read_json(launch)["session_id"], str(error) or type(error).__name__)
        if job["kind"] == "writer" and launch.exists() and state == "retained-failure":
            if interruption := brief_driver.interrupt_on_rql_quota(params, read_json(launch)["session_id"]):
                state = interruption
        if params["attest_cache"] and (path / "cache-attestation.json").exists():
            with self._state_lock():
                self.attested.add(job["kind"])
        self.retry_pending_revoke(path / "pending-revoke.json")
        return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=PILOT / "manifest.json")
    parser.add_argument("--instance", required=True, help="dedicated named Mjolnir instance")
    parser.add_argument("--profile", required=True, help="allotted Codex login for quota, writers, and reviewers")
    parser.add_argument("--rql-hook", type=Path, required=True,
                        help="executable that publishes each sealed session's RQL assignment")
    parser.add_argument("--menu-steps", type=Path, help="frozen menu-step semantics, required for R or R-loc")
    parser.add_argument("--rql-reference", type=Path, default=driver.SCRIPTS / "brief/rql-census/rql-documented-steps.md")
    parser.add_argument("--run-root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--diff", help="select one diff key for a smoke run")
    parser.add_argument("--arm", action="append", choices=ARMS, help="select an arm, repeat for multiple arms")
    parser.add_argument("--replicate", type=int, choices=(1, 2))
    parser.add_argument("--attest-cache", action="store_true",
                        help="accepted for compatibility; every writer and reviewer Git cache is attested")
    parser.add_argument("--mjolnir-clone", type=Path, default=Path.home() / "Projects/mjolnir")
    parser.add_argument("--bifrost-clone", type=Path, default=Path("/mnt/optane/tmp/rust10/bifrost-src"))
    parser.add_argument("--writer-diff-reading", choices=pilot_review.DIFF_READINGS, default=pilot_review.SINGLE_COMMAND,
                        help="how writers read the diff; the default is the Stage 2a single command")
    parser.add_argument("--reviewer-diff-reading", choices=pilot_review.DIFF_READINGS,
                        default=pilot_review.SINGLE_COMMAND,
                        help="how reviewers read the diff; independent of the writers' setting, so a reviewer-only "
                             "rerun keeps the accepted briefs")
    parser.add_argument("--concurrency", type=int, default=1,
                        help="max concurrent jobs within one admitted diff (default: 1)")
    args = parser.parse_args()
    if args.concurrency < 1:
        parser.error("--concurrency must be at least 1")
    config_dir = driver.use_named_instance(args.instance)
    args.manifest, args.run_root = args.manifest.resolve(), args.run_root.resolve()
    raw = args.manifest.read_bytes()
    manifest = load_manifest(raw)
    groups = select_jobs(manifest, args.diff, args.arm, args.replicate)
    if not groups:
        parser.error("selection has no manifest jobs")
    reference = args.rql_reference.read_bytes()
    if args.menu_steps is None and manifest["inputs"].get("steps_sha256") is not None:
        args.menu_steps = driver.SCRIPTS / "brief/rql-census/menu-steps.md"
    steps = args.menu_steps.read_bytes() if args.menu_steps else None
    uses_rql = any(job["arm"] in TOOL_ARMS for jobs in groups.values() for job in jobs)
    args.rql_hook = args.rql_hook.resolve()
    if not args.rql_hook.is_file() or not os.access(args.rql_hook, os.X_OK):
        parser.error("--rql-hook must be executable")
    if uses_rql and not steps:
        parser.error("R and R-loc require frozen --menu-steps")
    for data, key in ((reference, "catalogue_sha256"), (steps, "steps_sha256")):
        expected = manifest["inputs"].get(key)
        if expected is not None and (data is None or digest(data) != expected):
            parser.error(f"{key} differs from the frozen manifest")
    if uses_rql and manifest["inputs"].get("steps_sha256") is None:
        parser.error("freeze the menu semantics in the manifest before running R or R-loc")
    engine_identity = require_fidelity(args.run_root, raw, manifest, groups)
    # mjx strips inherited MJ_* and XDG settings, then passes the explicit named instance.
    config = tomllib.loads((config_dir / "config.toml").read_text())
    image = preflight(config, args.profile)
    driver.workspace_id()  # discover a missing workspace before touching quota
    args.run_root.mkdir(parents=True, exist_ok=True)
    with (config_dir / "pilot.lock").open("a") as instance_lock:
        try:
            fcntl.flock(instance_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as e:
            raise RuntimeError("another pilot owns the study instance") from e
        with pilot_lock(args.run_root):
            runner = Pilot(args, config, manifest, raw, image, reference, steps, engine_identity=engine_identity,
                           writer_diff_reading=args.writer_diff_reading,
                           reviewer_diff_reading=args.reviewer_diff_reading)
            try:
                runner.retry_pending_revokes()
                return run_matrix(args.run_root, groups, args.profile, runner.complete, runner.execute,
                                  concurrency=args.concurrency)
            finally:
                subprocess.run(driver.mj_invocation("daemon", "stop"), check=True)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as exc:
        sys.exit(str(exc))
