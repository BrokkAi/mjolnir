"""Regression tests for the engine's claude5 login snapshot."""
import json
import subprocess
import sys
from unittest.mock import patch

import pytest

from rql_engine_tools import ENGINE_SETTINGS

from . import runner as runner_module
from .runner import EngineRunner, LaunchPreflightFailure, TurnFailure, TURN_TIMEOUT
from .test_engine import FakeAcpRunner, VERSION


NOW = 1_800_000_000
SID = "a" * 32


def oauth(token, seconds_left=3600):
    return {"accessToken": token, "expiresAt": (NOW + seconds_left) * 1000}


def claude5_quota(profile):
    assert profile == "claude5"


@pytest.fixture
def engine_profile(tmp_path, monkeypatch):
    import driver

    for name in ("INSTANCE", "MJ_INSTANCE_ARGS", "STORE"):
        monkeypatch.setattr(driver, name, getattr(driver, name))
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("REVIEW_ARMS_INSTANCE", "default")
    login_home = tmp_path / ".claude5"
    login_home.mkdir()
    owner = login_home / ".credentials.json"
    instance = tmp_path / ".config/mjolnir/instances/mock-engine"
    engine_home = instance / "claude-engine"
    engine_home.mkdir(parents=True)
    (engine_home / "settings.json").write_text(json.dumps(ENGINE_SETTINGS))
    alias = engine_home / ".credentials.json"
    alias.write_text(json.dumps({"claudeAiOauth": oauth("previous")}))
    (instance / "config.toml").write_text(f"""
[profiles.claude5]
kind = "claude"
home = "{login_home}"

[profiles.claude5-engine]
kind = "claude"
home = "{engine_home}"
""")
    runner = EngineRunner({"instance": "mock-engine", "profile": "claude5",
                           "engine_profile": "claude5-engine", "bundle": "test", "model_version": VERSION})
    monkeypatch.setattr(runner.driver, "pilot_claude_hold", claude5_quota)
    return runner, owner, alias


def rotate(owner, token, seconds_left=3600):
    raw = json.dumps({"claudeAiOauth": {**oauth(token, seconds_left), "refreshToken": "canonical-only"}})
    replacement = owner.with_suffix(".tmp")
    replacement.write_text(raw)
    replacement.replace(owner)
    return raw


class TestEngineProfile:
    @pytest.mark.parametrize("seconds_left", [-1, TURN_TIMEOUT, TURN_TIMEOUT + 1])
    def test_launch_reseeds_only_current_claude5_access_credentials(self, engine_profile, seconds_left):
        runner, owner, alias = engine_profile
        raw = rotate(owner, "current", seconds_left)
        launches = []

        def launch(_bundle, _commit, profile, _title, **_options):
            assert profile == "claude5-engine"
            launches.append(profile)
            return SID

        runner.launch = launch
        with patch("time.time", return_value=NOW):
            if seconds_left > TURN_TIMEOUT:
                runner.start({"orphan": "ref"}, "query")
            else:
                with pytest.raises(LaunchPreflightFailure, match=r"claude5 access credentials.*30 minutes"):
                    runner.start({"orphan": "ref"}, "query")
        assert owner.read_text() == raw
        staged = json.loads(alias.read_text())["claudeAiOauth"]
        if launches:
            assert staged == oauth("current", seconds_left)
            assert alias.stat().st_mode & 0o777 == 0o600
        else:
            assert seconds_left <= TURN_TIMEOUT
            assert staged["accessToken"] == "previous"


@pytest.fixture
def turn_runner(engine_profile, tmp_path, monkeypatch):
    runner, owner, alias = engine_profile
    rotate(owner, "initial")
    with patch("time.time", return_value=NOW):
        runner.seed_login()
    staged = tmp_path / "staged-credentials.json"
    staged.touch(mode=0o600)
    staged.write_bytes(alias.read_bytes())
    acp = FakeAcpRunner()
    runner.driver = acp
    runner.command = acp.command
    monkeypatch.setattr(acp, "container", lambda sid: {"container": f"container-{sid}"}, raising=False)
    monkeypatch.setattr(acp, "pilot_claude_hold", claude5_quota)
    run = subprocess.run

    def podman(argv, *, input, **kwargs):
        assert argv[:6] == ["podman", "exec", "-i", f"container-{SID}", "python3", "-c"]
        assert argv[-1] == f"/var/lib/hel/profiles/{SID}/.credentials.json"
        assert "refreshToken" not in json.loads(input)["claudeAiOauth"]
        return run([sys.executable, "-c", argv[-2], str(staged)], input=input, **kwargs)

    monkeypatch.setattr(runner_module.subprocess, "run", podman)
    with patch("time.time", return_value=NOW):
        yield runner, acp, owner, alias, staged


def assert_staged(alias, staged, token):
    expected = {"claudeAiOauth": oauth(token)}
    assert json.loads(staged.read_bytes()) == json.loads(alias.read_bytes()) == expected
    assert staged.stat().st_mode & 0o777 == 0o600


def test_rotation_before_prompt_updates_container_and_alias(turn_runner):
    runner, acp, owner, alias, staged = turn_runner
    source = rotate(owner, "replacement")

    def before_send():
        assert_staged(alias, staged, "replacement")

    with staged.open("rb") as old_staged, alias.open("rb") as old_alias:
        response, record = runner.prompt(SID, "prompt", before_send=before_send)
        initial = {"claudeAiOauth": oauth("initial")}
        assert json.load(old_staged) == json.load(old_alias) == initial
    assert response == "Answer written"
    assert record["turn"] == 1
    assert len(acp.messages) == 1
    assert owner.read_text() == source


# Hard-won: 3a68d9227: a revoked engine credential ended the live gate.
@pytest.mark.parametrize("rotation,refuse_retry", [(True, False), (False, False), (True, True)])
def test_auth_refusal_retries_once_only_after_rotation(turn_runner, rotation, refuse_retry):
    runner, acp, owner, alias, staged = turn_runner
    mj = acp.mj
    owner_bytes = owner.read_text()
    original_inodes = (alias.stat().st_ino, staged.stat().st_ino)

    def observed_mj(operation, *args, **kwargs):
        nonlocal owner_bytes
        if operation == "wait":
            turn = len(acp.messages)
            if turn == 1:
                assert_staged(alias, staged, "initial")
                if rotation:
                    owner_bytes = rotate(owner, "replacement")
            else:
                assert_staged(alias, staged, "replacement")
                if not refuse_retry:
                    return mj(operation, *args, **kwargs)
                owner_bytes = rotate(owner, "third")
            return {"turn_id": turn, "outcome": "error", "stop_reason": "error",
                    "diagnostic": {"code": "Authentication required", "message": "Authentication required"}}
        return mj(operation, *args, **kwargs)

    acp.mj = observed_mj
    pending = []
    if rotation and not refuse_retry:
        response, record = runner.prompt(SID, "prompt", "answer", before_send=lambda: pending.append(True))
        assert response == "Answer written"
        assert record["turn"] == 2
        assert record["context_id"] == "answer"
    else:
        reason = "after token rotation" if rotation else "access token is unchanged"
        with pytest.raises(TurnFailure, match=f"claude5 login was refused.*{reason}") as failure:
            runner.prompt(SID, "prompt", "answer", before_send=lambda: pending.append(True))
        assert failure.value.record["turn"] == (2 if rotation else 1)
    assert pending == [True]
    assert len(acp.messages) == (2 if rotation else 1)
    assert {text for _, text in acp.messages} == {"prompt"}
    assert len({command for command, _ in acp.messages}) == len(acp.messages)
    assert owner.read_text() == owner_bytes
    if not rotation:
        assert (alias.stat().st_ino, staged.stat().st_ino) == original_inodes
