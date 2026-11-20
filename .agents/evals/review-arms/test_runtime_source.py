"""Offline CLI/Git boundary checks for historical study source isolation."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
FAKE_MJX = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
with (root / "calls.jsonl").open("a") as f: f.write(json.dumps(args) + "\n")
state_path = root / "state.json"
s = json.loads(state_path.read_text()) if state_path.exists() else {"cfg": {"model": "initial", "effort": "low"}}
def opt(name): return args[args.index(name) + 1]
result = {}
if args[0] == "new":
 s.update(at=opt("--at") if "--at" in args else None,
          base=opt("--base") if "--base" in args else None,
          profile=opt("--profile"))
 result = {"session_id": "fixture-session"}
elif args[0] == "set-config": s["cfg"][opt("--key")] = opt("--value")
elif args[0] == "sessions":
 result = {"is_idle": True, "lifecycle": "live", "no_later_history": True,
           "at": s.get("at"), "base": s.get("base"), "profile_id": s.get("profile"),
           "config_options": [{"key": k, "current": v} for k, v in s["cfg"].items()],
           "last_turn_outcome": {"command_id": s.get("command"), "outcome": {"kind": "completed"}}}
elif args[0] == "message":
 s["command"] = opt("--command-id")
 result = {"session_id": "fixture-session", "via": "turn", "turn_id": 1}
 text = pathlib.Path(opt("--prompt-file")).read_text()
 (root / "sent-prompt.txt").write_text(text)
 if "Write the brief" in text:
  (root / "repo/brief.md").write_text("## Affected callers\n- none\n## Paths\n- none\n## Value flow\n- none\n")
elif args[0] == "fetch":
 name = opt("--path")
 if name == "brief.md":
  print((root / "repo/brief.md").read_text()); raise SystemExit
 if name == "verdicts.jsonl":
  print(json.dumps({"output_id":"o1", "bug":"known", "label":"missed", "matching_finding":None,
                    "rationale":"No finding", "finding_count":0, "confidence":"high"})); raise SystemExit
 print('{"labels":[]}'); raise SystemExit
elif args[:2] == ["review", "status"]:
 result = {"review": {"verdict": {"kind":"failed", "note":"offline boundary stop"}}}
elif args[0] == "transcript": result = {"items": [], "latest_seq": 0}
elif args[0] not in ("put-file", "usage", "suspend") and args[:2] != ["review", "start"]:
 raise SystemExit("unexpected fake mjx: " + repr(args))
state_path.write_text(json.dumps(s))
print(json.dumps(result))
'''
FAKE_PODMAN = r'''#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
root = pathlib.Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
if args[0] == "ps": print("fixture-container")
elif args[0] == "inspect": print("[]" if "Mounts" in args[-1] else "fixture-image")
elif args[0] == "exec":
 request = json.load(sys.stdin)
 with (root / "container-cwds.jsonl").open("a") as f: f.write(json.dumps(request) + "\n")
 request["cwd"] = str(root / "repo")
 result = subprocess.run([sys.executable, "-c", args[-1]], input=json.dumps(request), text=True, capture_output=True)
 sys.stdout.write(result.stdout); sys.stderr.write(result.stderr); raise SystemExit(result.returncode)
else: raise SystemExit("unexpected fake podman: " + repr(args))
'''
RUN = "import runpy,sys,time;time.sleep=lambda _:None;sys.argv=sys.argv[1:];runpy.run_path(sys.argv[0],run_name='__main__')"


class RuntimeSourceChecks(unittest.TestCase):
    def prepare(self, root, custom):
        scripts = root / "scripts"
        scripts.mkdir()
        subprocess.run(["git", "init", "-q", str(scripts)], check=True)
        for name in ("driver.py", "brief_driver.py", "verify_driver.py", "inventory.py"):
            shutil.copyfile(SCRIPTS / name, scripts / name)
        (scripts / "brief").mkdir()
        shutil.copyfile(SCRIPTS / "brief/WRITER.md", scripts / "brief/WRITER.md")
        (scripts / "corpus").mkdir()
        (scripts / "mjx").write_text(FAKE_MJX)
        (scripts / "mjx").chmod(0o755)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        (bin_dir / "podman").write_text(FAKE_PODMAN)
        (bin_dir / "podman").chmod(0o755)
        repo = root / "repo"
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        def git(*args):
            return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()
        git("config", "user.name", "Offline fixture")
        git("config", "user.email", "fixture@example.invalid")
        (repo / "source.rs").write_text("fn old() {}\n")
        git("add", "source.rs")
        git("commit", "-qm", "Base")
        base = git("rev-parse", "HEAD")
        (repo / "source.rs").write_text("fn new() {}\n")
        git("commit", "-qam", "Historical intent")
        intro = git("rev-parse", "HEAD")
        git("update-ref", "refs/mj/session-start/fixture", intro)
        known = {"id": "known", "base_commit": base, "intro_commit": intro}
        ordinary = {**known, "id": "ordinary"}
        corpus = root / "selected-known.jsonl" if custom else scripts / "corpus/candidates-all.jsonl"
        diffs = root / "selected-ordinary.jsonl" if custom else scripts / "corpus/diffs.jsonl"
        corpus.write_text(json.dumps(known) + "\n")
        diffs.write_text(json.dumps(ordinary) + "\n")
        if custom:
            (scripts / "corpus/candidates-all.jsonl").write_text('{"id":"wrong-default"}\n')
        data = root / "data"
        for sub in ("default-instance", "default-instance/verify"):
            (data / sub).mkdir(parents=True, exist_ok=True)
            (data / sub / "workspace.json").write_text('{"workspace":{"id":"isolated-fixture"}}')
        (data / "verify/repo").mkdir(parents=True)
        env = {k: v for k, v in os.environ.items() if not k.startswith("REVIEW_ARMS_")}
        env.update(REVIEW_ARMS_DIR=str(data), REVIEW_ARMS_REPO=str(repo), REVIEW_ARMS_INSTANCE="default",
                   REVIEW_ARMS_QUOTA_FLOOR="", FIXTURE_ROOT=str(root), PYTHONDONTWRITEBYTECODE="1",
                   PATH=f"{bin_dir}:{os.environ['PATH']}")
        if custom:
            env.update(REVIEW_ARMS_CORPUS=str(corpus), REVIEW_ARMS_DIFFS=str(diffs),
                       REVIEW_ARMS_BUNDLE="fixture-bifrost", REVIEW_ARMS_REPO_DIR="bifrost-tree")
        return scripts, data, env, known

    def run_cli(self, scripts, env, name, *args):
        return subprocess.run([sys.executable, "-B", "-c", RUN, str(scripts / name), *args],
                              env=env, cwd=scripts, capture_output=True, text=True, timeout=30)

    def test_writer_and_carrier_use_one_selected_historical_source(self):
        for custom in (False, True):
            for cli in ("brief_driver.py", "driver.py"):
                with self.subTest(custom=custom, cli=cli), tempfile.TemporaryDirectory(prefix="source-cli-") as tmp:
                    root = Path(tmp)
                    scripts, data, env, known = self.prepare(root, custom)
                    probe = subprocess.run([sys.executable, "-B", "-c",
                        "import driver,json; print(json.dumps({'ids':sorted(driver.CORPUS),"
                        "'known':str(driver.CORPUS_PATH),'ordinary':str(driver.DIFFS_PATH)}))"],
                        env=env, cwd=scripts, capture_output=True, text=True)
                    self.assertEqual(probe.returncode, 0, probe.stderr)
                    selected = json.loads(probe.stdout)
                    self.assertEqual(selected["ids"], ["known", "ordinary"])
                    self.assertTrue(Path(selected["known"]).is_absolute())
                    self.assertTrue(Path(selected["ordinary"]).is_absolute())
                    if cli == "driver.py":
                        jobs = root / "jobs.json"
                        jobs.write_text('[{"bug":"known","arm":"A"}]')
                        result = self.run_cli(scripts, env, cli, "--jobs", str(jobs))
                        self.assertEqual(result.returncode, 1, result.stderr)
                        self.assertIn("offline boundary stop", result.stdout)
                    else:
                        result = self.run_cli(scripts, env, cli, "known", "--profile", "codex4")
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertTrue((data / "briefs/blind/known/done.json").exists())
                    calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
                    launch = next(c for c in calls if c[0] == "new")
                    self.assertEqual(launch[launch.index("--bundle") + 1], "fixture-bifrost" if custom else "mjolnir")
                    self.assertEqual(launch[launch.index("--at") + 1], known["intro_commit"])
                    self.assertEqual(launch[launch.index("--base") + 1], known["base_commit"])
                    for flag in ("--no-later-history", "--no-github-credentials", "--no-project-memory"):
                        self.assertIn(flag, launch)
                    placements = [json.loads(line) for line in (root / "container-cwds.jsonl").read_text().splitlines()]
                    self.assertEqual({p["cwd"] for p in placements},
                                     {"/workspace/fixture-session/" + ("bifrost-tree" if custom else "mjolnir")})
                    prompt = (root / "sent-prompt.txt").read_text()
                    self.assertIn("Historical intent", prompt)
                    if cli == "brief_driver.py":
                        expected_prompt = (SCRIPTS / "brief/WRITER.md").read_text().replace(
                            "{base}", known["base_commit"]).replace("{message}", "Historical intent")
                        self.assertEqual(prompt, expected_prompt)
                    self.assertEqual(launch[launch.index("--target") + 1], "podman")
                    if cli == "driver.py":
                        self.assertEqual(launch.count("gpt-6.1-terra:high"), 3)
                        self.assertTrue(any(c[:2] == ["review", "start"] for c in calls))

    def test_verifier_uploads_only_unit_inputs_and_names_bundle_without_credentials(self):
        for bundle in (None, "fixture-bifrost"):
            for kind in ("task", "judge"):
                with self.subTest(bundle=bundle, kind=kind), tempfile.TemporaryDirectory(prefix="source-verify-") as tmp:
                    root = Path(tmp)
                    scripts, data, env, _ = self.prepare(root, False)
                    unit = data / "checks" / "unit" if kind == "task" else data / "checks"
                    unit.mkdir(parents=True)
                    (unit / "SPEC.md").write_text("Isolated specification")
                    if kind == "task":
                        (unit / "INSTRUCTIONS.md").write_text("Write labels")
                        (unit / "batch.json").write_text("[]")
                    else:
                        (unit / "batch-0.json").write_text('[{"outputs":[{"output_id":"o1"}]}]')
                    # Hard-won: be74a5cb9: parallel judges raced on the fake's single state file.
                    args = ["checks", "--kind", kind, "--profiles", "codex4", "--model", "gpt-6.1-sol",
                            "--concurrency", "1"]
                    if kind == "task": args += ["--output", "label-out.json"]
                    if bundle: args += ["--bundle", bundle, "--repo-commit", "pinned-history"]
                    result = self.run_cli(scripts, env, "verify_driver.py", *args)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
                    launches = [c for c in calls if c[0] == "new"]
                    self.assertEqual(len(launches), 1 if kind == "task" else 2)
                    for launch in launches:
                        self.assertIn("--no-github-credentials", launch)
                        self.assertIn("--no-project-memory", launch)
                    uploads = {c[c.index("--path") + 1] for c in calls if c[0] == "put-file"}
                    expected = ({"review-input/unit/INSTRUCTIONS.md", "review-input/unit/SPEC.md",
                                 "review-input/unit/batch.json"} if kind == "task" else
                                {"review-input/SPEC.md", "review-input/batch-0.json"})
                    self.assertEqual(uploads, expected)
                    prompts = list((data / "checks/prompts").glob("*.txt"))
                    for prompt in prompts:
                        text = prompt.read_text()
                        self.assertIn(f"The {bundle or 'mjolnir'} checkout with full history", text)
                        self.assertEqual("git checkout -q --detach pinned-history" in text, bool(bundle))
                    self.assertEqual(sum(c[0] == "suspend" for c in calls), len(launches))
                    state = json.loads((root / "state.json").read_text())
                    self.assertEqual(state["cfg"], {"model": "gpt-6.1-sol", "effort": "high"})
                    self.assertEqual(state["profile"], "codex4")


if __name__ == "__main__":
    unittest.main()
