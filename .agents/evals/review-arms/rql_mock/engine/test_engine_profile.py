"""Regression tests for engine admission and session-bound login snapshots."""
import json
import subprocess
import sys
from unittest.mock import patch

import pytest

from rql_engine_tools import ENGINE_SETTINGS

from . import runner as runner_module
from .runner import EngineLogin, EngineRunner, LaunchHeld, LaunchPreflightFailure, TurnFailure, TURN_TIMEOUT
from .test_engine import CATALOGUE, COMMIT, MENU, Engine, FakeAcpRunner, VERSION


NOW = 1_800_000_000
SID = "a" * 32


def oauth(token, seconds_left=3600):
    return {"accessToken": token, "expiresAt": (NOW + seconds_left) * 1000}


QUOTA = {"claude5": (90, 80), "claude4": (80, 90), "claude7": (25, 99)}


@pytest.fixture
def engine_profiles(tmp_path, monkeypatch):
    import driver

    for name in ("INSTANCE", "MJ_INSTANCE_ARGS", "STORE"):
        monkeypatch.setattr(driver, name, getattr(driver, name))
    # Admission logic is tested under the study's default floors for every login;
    # claude5's own exemption has its own test.
    monkeypatch.setattr(driver, "PILOT_CLAUDE_FLOORS", {})
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("REVIEW_ARMS_INSTANCE", "default")
    logins, paths, config = [], {}, []
    instance = tmp_path / ".config/mjolnir/instances/mock-engine"
    for profile in QUOTA:
        login_home = tmp_path / f".{profile}"
        login_home.mkdir()
        alias = f"{profile}-engine"
        engine_home = instance / alias
        engine_home.mkdir(parents=True)
        (engine_home / "settings.json").write_text(json.dumps(ENGINE_SETTINGS))
        alias_file = engine_home / ".credentials.json"
        alias_file.write_text(json.dumps({"claudeAiOauth": oauth("previous")}))
        owner_file = login_home / ".credentials.json"
        rotate(owner_file, f"{profile}-initial")
        paths[profile] = (owner_file, alias_file)
        logins.append({"profile": profile, "engine_profile": alias})
        config.append(f"""
[profiles.{profile}]
kind = "claude"
home = "{login_home}"

[profiles.{alias}]
kind = "claude"
home = "{engine_home}"
""")
    (instance / "config.toml").write_text("".join(config))
    runner = EngineRunner({"instance": "mock-engine", "logins": logins,
                           "bundle": "test", "model_version": VERSION})
    scores = QUOTA.copy()
    calls = []

    def quota(operation, flag, profile):
        assert (operation, flag) == ("quota", "--profile")
        calls.append(profile)
        balances = scores[profile]
        if balances is None:
            raise OSError("report unavailable")
        windows = [{"remaining_percent": balance, "duration_seconds": seconds}
                   for balance, seconds in zip(balances, (604800, 18_000))]
        return {"profiles": [{"report": {"windows": windows}}]}

    monkeypatch.setattr(driver, "mj", quota)
    return runner, paths, scores, calls


def rotate(owner, token, seconds_left=3600):
    raw = json.dumps({"claudeAiOauth": {**oauth(token, seconds_left), "refreshToken": "canonical-only"}})
    replacement = owner.with_suffix(".tmp")
    replacement.write_text(raw)
    replacement.replace(owner)
    return raw


class TestEngineProfile:
    @pytest.mark.parametrize("seconds_left", [-1, TURN_TIMEOUT, TURN_TIMEOUT + 1])
    def test_launch_reseeds_only_current_claude5_access_credentials(self, engine_profiles, seconds_left):
        runner, paths, _, _ = engine_profiles
        owner, alias = paths["claude5"]
        raw = rotate(owner, "current", seconds_left)
        launches = []

        def launch(_bundle, _commit, profile, _title, **_options):
            launches.append(profile)
            return SID

        runner.launch = launch
        with patch("time.time", return_value=NOW):
            runner.for_launch().start({"orphan": "ref"}, "query")
        assert owner.read_text() == raw
        staged = json.loads(alias.read_text())["claudeAiOauth"]
        if seconds_left > TURN_TIMEOUT:
            assert launches == ["claude5-engine"]
            assert staged == oauth("current", seconds_left)
            assert alias.stat().st_mode & 0o777 == 0o600
        else:
            # A stale owner is passed over at admission; the next login launches.
            assert launches == ["claude4-engine"]
            assert staged["accessToken"] == "previous"

    def test_seeding_still_refuses_credentials_that_went_stale_after_admission(self, engine_profiles):
        runner, paths, _, _ = engine_profiles
        bound = None
        with patch("time.time", return_value=NOW):
            bound = runner.for_launch()
        assert bound.login.login_profile == "claude5"
        rotate(paths["claude5"][0], "stale", seconds_left=60)
        with patch("time.time", return_value=NOW), \
                pytest.raises(LaunchPreflightFailure, match=r"claude5 access credentials.*30 minutes"):
            bound.start({"orphan": "ref"}, "query")


@pytest.fixture
def turn_runner(engine_profiles, tmp_path, monkeypatch):
    unbound, paths, _, _ = engine_profiles
    owner, alias = paths["claude5"]
    runner = unbound.for_launch()
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
    monkeypatch.setattr(acp, "pilot_claude_hold", unbound.driver.pilot_claude_hold)
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


@pytest.mark.parametrize("balances,expected", [
    ([(70, 40), (69, 90), (65, 99)], "claude5"),
    ([(70, 40), (70, 41), (70, 21)], "claude4"),
    ([(70, 40), (70, 40), (70, 40)], "claude5"),
    ([(99, 20), None, (26, 21)], "claude7"),
])
def test_launch_admits_once_and_records_login_before_mj_new(engine_profiles, tmp_path, balances, expected):
    runner, _, scores, quota_calls = engine_profiles
    scores.update(zip(scores, balances))
    cache = tmp_path / "cache"
    intent = cache / COMMIT / "active/session.json"
    engine = Engine(cache, (CATALOGUE, MENU), None, lambda: runner, VERSION)
    login = EngineLogin(expected, f"{expected}-engine")

    def launch(_bundle, _commit, profile, _title, **_options):
        assert profile == login.engine_profile
        pending = json.loads(intent.read_bytes())
        assert pending == {"commit": COMMIT, "orphan": "ref", **login._asdict()}
        return SID

    runner.launch = launch
    with patch("time.time", return_value=NOW):
        bound, sid = engine.session(COMMIT, {"orphan": "ref"}, intent)
    assert sid == SID and bound.login == login
    assert quota_calls == list(scores)
    assert json.loads(intent.read_bytes()) == {"commit": COMMIT, "orphan": "ref", **login._asdict(),
                                               "session_id": SID, "instance": "mock-engine",
                                               "model_version": VERSION}


def test_all_logins_held_names_each_quota_state(engine_profiles, tmp_path):
    runner, _, scores, quota_calls = engine_profiles
    scores.update({"claude5": (25, 99), "claude4": (99, 20), "claude7": None})
    cache = tmp_path / "cache"
    engine = Engine(cache, (CATALOGUE, MENU), None, lambda: runner, VERSION)
    intent = cache / COMMIT / "active/session.json"
    with pytest.raises(LaunchHeld) as held:
        engine.session(COMMIT, {"orphan": "ref"}, intent)
    assert "claude5: 25% weekly remaining" in str(held.value)
    assert "claude4: 20% 5-hour remaining" in str(held.value)
    assert "claude7: no usable quota report" in str(held.value)
    assert quota_calls == list(scores)
    assert not intent.parent.exists()


def test_quota_and_auth_retry_stay_with_each_session_while_logins_rotate(engine_profiles, monkeypatch):
    runner, paths, scores, quota_calls = engine_profiles
    first = runner.for_launch()
    scores["claude4"] = (95, 90)
    second = runner.for_launch()
    assert first.login.login_profile == "claude5" and second.login.login_profile == "claude4"
    real_driver = runner.driver
    stages = {}
    other_sid = "b" * 32
    acps = {}
    for bound, sid in ((first, SID), (second, other_sid)):
        acp = FakeAcpRunner()
        monkeypatch.setattr(acp, "pilot_claude_hold", real_driver.pilot_claude_hold)
        bound.driver, bound.command = acp, acp.command
        bound.sync_container_login = lambda sid, data: stages.update({sid: json.loads(data)["claudeAiOauth"]})
        acps[sid] = acp

    first_owner, first_alias = paths["claude5"]
    second_owner, second_alias = paths["claude4"]
    second_mj = acps[other_sid].mj

    def observe_second_auth(operation, *args, **kwargs):
        if operation == "wait" and len(acps[other_sid].messages) == 1:
            assert stages[other_sid] == oauth("claude4-before-send")
            rotate(second_owner, "claude4-after-refusal")
            return {"turn_id": 1, "outcome": "error",
                    "diagnostic": {"code": "Authentication required"}}
        return second_mj(operation, *args, **kwargs)

    acps[other_sid].mj = observe_second_auth
    scores["claude5"] = (25, 90)
    quota_calls.clear()
    with patch("time.time", return_value=NOW):
        with pytest.raises(LaunchHeld, match="claude5: 25% weekly"):
            first.prompt(SID, "held")
        rotate(second_owner, "claude4-before-send")
        rotate(first_owner, "claude5-independent-rotation")
        response, record = second.prompt(other_sid, "allowed")
        assert response == "Answer written" and record["turn"] == 2
        assert stages[other_sid] == oauth("claude4-after-refusal")
        assert stages[SID] != stages[other_sid]
        assert json.loads(second_alias.read_bytes())["claudeAiOauth"] == stages[other_sid]
        scores["claude5"] = (80, 90)
        first.prompt(SID, "resumed")
        assert stages[SID] == oauth("claude5-independent-rotation")
        assert json.loads(first_alias.read_bytes())["claudeAiOauth"] == stages[SID]
    assert quota_calls == ["claude5", "claude4", "claude4", "claude5"]
    assert len(acps[SID].messages) == 1


# Hard-won: 1adb52f9b: list-based admission could choose the reserved claude login
@pytest.mark.parametrize("row", [{"profile": "claude", "engine_profile": "claude-engine"},
                                 {"profile": "claude5", "engine_profile": "claude"}])
def test_configured_logins_refuse_the_reserved_claude_login(row):
    with pytest.raises(ValueError, match="reserved for owner sessions"):
        EngineRunner.configured_logins({"logins": [{"profile": "claude4", "engine_profile": "claude4-engine"}, row]})


# Hard-won: a3b312cf1: admission chose the login with the most quota even when its owner had not refreshed
def test_admission_skips_a_login_whose_owner_credentials_cannot_start_a_session(engine_profiles):
    runner, paths, scores, _ = engine_profiles
    best, *others = list(scores)
    scores.update({best: (90, 90), **{profile: (40, 40) for profile in others}})
    rotate(paths[best][0], f"{best}-stale", seconds_left=60)
    with patch("time.time", return_value=NOW):
        assert runner.select_login().login_profile == others[0]
        for profile in others:
            rotate(paths[profile][0], f"{profile}-stale", seconds_left=60)
        with pytest.raises(LaunchHeld) as held:
            runner.select_login()
    assert all(f"{profile} access credentials must stay valid" in str(held.value) for profile in scores)


def test_claude5_runs_to_the_provider_limit_while_other_logins_keep_their_floors(engine_profiles, monkeypatch):
    import driver

    runner, _, scores, _ = engine_profiles
    monkeypatch.setattr(driver, "PILOT_CLAUDE_FLOORS", {"claude5": (0, 0)})
    scores.update({"claude5": (5, 3), "claude4": (24, 99), "claude7": (99, 20)})
    with patch("time.time", return_value=NOW):
        assert runner.select_login().login_profile == "claude5"
        scores["claude5"] = (0, 50)
        with pytest.raises(LaunchHeld) as held:
            runner.select_login()
    assert "claude5: 0% weekly remaining" in str(held.value)
    assert "claude4: 24% weekly remaining" in str(held.value)
    assert "claude7: 20% 5-hour remaining" in str(held.value)
