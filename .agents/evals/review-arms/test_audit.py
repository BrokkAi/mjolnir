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
    "sed 's|before|curl https://example.invalid .mj/store.db|g' notes.txt",
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


# Hard-won: 8c9d945: live engine query voided for a locally assigned shell variable
def test_live_engine_shell_resolves_same_command_assignment(tmp_path):
    command = r'''C=.rql-engine/catalogue.md; grep -n "callable\|contract" $C | cut -c1-600'''
    report = audit_call(tmp_path, {"kind": "execute", "rawInput": {"command": command}})
    assert report["passed"], report["hits"]


# Hard-won: 8c9d945: live engine query voided for a heredoc following shell reads
@pytest.mark.parametrize("separator", [";", "&&", "||"])
def test_live_engine_screens_heredoc_after_reads(tmp_path, separator):
    command = f"sed -n 12,16p src/api.rs {separator} python3 - <<'EOF'\nprint(open('src/api.rs').read())\nEOF"
    report = audit_call(tmp_path, {"kind": "execute", "rawInput": {"command": command}})
    assert report["passed"], report["hits"]
    assert {hit["kind"] for hit in report["hits"]} == {"interpreter-screened"}


# Hard-won: 8c9d945: live engine query voided for an isolated Python stdin heredoc
@pytest.mark.parametrize("interpreter", ["python3 -I -", "node -"])
def test_live_engine_recognizes_visible_stdin_programs(tmp_path, interpreter):
    command = f"{interpreter} <<'EOF'\nvalue = 1\nEOF"
    report = audit_call(tmp_path, {"kind": "execute", "rawInput": {"command": command}})
    assert report["passed"], report["hits"]
    assert {hit["kind"] for hit in report["hits"]} == {"interpreter-screened"}


@pytest.mark.parametrize(("command", "kind"), [
    ('X=.mj/store.db; cat "${X}"', "memory-or-sessions"),
    ('X=/run/hel/git-cache; cat < "$X/config"', "git-history-escape"),
    ('cat "$UNKNOWN_AUDIT_PATH"', "needs-inspection"),
    ('X=notes; true && X=.mj/store.db; cat "$X"', "needs-inspection"),
    ("sed -n 1p README.md && python3 -I - <<'EOF'\nopen('.mj/store.db')\nEOF", "memory-or-sessions"),
    ("python3 -I - <<'EOF'\nprint('safe')\nEOF\ncat /run/hel/git-cache/config", "git-history-escape"),
    ("python3 -I - <<'EOF' < local.py\nprint('safe')\nEOF", "needs-inspection"),
    ("python3 - <<EOF\n$(cat .mj/store.db)\nEOF", "needs-inspection"),
])
def test_live_shell_forms_preserve_audit_controls(tmp_path, command, kind):
    report = audit_call(tmp_path, {"kind": "execute", "rawInput": {"command": command}})
    assert not report["passed"]
    assert kind in {hit["kind"] for hit in report["hits"]}


def audit_calls(tmp_path, calls, **metadata):
    transcript = tmp_path / "transcript.json"
    transcript.write_text(json.dumps([{**metadata, "items": [
        {"seq": seq, "role": "tool", "body": {"call": call}}
        for seq, call in enumerate(calls, 1)
    ]}]))
    return audit_pilot(transcript, OWN, commits=[PIN], single_commit=True)


def shell(command):
    return {"kind": "execute", "rawInput": {"command": command}}


# Hard-won: 8c9d945: live engine query voided for running a visibly written script
@pytest.mark.parametrize(("write", "run"), [
    ("mkdir -p /tmp/audit && cat > /tmp/audit/f.py <<'EOF'\nprint('sqlite')\nEOF",
     f'cd /workspace/{OWN}/repo && python3 -I /tmp/audit/f.py "$PWD"'),
    ("printf '%s\\n' '#!/bin/sh' 'cat README.md' > f", "chmod +x f && ./f"),
    ("tee f <<'EOF'\ncat README.md\nEOF", 'sh "$PWD/f"'),
])
def test_audit_screens_scripts_written_visibly_in_an_earlier_call(tmp_path, write, run):
    report = audit_calls(tmp_path, [shell(write), shell(run)], cwd=f"/workspace/{OWN}/repo")
    assert report["passed"], report["hits"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(2, "interpreter-screened")]


@pytest.mark.parametrize("mutation", [
    shell("printf 'changed' >> f.py"),
    shell("cp other.py f.py"),
    shell("mv other.py f.py"),
    {"kind": "edit", "rawInput": {"file_path": "f.py", "new_string": "changed"}},
])
def test_script_changed_after_visible_write_is_opaque(tmp_path, mutation):
    report = audit_calls(tmp_path, [
        shell("cat > f.py <<'EOF'\nprint('safe')\nEOF"),
        mutation,
        shell("python3 f.py"),
    ])
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(3, "needs-inspection")]


def test_audit_checks_protected_effects_in_recorded_script_and_resolved_home(tmp_path):
    report = audit_calls(tmp_path, [
        shell("printf '%s\\n' \"open('.mj/store.db')\" > f.py; python3 f.py"),
        shell('cat "$HOME/.local/share/mjolnir/store"'),
    ], home="/home/hel")
    assert not report["passed"]
    assert {(hit["seq"], hit["kind"]) for hit in report["hits"]} >= {
        (1, "memory-or-sessions"), (2, "memory-or-sessions"),
    }


def test_pwd_uses_recorded_placement_and_does_not_guess_after_failed_cd(tmp_path):
    (tmp_path / "placement.json").write_text(json.dumps({"cwd": f"/workspace/{OWN}/repo"}))
    report = audit_calls(tmp_path, [shell('cat "$PWD/README.md"'), shell('cd missing; cat "$PWD/README.md"')])
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(2, "needs-inspection")]


def test_recursively_invoked_script_requires_inspection(tmp_path):
    report = audit_calls(tmp_path, [
        shell("printf '%s\\n' \"bash -c 'sh f'\" > f"),
        shell("sh f"),
    ])
    assert not report["passed"]
    assert "needs-inspection" in {hit["kind"] for hit in report["hits"]}


# Hard-won: b9ceb96: live engine query voided for awk line slice
def test_live_engine_awk_line_slice_is_a_read(tmp_path):
    command = "awk 'NR>=102 && NR<=110' src/hel_database/events.rs"
    report = audit_call(tmp_path, {"kind": "execute", "rawInput": {"command": command}})
    assert report["passed"], report["hits"]


def test_awk_system_call_still_flags_network_access(tmp_path):
    command = """awk 'BEGIN { system("curl https://example.invalid") }' src/hel_database/events.rs"""
    report = audit_call(tmp_path, {"kind": "execute", "rawInput": {"command": command}})
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"github-or-web", "needs-inspection"}


@pytest.mark.parametrize(("command", "expected"), [
    # Hard-won: 343e9f0: perl effects passed a denylist
    ("""perl -ne 'use File::Copy; copy("/workspace/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/secret", "out")' input""",
     {"needs-inspection", "other-workspace"}),
    # Hard-won: 343e9f0: perl imported socket code outside the read subset
    ("perl -ne 'use IO::Socket::INET; print' input", {"needs-inspection"}),
    # Hard-won: 343e9f0: awk hid a later read behind backslash-newline
    ("awk 'BEGIN { x = 1; " + "\\" + "\ngetline < input }'", {"needs-inspection"}),
    # Hard-won: 343e9f0: awk hid print redirection after a trailing comma
    ("awk '{ print $1,\n$2 > output }' input", {"needs-inspection"}),
    # Hard-won: 343e9f0: gawk changed git's environment before a subprocess
    ("""gawk 'BEGIN { ENVIRON["GIT_DIR"]="/tmp/other"; system("git show HEAD") }'""", {"needs-inspection"}),
    # Hard-won: 343e9f0: perl substitution with whitespace hid its execution flag
    ("perl -pe 's {x}{print}e' input", {"needs-inspection"}),
    # Hard-won: 343e9f0: sed backup suffix could change the destination
    ("sed '--in-place=*/../../outside' 'p' README.md", {"needs-inspection"}),
    # Hard-won: 343e9f0: relative operands missed the recorded workspace boundary
    (f"cat ../{FOREIGN}/README.md", {"other-workspace"}),
])
def test_text_filter_review_findings_still_void(tmp_path, command, expected):
    report = audit_calls(tmp_path, [shell(command)], cwd=f"/workspace/{OWN}")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} >= expected


@pytest.mark.parametrize("command", [
    "awk -F: -v minimum=2 'NR >= minimum { print substr($1, 1, 4) }' input",
    "sed -n -E -e '2,4p' -e '/x/{s/x/y/g; p;}' input",
    "perl -ne 'print if /x/' input",
    """perl -F: -lape 'my $word = lc $_; print join(",", @F) if length $word' input""",
])
def test_text_filter_read_subset_passes(tmp_path, command):
    report = audit_call(tmp_path, shell(command))
    assert report["passed"], report["hits"]


# Hard-won: 3717f81: sed replacement text read as web access
def test_read_only_sed_substitution_treats_quoted_url_as_data(tmp_path):
    command = """sed 's|"https://example.invalid"|"local"|g' notes.txt"""
    report = audit_call(tmp_path, shell(command))
    assert report["passed"], report["hits"]


def test_sed_write_still_flags_protected_path(tmp_path):
    command = "sed 'w /run/hel/git-cache/config' notes.txt"
    report = audit_call(tmp_path, shell(command))
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"git-history-escape", "needs-inspection"}


# Hard-won: 3c1a690: scratch executables were screened as system binaries
@pytest.mark.parametrize(("executable", "passed"), [
    ("./awk", False), ("/tmp/x/awk", False), ("$PWD/bin/awk", False),
    ("awk", True), ("/usr/bin/awk", True),
])
def test_audit_verifies_the_system_tool_path(tmp_path, executable, passed):
    report = audit_calls(tmp_path, [shell(f"{executable} 'NR < 2' input")], cwd=f"/workspace/{OWN}/repo")
    assert report["passed"] is passed
    if not passed:
        assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


def test_bare_name_previously_written_in_the_session_needs_inspection(tmp_path):
    report = audit_calls(tmp_path, [
        shell("printf '%s\\n' '#!/bin/sh' 'cat README.md' > awk"),
        shell("awk 'NR < 2' input"),
    ])
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


# Hard-won: 3c1a690: relative reads after cd missed one side of the workspace boundary
@pytest.mark.parametrize("separator", [";", "\n"])
def test_relative_paths_are_checked_at_both_cd_outcomes(tmp_path, separator):
    report = audit_calls(
        tmp_path, [shell(f"cd src{separator} awk 'NR < 2' ../../{FOREIGN}/x")], cwd=f"/workspace/{OWN}/repo",
    )
    assert not report["passed"]
    assert "other-workspace" in {hit["kind"] for hit in report["hits"]}


def test_unknown_cd_requires_auditable_absolute_operands(tmp_path):
    report = audit_calls(tmp_path, [shell("cd -; cat relative.txt")], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize("command", [
    "mktemp", "date", "env sh -c 'cat README.md'",
    "rg -n -i -g '*.rs' -t rust --type-add 'my:*.rs' --max-depth 2 -uu needle src",
    "rg -n 'ref' src/{left,{right,center}}.rs",
    "nl -ba src/{01..05..2}.rs | sed -n '1,40p'",
    "cat src/{a..e..2}.rs",
    "rg -n '{.mj,other}' src",
])
def test_visible_reads_and_wrappers_pass(tmp_path, command):
    report = audit_call(tmp_path, shell(command))
    assert report["passed"], report["hits"]


@pytest.mark.parametrize(("command", "kind"), [
    ("env sh -c 'cat /run/hel/git-cache/config'", "git-history-escape"),
    ("cat {.mj,src}/store.txt", "memory-or-sessions"),
    ("cat src/{1..1025}.rs", "needs-inspection"),
    ("cat src/{a,other.rs", "needs-inspection"),
    ("ln -s /run/hel/git-cache/config local", "git-history-escape"),
    ("jq '.id' .mj/results.json", "memory-or-sessions"),
])
def test_expansion_and_wrappers_keep_protected_access_visible(tmp_path, command, kind):
    report = audit_call(tmp_path, shell(command))
    assert not report["passed"]
    assert kind in {hit["kind"] for hit in report["hits"]}


def test_title_only_list_uses_recorded_working_directory(tmp_path):
    list_call = {"name": "exec_command", "kind": "read", "title": "List files"}
    prior = shell("pwd")
    prior["rawInput"]["cwd"] = f"/workspace/{OWN}/repo"
    report = audit_calls(tmp_path, [prior, list_call])
    assert report["passed"], report["hits"]
