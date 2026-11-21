"""Regression tests for the engine's claude5 login snapshot."""
import json
from unittest.mock import patch

import pytest

from rql_engine_tools import ENGINE_SETTINGS

from .runner import EngineRunner, LaunchPreflightFailure, TURN_TIMEOUT


class TestEngineProfile:
    @pytest.mark.parametrize("seconds_left", [-1, TURN_TIMEOUT, TURN_TIMEOUT + 1])
    def test_launch_reseeds_only_current_claude5_access_credentials(
        self, tmp_path, monkeypatch, seconds_left,
    ):
        from driver import INSTANCE, MJ_INSTANCE_ARGS, STORE
        import driver

        monkeypatch.setattr(driver, "INSTANCE", INSTANCE)
        monkeypatch.setattr(driver, "MJ_INSTANCE_ARGS", MJ_INSTANCE_ARGS)
        monkeypatch.setattr(driver, "STORE", STORE)
        now = 1_800_000_000
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("REVIEW_ARMS_INSTANCE", "default")
        login_home = tmp_path / ".claude5"
        login_home.mkdir()
        owner = login_home / ".credentials.json"
        credentials = {"claudeAiOauth": {
            "accessToken": "current", "refreshToken": "canonical-only",
            "expiresAt": (now + seconds_left) * 1000,
        }}
        raw = json.dumps(credentials)
        owner.write_text(raw)
        instance = tmp_path / ".config/mjolnir/instances/mock-engine"
        engine_home = instance / "claude-engine"
        engine_home.mkdir(parents=True)
        (engine_home / "settings.json").write_text(json.dumps(ENGINE_SETTINGS))
        alias = engine_home / ".credentials.json"
        alias.write_text('{"claudeAiOauth":{"accessToken":"previous"}}')
        (instance / "config.toml").write_text(f"""
[profiles.claude5]
kind = "claude"
home = "{login_home}"

[profiles.claude5-engine]
kind = "claude"
home = "{engine_home}"
""")
        runner = EngineRunner({"instance": "mock-engine", "profile": "claude5",
                               "engine_profile": "claude5-engine", "bundle": "test"})
        monkeypatch.setattr(runner.driver, "pilot_claude_hold", lambda profile: (
            None if profile == "claude5" else pytest.fail("quota read from the engine alias")
        ))
        launches = []

        def launch(_bundle, _commit, profile, _title, **_options):
            assert profile == "claude5-engine"
            launches.append(profile)
            return "a" * 32

        runner.launch = launch
        with patch("time.time", return_value=now):
            if seconds_left > TURN_TIMEOUT:
                runner.start({"orphan": "ref"}, "query")
            else:
                with pytest.raises(LaunchPreflightFailure, match=r"claude5 access credentials.*30 minutes"):
                    runner.start({"orphan": "ref"}, "query")
        assert owner.read_text() == raw
        staged = json.loads(alias.read_text())["claudeAiOauth"]
        if launches:
            assert staged == {"accessToken": "current", "expiresAt": (now + seconds_left) * 1000}
            assert alias.stat().st_mode & 0o777 == 0o600
        else:
            assert seconds_left <= TURN_TIMEOUT
            assert staged["accessToken"] == "previous"
