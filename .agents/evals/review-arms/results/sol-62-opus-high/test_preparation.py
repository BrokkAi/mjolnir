"""Filesystem checks for preparing the verification round."""
import json
import pathlib
import tempfile
import unittest

from audit_reviewers import freeze_new
from audit_round import ready


class PreparationChecks(unittest.TestCase):
    def test_ready_creates_manifest_and_preserves_exact_prepared_membership(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            study = root / "sol-62-repeats"
            preflight = study / "preflight"
            preflight.mkdir(parents=True)
            diffs = [f"diff-{i}" for i in range(70)]
            historical = [dict(bug=bug, arm=arm) for bug in diffs for arm in "ABCDEFGHIJK"]
            historical += [dict(bug=bug, arm="L") for bug in diffs[:14]]
            additions = [dict(bug=bug, arm=arm) for bug in diffs for arm in "QRST"]
            for job in historical + additions:
                path = root / "runs" / job["bug"] / job["arm"] / "done.json"
                path.parent.mkdir(parents=True)
                path.touch()
            (preflight / "historical-and-op-ready-jobs.json").write_text(json.dumps(historical))
            (study / "jobs.json").write_text(json.dumps(additions))

            path = study / "ready-jobs.json"
            ready(root)
            self.assertEqual(json.loads(path.read_text()), historical + additions)
            manually_prepared = json.dumps(additions + historical).encode()
            path.write_bytes(manually_prepared)
            ready(root)
            self.assertEqual(path.read_bytes(), manually_prepared)

            unexpected = json.dumps(additions + historical[1:] + additions[:1]).encode()
            path.write_bytes(unexpected)
            with self.assertRaises(AssertionError):
                ready(root)
            self.assertEqual(path.read_bytes(), unexpected)
            self.assertFalse((root / "verify9").exists())

    def test_hash_preparation_reuses_exact_manifests_but_rejects_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            first, second, manifest = (root / name for name in ("usage.json", "output.md", "review.sha256"))
            first.write_text("turn evidence")
            second.write_text("finding")
            paths = [first, second]
            freeze_new(root, paths, manifest)
            lines = manifest.read_text().splitlines()
            prepared = "\n".join(reversed(lines)) + "\n"
            manifest.write_text(prepared)
            freeze_new(root, paths, manifest)
            self.assertEqual(manifest.read_text(), prepared)

            second.write_text("edited finding")
            with self.assertRaises(AssertionError):
                freeze_new(root, paths, manifest)
            self.assertEqual(manifest.read_text(), prepared)
            second.write_text("finding")

            for bad in (lines[:1], lines + lines[:1], [lines[0].replace("output.md", "other"), lines[1]]):
                malformed = "\n".join(bad) + "\n"
                manifest.write_text(malformed)
                with self.assertRaises(AssertionError):
                    freeze_new(root, paths, manifest)
                self.assertEqual(manifest.read_text(), malformed)
