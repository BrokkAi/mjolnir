"""Run a frozen RQL census pilot matrix, one admitted diff at a time."""
import argparse
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
import pilot_review

PILOT = Path(__file__).resolve().parent / "brief/rql-census/pilot"
DEFAULT_ROOT = Path("/mnt/optane/review-arms-pilot")
BUNDLES = {"mjolnir": "mjolnir", "bifrost-dev": "bifrost-history"}
WRITER_ARMS = ("G", "R", "R-loc")
ARMS = ("N", *WRITER_ARMS)
TOOL_ARMS = ("R", "R-loc")
FLOOR = 2500
# Hundredths of a percentage point. Registered lean-pilot Sol/Terra rates.
COST = {"writer": 43, "reviewer": 20}


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
    threshold = FLOOR + sum(COST[job["kind"]] for job in jobs)
    return quota(floor=f"{profile}={threshold // 100}.{threshold % 100:02}",
                 weekly=True, strict=True)


def run_matrix(root, diff_jobs, profile, complete, execute, quota=driver.quota_hold):
    """A persisted admission owns the next slot until all its selected jobs validate."""
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
            for job in sorted(jobs, key=lambda job: job["kind"] != "writer"):
                if complete(job):
                    continue
                state = execute(job)
                print(json.dumps({"job": job["job"], "state": state}), flush=True)
                if state != "done":
                    return 1
                if not complete(job):
                    raise RuntimeError(f"{job['job']} completed without valid artifacts")
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


def preflight(config, profile, rql_profile, writer_model):
    enabled = {name: spec for name, spec in config["profiles"].items() if spec.get("enabled", True)}
    allowed = {profile}
    if rql_profile:
        allowed.add(rql_profile)
    if {name for name, spec in enabled.items() if spec["kind"] == "codex"} != allowed:
        raise ValueError("enable the allotted Codex profile and only its Sol-only alias if selected")
    if rql_servers(codex_config(enabled[profile])):
        raise ValueError(f"{profile}: G writers and reviewers require a profile without an RQL MCP server")
    if rql_profile:
        alias, base = enabled[rql_profile], enabled[profile]
        rql = codex_config(alias)
        if (rql_profile == profile or alias["kind"] != "codex"
                or Path(alias["home"]).expanduser().resolve() != Path(base["home"]).expanduser().resolve()
                or alias.get("models") != [writer_model] or rql.get("model") != writer_model
                or rql_servers(rql) != ["rql"]):
            raise ValueError(f"{rql_profile}: configure a Sol-only alias of {profile} with exactly one RQL MCP server")
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


def repository(config, row, args):
    repo = row["repo"]
    bundle = BUNDLES[repo]
    spec = config["bundles"][bundle]
    primary = next(r for r in spec["repositories"] if r["id"] == spec["primary_repo"])
    driver.BUNDLE, driver.REPO_DIR = bundle, primary["destination"]
    driver.REPO = args.mjolnir_clone if repo == "mjolnir" else args.bifrost_clone


def prompt_set(reference, steps):
    base = (driver.SCRIPTS / "brief/WRITER.md").read_text()
    grep = base + "\n" + (PILOT / "writer-grep.md").read_text()
    prompts = {"G": grep}
    if steps is not None:
        # Put the tool exception inside the existing rule too, so the directions agree.
        with_tool = grep.replace("use the\n  network", "use the\n  network except through the `rql_query` tool")
        tool = (PILOT / "writer-rql.md").read_text().format(reference=reference, menu_steps=steps)
        prompts.update(dict.fromkeys(TOOL_ARMS, with_tool + "\n" + tool))
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


def prepare_tool(path, args, job, diff, sid):
    marker = path / "tool-ready.json"
    log = path / "queries.jsonl"
    if not marker.exists():
        assignment = path / "tool-assignment.json"
        driver.save(assignment, {"session_id": sid, "diff": job["diff"], "arm": job["arm"], "intro": diff["intro"],
                                 "manifest": str(args.manifest), "log": str(log)})
        # The installer binds the credential to this session and publishes it
        # only after the host assignment is ready. It must be idempotent.
        check = subprocess.run([str(args.rql_hook), str(assignment)], check=True, stdout=subprocess.PIPE)
        listed = json.loads(check.stdout)
        if listed != {"session_id": sid, "tools": ["rql_query"]}:
            raise RuntimeError(f"RQL handshake for {job['job']} did not expose exactly rql_query")
        log.touch(exist_ok=True)
        driver.save(marker, listed)


class Pilot:
    def __init__(self, args, config, manifest, raw, image, reference, steps):
        self.args, self.config = args, config
        self.diffs = {row["key"]: row for row in manifest["diffs"]}
        self.writer_model = tuple(manifest["models"]["writer"].rsplit("/", 1))
        self.reviewer_model = tuple(manifest["models"]["reviewer"].rsplit("/", 1))
        self.common = {"manifest_sha256": digest(raw), "image_digest": image,
                       "rql_reference_sha256": digest(reference),
                       "menu_steps_sha256": digest(steps) if steps is not None else None}
        self.prompts = prompt_set(reference.decode(), steps.decode() if steps is not None else None)
        self.rubric = pilot_review.RUBRIC_PATH.read_bytes()
        self.gold = (driver.SCRIPTS / "brief/rql-census/menu-steps-cases.md").read_text().strip()
        self.bug_ids = {row["id"].casefold() for row in manifest["diffs"]}
        self.gate = threading.Semaphore(1)
        self.messages = {}
        self.captures = {}
        self.attested = set()

    def render(self, job):
        row = self.diffs[job["diff"]]
        repository(self.config, row, self.args)
        bug = {"id": row["id"], "intro_commit": row["intro"], "base_commit": row["base"]}
        profile = self.args.rql_profile if job["kind"] == "writer" and job["arm"] in TOOL_ARMS else self.args.profile
        params = {"path": job_path(self.args.run_root, job), "bug": bug, "profile": profile}
        inputs = dict(self.common)
        if job["kind"] == "writer":
            if row["key"] not in self.messages:
                self.messages[row["key"]] = driver.commit_message(bug)
            # Replacing both placeholders in one pass leaves source text and
            # RQL reference examples untouched.
            replacements = {"base": row["base"], "message": self.messages[row["key"]]}
            prompt = re.sub(r"\{(base|message)\}", lambda m: replacements[m[1]], self.prompts[job["arm"]])
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
            prompt = pilot_review.prompt_with_rubric(self.rubric, self.captures[row["key"]], task, brief)
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
            if job["kind"] == "writer" and job["arm"] in TOOL_ARMS and not (path / "queries.jsonl").is_file():
                return None
            return record
        return None

    def execute(self, job):
        params = self.render(job)
        if params is None:
            raise RuntimeError(f"{job['job']} requires an accepted brief")
        path = params["path"]
        if (path / "done.json").exists() or ((path / "failed.json").exists() and (path / "suspend.json").exists()):
            archive(path)
        receipt = path / "inputs.json"
        if receipt.exists() and read_json(receipt) != params["inputs"] and (path / "launch-intent.json").exists():
            return "needs-recovery: inputs changed with a launch in progress"
        driver.save(receipt, params["inputs"])
        attempt = len(list(path.parent.glob(f"{path.name}.superseded-*")))
        params["tag"] = f"{job['job']}/{attempt}"
        # Every writer and reviewer is attested before its prompt: the transcript audit is only a
        # partial check, so the mounted cache cutoff is verified structurally in each session.
        params["attest_cache"] = True
        if job["kind"] == "writer":
            params["model"] = self.writer_model
            params["prepare_tool"] = (lambda sid: prepare_tool(path, self.args, job, self.diffs[job["diff"]], sid)
                                      ) if job["arm"] in TOOL_ARMS else None
            state = brief_driver.run(params["bug"]["id"], self.gate, pilot=params)
        else:
            params["model"] = self.reviewer_model
            state = pilot_review.run(params)
        if params["attest_cache"] and (path / "cache-attestation.json").exists():
            self.attested.add(job["kind"])
        return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=PILOT / "manifest.json")
    parser.add_argument("--instance", required=True, help="dedicated named Mjolnir instance")
    parser.add_argument("--profile", required=True, help="allotted Codex login for quota, G, and Terra reviewers")
    parser.add_argument("--rql-profile", help="Sol-only Codex alias of --profile for R and R-loc")
    parser.add_argument("--rql-hook", type=Path, help="executable that installs an RQL assignment before the writer turn")
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
    args = parser.parse_args()
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
    if uses_rql:
        if not (args.rql_profile and args.rql_hook and steps):
            parser.error("R and R-loc require --rql-profile, --rql-hook and --menu-steps")
        args.rql_hook = args.rql_hook.resolve()
        if not os.access(args.rql_hook, os.X_OK):
            parser.error("--rql-hook must be executable")
    for data, key in ((reference, "catalogue_sha256"), (steps, "steps_sha256")):
        expected = manifest["inputs"].get(key)
        if expected is not None and (data is None or digest(data) != expected):
            parser.error(f"{key} differs from the frozen manifest")
    if uses_rql and manifest["inputs"].get("steps_sha256") is None:
        parser.error("freeze the menu semantics in the manifest before running R or R-loc")
    # mjx strips inherited MJ_* and XDG settings, then passes the explicit named instance.
    config = tomllib.loads((config_dir / "config.toml").read_text())
    image = preflight(config, args.profile, args.rql_profile, manifest["models"]["writer"].rsplit("/", 1)[0])
    driver.workspace_id()  # discover a missing workspace before touching quota
    args.run_root.mkdir(parents=True, exist_ok=True)
    with (config_dir / "pilot.lock").open("a") as instance_lock, (args.run_root / "pilot.lock").open("a") as run_lock:
        for lock in (instance_lock, run_lock):
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as e:
                raise RuntimeError("another pilot owns the study instance or run root") from e
        runner = Pilot(args, config, manifest, raw, image, reference, steps)
        try:
            return run_matrix(args.run_root, groups, args.profile, runner.complete, runner.execute)
        finally:
            subprocess.run(driver.mj_invocation("daemon", "stop"), check=True)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as exc:
        sys.exit(str(exc))
