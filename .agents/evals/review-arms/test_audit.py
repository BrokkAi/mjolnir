"""Audit recorded commands without treating searched or edited text as access."""
import json

import pytest

from audit import audit_pilot


PIN = "a" * 40
OTHER = "b" * 40
OWN = "c" * 32
FOREIGN = "d" * 32


def audit_call(tmp_path, call, **kwargs):
    transcript = tmp_path / "transcript.json"
    transcript.write_text(json.dumps([{"items": [
        {"seq": 1, "role": "tool", "body": {"call": call}},
    ]}]))
    return audit_pilot(transcript, OWN, commits=[PIN], **kwargs)


# Hard-won: 49e16b8: the pilot audit voided a search when its pattern mentioned a protected source or command
@pytest.mark.parametrize("command", [
    "rg -ni sqlite src/analyzer/store/mod.rs",
    "find src -type f -path '/run/hel/git-cache'",
    "git log --grep='git fetch /run/hel/git-cache' HEAD",
    "sed -i 's|before|curl https://example.invalid .mj/store.db|g' notes.txt",
    "grep -in 'curl https://github.com' README.md",
    "git grep -n 'git fetch /run/hel/git-cache' HEAD -- docs",
    "rg '/home/jonathan' src",
    "rg -e 'cargo test' README.md",
    "rg -e 'ssh host' README.md",
    f"git grep -n -e {OTHER} HEAD -- src",
    f"rg '/workspace/{FOREIGN}' docs",
    r"rg -n '\$HOME|\(sqlite\)' src | head -n 4",
    "echo 'curl https://example.invalid .mj/store.db' > notes.txt",
    "cat > notes.txt <<'EOF'\ntext with ssh and .mj/store.db\nEOF",
])
def test_pattern_text_does_not_trigger_protected_access(tmp_path, command):
    result = audit_call(tmp_path, {"title": "Shell", "kind": "execute", "rawInput": {"command": command}})
    assert result["passed"], result["hits"]


# Hard-won: 49e16b8: the pilot audit treated strings in edit payloads as tool accesses
def test_structured_edit_text_does_not_trigger_protected_access(tmp_path):
    result = audit_call(tmp_path, {"title": "Edit README.md", "kind": "edit", "rawInput": {
        "file_path": "README.md",
        "old_string": "search_sessions",
        "new_string": "Use ssh host or curl https://example.invalid to find /run/hel/git-cache.",
    }})
    assert result["passed"], result["hits"]


@pytest.mark.parametrize(("command", "hit"), [
    ("cat ~/.local/share/mjolnir/store", "memory-or-sessions"),
    ("rg -ni sqlite .mj/", "memory-or-sessions"),
    ("grep -T sqlite state.db", "memory-or-sessions"),
    ("grep --file=state.db query.txt", "memory-or-sessions"),
    ("sed -n '1,4p' memory/MEMORY.md", "memory-or-sessions"),
    ("cat < /run/hel/git-cache/objects/pack/x", "git-history-escape"),
    ("git fetch", "git-history-escape"),
    ("git log --all", "git-history-escape"),
    ("cat /home/jonathan/config", "outside-workspace"),
    (f"cat /workspace/{FOREIGN}/README", "other-workspace"),
    ("curl https://example.invalid", "github-or-web"),
    ("echo data > /dev/tcp/example.invalid/80", "network-command"),
    ("find .mj -name sqlite", "memory-or-sessions"),
    ("head README | /bin/ssh host cat README", "network-command"),
    ("cargo +stable test -p example", "build-or-test"),
    (f"git grep -e -- {OTHER}", "git-revision-outside-pin"),
    (f"git grep sqlite {OTHER} -- src", "git-revision-outside-pin"),
    (f"git diff -p {OTHER} HEAD -- src", "git-revision-outside-pin"),
    ("git show HEAD~2:README", "git-revision-outside-pin"),
    ("git log main", "git-revision-outside-pin"),
    (f"git checkout --detach {OTHER}", "git-revision-outside-pin"),
])
def test_protected_command_access_voids(tmp_path, command, hit):
    result = audit_call(tmp_path, {"title": command, "kind": "execute"}, single_commit=True)
    assert not result["passed"]
    assert {item["kind"] for item in result["hits"]} == {hit}


@pytest.mark.parametrize(("call", "hit"), [
    ({"title": "mcp__mj_memory__search_sessions", "rawInput": {"query": "source"}}, "memory-or-sessions"),
    ({"title": "WebSearch", "rawInput": {"query": "source"}}, "github-or-web"),
    ({"title": "Read file", "kind": "read", "locations": [{"path": ".mj/store.db"}]}, "memory-or-sessions"),
    ({"title": "Edit file", "kind": "edit", "rawInput": {"file_path": "memory/MEMORY.md"}}, "memory-or-sessions"),
])
def test_protected_structured_access_voids(tmp_path, call, hit):
    result = audit_call(tmp_path, call)
    assert not result["passed"]
    assert {item["kind"] for item in result["hits"]} == {hit}


@pytest.mark.parametrize("command", [
    "eval 'cat secret'",
    "cat local.py | python3",
    "if test -f local; then cat local; fi",
    'cat "$(printf other_file)"',
])
def test_ambiguous_execution_requires_inspection(tmp_path, command):
    result = audit_call(tmp_path, {"title": command, "kind": "execute"})
    assert not result["passed"]
    assert {item["kind"] for item in result["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize("command", [
    """python3 -c 'print("sqlite")'""",
    """node -e 'console.log("sqlite")'""",
    """python3 -c 'import subprocess; subprocess.run(["rg", "/run/hel/git-cache", "src"])'""",
    "python3 - <<'PY'\nimport subprocess\nsubprocess.run(['git', 'show', 'HEAD'])\nPY\ncat README.md",
])
def test_visible_safe_interpreter_source_is_recorded_and_passes(tmp_path, command):
    result = audit_call(tmp_path, {"title": command, "kind": "execute"})
    assert result["passed"], result["hits"]
    assert {hit["kind"] for hit in result["hits"]} == {"interpreter-screened"}


@pytest.mark.parametrize(("command", "kind"), [
    ("""python3 -c 'from pathlib import Path; Path(".mj/store.db").read_text()'""", "memory-or-sessions"),
    ("python3 - <<'PY'\nopen('/run/hel/git-cache/config')\nPY", "git-history-escape"),
    ("""python3 -c 'from urllib.request import urlopen; urlopen("https://example.invalid")'""", "network-command"),
    (f"""python3 -c 'import subprocess; subprocess.run(["git", "show", "{OTHER}"])'""", "git-revision-outside-pin"),
    ("""node -e 'fetch("https://example.invalid")'""", "network-command"),
    ("""python3 -c 'import base64; print(base64.b64decode("ZjQ="))'""", "needs-inspection"),
    ("python3 local.py", "needs-inspection"),
    ("source local.sh", "needs-inspection"),
])
def test_interpreter_screen_blocks_protected_access_and_opaque_source(tmp_path, command, kind):
    result = audit_call(tmp_path, {"title": command, "kind": "execute"}, single_commit=True)
    assert not result["passed"]
    assert kind in {hit["kind"] for hit in result["hits"]}
