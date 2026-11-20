import contextlib
import copy
import io
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import pilot_judge as j
import pilot_readout as r


def fixture(n_diffs=1, replicate_first=True):
    manifest = {"version": 1, "seed": 20261120, "plan_commit": "fixture",
                "inputs": {"labels_sha256": "fixture"}, "models": {"judge": "fable/high"},
                "page_rows": 50, "diffs": [], "jobs": []}
    labels, defects = [], {}
    for index in range(n_diffs):
        key, bug = f"repo/d{index}", f"d{index}"
        reps = {arm: 2 if index == 0 and replicate_first and arm in {"G", "R"} else 1
                for arm in ("G", "R", "R-loc")}
        manifest["diffs"].append({"key": key, "id": bug, "repo": "repo", "intro": "intro",
                                  "base": "base", "label_line": index + 1,
                                  "replicates": reps})
        defects[key] = [{"id": bug, **dict.fromkeys(j.DEFECT_FIELDS, "known behavior")}]
        labels.append({"repo": "repo", "id": bug, "labeller": "primary",
                       "defect_check": "confirmed", "relation_bug": True,
                       "decisive_sets": [{"facts": [
                           {"claim": "inside fact", "sites": ["in.rs:1"], "in_diff": True},
                           {"claim": "outside one", "sites": ["out.rs:1", "out.rs:2"], "in_diff": False},
                           {"claim": "outside two", "sites": ["out.rs:3"], "in_diff": False},
                       ]}]})
        for arm, count in {"N": 1, **reps}.items():
            for rep in range(1, count + 1):
                for slot in ([1, 2] if arm == "N" else [None, 1, 2]):
                    kind = "writer" if slot is None else "reviewer"
                    role = "writer" if slot is None else f"reviewer-{slot}"
                    manifest["jobs"].append({"job": f"{key}/{arm}/r{rep}/{role}", "diff": key,
                                             "arm": arm, "replicate": rep, "kind": kind, "slot": slot})
    snapshot = "".join(json.dumps(label) + "\n" for label in labels).encode()
    manifest["inputs"]["labels_snapshot_sha256"] = j.sha(snapshot)
    return manifest, snapshot, defects


def setup(root, manifest, snapshot_bytes, defects):
    manifest_path = root / "pilot/manifest.json"
    j.save(manifest_path, manifest)
    manifest_path.with_name("labels-snapshot.jsonl").write_bytes(snapshot_bytes)
    snapshot = j.load_snapshot(manifest_path, manifest)
    for n, job in enumerate(manifest["jobs"]):
        path = root / j.artifact(job)
        path.parent.mkdir(parents=True, exist_ok=True)
        kind = job["kind"]
        text = f"review text {n}\n" if kind == "reviewer" else f"brief text {n}\r\n"
        path.write_bytes(text.encode())
        field = "brief_sha256" if kind == "writer" else "output_sha256"
        j.save(path.parent / "done.json", {field: j.sha(text.encode())})
        j.save(path.parent / "usage.json", {})
        if kind == "writer" and job["arm"] in {"R", "R-loc"}:
            event = {"status": "ok", "page": 2, "payload": {"rows": [
                {"sites": [{"path": "out.rs", "start_line": 2, "end_line": 2}]},
                {"sites": [{"path": "out.rs", "start_line": 3, "end_line": 3}]},
            ]}}
            (path.parent / "queries.jsonl").write_text(json.dumps(event) + "\n")
    return j.prepare(root, manifest, snapshot, defects)


def blank_judgments(key):
    return {
        oid: ({"catch": False, "recovered_facts": [], "matched_defects": []}
              if entry["kind"] == "reviewer" else
              {"contains_decisive": False, "contained_facts": [], "states_defect": False})
        for oid, entry in key["outputs"].items()
    }


def caught(key, judgments, diff, arm, *, rep=None, slots=(1, 2)):
    for oid, job in key["outputs"].items():
        if (job["diff"] == diff and job["arm"] == arm and job["slot"] in slots
                and (rep is None or job["replicate"] == rep)):
            judgments[oid] = {"catch": True, "recovered_facts": ["1/2"], "matched_defects": job["defect_ids"]}


def raw_verdict(payload, item):
    record = {"output_id": item["output_id"],
              "defects": [{"defect_id": defect["id"], "label": "missed", "matching_finding": None,
                           "rationale": "No match.", "confidence": "high"} for defect in payload["defects"]]}
    if payload["kind"] == "review":
        return {**record, "recovered_facts": [], "finding_count": 0}
    return {**record, "contained_facts": ["1/2"]}


class PilotFixture(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def test_batches_mix_arms_without_metadata_and_keep_exact_text(self):
        manifest, labels, defects = fixture()
        key = setup(self.root, manifest, labels, defects)
        snapshot = j.load_snapshot(self.root / "pilot/manifest.json", manifest)
        self.assertEqual(key, j.prepare(self.root, manifest, snapshot, defects))
        self.assertTrue((self.root / j.KEY_PATH).is_file())
        tasks = list((self.root / j.INPUT_PATH).iterdir())
        self.assertEqual(len(tasks), 2)
        for task in tasks:
            self.assertRegex(task.name, r"^[a-f0-9]{16}$")
            self.assertEqual({p.name for p in task.iterdir()}, {"PILOT.md", "SPEC.md", "INSTRUCTIONS.md", "items.json"})
            payload = j.load(task / "items.json")
            expected_arms = {"N", "G", "R", "R-loc"} if payload["kind"] == "review" else {"G", "R", "R-loc"}
            self.assertEqual({key["outputs"][item["output_id"]]["arm"] for item in payload["items"]}, expected_arms)
            self.assertNotIn("R-loc", json.dumps(payload))
            self.assertNotIn("repo/d0/", json.dumps(payload))
            self.assertNotIn('"arm"', json.dumps(payload))
            if payload["kind"] == "review":
                self.assertEqual([fact["id"] for fact in payload["facts"]], ["1/2", "1/3"])
                self.assertNotIn("brief text", json.dumps(payload))
            for item in payload["items"]:
                self.assertEqual(set(item), {"output_id", "text"})
                self.assertRegex(item["output_id"], r"^[a-f0-9]{16}$")
                entry = key["outputs"][item["output_id"]]
                self.assertEqual(j.sha(item["text"].encode()), entry["text_sha256"])

    def test_collect_requires_exact_coverage_and_hash_bound_bytes(self):
        key = setup(self.root, *fixture())
        batch = next(iter((self.root / j.INPUT_PATH).iterdir()))
        payload = j.load(batch / "items.json")
        data = "".join(json.dumps(raw_verdict(payload, item)) + "\n" for item in payload["items"]).encode()
        proof = batch.parent / "out" / f"task-{batch.name}.json"
        result = {"producer": {"profile": "claude-pinned", "launch_profile": "claude-pinned",
                               "config": {"model": "fable", "effort": "high"}, "output_sha256": j.sha(data),
                               "session_id": "first-pass"}}
        j.save(proof, result)
        (batch / "verdicts.jsonl").write_bytes(data)
        with self.assertRaisesRegex(ValueError, "pinned Fable/high"):
            j.collect(self.root, key, profile="other-profile")
        j.collect(self.root, key, profile="claude-pinned")
        self.assertEqual(len(j.records(self.root, key, "judge")), len(payload["items"]))
        (batch / "verdicts.jsonl").write_bytes(data + b"\n")
        with self.assertRaisesRegex(ValueError, "fetched output"):
            j.collect(self.root, key, profile="claude-pinned")
        (batch / "verdicts.jsonl").write_bytes(data)
        oid = payload["items"][0]["output_id"]
        (self.root / key["outputs"][oid]["artifact"]).write_text("changed")
        with self.assertRaisesRegex(ValueError, "hash-bound text"):
            j.collect(self.root, key, profile="claude-pinned")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            j.validate_payload_verdicts(payload, data + data)
        with self.assertRaisesRegex(ValueError, "missing verdicts"):
            j.validate_payload_verdicts(payload, b"")

    def test_second_pass_contains_every_output_and_no_first_pass_results(self):
        key = setup(self.root, *fixture(3))
        first = self.root / j.INPUT_PATH
        second = self.root / j.HAND_PATH
        self.assertEqual({path.name for path in first.iterdir()}, {path.name for path in second.iterdir()})
        items = []
        for batch in second.iterdir():
            hand_payload = j.load(batch / "items.json")
            judged_payload = j.load(first / batch.name / "items.json")
            hand_items = {row["output_id"]: row["text"] for row in hand_payload.pop("items")}
            judged_items = {row["output_id"]: row["text"] for row in judged_payload.pop("items")}
            self.assertEqual(hand_items, judged_items)
            self.assertEqual(hand_payload, judged_payload)
            self.assertFalse((batch / "verdicts.jsonl").exists())
            items.extend(hand_items)
        self.assertCountEqual(items, key["outputs"].keys())

    def test_readout_uses_only_complete_diffs_and_reports_corrections(self):
        key = setup(self.root, *fixture(3))
        judged = blank_judgments(key)
        caught(key, judged, "repo/d0", "R", rep=2)
        caught(key, judged, "repo/d0", "R-loc", slots=(1,))
        unused = next(oid for oid, job in key["outputs"].items() if job["diff"] == "repo/d2")
        del judged[unused]
        checked = copy.deepcopy({oid: v for oid, v in judged.items() if key["outputs"][oid]["diff"] != "repo/d2"})
        caught(key, checked, "repo/d1", "G", slots=(1,))
        for entry in key["outputs"].values():
            (self.root / entry["artifact"]).unlink()
        for directory, values in (("judge", judged), ("hand-check", checked)):
            for oid, value in values.items():
                j.save(self.root / directory / f"{oid}.json", value)
        # The isolated study directory contains only the committed-style snapshot.
        self.assertFalse((self.root / "labels" / "development-primary.jsonl").exists())
        manifest_path = self.root / "pilot/manifest.json"
        readout_path = self.root / "readout.json"
        args = ["pilot_readout.py", "--root", str(self.root), "--manifest", str(manifest_path),
                "--output", str(readout_path)]
        with patch.object(sys, "argv", args):
            r.main()
            first = readout_path.read_bytes()
            r.main()
        self.assertEqual(first, readout_path.read_bytes())
        result = j.load(readout_path)
        self.assertEqual(result["unfinished_diffs"], ["repo/d2"])
        self.assertEqual(result["as_judged"]["arms"]["R"]["catch_rate"], 0.25)
        query = result["as_judged"]["arms"]["R"]["querying"]
        self.assertEqual(query["site_coverage_counts"], {"some": 3, "all": 3})
        self.assertEqual(result["as_judged"]["brief_level_catch_variance"]["R"]["brief_variance"], 0.5)
        contrast = result["paired_contrasts"]["R - G"]["catch"]
        self.assertEqual(contrast["as_judged"]["by_diff"], {"repo/d0": 0.5, "repo/d1": 0})
        self.assertEqual(contrast["hand_checked"]["mean"], 0)
        agreement = result["judge_hand_check_agreement"]
        self.assertEqual(agreement["reviewer"]["catch"]["decisions"], 20)
        self.assertEqual(agreement["reviewer"]["catch"]["agreements"], 19)
        self.assertEqual(agreement["reviewer"]["recovered_facts"]["rate"], 39 / 40)
        self.assertEqual(agreement["writer"]["whole_record"]["rate"], 1)

    def test_missing_hand_check_keeps_a_diff_out_of_both_contrasts(self):
        key = setup(self.root, *fixture(2))
        judged = blank_judgments(key)
        checked = copy.deepcopy(judged)
        absent = next(oid for oid, entry in key["outputs"].items() if entry["diff"] == "repo/d1")
        del checked[absent]
        snapshot = j.load_snapshot(self.root / "pilot/manifest.json", key["manifest"])
        result = r.readout(key["manifest"], {"key": key, "judge": judged, "hand_check": checked},
                           r.load_deliveries(self.root / "runs", key), snapshot)
        self.assertEqual(result["judge_complete_diffs"], ["repo/d0", "repo/d1"])
        self.assertEqual(result["complete_diffs"], ["repo/d0"])
        self.assertEqual(result["unfinished_diffs"], ["repo/d1"])
        self.assertEqual(result["paired_contrasts"]["R - G"]["catch"]["as_judged"]["by_diff"], {"repo/d0": 0})

    def test_snapshot_mismatch_stops_preparation_and_readout_at_startup(self):
        setup(self.root, *fixture())
        manifest_path = self.root / "pilot/manifest.json"
        snapshot = manifest_path.with_name("labels-snapshot.jsonl")
        snapshot.write_bytes(snapshot.read_bytes() + b"\n")
        options = ["--root", str(self.root), "--manifest", str(manifest_path)]
        for module, args in ((j, ["pilot_judge.py", "prepare"]), (r, ["pilot_readout.py"])):
            with self.subTest(module=module.__name__), patch.object(sys, "argv", args + options):
                with self.assertRaisesRegex(ValueError, r"labels-snapshot\.jsonl: SHA-256 mismatch"):
                    module.main()

    def test_verifier_launch_and_resume_use_pinned_profile_without_ack(self):
        key = setup(self.root, *fixture())
        calls = []
        states = {}
        pending_uploads = set()
        profile = "claude-pinned"
        manifest_tasks = self.root / j.INPUT_PATH
        task_by_session = {}
        next_session = 0

        def fake_process(args, **_kw):
            nonlocal next_session
            if args[0] == "git":
                return subprocess.CompletedProcess(args, 0, stdout=str(j.HERE))
            calls.append(args)
            verb = args[1]
            sid = args[args.index("--session") + 1] if "--session" in args else None
            tag = task_by_session.get(sid)
            value = {}
            if verb == "workspaces":
                value = {"workspace": {"id": "fixture-workspace"}}
            elif verb == "new":
                self.assertEqual(args[args.index("--profile") + 1], profile)
                self.assertEqual(args[args.index("--effort") + 1], "high")
                self.assertNotIn("--model", args)
                tag = args[args.index("--title") + 1].split()[-1]
                next_session += 1
                new_id = f"s-{next_session}"
                task_by_session[new_id] = tag
                value = {"session_id": new_id}
                states[tag] = {"lifecycle": "live", "is_idle": True, "profile_id": profile,
                               "config_options": []}
            elif verb == "sessions":
                value = states[tag]
            elif verb == "set-config":
                states[tag]["config_options"].append({"key": args[args.index("--key") + 1],
                                                     "current": args[args.index("--value") + 1]})
            elif verb == "put-file":
                self.assertTrue(Path(args[-1]).is_relative_to(manifest_tasks / tag))
                pending_uploads.add((tag, Path(args[-1]).name))
            elif verb == "message":
                self.assertIn((tag, "items.json"), pending_uploads)
                self.assertIn({"key": "model", "current": "fable"}, states[tag]["config_options"])
                states[tag]["last_turn_outcome"] = {"outcome": {"stop_reason": "EndTurn"}}
            elif verb == "fetch":
                payload = j.load(manifest_tasks / tag / "items.json")
                data = "".join(json.dumps(raw_verdict(payload, item)) + "\n" for item in payload["items"])
                return subprocess.CompletedProcess(args, 0, stdout=data.encode())
            elif verb == "suspend":
                self.assertNotIn("--acknowledge-unpublished-work", args)
                states[tag]["lifecycle"] = "suspended"
            else:
                self.assertEqual(verb, "usage")
            result = json.dumps(value).encode()
            return subprocess.CompletedProcess(args, 0, stdout=result, stderr=b"")

        args = [str(j.HERE / "verify_driver.py"), str(manifest_tasks), "--kind", "task", "--text-only",
                "--output", "verdicts.jsonl", "--profiles", profile, "--model", "fable",
                "--quota-scope", "fable", "--concurrency", "1"]
        with (patch.object(sys, "argv", args), patch.dict(os.environ, {"REVIEW_ARMS_DIR": str(self.root),
                "REVIEW_ARMS_INSTANCE": "default"}), patch.dict(sys.modules,
                {"driver": SimpleNamespace(quota_hold=lambda *a, **k: None)}),
              patch("subprocess.run", side_effect=fake_process), patch("time.sleep"),
              contextlib.redirect_stdout(io.StringIO()) as output):
            for _ in range(2):
                with self.assertRaises(SystemExit) as finished:
                    runpy.run_path(args[0], run_name="__main__")
                self.assertEqual(finished.exception.code, 0, output.getvalue())
            j.collect(self.root, key, profile=profile)
            manifest_tasks = self.root / j.HAND_PATH
            args[1] = str(manifest_tasks)
            with self.assertRaises(SystemExit) as finished:
                runpy.run_path(args[0], run_name="__main__")
            self.assertEqual(finished.exception.code, 0, output.getvalue())
            j.collect(self.root, key, hand=True, profile=profile)
        self.assertEqual(sum(call[1] == "new" for call in calls), 4)
        self.assertEqual(sum(call[1] == "message" for call in calls), 4)
        self.assertEqual(j.records(self.root, key, "judge"), j.records(self.root, key, "hand-check"))
        self.assertEqual(len(j.records(self.root, key, "judge")), len(key["outputs"]))


if __name__ == "__main__":
    unittest.main()
