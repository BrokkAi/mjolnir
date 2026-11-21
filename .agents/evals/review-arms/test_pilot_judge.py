import contextlib
from concurrent.futures import ThreadPoolExecutor
import copy
import io
import hashlib
import json
import os
from pathlib import Path
import runpy
from shutil import copytree
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Event
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import driver as shared_driver
import pilot_judge as j
from pilot_lock import pilot_lock
import pilot_readout as r
import pilot_run


def fixture(n_diffs=1, replicate_first=True):
    manifest = {"version": 1, "seed": 20261120, "plan_commit": "fixture",
                "inputs": {"labels_sha256": "fixture"}, "models": {"judge": "sonnet/high"},
                "model_versions": {"claude": "Sonnet 5.5", "claude_provider_id": "claude-sonnet-5-5"},
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
        inputs = {"manifest_sha256": j.sha(manifest_path.read_bytes())}
        if kind == "reviewer" and job["arm"] != "N":
            writer = j.load(path.parents[2] / "writer/done.json")
            inputs.update(writer_session_id=writer["session_id"], brief_sha256=writer["brief_sha256"])
        j.save(path.parent / "done.json", {field: j.sha(text.encode()), "session_id": f"job-{n}", "inputs": inputs})
        j.save(path.parent / "usage.json", {})
        if kind == "writer" and job["arm"] in {"R", "R-loc"}:
            session_id = f"job-{n}"
            event = {"session": session_id, "status": "ok", "delivered": True, "page": 2, "latency_ms": 3, "payload": {"rows": [
                {"sites": [{"path": "out.rs", "start_line": 2, "end_line": 2}]},
                {"sites": [{"path": "out.rs", "start_line": 3, "end_line": 3}]},
            ]}}
            (path.parent / f"queries-{session_id}.jsonl").write_text(json.dumps(event) + "\n")
    return j.prepare(root, manifest, snapshot, defects, manifest_sha256=j.sha(manifest_path.read_bytes()))


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
    return {**record, "contained_facts": ["1/2"],
            "evidence": [{"claim": "contained_fact:1/2", "quote": item["text"]}]}


def usage_record(command, model="claude-sonnet-5-5", turn=1):
    return {"latest_seq": turn, "turns": [{"command_id": command, "accepted_ordinal": turn,
                       "usage": {"provider_details": {"model_usage": {model: {}}}}}],
            "turn_selections": {command: {"model": "sonnet", "effort": "high"}}}


def captured_receipt(batch, data, model="claude-sonnet-5-5"):
    attempt = batch.parent / "sessions" / batch.name / "attempt"
    producer = {"sealed": True, "profile": "claude-pinned", "launch_profile": "claude-pinned",
                "config": {"model": "sonnet", "effort": "high"}, "output_sha256": j.sha(data),
                "session_id": "first-pass", "provider_model": model, "command": "cmd", "turn": 1}
    j.save(attempt / "launch.json", {"session_id": "first-pass"})
    j.save(attempt / "usage.json", usage_record("cmd", model))
    j.save(attempt / "audit.json", {"passed": True})
    j.save(attempt / "transcript.json", [{"items": [{"role": "message", "text": "done"}]}])
    (attempt / "verdicts.jsonl").write_bytes(data)
    j.save(attempt / "verification.json", j.verification_record([]))
    receipt = {"producer": producer,
               "evidence_sha256": {name: j.sha((attempt / name).read_bytes()) for name in j.EVIDENCE_FILES}}
    j.save(batch.parent / "out" / f"task-{batch.name}.json", receipt)
    (batch / "verdicts.jsonl").write_bytes(data)
    return attempt, receipt


class PilotFixture(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    # Hard-won: 7088aaa: an isolation scan could pair a retired session with its replacement's audit.
    def test_retirement_waits_for_judge_isolation_snapshot(self):
        manifest, labels, defects = fixture()
        initial = setup(self.root, manifest, labels, defects)
        manifest_path = self.root / "pilot/manifest.json"
        snapshot_labels = j.load_snapshot(manifest_path, manifest)
        oid, entry = next((oid, row) for oid, row in initial["outputs"].items()
                          if row["kind"] == "writer" and row["arm"] == "G")
        brief = self.root / entry["artifact"]
        original = brief.read_bytes().decode()
        sid = entry["session_id"]
        current = brief.parent
        retired = current.with_name("writer.superseded-1")
        launch = current / "launch.json"
        old_hit = {"kind": "github-or-web", "snippet": "original", "seq": 1}
        new_hit = {**old_hit, "snippet": "replacement"}
        j.save(launch, {"session_id": sid})
        j.save(current / "audit.json", {"passed": False, "hits": [old_hit]})
        read_launch, continue_snapshot, noticed = Event(), Event(), Event()
        original_load = j.load

        class LockNotices(io.StringIO):
            def write(self, message):
                written = super().write(message)
                if message.endswith("\n"):
                    noticed.set()
                return written

        def load_and_pause(path):
            result = original_load(path)
            if path == current / "done.json":
                read_launch.set()
                self.assertTrue(continue_snapshot.wait(5))
            return result

        def retire():
            with pilot_lock(self.root):
                pilot_run.archive(current)
                j.save(launch, {"session_id": "replacement"})
                j.save(current / "audit.json", {"passed": False, "hits": [new_hit]})
                brief.write_text("replacement brief")

        notices = LockNotices()
        with (patch.object(j, "load", load_and_pause),
              contextlib.redirect_stderr(notices),
              ThreadPoolExecutor(2) as workers):
            snapshot = workers.submit(j.prepare, self.root, manifest, snapshot_labels, defects)
            try:
                self.assertTrue(read_launch.wait(5))
                retirement = workers.submit(retire)
                self.assertTrue(noticed.wait(5))
                self.assertEqual(notices.getvalue().splitlines(), [
                    f"waiting for the pilot run on {self.root} (PID {os.getpid()}) to release pilot.lock "
                    "(pilot runs exit at quota holds or completion)",
                ])
                self.assertFalse(retirement.done())
            finally:
                continue_snapshot.set()
            prepared = snapshot.result(timeout=5)
            retirement.result(timeout=5)
        isolated = prepared["isolation"]
        self.assertEqual(isolated["void_reasons"]["writer"], {sid: [old_hit]})
        self.assertEqual([entry["session_id"] for entry in isolated["attempts"]], [sid])
        self.assertEqual(prepared["outputs"][oid], entry)
        items = j.load(self.root / j.INPUT_PATH / entry["batch"] / "items.json")["items"]
        self.assertEqual(next(item["text"] for item in items if item["output_id"] == oid), original)
        self.assertEqual(j.load(retired / "launch.json")["session_id"], sid)
        self.assertEqual(j.load(launch)["session_id"], "replacement")
        self.assertEqual((self.root / "pilot.lock").read_bytes(), b"")

    # Hard-won: 7088aaa: a later judge command could silently switch its frozen egress evidence.
    def test_only_a_different_explicit_egress_source_replaces_the_frozen_copy(self):
        manifest, _, defects = fixture()
        arguments = (self.root, manifest, {}, defects, [])
        custom = self.root / "custom-egress.jsonl"
        with self.assertRaises(FileNotFoundError):
            j.prepare(*arguments, egress_log=custom)
        uncaptured = j.prepare(*arguments)
        default = self.root / "egress.jsonl"
        self.assertEqual(uncaptured["egress_source"],
                         {"path": str(default), "sha256": None, "status": "uncaptured"})
        self.assertEqual(uncaptured["isolation"]["egress_source"], "missing")
        self.assertNotIn("egress_measurements", uncaptured["isolation"])
        original = {"container": "first"}
        raw = (json.dumps(original) + "\n").encode()
        default.write_bytes(raw)
        initial = j.prepare(*arguments)
        self.assertEqual(initial["egress_source"], {"path": str(default), "sha256": j.sha(raw)})
        self.assertNotIn("egress_source_history", initial)

        default.write_text('{"container":"changed"}\n')
        for flag in (None, default):
            later = j.prepare(*arguments, egress_log=flag)
            self.assertEqual(later["egress_source"], initial["egress_source"])
            self.assertEqual(later["isolation"]["egress_measurements"], [original])
            self.assertNotIn("egress_source_history", later)

        default.unlink()
        self.assertEqual(j.prepare(*arguments), initial)
        for flag in (default, custom):
            with self.assertRaises(FileNotFoundError):
                j.prepare(*arguments, egress_log=flag)
        self.assertEqual(j.load(self.root / j.KEY_PATH), initial)

        custom.write_text('{"container":"new"}\n')
        updated = j.prepare(*arguments, egress_log=custom)
        self.assertEqual(updated["egress_source"], {"path": str(custom), "sha256": j.sha(custom.read_bytes())})
        self.assertEqual(updated["isolation"]["egress_measurements"], [{"container": "new"}])
        self.assertEqual(updated["egress_source_history"], [initial["egress_source"]])
        self.assertEqual(j.prepare(*arguments), updated)

    # Hard-won: 49e16b8: a retained writer's reconnect polluted its replacement's exposure.
    def test_retained_writer_log_record_is_not_credited_to_replacement(self):
        key = setup(self.root, *fixture())
        writer = next(entry for entry in key["outputs"].values()
                      if entry["kind"] == "writer" and entry["arm"] == "R")
        path = self.root / "runs" / j.artifact(writer).relative_to("runs").parent / f"queries-{writer['session_id']}.jsonl"
        record = {"session": "retained-session", "delivered": True, "status": "ok", "payload": {"rows": []}}
        with path.open("a") as stream:
            stream.write(json.dumps(record) + "\n")
        # The stray record was already in the log when the judge froze it; a later append
        # instead fails the frozen-hash check, which is the visible failure we want.
        writer["query_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
        deliveries = r.load_deliveries(self.root / "runs", key)[writer["job"]]
        self.assertEqual(len(deliveries), 1)
        self.assertEqual(deliveries[0]["session"], writer["session_id"])

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

    # Hard-won: a19cfbf: changed rubric text wedged retries against a stale batch.
    def test_changed_rubric_retires_old_batch_and_prepares_new_input(self):
        manifest, labels, defects = fixture()
        key = setup(self.root, manifest, labels, defects)
        old_batch = next(iter(key["outputs"].values()))["batch"]
        judge = j.INPUT_PATH / "sessions" / old_batch / "attempt"
        j.save(self.root / judge / "launch.json", {"session_id": "retired-judge"})
        rubric = j.HERE / "judge/PILOT.md"
        original = rubric.read_bytes()
        try:
            rubric.write_bytes(original + b"\nUpdated scoring rule.\n")
            snapshot = j.load_snapshot(self.root / "pilot/manifest.json", manifest)
            updated = j.prepare(self.root, manifest, snapshot, defects)
            self.assertNotEqual(next(iter(updated["outputs"].values()))["batch"], old_batch)
            retired = j.KEY_PATH.parent / "retired" / old_batch
            self.assertTrue((self.root / retired / "key.json").is_file())
            self.assertEqual(updated["isolation"]["attempts"], [
                {"kind": "judge", "session_id": "retired-judge", "path": (retired / judge).as_posix()},
            ])
            self.assertTrue((self.root / retired / judge / "launch.json").is_file())
            for batch in {entry["batch"] for entry in updated["outputs"].values()}:
                payload = j.load(self.root / j.INPUT_PATH / batch / "items.json")
                j.write_task(self.root / j.INPUT_PATH / batch, payload)
        finally:
            rubric.write_bytes(original)

    def test_collect_requires_exact_coverage_and_hash_bound_bytes(self):
        key = setup(self.root, *fixture())
        batch = next(iter((self.root / j.INPUT_PATH).iterdir()))
        payload = j.load(batch / "items.json")
        data = "".join(json.dumps(raw_verdict(payload, item)) + "\n" for item in payload["items"]).encode()
        attempt, _ = captured_receipt(batch, data)
        transcript = attempt / "transcript.json"
        original = transcript.read_bytes()
        transcript.write_bytes(original + b"\n")
        with self.assertRaisesRegex(ValueError, "transcript.json: evidence differs"):
            j.collect(self.root, key, profile="claude-pinned")
        transcript.write_bytes(original)
        with self.assertRaisesRegex(ValueError, "pinned Sonnet/high"):
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
        with self.assertRaisesRegex(ValueError, "duplicate-verdict-id"):
            j.validate_payload_verdicts(payload, data + data)
        with self.assertRaisesRegex(ValueError, "missing-verdict"):
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
        manifest, labels, defects = fixture(3)
        egress = {"time": "fixture", "container": "engine", "reachable": {"github.com": True}, "enforced": False}
        egress_log = self.root / "egress.jsonl"
        egress_log.write_text(json.dumps(egress) + "\n")
        key = setup(self.root, manifest, labels, defects)
        judged = blank_judgments(key)
        caught(key, judged, "repo/d0", "R", rep=2)
        caught(key, judged, "repo/d0", "R-loc", slots=(1,))
        unused = next(oid for oid, job in key["outputs"].items() if job["diff"] == "repo/d2")
        del judged[unused]
        checked = copy.deepcopy({oid: v for oid, v in judged.items() if key["outputs"][oid]["diff"] != "repo/d2"})
        caught(key, checked, "repo/d1", "G", slots=(1,))
        for directory, values in (("judge", judged), ("hand-check", checked)):
            for oid, value in values.items():
                j.save(self.root / directory / f"{oid}.json", value)
        writer = next(entry for entry in key["outputs"].values() if entry["kind"] == "writer")
        writer_path = (self.root / writer["artifact"]).parent.with_name("writer.superseded-1")
        reviewer = next(entry for entry in key["outputs"].values() if entry["kind"] == "reviewer")
        reviewer_path = (self.root / reviewer["artifact"]).parent
        engine = self.root / "engine-cache" / "commit" / "retired" / "engine"
        judge = self.root / j.KEY_PATH.parent / "retired" / "batch" / j.HAND_PATH / "sessions" / "batch" / "void-judge"
        screened = {"kind": "interpreter-screened", "seq": 5}
        rejected = {"kind": "github-or-web", "seq": 6}
        edits = [{"kind": "in-place-edit", "seq": 5, "edit_index": i} for i in range(2)]
        isolation_files = []
        for kind, path, launch in (("writer", writer_path, "launch.json"),
                                   ("reviewer", reviewer_path, "launch.json"),
                                   ("engine", engine, "session.json"), ("judge", judge, "launch.json")):
            j.save(path / launch, {"session_id": kind})
            hits = [screened] if kind == "reviewer" else [screened, rejected]
            if kind == "writer":
                hits += edits
            j.save(path / "audit.json", {"passed": kind == "reviewer", "hits": hits})
            isolation_files.extend((path / launch, path / "audit.json"))
        j.save(engine / "turn-3-audit.json", {"passed": False, "hits": [screened, rejected]})
        isolation_files.append(engine / "turn-3-audit.json")
        j.save(writer_path / "turn-1-audit.json", {"passed": True, "hits": edits})
        isolation_files.append(writer_path / "turn-1-audit.json")
        isolation_files.append(egress_log)
        # The isolated study directory contains only the committed-style snapshot.
        self.assertFalse((self.root / "labels" / "development-primary.jsonl").exists())
        manifest_path = self.root / "pilot/manifest.json"
        key = j.prepare(self.root, manifest, j.load_snapshot(manifest_path, manifest), defects,
                        manifest_sha256=key["manifest_sha256"])
        readout_path = self.root / "readout.json"
        args = ["pilot_readout.py", "--root", str(self.root), "--manifest", str(manifest_path),
                "--output", str(readout_path)]
        with patch.object(sys, "argv", args):
            r.main()
            first = readout_path.read_bytes()
            r.main()
        self.assertEqual(first, readout_path.read_bytes())
        result = j.load(readout_path)
        isolation = result["isolation"]
        self.assertEqual(isolation["egress_measurements"], [egress])
        self.assertEqual(isolation["void_sessions"], {"writer": 1, "reviewer": 0, "engine": 1, "judge": 1})
        self.assertEqual(isolation["interpreter_screened"], {"writer": 1, "reviewer": 1, "engine": 1, "judge": 1})
        self.assertEqual(isolation["void_reasons"]["engine"], {"engine": [rejected]})
        self.assertEqual(isolation["in_place_edits"], {"writer": 2, "reviewer": 0, "engine": 0, "judge": 0})
        self.assertEqual(isolation["void_reasons"]["writer"], {"writer": [rejected]})
        self.assertEqual({attempt["session_id"] for attempt in isolation["attempts"]},
                         {"writer", "reviewer", "engine", "judge"})
        self.assertEqual(isolation, key["isolation"])
        self.assertIn("does not restrict", isolation["enforcement_gap"])
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
        self.assertEqual(result["stale_diffs"], [])
        alternate_runs = self.root / "alternate" / "runs"
        copytree(self.root / "runs", alternate_runs)

        # Supersede one G writer without changing the bytes. Its old reviews
        # still refer to the old session. Also change one N review's text.
        g = next(e for e in key["outputs"].values()
                 if e["diff"] == "repo/d0" and e["arm"] == "G" and e["kind"] == "writer")
        done_path = (self.root / g["artifact"]).parent / "done.json"
        j.save(done_path, {**j.load(done_path), "session_id": "replacement"})
        n = next(e for e in key["outputs"].values() if e["diff"] == "repo/d1" and e["arm"] == "N")
        (self.root / n["artifact"]).write_text("changed N review")
        with patch.object(sys, "argv", args):
            r.main()
        stale = j.load(readout_path)
        self.assertEqual(stale["stale_diffs"], ["repo/d0", "repo/d1"])
        self.assertEqual(stale["complete_diffs"], [])
        self.assertEqual(stale["unfinished_diffs"], ["repo/d2"])
        self.assertEqual(stale["paired_contrasts"]["R - G"]["catch"]["hand_checked"]["by_diff"], {})

        late = writer_path.with_name("writer.superseded-2")
        j.save(late / "launch.json", {"session_id": "later-writer"})
        j.save(late / "audit.json", {"passed": False, "hits": [screened, rejected]})
        with egress_log.open("a") as stream:
            stream.write(json.dumps({**egress, "container": "later"}) + "\n")
        archive_args = [*args, "--archived"]
        with patch.object(sys, "argv", archive_args):
            r.main()
            self.assertEqual(readout_path.read_bytes(), first)
        with patch.object(sys, "argv", args):
            r.main()
        self.assertIn("later-writer", j.load(readout_path)["isolation"]["void_reasons"]["writer"])

        alternate_late = alternate_runs / late.relative_to(self.root / "runs")
        j.save(alternate_late / "launch.json", {"session_id": "alternate-later-writer"})
        j.save(alternate_late / "audit.json", {"passed": False, "hits": [screened, rejected]})
        with patch.object(sys, "argv", [*args, "--runs", str(alternate_runs)]):
            r.main()
        live = j.load(readout_path)
        self.assertEqual(live["stale_diffs"], [])
        self.assertEqual(live["complete_diffs"], result["complete_diffs"])
        self.assertEqual(live["isolation"]["void_reasons"]["writer"],
                         {"writer": [rejected], "alternate-later-writer": [rejected]})
        self.assertEqual(live["isolation"]["interpreter_screened"]["writer"], 2)
        self.assertEqual(live["isolation"]["egress_source"], "missing")
        self.assertNotIn("egress_measurements", live["isolation"])
        with patch.object(sys, "argv", args):
            egress_log.write_bytes(b"changed bytes")
            r.main()
            self.assertEqual(j.load(readout_path)["isolation"]["egress_source"], "missing")
            egress_log.unlink()
            r.main()
            self.assertEqual(j.load(readout_path)["isolation"]["egress_source"], "missing")
        explicit = self.root / "missing-explicit.jsonl"
        with patch.object(sys, "argv", [*args, "--egress-log", str(explicit)]):
            r.main()
        self.assertEqual(j.load(readout_path)["isolation"]["egress_source"], "missing")
        self.assertNotIn("egress_measurements", j.load(readout_path)["isolation"])
        isolation_files.remove(egress_log)

        for entry in key["outputs"].values():
            artifact = self.root / entry["artifact"]
            artifact.unlink()
            (artifact.parent / "done.json").unlink()
            (artifact.parent / "usage.json").unlink()
        for path in [*isolation_files, late / "launch.json", late / "audit.json"]:
            path.unlink()
        with patch.object(sys, "argv", archive_args):
            r.main()
            self.assertEqual(readout_path.read_bytes(), first)

            rql = next(e for e in key["outputs"].values() if e.get("query_sha256"))
            delivery = (self.root / rql["artifact"]).parent / f"queries-{rql['session_id']}.jsonl"
            original_delivery = delivery.read_bytes()
            delivery.write_bytes(original_delivery + b"\n")
            with self.assertRaisesRegex(ValueError, "delivery log differs"):
                r.main()
            delivery.write_bytes(original_delivery)

            snapshot_path = manifest_path.with_name("labels-snapshot.jsonl")
            original_snapshot = snapshot_path.read_bytes()
            snapshot_path.write_bytes(original_snapshot + b"\n")
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                r.main()
            snapshot_path.write_bytes(original_snapshot)
            manifest_path.write_bytes(manifest_path.read_bytes() + b"\n")
            with self.assertRaisesRegex(ValueError, "manifest SHA-256 differs"):
                r.main()

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

    def test_replaced_writer_stales_previous_reviews_and_archives_both_batches(self):
        manifest, labels, defects = fixture()
        initial = setup(self.root, manifest, labels, defects)
        arguments = (self.root, manifest, j.load_snapshot(self.root / "pilot/manifest.json", manifest), defects)
        original = initial["outputs"]
        writer = next(job for job in manifest["jobs"] if job["job"] == "repo/d0/R/r1/writer")
        path = self.root / j.artifact(writer)
        done = j.load(path.parent / "done.json")
        old_log = path.parent / f"queries-{done['session_id']}.jsonl"
        done["session_id"] = "new-writer-same-bytes"
        j.save(path.parent / "done.json", done)
        # The replacement writes its own session-bound delivery log.
        records = [json.loads(line) for line in old_log.read_text().splitlines() if line.strip()]
        (path.parent / f"queries-{done['session_id']}.jsonl").write_text(
            "".join(json.dumps({**record, "session": done["session_id"]}) + "\n" for record in records))
        # Same text still makes the earlier reviewer receipt stale.
        old_id, old_entry = next((oid, job) for oid, job in original.items() if job["kind"] == "reviewer")
        j.save(self.root / "judge" / f"{old_id}.json", blank_judgments(initial)[old_id])
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            stale = j.prepare(*arguments)
        self.assertFalse(stale["outputs"])
        self.assertIn('"state": "stale-review"', errors.getvalue())
        retired = self.root / j.KEY_PATH.parent / "retired"
        self.assertTrue((retired / old_entry["batch"] / "judge" / f"{old_id}.json").is_file())
        for slot in (1, 2):
            receipt = path.parents[1] / f"reviewers/{slot}/done.json"
            record = j.load(receipt)
            record["inputs"]["writer_session_id"] = done["session_id"]
            record["session_id"] += "-replacement"
            j.save(receipt, record)
        fresh = j.prepare(*arguments)
        self.assertEqual(len(fresh["outputs"]), len(original))
        self.assertTrue(set(fresh["outputs"]).isdisjoint(original))
        self.assertEqual(len(list(retired.iterdir())), 2)

    def test_replaced_review_preserves_unaffected_brief_batch(self):
        manifest, labels, defects = fixture()
        initial = setup(self.root, manifest, labels, defects)
        reviewer = next(job for job in manifest["jobs"] if job["kind"] == "reviewer")
        path = self.root / j.artifact(reviewer)
        done = j.load(path.parent / "done.json")
        done["session_id"] = "new-reviewer"
        j.save(path.parent / "done.json", done)
        fresh = j.prepare(self.root, manifest, j.load_snapshot(self.root / "pilot/manifest.json", manifest), defects)
        self.assertEqual({oid for oid, job in initial["outputs"].items() if job["kind"] == "writer"},
                         set(initial["outputs"]) & set(fresh["outputs"]))

    def test_collect_rejects_a_different_provider_model(self):
        key = setup(self.root, *fixture())
        batch = next(iter((self.root / j.INPUT_PATH).iterdir()))
        payload = j.load(batch / "items.json")
        data = "".join(json.dumps(raw_verdict(payload, item)) + "\n" for item in payload["items"]).encode()
        captured_receipt(batch, data, "claude-sonnet-5-4")
        with self.assertRaisesRegex(ValueError, "expected provider usage for claude-sonnet-5-5"):
            j.collect(self.root, key)
        self.assertFalse(j.records(self.root, key, "judge"))

    def test_query_outages_do_not_count_as_exposure(self):
        key = setup(self.root, *fixture())
        judged = blank_judgments(key)
        deliveries = r.load_deliveries(self.root / "runs", key)
        for oid, entry in key["outputs"].items():
            if entry["kind"] == "writer" and entry["arm"] == "R" and entry["replicate"] == 2:
                judged[oid].update(contains_decisive=True, contained_facts=["1/1", "1/2", "1/3"])
                deliveries[entry["job"]] = [{"status": "parse_error", "delivered": True, "latency_ms": 2},
                                            {"status": "engine_error", "delivered": True, "latency_ms": 100}]
        manifest = key["manifest"]
        result = r.readout(manifest, {"key": key, "judge": judged, "hand_check": judged},
                           deliveries, j.load_snapshot(self.root / "pilot/manifest.json", manifest))
        stats = result["hand_checked"]["arms"]["R"]["querying"]
        self.assertEqual((stats["queried"], stats["exposed"], stats["queries"], stats["ok_answers"]),
                         (2, 1, 3, 1))
        self.assertEqual((stats["parse_error"], stats["engine_error"]), (1, 1))
        self.assertEqual(stats["latency_ms"]["engine_error"]["mean"], 100)
        self.assertEqual(stats["exposed_only_contains_decisive"], 0)
        self.assertEqual(stats["site_coverage_counts"]["none"], 2)

    def run_fake_judges(self, key, *, failure=None, repair=None):
        profile, pin = "claude-pinned", "a" * 40
        instance = self.root / "instance"
        instance.mkdir()
        (instance / "config.toml").write_text(
            '[bundles.empty]\nprimary_repo = "empty"\n'
            '[[bundles.empty.repositories]]\nid = "empty"\ndestination = "empty"\n')
        probe = self.root / "probe"
        probe.touch(mode=0o700)
        tasks = self.root / j.INPUT_PATH
        sessions, stages, destroyed = {}, {}, []
        bad_audit_sid = None if failure or repair else "session-0"
        model = "claude-sonnet-5-4" if failure == "version" else "claude-sonnet-5-5"

        def launch(bundle, commit, selected_profile, title, *, base, effort):
            self.assertEqual((bundle, commit, selected_profile, base, effort), ("empty", pin, profile, pin, "high"))
            sid = f"session-{len(sessions)}"
            sessions[sid] = {"tag": title.split()[-1], "phase": tasks, "commands": []}
            stages[sid] = []
            return sid

        def state(sid):
            return {"at": pin, "base": pin, "no_later_history": True, "profile_id": profile,
                    "config_options": [{"key": "model", "current": "sonnet",
                                        "choices": [{"name": "Claude Sonnet 5.5", "value": "sonnet"}]},
                                       {"key": "effort", "current": "high"}],
                    "last_turn_outcome": {"outcome": {"kind": "completed"}}}

        def transcript(sid):
            stages[sid].append("transcript")
            if failure == "transcript":
                return [{"items": []}]
            if sid == bad_audit_sid:
                return [{"items": [{"role": "tool", "seq": 1, "body": {"call": {
                    "title": "Bash", "rawInput": "curl https://example.invalid"}}}]}]
            return [{"items": [{"role": "message", "seq": 1, "body": {"text": "done"}}]}]

        def command(verb, *args, **kwargs):
            self.assertNotIn("--acknowledge-unpublished-work", args)
            if verb == "sessions":
                return {"sessions": [{"id": sid} for sid in sessions if sid not in destroyed]}
            sid = args[args.index("--session") + 1]
            stages[sid].append(verb)
            if verb == "message":
                commands = sessions[sid]["commands"]
                commands.append(args[args.index("--command-id") + 1])
                self.assertEqual(stages[sid].count("put-file"), 4)
                self.assertLess(stages[sid].index("seal"), stages[sid].index("put-file"))
                if len(commands) > 1:
                    prompt = Path(args[args.index("--prompt-file") + 1]).read_text()
                    self.assertIn("unmatched-quote", prompt)
                    self.assertNotIn("fabricated phrase", prompt)
                return {"turn_id": len(commands)}
            if verb == "wait":
                return {"turn_id": int(args[args.index("--turn") + 1]), "outcome": "finished"}
            if verb == "usage":
                commands = sessions[sid]["commands"]
                self.assertEqual(int(args[args.index("--after-seq") + 1]), len(commands) - 1)
                return usage_record(commands[-1], model, turn=len(commands))
            if verb == "destroy":
                info = sessions[sid]
                attempt = info["phase"] / "sessions" / info["tag"] / "attempt"
                receipt = j.load(attempt / "receipt.json")
                j.verify_capture(attempt, receipt, "claude-sonnet-5-5")
                self.assertEqual(receipt["producer"]["session_id"], sid)
                destroyed.append(sid)
                return {"accepted": True}
            self.fail(f"unexpected command {verb}")

        def fake_process(args, **_kw):
            if args[0] == "git":
                return subprocess.CompletedProcess(args, 0, stdout=str(j.HERE))
            if args[0] == str(probe):
                stages[args[1]].append("seal")
                return subprocess.CompletedProcess(args, 0, stdout=b"", stderr=b"")
            self.assertEqual(args[:3], ["mj", "--instance", "fixture-instance"])
            self.assertNotIn("--acknowledge-unpublished-work", args)
            verb = args[3]
            sid = args[args.index("--session") + 1]
            info = sessions[sid]
            stages[sid].append(verb)
            if verb == "put-file":
                src = Path(args[-1])
                self.assertEqual(src.parent, info["phase"] / info["tag"])
                self.assertIn(src.name, {"INSTRUCTIONS.md", "PILOT.md", "SPEC.md", "items.json"})
                return subprocess.CompletedProcess(args, 0, stdout=b"")
            self.assertEqual(verb, "fetch")
            self.assertLess(stages[sid].index("transcript"), stages[sid].index("fetch"))
            payload = j.load(info["phase"] / info["tag"] / "items.json")
            records = [raw_verdict(payload, item) for item in payload["items"]]
            if repair and payload["kind"] == "review":
                record, item = records[0], payload["items"][0]
                record["defects"][0].update(label="caught", matching_finding=item["text"])
                turn = len(info["commands"])
                fabricated = f"fabricated phrase {turn}"
                other_text = next(other["text"] for other in payload["items"][1:]
                                  if other["text"].strip() not in item["text"])
                record["evidence"] = [
                    {"claim": "catch", "quote": other_text if turn == 1 and repair == "verify" else item["text"]},
                    {"claim": f"matched_defect:{payload['defects'][0]['id']}",
                     "quote": fabricated if repair == "stalled" or turn < 3 else "  ".join(item["text"].split())},
                ]
            data = "".join(json.dumps(record) + "\n" for record in records)
            return subprocess.CompletedProcess(args, 0, stdout=data.encode())

        fake_driver = SimpleNamespace(use_named_instance=lambda name: instance, workspace_id=lambda: "workspace",
                                      mj_invocation=lambda *args: ["mj", "--instance", "fixture-instance", *args],
                                      mj=command, pilot_claude_hold=lambda profile: None, wait_idle=state, session=state,
                                      PILOT_CLAUDE_MODEL=shared_driver.PILOT_CLAUDE_MODEL,
                                      PILOT_CLAUDE_EFFORT=shared_driver.PILOT_CLAUDE_EFFORT,
                                      expected_provider_model=shared_driver.expected_provider_model,
                                      provider_model=shared_driver.provider_model,
                                      verify_session_selection=shared_driver.verify_session_selection,
                                      repair_progress=shared_driver.repair_progress,
                                      container=lambda sid: {"container": sid},
                                      inventory=lambda *args: {"head": pin, "head_tree":
                                          "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                                          "commits": [pin], "object_count": 2, "remotes": [], "alternates": False,
                                          "status": "", "ignored": ""},
                                      attest_pilot_cache=lambda *a: {"snapshots": {"only": {"commits": [pin]}}},
                                      transcript=transcript)
        configured = lambda *a, **k: {"model": "sonnet", "effort": "high"}
        fake_brief = SimpleNamespace(sealed_launch=launch, configure_pilot_model=configured,
                                     configure_pilot_model_ready=configured)
        args = [str(j.HERE / "verify_driver.py"), str(tasks), "--kind", "task", "--text-only",
                "--output", "verdicts.jsonl", "--profiles", profile, "--model", "sonnet", "--instance",
                "fixture-instance", "--bundle", "empty", "--repo-commit", pin, "--seal-probe", str(probe),
                "--provider-model", "claude-sonnet-5-5", "--model-version", "Sonnet 5.5", "--concurrency", "1"]
        with (patch.object(sys, "argv", args), patch.dict(os.environ, {"REVIEW_ARMS_DIR": str(self.root),
                "REVIEW_ARMS_INSTANCE": "default"}), patch.dict(sys.modules, {"driver": fake_driver,
                "brief_driver": fake_brief}), patch("subprocess.run", side_effect=fake_process),
              contextlib.redirect_stdout(io.StringIO()) as output, contextlib.redirect_stderr(io.StringIO()) as errors):
            phases = (j.INPUT_PATH,) if failure else (j.INPUT_PATH, j.INPUT_PATH)
            if not failure and not repair:
                phases += (j.HAND_PATH,)
            for phase in phases:
                tasks = self.root / phase
                args[1] = str(tasks)
                with self.assertRaises(SystemExit) as finished:
                    runpy.run_path(args[0], run_name="__main__")
                self.assertEqual(finished.exception.code, int(bool(failure)), output.getvalue())
            if failure:
                expected = {"version": "expected provider usage", "transcript": "could not be audited"}
                self.assertIn(expected[failure], output.getvalue())
                self.assertIn("session retained", output.getvalue())
            else:
                if repair:
                    expected = "unscored" if repair == "stalled" else "done"
                    self.assertIn(f'"state": "{expected}"', output.getvalue())
                else:
                    self.assertIn('"state": "void"', errors.getvalue())
                j.collect(self.root, key, profile=profile)
                if not repair:
                    j.collect(self.root, key, hand=True, profile=profile)
        return stages, destroyed

    def test_sealed_judges_audit_and_retry_before_capture_and_destroy(self):
        key = setup(self.root, *fixture())
        stages, destroyed = self.run_fake_judges(key)
        self.assertEqual(len(stages), 5)
        self.assertEqual(len(destroyed), 4)
        self.assertNotIn("fetch", stages["session-0"])
        self.assertNotIn("session-0", destroyed)
        self.assertEqual(len(list((self.root / j.INPUT_PATH).glob("sessions/*/void-session-0/audit.json"))), 1)
        self.assertEqual(j.records(self.root, key, "judge"), j.records(self.root, key, "hand-check"))
        self.assertEqual(len(j.records(self.root, key, "judge")), len(key["outputs"]))

    def test_fabricated_quotes_repair_until_the_catch_verifies(self):
        key = setup(self.root, *fixture())
        stages, destroyed = self.run_fake_judges(key, repair="verify")
        self.assertEqual(sorted(events.count("message") for events in stages.values()), [1, 3])
        self.assertEqual(len(destroyed), 2)
        verdicts = j.records(self.root, key, "judge")
        self.assertEqual(len(verdicts), len(key["outputs"]))
        self.assertEqual(sum(row.get("catch") is True for row in verdicts.values()), 1)
        self.assertTrue(any(row.get("catch") is False for row in verdicts.values()))

    def test_containment_quotes_do_not_substitute_for_leak_or_matched_defect_evidence(self):
        key = setup(self.root, *fixture())
        entry = next(entry for entry in key["outputs"].values() if entry["kind"] == "writer")
        payload = j.load(self.root / j.INPUT_PATH / entry["batch"] / "items.json")
        records = [raw_verdict(payload, item) for item in payload["items"]]
        records[0]["defects"][0]["label"] = "caught"
        data = "".join(json.dumps(record) + "\n" for record in records).encode()
        verified, errors = j.check_payload_verdicts(payload, data)
        oid = records[0]["output_id"]
        self.assertNotIn(oid, verified)
        self.assertEqual(j.error_set(errors), {
            ("missing-quote", oid, "leak"), ("missing-quote", oid, "matched_defect:d0"),
        })

    def test_same_normalized_error_stops_repair_and_leaves_the_catch_unscored(self):
        from rql_mock.engine.test_engine import EngineTests

        key = setup(self.root, *fixture())
        stages, destroyed = self.run_fake_judges(key, repair="stalled")
        self.assertEqual(sorted(events.count("message") for events in stages.values()), [1, 2])
        self.assertEqual(len(destroyed), 2)
        reports = (self.root / j.INPUT_PATH).glob("sessions/*/attempt/verification.json")
        report_path, = [path for path in reports if j.load(path)["state"] == "unscored"]
        report = j.load(report_path)
        error, = report["errors"]
        self.assertEqual((error["code"], error["site"]), ("unmatched-quote", "matched_defect:d0"))
        verdicts = j.records(self.root, key, "judge")
        self.assertEqual(set(verdicts), set(key["outputs"]) - {error["row"]})
        # Pass the judge's exact pair of normalized findings through the engine. The
        # shared rule stalls each engine session; the engine's one fresh session
        # meets the same pair and stalls too.
        rounds = [j.load(report_path.parent / "rounds" / str(i) / "result.json")["errors"] for i in range(2)]
        answer, runner = EngineTests.verified_query(self.root / "engine-fixture", rounds * 2)
        self.assertEqual(answer["status"], "engine_error")
        self.assertEqual(len(runner.turns), 4)
        self.assertEqual(len(runner.launches), 2)

    def test_provider_mismatch_keeps_sealed_session_and_usage(self):
        key = setup(self.root, *fixture())
        stages, destroyed = self.run_fake_judges(key, failure="version")
        self.assertEqual(len(stages), 2)
        self.assertEqual(destroyed, [])
        self.assertEqual(len(list((self.root / j.INPUT_PATH).glob("sessions/*/attempt/usage.json"))), 2)
        self.assertFalse(list((self.root / j.INPUT_PATH).glob("out/task-*.json")))

    def test_unreadable_transcript_keeps_sessions_and_stops_collection(self):
        key = setup(self.root, *fixture())
        stages, destroyed = self.run_fake_judges(key, failure="transcript")
        self.assertEqual(len(stages), 2)
        self.assertEqual(destroyed, [])
        self.assertTrue(all("fetch" not in events for events in stages.values()))
        self.assertFalse(list((self.root / j.INPUT_PATH).glob("out/task-*.json")))



if __name__ == "__main__":
    unittest.main()
