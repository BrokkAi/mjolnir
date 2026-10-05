#!/usr/bin/env python3
"""Behavior checks for ssh_docker_lab.py's tag-based recovery, without any
AWS or SSH calls.  Run directly: python3 tests/e2e/ssh_docker_lab.test.py

Covers Launch campaign finding J-9: a `cleanup --run-tag <tag>` path that
discovers and removes a run's resources by their mj-ssh-docker-run tag when
the ledger that would normally name them is lost, plus the plain `cleanup`
fallback that lists candidate tags instead of failing opaquely, plus the
dangling-symlink --artifact-dir fix.
"""

from __future__ import annotations

import argparse
import pathlib
import sys
import tempfile
import unittest
from typing import Any

SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import ssh_docker_lab as lab  # noqa: E402

def tagged(resource_id_field: str, resource_id: str, run_tag: str | None, **extra: Any) -> dict[str, Any]:
    tag_list = [{"Key": "Name", "Value": run_tag or "untagged"}]
    if run_tag is not None:
        tag_list.append({"Key": "mj-ssh-docker-run", "Value": run_tag})
    resource = {resource_id_field: resource_id, "Tags": tag_list}
    resource.update(extra)
    return resource

class FakeAws:
    """A hand-written stand-in for ssh_docker_lab.Aws tag-discovery queries.

    It holds an in-memory table per resource type, so ownership selection can
    be exercised without touching AWS.
    """

    def __init__(self, profile: str = "default", region: str = "us-east-1", timeout: float = 5.0) -> None:
        self.profile = profile
        self.region = region
        self.timeout = timeout
        self.instances: dict[str, dict[str, Any]] = {}
        self.volumes: dict[str, dict[str, Any]] = {}
        self.groups: dict[str, dict[str, Any]] = {}
        self.keys: dict[str, dict[str, Any]] = {}
        self.calls: list[tuple[str, ...]] = []

    @staticmethod
    def _run_tag_filter(rest: tuple[str, ...]) -> str | None:
        for arg in rest:
            if arg.startswith("Name=tag:mj-ssh-docker-run,Values="):
                return arg.split("Values=", 1)[1]
        return None

    @staticmethod
    def _by_tag(table: dict[str, dict[str, Any]], run_tag: str | None) -> list[dict[str, Any]]:
        items = list(table.values())
        if run_tag is not None:
            return [item for item in items if lab.tag_map(item).get("mj-ssh-docker-run") == run_tag]
        return [item for item in items if "mj-ssh-docker-run" in lab.tag_map(item)]

    def optional_json(self, service: str, *arguments: str) -> dict[str, Any] | None:
        self.calls.append((service, *arguments))
        assert service == "ec2", service
        command, rest = arguments[0], arguments[1:]

        # A by-ID lookup for a resource AWS no longer has raises
        # InvalidX.NotFound, which Aws.optional_json turns into None; a
        # by-tag filter query instead succeeds with an empty list.  Mirror
        # that distinction, since cleanup's poll-for-absence loops depend on
        # the by-ID lookup going to None once a delete call lands.
        def by_id(table: dict[str, dict[str, Any]], flag: str) -> list[dict[str, Any]] | None:
            resource_id = rest[rest.index(flag) + 1]
            item = table.get(resource_id)
            return [item] if item else None

        if command == "describe-instances":
            if "--instance-ids" in rest:
                items = by_id(self.instances, "--instance-ids")
                if items is None:
                    return None
            else:
                items = self._by_tag(self.instances, self._run_tag_filter(rest))
            return {"Reservations": [{"Instances": items}]} if items else {"Reservations": []}
        if command == "describe-volumes":
            if "--volume-ids" in rest:
                items = by_id(self.volumes, "--volume-ids")
                if items is None:
                    return None
            else:
                items = self._by_tag(self.volumes, self._run_tag_filter(rest))
            return {"Volumes": items}
        if command == "describe-security-groups":
            if "--group-ids" in rest:
                items = by_id(self.groups, "--group-ids")
                if items is None:
                    return None
            else:
                items = self._by_tag(self.groups, self._run_tag_filter(rest))
            return {"SecurityGroups": items}
        if command == "describe-key-pairs":
            if "--key-names" in rest:
                items = by_id(self.keys, "--key-names")
                if items is None:
                    return None
            else:
                items = self._by_tag(self.keys, self._run_tag_filter(rest))
            return {"KeyPairs": items}
        raise AssertionError(f"unexpected optional_json command: {command} {rest}")


def make_args(**overrides: Any) -> argparse.Namespace:
    base = dict(
        profile="default",
        region="us-east-1",
        artifact_dir=None,
        command_timeout=5.0,
        wait_timeout=5.0,
        cleanup_timeout=5.0,
        run_tag=None,
    )
    base.update(overrides)
    return argparse.Namespace(**base)

class DiscoverCandidateRunTagsTests(unittest.TestCase):
    """Tag-discovery selection logic backing the plain `cleanup` fallback."""

    def test_ignores_resources_without_the_tag(self) -> None:
        aws = FakeAws()
        aws.instances["i-1"] = tagged("InstanceId", "i-1", None)
        self.assertEqual(lab.discover_candidate_run_tags(aws), [])

class DiscoverOwnedKeyPairTests(unittest.TestCase):

    def test_raises_on_ambiguous_match(self) -> None:
        aws = FakeAws()
        aws.keys["key-1"] = tagged("KeyName", "key-1", "mj-ssh-docker-aaa")
        aws.keys["key-2"] = tagged("KeyName", "key-2", "mj-ssh-docker-aaa")
        with self.assertRaises(lab.LabError):
            lab.discover_owned_key_pair(aws, "mj-ssh-docker-aaa")

class CleanupByRunTagTests(unittest.TestCase):
    """End-to-end (against the fake) exercise of the J-9 recovery path."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.artifact_dir = pathlib.Path(self.tmp.name) / "recovered"
        self.run_tag = "mj-ssh-docker-20260101T000000Z-abc1234567"
        self.fake = FakeAws()
        self._real_aws = lab.Aws
        lab.Aws = lambda *a, **k: self.fake  # type: ignore[assignment]
        self.addCleanup(setattr, lab, "Aws", self._real_aws)

    def test_refuses_to_overwrite_an_existing_ledger(self) -> None:
        lab.ensure_private_directory(self.artifact_dir)
        lab.save_ledger(
            lab.ledger_path(self.artifact_dir),
            {"schema": 1, "run_tag": "some-other-run", "resources": {}, "cleanup_errors": []},
        )
        args = make_args(artifact_dir=self.artifact_dir, run_tag=self.run_tag)
        with self.assertRaises(lab.LabError) as excinfo:
            lab.cleanup_by_run_tag(args, self.run_tag)
        self.assertIn("already exists", str(excinfo.exception))

class BrokenSymlinkArtifactDirTests(unittest.TestCase):
    """A dangling `target` symlink under --artifact-dir must fail clearly
    instead of raising a bare FileExistsError from Path.mkdir."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)

    def test_dangling_symlink_ancestor_is_named_in_the_error(self) -> None:
        broken_link = self.root / "target"
        broken_link.symlink_to(self.root / "does-not-exist")
        artifact_dir = broken_link / "ssh-docker-e2e" / "run123"

        with self.assertRaises(lab.LabError) as excinfo:
            lab.ensure_private_directory(artifact_dir)
        message = str(excinfo.exception)
        self.assertIn(str(broken_link), message)
        self.assertIn("broken symlink", message)
        self.assertNotIsInstance(excinfo.exception, FileExistsError)

if __name__ == "__main__":
    unittest.main()
