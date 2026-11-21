"""Audit recorded commands without treating searched or edited text as access."""
import json
from pathlib import Path
import subprocess
import sys

import pytest

from audit import audit_pilot, audit_reviewer


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


@pytest.fixture
def antigravity_cards():
    # Scrubbed ACP cards from 6dd20fb9, fa36c417, d4fa8e03 and f4896f4e.
    return json.loads(Path(__file__).with_name("fixtures").joinpath("antigravity-tool-cards.json").read_text())


# Hard-won: 8c9d945f3: stored Antigravity cards could bypass the transcript audit
@pytest.mark.parametrize(("card", "hit"), [
    ("web-search", "github-or-web"),
    ("history", "memory-or-sessions"),
    ("native-task", "needs-inspection"),
    ("schedule", "needs-inspection"),
    ("title-only-shell", "needs-inspection"),
    ("structured-shell", "needs-inspection"),
])
def test_antigravity_cannot_pass_forbidden_native_calls(tmp_path, antigravity_cards, card, hit):
    report = audit_call(tmp_path, antigravity_cards[card], cwd=f"/workspace/{OWN}/project")
    assert not report["passed"]
    assert {finding["kind"] for finding in report["hits"]} == {hit}


@pytest.mark.parametrize("override", [
    {"name": "mcp_unowned_read_file"},
    {"name": "mcp__unowned__read_file"},
    {"name": "mcp_mj-agents_handback_more"},
    {"name": "future_native_tool"},
    {"name": "search_files", "kind": "search"},
    {"rawInput": {"unknown": "value"}, "locations": []},
    {"rawInput": {"Path": "AGENTS.md", "unknown": "value"}},
    {"rawInput": None, "locations": [], "title": "Read file 'AGENTS.md'"},
])
def test_antigravity_rejects_opaque_calls(tmp_path, antigravity_cards, override):
    call = {**antigravity_cards["file-read"], **override}
    report = audit_call(tmp_path, call, cwd=f"/workspace/{OWN}/project")
    assert not report["passed"]
    assert {finding["kind"] for finding in report["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize("card", ["file-read", "file-edit"])
def test_antigravity_raw_path_is_screened_independently_of_locations(tmp_path, antigravity_cards, card):
    call = antigravity_cards[card]
    call["rawInput"]["Path"] = f"/workspace/{FOREIGN}/project/AGENTS.md"
    report = audit_call(tmp_path, call)
    assert not report["passed"]
    assert "other-workspace" in {finding["kind"] for finding in report["hits"]}


def test_antigravity_session_metadata_applies_to_legacy_and_pilot_audits(tmp_path, antigravity_cards):
    (tmp_path / "session.json").write_text(json.dumps({"harness_kind": "antigravity"}))
    call = dict(antigravity_cards["file-read"], name="unknown", rawInput={"opaque": True}, locations=[])
    del call["toolCallId"]
    report = audit_call(tmp_path, call, allow_rql=True)
    legacy = audit_reviewer(tmp_path / "transcript.json", OWN)
    assert not report["passed"]
    assert report["hits"] == legacy["hits"]
    assert [finding["kind"] for finding in legacy["hits"]] == ["needs-inspection"]


def test_antigravity_rql_title_cannot_authorize_an_unowned_mcp_call(tmp_path, antigravity_cards):
    call = dict(antigravity_cards["history"], name="mcp_unowned_rql_query", title="rql_query")
    report = audit_call(tmp_path, call, allow_rql=True)
    assert not report["passed"]
    assert [finding["kind"] for finding in report["hits"]] == ["needs-inspection"]


# Hard-won: c5d02f5fd: arbitrary structured fields suppressed the missing read-path hit
def test_unrecognized_raw_read_fields_do_not_supply_path_evidence(tmp_path):
    report = audit_call(tmp_path, {"name": "Read", "title": "Read", "kind": "read",
                                   "rawInput": {"unknown": "value"}})
    assert not report["passed"]
    assert [finding["kind"] for finding in report["hits"]] == ["needs-inspection"]


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
    }}, cwd=writer_checkout(tmp_path))
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
    result = audit_call(tmp_path, call, cwd=writer_checkout(tmp_path))
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


def audit_calls(tmp_path, calls, *, allow_in_place_edits=False, terminal_results=None, **metadata):
    transcript = tmp_path / "transcript.json"
    transcript.write_text(json.dumps([{**metadata, "items": [
        {"seq": seq, "role": "tool", "body": {
            "call": call, "terminal_outputs": (terminal_results or {}).get(seq, []),
        }}
        for seq, call in enumerate(calls, 1)
    ]}]))
    return audit_pilot(transcript, OWN, commits=[PIN], single_commit=True, allow_in_place_edits=allow_in_place_edits)


def shell(command, **result):
    return {"kind": "execute", "rawInput": {"command": command}, **result}


def successful_bash(command):
    return shell(command, name="Bash", status="completed")


# Hard-won: 8c9d945: live engine query voided for running a visibly written script
@pytest.mark.parametrize(("write", "run"), [
    ("mkdir -p /tmp/audit && cat > /tmp/audit/f.py <<'EOF'\nprint('sqlite')\nEOF",
     f'cd /workspace/{OWN}/repo && python3 -I /tmp/audit/f.py "$PWD"'),
    ("printf '%s\\n' '#!/bin/sh' 'cat README.md' > f", "chmod +x f && ./f"),
    ("tee f <<'EOF'\ncat README.md\nEOF", 'sh "$PWD/f"'),
])
def test_audit_screens_scripts_written_visibly_in_an_earlier_call(tmp_path, write, run):
    report = audit_calls(tmp_path, [successful_bash(write), shell(run)], cwd=f"/workspace/{OWN}/repo")
    assert report["passed"], report["hits"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(2, "interpreter-screened")]


# Hard-won: e7cee4b: python3 -I script run voided a visible engine script
@pytest.mark.parametrize(("argument", "extra_hits"), [
    ("$D/answer.json", set()),
    (".mj/store.db", {"memory-or-sessions"}),
])
def test_engine_correlates_a_relative_scratch_script_and_screens_its_argument(tmp_path, argument, extra_hits):
    checkout = f"/workspace/{OWN}/engine-trees"
    command = (
        f"cd {checkout}; D=.rql-engine-{OWN}; cat > $D/build.py <<'EOF' &&\n"
        f"print('safe')\nEOF\npython3 -I $D/build.py {argument}"
    )
    report = audit_calls(tmp_path, [shell(command)], cwd=f"/workspace/{OWN}/repo")
    assert report["passed"] is (not extra_hits)
    assert {hit["kind"] for hit in report["hits"]} == {"interpreter-screened"} | extra_hits


def engine_scratch_script_calls(tmp_path, *, edit="", argument=""):
    checkout = f"/workspace/{OWN}/engine-trees"
    (tmp_path / "pre-inventory.json").write_text(json.dumps({"cwd": checkout, "tracked_paths": ["README.md"]}))
    setup = f"cd {checkout}; D=.rql-engine-{OWN}"
    write = (f"{setup}; cat > $D/build.py <<'EOF'\n"
             f"import json\njson.dump(['old'], open('.rql-engine-{OWN}/answer.json', 'w'))\nEOF")
    preview = """python3 -c "import json; print(json.load(open('$D/answer.json')))" """
    run = f"{setup}\n{edit}python3 -I $D/build.py{argument} && {preview}"
    return checkout, [shell(write), shell(run)]


def test_engine_checkout_cd_does_not_attest_a_script_edited_in_place(tmp_path):
    _, calls = engine_scratch_script_calls(tmp_path, edit="sed -i 's/old/new/' $D/build.py\n")
    report = audit_calls(tmp_path, calls)
    assert not report["passed"]
    hit, = [hit for hit in report["hits"] if hit["kind"] == "needs-inspection"]
    assert hit["seq"] == 2
    assert hit["snippet"] == f"unsupported command python3: python3 -I .rql-engine-{OWN}/build.py"


@pytest.mark.parametrize("suffix", ["", "/."])
def test_engine_cd_to_same_checkout_resolves_a_visibly_written_scratch_script(tmp_path, suffix):
    checkout, calls = engine_scratch_script_calls(tmp_path, argument=' "$PWD"')
    report = audit_calls(tmp_path, calls, cwd=checkout + suffix)
    assert report["passed"], report["hits"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [
        (1, "interpreter-screened"), (2, "interpreter-screened"),
    ]


def test_engine_runs_a_visible_script_in_the_same_success_chain(tmp_path):
    checkout = f"/workspace/{OWN}/engine-trees"
    scratch = f"{checkout}/.rql-engine-{OWN}"
    command = (
        f"cd {scratch} && cat > build_answer.py <<'EOF' &&\n"
        "import json\njson.dump({'rows': []}, open('answer.json', 'w'))\nEOF\n"
        """python3 -I build_answer.py && python3 -I -c "import json; print(json.load(open('answer.json')))" """
    )
    report = audit_calls(tmp_path, [shell(command)], cwd=checkout)
    assert report["passed"], report["hits"]
    assert {hit["kind"] for hit in report["hits"]} == {"interpreter-screened"}


def test_ambiguous_relative_write_cannot_attest_an_absolute_script(tmp_path):
    checkout = f"/workspace/{OWN}/engine-trees"
    report = audit_calls(tmp_path, [
        shell(f"cd {checkout}; cat > build.py <<'EOF' &&\nprint('safe')\nEOF\npython3 -I {checkout}/build.py"),
    ], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize(("before", "connector", "after"), [
    ("", "\n", "python3 -I build.py"),
    ("", ";", "python3 -I build.py"),
    ("true || ", " && ", "python3 -I build.py"),
    ("", " && ", "true; python3 -I build.py"),
])
def test_unguarded_run_does_not_use_contents_from_a_potentially_failed_write(tmp_path, before, connector, after):
    command = f"{before}cat > build.py <<'EOF'{connector}"
    if connector == "\n":
        command += f"print('safe')\nEOF\n{after}"
    else:
        command += f"{after}\nprint('safe')\nEOF"
    report = audit_calls(tmp_path, [successful_bash(command)], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize(("tracked", "passed"), [
    ([".rql-engine-" + OWN + "/build.py"], False),
    (None, False),
    ([], True),
])
def test_unconfirmed_heredoc_run_requires_a_proven_new_script_path(tmp_path, tracked, passed):
    checkout = f"/workspace/{OWN}/engine-trees"
    if tracked is not None:
        (tmp_path / "pre-inventory.json").write_text(json.dumps({"tracked_paths": tracked}))
    path = f"{checkout}/.rql-engine-{OWN}/build.py"
    command = f"cat > {path} <<'EOF'\nprint('visible')\nEOF\npython3 -I {path}"
    report = audit_calls(tmp_path, [shell(command, name="Bash", status="failed")], cwd=checkout)
    assert report["passed"] is passed
    assert {hit["kind"] for hit in report["hits"]} == {"interpreter-screened" if passed else "needs-inspection"}


@pytest.mark.parametrize("first", ["print('first')", "open('/run/hel/git-cache/config')"])
def test_every_visible_version_is_screened_before_an_unconfirmed_run(tmp_path, first):
    checkout = f"/workspace/{OWN}/engine-trees"
    (tmp_path / "pre-inventory.json").write_text('{"tracked_paths":[]}')
    path = f"{checkout}/.rql-engine-{OWN}/build.py"
    report = audit_calls(tmp_path, [
        shell(f"cat > {path} <<'EOF'\n{first}\nEOF"),
        shell(f"cat > {path} <<'EOF'\nprint('second')\nEOF"),
        shell(f"python3 -I {path}"),
    ], cwd=checkout)
    if first.startswith("print"):
        assert report["passed"], report["hits"]
        assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [
            (1, "interpreter-screened"), (2, "interpreter-screened"), (3, "interpreter-screened"),
        ]
    else:
        assert not report["passed"]
        assert (1, "git-history-escape") in {(hit["seq"], hit["kind"]) for hit in report["hits"]}


@pytest.mark.parametrize("opaque_write", [
    "cat seed.py > {path}",
    """python3 -I -c 'open("{path}", "w").write("print(1)")'""",
])
def test_opaque_earlier_write_prevents_the_new_script_exception(tmp_path, opaque_write):
    checkout = f"/workspace/{OWN}/engine-trees"
    (tmp_path / "pre-inventory.json").write_text('{"tracked_paths":[]}')
    path = f"{checkout}/.rql-engine-{OWN}/build.py"
    report = audit_calls(tmp_path, [
        shell(opaque_write.format(path=path)),
        shell(f"cat > {path} <<'EOF'; python3 -I {path}\nprint('visible')\nEOF"),
    ], cwd=checkout)
    assert not report["passed"]
    assert (2, "needs-inspection") in {(hit["seq"], hit["kind"]) for hit in report["hits"]}


@pytest.mark.parametrize(("name", "status", "raw_output", "terminal_exit", "passed"), [
    ("Bash", "completed", None, 0, True),
    ("Bash", "failed", None, 1, False),
    ("Bash", None, None, None, False),
    ("exec_command", "completed", {"exit_code": 0}, None, True),
    ("exec_command", "completed", {"exit_code": 1}, None, False),
    ("exec_command", "completed", {"exit_code": None}, None, False),
    ("exec_command", "completed", None, 0, True),
])
def test_separate_script_run_requires_recorded_write_success(tmp_path, name, status, raw_output, terminal_exit, passed):
    write = shell("cat > build.py <<'EOF'\nprint('safe')\nEOF", name=name, status=status, rawOutput=raw_output)
    results = {} if terminal_exit is None else {1: [{"terminal_id": "write", "exit_code": terminal_exit}]}
    report = audit_calls(tmp_path, [write, shell("python3 -I build.py")], terminal_results=results)
    assert report["passed"] is passed
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [
        (2, "interpreter-screened" if passed else "needs-inspection"),
    ]


@pytest.mark.parametrize("exit_code", [0, 1, None])
def test_codex_rollout_exit_status_confirms_a_wrapped_write(tmp_path, exit_code):
    cwd = f"/workspace/{OWN}/repo"
    directory = tmp_path / "codex-rollout"
    directory.mkdir()
    item = {"type": "CommandExecution", "id": "exec-write", "cwd": f"file://{cwd}",
            "command": ["/bin/bash", "-lc", "cat > f.py <<'EOF'\nprint('safe')\nEOF"],
            "exit_code": exit_code}
    row = {"type": "event_msg", "payload": {"type": "item_completed", "item": item}}
    (directory / "rollout-fixture.jsonl").write_text(json.dumps(row) + "\n")
    report = audit_calls(tmp_path, [
        {"name": "exec_command", "toolCallId": "exec-write", "status": "completed", "kind": "execute"},
        shell("python3 -I f.py"),
    ], cwd=cwd)
    assert report["passed"] is (exit_code == 0)
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [
        (2, "interpreter-screened" if exit_code == 0 else "needs-inspection"),
    ]


@pytest.mark.parametrize("suffix", ["; true", " || true"])
def test_whole_call_success_does_not_confirm_a_write_whose_status_was_masked(tmp_path, suffix):
    write = successful_bash(f"cat > build.py <<'EOF'{suffix}\nprint('safe')\nEOF")
    report = audit_calls(tmp_path, [write, shell("python3 -I build.py")])
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(2, "needs-inspection")]


def test_script_evidence_does_not_flow_into_a_heredoc_failure_branch(tmp_path):
    checkout = f"/workspace/{OWN}/engine-trees"
    report = audit_calls(tmp_path, [
        shell(f"cd {checkout}; cat > build.py <<'EOF' ||\nprint('safe')\nEOF\npython3 -I build.py"),
    ], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


def test_engine_script_remains_opaque_after_an_in_place_edit(tmp_path):
    checkout = f"/workspace/{OWN}/engine-trees"
    scratch = f"{checkout}/.rql-engine-{OWN}"
    report = audit_calls(tmp_path, [
        successful_bash(f"cd {scratch} && cat > build_answer.py <<'EOF'\nprint('before')\nEOF"),
        shell(f"cd {scratch} && sed -i 's/before/after/g' build_answer.py && python3 -I build_answer.py"),
    ], cwd=checkout)
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(2, "needs-inspection")]


def test_cd_failure_cannot_run_a_script_recorded_only_in_the_target_directory(tmp_path):
    checkout = f"/workspace/{OWN}/engine-trees"
    scratch = f"{checkout}/.rql-engine-{OWN}"
    report = audit_calls(tmp_path, [
        shell(f"cd {scratch} && cat > build_answer.py <<'EOF'\nprint('safe')\nEOF\npython3 -I build_answer.py"),
    ], cwd=checkout)
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize(("program", "source"), [
    ("python3 -IBuEsSO -W ignore::DeprecationWarning -Xutf8 -X dev", "print('safe')"),
    ("node --no-warnings --no-addons", "console.log('safe')"),
])
def test_recorded_script_accepts_safe_interpreter_flags(tmp_path, program, source):
    report = audit_calls(tmp_path, [
        shell(f"cat > answer <<'EOF' &&\n{source}\nEOF\n{program} answer"),
    ], cwd=f"/workspace/{OWN}/repo")
    assert report["passed"], report["hits"]
    assert {hit["kind"] for hit in report["hits"]} == {"interpreter-screened"}


@pytest.mark.parametrize("run", [
    "python3 -I unrecorded.py",
    "python3 -X presite=other answer",
    "python3 -W ignore::other.Warning answer",
    "node --require=./preload.js answer",
])
def test_interpreter_options_do_not_hide_unrecorded_code(tmp_path, run):
    report = audit_calls(tmp_path, [
        shell(f"cat > answer <<'EOF' &&\nprint('safe')\nEOF\n{run}"),
    ], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


@pytest.mark.parametrize("mutation", [
    shell("printf 'changed' >> f.py"),
    shell("cp other.py f.py"),
    shell("mv other.py f.py"),
    {"kind": "edit", "rawInput": {"file_path": "f.py", "new_string": "changed"}},
])
def test_script_changed_after_visible_write_is_opaque(tmp_path, mutation):
    report = audit_calls(tmp_path, [
        successful_bash("cat > f.py <<'EOF'\nprint('safe')\nEOF"),
        mutation,
        shell("python3 f.py"),
    ], cwd=writer_checkout(tmp_path))
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(3, "needs-inspection")]


def test_audit_checks_protected_effects_in_recorded_script_and_resolved_home(tmp_path):
    report = audit_calls(tmp_path, [
        shell("printf '%s\\n' \"open('.mj/store.db')\" > f.py && python3 f.py"),
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
        successful_bash("printf '%s\\n' \"bash -c 'sh f'\" > f"),
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


# Hard-won: 23e9afc: title-only list inferred a target from the working directory
@pytest.mark.parametrize("raw", [None, {"cwd": f"/workspace/{OWN}/repo"}])
def test_title_only_list_requires_target_evidence(tmp_path, raw):
    list_call = {"name": "exec_command", "kind": "read", "title": "List files", "rawInput": raw}
    prior = shell("pwd")
    prior["rawInput"]["cwd"] = f"/workspace/{OWN}/repo"
    report = audit_calls(tmp_path, [prior, list_call], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(2, "needs-inspection")]


@pytest.mark.parametrize("evidence", [
    {"name": "list_files", "locations": [{"path": "src"}]},
    {"rawInput": {"argv": ["ls"]}},
])
def test_list_with_recorded_target_or_visible_default_passes(tmp_path, evidence):
    report = audit_calls(tmp_path, [{"name": "exec_command", "kind": "read", "title": "List files", **evidence}],
                         cwd=writer_checkout(tmp_path))
    assert report["passed"], report["hits"]


# Hard-won: 23e9afc: wrapped file writers left a stale recorded script
@pytest.mark.parametrize("overwrite", [
    "env cp incoming.py script.py", "timeout 2 env tee script.py",
    # Hard-won: f0beee63: xxd left overwritten script evidence active.
    "xxd bytes.bin script.py", "xxd -r -c 16 hex.txt script.py",
])
def test_writes_invalidate_recorded_scripts(tmp_path, overwrite):
    report = audit_calls(tmp_path, [
        successful_bash("cat > script.py <<'PY'\nprint('recorded content')\nPY"),
        shell(overwrite),
        shell("python3 script.py"),
    ])
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [(3, "needs-inspection")]


# Hard-won: 23e9afc: relative symlink target was screened from the shell directory
@pytest.mark.parametrize("destination", ["../alias", f"-t /workspace/{OWN}"])
def test_relative_symbolic_link_target_uses_link_directory(tmp_path, destination):
    report = audit_calls(tmp_path, [shell(f"ln -s ../{FOREIGN}/x {destination}")], cwd=f"/workspace/{OWN}/repo")
    assert not report["passed"]
    assert "other-workspace" in {hit["kind"] for hit in report["hits"]}


def test_relative_link_with_unknown_destination_directory_requires_inspection(tmp_path):
    report = audit_call(tmp_path, shell("ln -s relative-file link-name"))
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


# Hard-won: 23e9afc: dash-prefixed file operands were skipped after --
@pytest.mark.parametrize("command", ["cat -- -state.db", "sort -- --compress-state.db", "xxd input -state.db"])
def test_dash_prefixed_path_after_end_of_options_is_screened(tmp_path, command):
    report = audit_call(tmp_path, shell(command))
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"memory-or-sessions"}


def summarized_ls(tmp_path, cwd, *, command="ls", title="List files", rollout=True, raw=None):
    transcript = {"toolCallId": "exec-terminal-1", "name": "exec_command", "title": title,
                  "kind": "read", "status": "completed"}
    if raw is not None:
        transcript["rawInput"] = raw
    if rollout:
        directory = tmp_path / "codex-rollout"
        directory.mkdir()
        item = {"type": "CommandExecution", "id": transcript["toolCallId"],
                "command": ["/bin/bash", "-lc", command], "cwd": f"file://{cwd}",
                "parsed_cmd": [{"type": "list_files", "path": "summary only"}]}
        row = {"type": "event_msg", "payload": {"type": "item_completed", "item": item}}
        (directory / "rollout-fixture.jsonl").write_text(json.dumps(row) + "\n")
    return audit_calls(tmp_path, [transcript], cwd=f"/workspace/{OWN}")


# Hard-won: 23e9afc: summarized list events hid their target
def test_summarized_ls_uses_rollout_workdir_instead_of_placement(tmp_path):
    report = summarized_ls(tmp_path, "/run/hel/git-cache")
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"git-history-escape"}


def test_summarized_ls_in_the_session_workspace_passes(tmp_path):
    report = summarized_ls(tmp_path, f"/workspace/{OWN}/repo")
    assert report["passed"], report["hits"]


@pytest.mark.parametrize(("command", "title"), [
    ("ls --ignore README.md mj-cli", "List files in 'README.md'"),
    ("ls --ignore /run/hel/git-cache mj-cli", "List files in '/run/hel/git-cache'"),
])
def test_summarized_ls_uses_real_operands_instead_of_the_parsed_title(tmp_path, command, title):
    report = summarized_ls(tmp_path, f"/workspace/{OWN}/repo", command=command, title=title)
    assert report["passed"], report["hits"]


@pytest.mark.parametrize("title", ["List files", "List files in 'mj-cli'", "Read file 'README.md'"])
def test_summarized_exec_without_rollout_needs_inspection(tmp_path, title):
    report = summarized_ls(tmp_path, f"/workspace/{OWN}/repo", title=title, rollout=False)
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {"needs-inspection"}


# Hard-won: f0beee63: visible argv suppressed the rollout working directory.
@pytest.mark.parametrize(("directory", "expected"), [
    ({}, "git-history-escape"),
    ({"workdir": f"/workspace/{OWN}"}, "needs-inspection"),
])
def test_visible_command_uses_matching_rollout_directory(tmp_path, directory, expected):
    command = "cat config"
    report = summarized_ls(tmp_path, "/run/hel/git-cache", command=command,
                           raw={"command": command, **directory})
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == {expected}


def writer_checkout(tmp_path, tracked=()):
    if tracked is not None:
        (tmp_path / "pre-inventory.json").write_text(json.dumps({"tracked_symlinks": list(tracked)}))
    return f"/workspace/{OWN}/repo"


# Hard-won: c5d02f5: writer in-place brief edit voided
@pytest.mark.parametrize("command", [
    "sed -i 's/package_function_value.rs:178/package_function_value.rs:168/' brief.md",
    "sed -i 's/that collection receives balanced lexical definitions before member-owner metadata is compared/that collection receives balanced lexical definitions regardless of the member-owner metadata comparison/; s/fact_resolution.rs:14350/fact_resolution.rs:14348/' brief.md",
    "sed -i '/^- Attribute-name recognition/d; /^- The new fixtures/d; /^- The type branch derives/d' brief.md",
    "perl -i -pe 's/\\xC2\\xAD//g' brief.md",
])
def test_suffix_free_in_place_brief_edits_pass(tmp_path, command):
    report = audit_calls(tmp_path, [shell(command)], cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert report["passed"], report["hits"]
    assert {hit["kind"] for hit in report["hits"]} == {"in-place-edit"}


@pytest.mark.parametrize(("command", "expected"), [
    ("sed -i.bak 's/a/b/' brief.md", {"needs-inspection"}),
    ("sed -i''backup 's/a/b/' brief.md", {"needs-inspection"}),
    ("sed --in-place=backup 's/a/b/' brief.md", {"needs-inspection"}),
    ("sed -Ibackup 's/a/b/' brief.md", {"needs-inspection"}),
    (f"sed -i 's/a/b/' /workspace/{FOREIGN}/x", {"needs-inspection", "other-workspace"}),
    ("sed -i 's/a/b/' '*.md'", {"needs-inspection"}),
    ("sed -i 's/a/b/' /tmp/x", {"needs-inspection"}),
    ("sed -i 's/a/b/e' brief.md", {"needs-inspection"}),
    ("perl -i.orig -pe 's/a/b/' brief.md", {"needs-inspection"}),
    ("perl -i -n -e 's/a/b/' brief.md", {"needs-inspection"}),
])
def test_in_place_edits_keep_the_workspace_and_read_subset_checks(tmp_path, command, expected):
    report = audit_calls(tmp_path, [shell(command)], cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert not report["passed"]
    assert {hit["kind"] for hit in report["hits"]} == expected


def test_in_place_edits_count_separate_commands_in_one_call(tmp_path):
    command = "sed -i 's/a/b/' brief.md; perl -i -lpe 's/b/c/' brief.md"
    report = audit_calls(tmp_path, [
        shell("head brief.md"),
        shell(command),
    ], cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert report["passed"], report["hits"]
    assert [(hit["seq"], hit["edit_index"]) for hit in report["hits"] if hit["kind"] == "in-place-edit"] == [
        (2, 0), (2, 1),
    ]


# Hard-won: 59c3dcb: an in-place edit through an alias left the original script cached.
def test_in_place_edit_invalidates_scripts_recorded_under_another_path(tmp_path):
    report = audit_calls(tmp_path, [
        successful_bash("cat > real/script.sh <<'EOF'\ncat README.md\nEOF"),
        shell("sed -i 's/README.md/notes.md/' notes.md"),
        shell("sh real/script.sh"),
    ], cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert not report["passed"]
    assert [(hit["seq"], hit["kind"]) for hit in report["hits"]] == [
        (2, "in-place-edit"), (3, "needs-inspection"),
    ]


# Hard-won: 59c3dcb: the writer's in-place edit exception applied to every session.
@pytest.mark.parametrize("writer", [True, False])
def test_in_place_edit_requires_writer_permission(tmp_path, writer):
    command = """bash -lc "sed -i 's/a/b/' brief.md" """
    report = audit_calls(tmp_path, [shell(command)], cwd=writer_checkout(tmp_path),
                         **({"allow_in_place_edits": True} if writer else {}))
    assert report["passed"] is writer
    expected = "in-place-edit" if writer else "needs-inspection"
    assert {hit["kind"] for hit in report["hits"]} == {expected}


# Hard-won: 129387d: in-place edit through a symlink escaped the workspace
def test_in_place_edit_rejects_symlinked_directory_created_in_session(tmp_path):
    report = audit_calls(tmp_path, [shell("ln -s /tmp alias; sed -i 's/a/b/' alias/file")],
                         cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert [hit["kind"] for hit in report["hits"]] == ["needs-inspection"]


# Hard-won: b7a6b8c: in-place edit through a relocated link
@pytest.mark.parametrize(("relocate", "target", "reason"), [
    ("ln -s /tmp alias; mv alias other", "other/file", "bare filename"),
    ("ln -s /tmp/f x; mv x brief.md", "brief.md", "link handling"),
])
def test_in_place_edit_rejects_relocated_link(tmp_path, relocate, target, reason):
    report = audit_calls(tmp_path, [shell(f"{relocate}; sed -i 's/a/b/' {target}")],
                         cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert not report["passed"]
    assert [hit["kind"] for hit in report["hits"]] == ["needs-inspection"]
    assert reason in report["hits"][0]["snippet"]


@pytest.mark.parametrize("prior", [
    "cp -P x y",
    "cp -vR src dest",
    "cp --preserve=mode,links x y",
    "cp --no-deref x y",
    "env mv x y",
    "mv x y",
    "ln x y",
    """python3 -c 'import os; os.symlink("/tmp/f", "x")'""",
    """python3 -c 'from os import rename as move; move("x", "y")'""",
    """python3 -c 'from pathlib import Path; Path("x").replace("y")'""",
    "unresolvable-command",
])
def test_root_in_place_edit_rejects_earlier_link_handling(tmp_path, prior):
    report = audit_calls(tmp_path, [shell(prior), shell("sed -i 's/a/b/' brief.md")],
                         cwd=writer_checkout(tmp_path), allow_in_place_edits=True)
    assert not report["passed"]
    edit_hits = [hit for hit in report["hits"] if hit["seq"] == 2]
    assert [hit["kind"] for hit in edit_hits] == ["needs-inspection"]
    assert "link handling or an unresolved command" in edit_hits[0]["snippet"]


@pytest.mark.parametrize(("tracked", "command", "allowed"), [
    (["alias"], "sed -i 's/a/b/' alias/file", False),
    (["brief.md"], "sed -i 's/a/b/' brief.md", False),
    (["brief.md"], """perl -i -pe 's/a/b/' " brief.md" """, False),
    ([], "sed -i 's/a/b/' src/file", False),
    ([], "cd src && sed -i 's/a/b/' brief.md", False),
    (None, "sed -i 's/a/b/' brief.md", False),
    (["sub/brief.md"], "sed -i 's/a/b/' brief.md", True),
])
def test_in_place_edit_requires_root_and_tracked_link_inventory(tmp_path, tracked, command, allowed):
    report = audit_calls(tmp_path, [shell(command)], cwd=writer_checkout(tmp_path, tracked),
                         allow_in_place_edits=True)
    assert report["passed"] is allowed
    assert {hit["kind"] for hit in report["hits"]} == ({"in-place-edit"} if allowed else {"needs-inspection"})


ENGINE_CWD = f"/workspace/{OWN}/engine-trees"


def symlink_inventory(tmp_path, targets):
    (tmp_path / "pre-inventory.json").write_text(json.dumps({
        "cwd": ENGINE_CWD, "tracked_symlinks": list(targets), "tracked_symlink_targets": targets,
    }))


def record_engine_runtime(tmp_path, version="2.1.326", *, session_id=OWN):
    (tmp_path / "engine-runtime.json").write_text(json.dumps({
        "session_id": session_id, "claude_code_version": version,
    }))


# Hard-won: 92aaa39c0: an in-repo CLAUDE.md symlink voided every root search
def test_engine_root_glob_accepts_internal_symlinks(tmp_path):
    symlink_inventory(tmp_path, {"CLAUDE.md": "AGENTS.md"})
    call = {"name": "Glob", "kind": "search", "rawInput": {"pattern": ".rql-engine-*/answer.json"}}
    assert audit_call(tmp_path, call, engine=True)["passed"]


@pytest.mark.parametrize("target", ["/etc", f"{ENGINE_CWD}/src", "../outside"])
def test_file_tools_and_root_globs_reject_escaping_symlink_targets(tmp_path, target):
    symlink_inventory(tmp_path, {"alias": target})
    for name, kind, raw in (
        ("Read", "read", {"file_path": "alias/input"}),
        ("Glob", "search", {"pattern": "**/*.rs"}),
    ):
        report = audit_call(tmp_path, {"name": name, "kind": kind, "rawInput": raw}, engine=True)
        assert not report["passed"]
        assert any("tracked symlink" in hit["snippet"] for hit in report["hits"])


def test_symlink_chain_resolves_relative_to_each_link(tmp_path):
    call = {"name": "Read", "kind": "read", "rawInput": {"file_path": "docs/first/input"}}
    symlink_inventory(tmp_path, {"docs/first": "../links/next", "links/next": "../src"})
    assert audit_call(tmp_path, call, engine=True)["passed"]
    symlink_inventory(tmp_path, {"docs/first": "../links/next", "links/next": "../.mj"})
    report = audit_call(tmp_path, call, engine=True)
    assert [finding["kind"] for finding in report["hits"]] == ["memory-or-sessions"]


def test_glob_finds_an_escaping_link_under_an_internal_directory_alias(tmp_path):
    symlink_inventory(tmp_path, {"src/alias": "../other", "other/escape": "/etc"})
    call = {"name": "Glob", "kind": "search", "rawInput": {"path": "src", "pattern": "**/example"}}
    report = audit_call(tmp_path, call, engine=True)
    assert not report["passed"]
    assert any("tracked symlink" in hit["snippet"] for hit in report["hits"])
    call["rawInput"]["pattern"] = "alias/safe/*"
    assert audit_call(tmp_path, call, engine=True)["passed"]


@pytest.fixture
def engine_cache_links(tmp_path):
    prefix = ".agents/records/tract-warm-latency"
    links = {f"{prefix}/{name}/target": "/mnt/optane/mbx-targets/v1/scrubbed" for name in (
        "codeql-h2h/fixture",
        "rust-sql-taint-gap/fixtures/api-wrapper-sink",
        "rust-sql-taint-gap/fixtures/direct-postgres",
        "rust-sql-taint-gap/fixtures/minimal-postgres",
    )}
    symlink_inventory(tmp_path, {**links, "CLAUDE.md": "AGENTS.md"})
    return tuple(links)


# Hard-won: 1d9846951: a scratch-only Glob voided a live turn because of unrelated cache links
@pytest.mark.parametrize(("pattern", "passed"), [
    (".rql-engine-*/*", True),
    ("**/*.rs", False),
    (".agents/records/**", False),
    ("*/records/**", False),
    (".agents/records/tract-warm-latency/codeql-h2h/*/target/*", False),
    (".agents/records/tract-warm-latency/codeql-h2h/*/notes/*", True),
    (".agents/records/tract-warm-latency/codeql-h2h/*", True),
    (".rql-engine-[a-z]/*", False),
    ("answers.json", False),
])
def test_engine_glob_checks_only_reachable_links(tmp_path, engine_cache_links, pattern, passed):
    call = {"name": "Glob", "kind": "search", "rawInput": {"pattern": pattern, "path": ENGINE_CWD},
            "locations": [{"path": ENGINE_CWD}]}
    report = audit_call(tmp_path, call, engine=True)
    assert report["passed"] is passed, report["hits"]
    if not passed:
        assert any(hit["kind"] == "needs-inspection" for hit in report["hits"])


def test_engine_grep_checks_explicit_links_and_leaves_descendants_to_no_follow(tmp_path, engine_cache_links):
    record_engine_runtime(tmp_path)
    link = engine_cache_links[0]
    call = {"name": "Grep", "kind": "search", "rawInput": {"pattern": "data", "path": ENGINE_CWD}}
    assert audit_call(tmp_path, call, engine=True)["passed"]
    call["rawInput"]["glob"] = f"{link}/sample.rs"
    assert audit_call(tmp_path, call, engine=True)["passed"]
    for path in (link, f"{link}/sample.rs"):
        call["rawInput"]["path"] = path
        report = audit_call(tmp_path, call, engine=True)
        assert not report["passed"]
        assert any("tracked symlink" in hit["snippet"] for hit in report["hits"])


@pytest.mark.parametrize(("version", "session"), [(None, OWN), ("2.1.327", OWN), ("2.1.326", FOREIGN)])
def test_engine_grep_requires_verified_session_runtime(tmp_path, engine_cache_links, version, session):
    if version is not None:
        record_engine_runtime(tmp_path, version, session_id=session)
    call = {"name": "Grep", "kind": "search", "rawInput": {"pattern": "data", "path": ENGINE_CWD}}
    report = audit_call(tmp_path, call, engine=True)
    assert not report["passed"]
    assert any("unverified Claude Code version" in hit["snippet"] for hit in report["hits"])


@pytest.mark.parametrize("extra", [{"follow": True}, {"glob": "{unclosed"}])
def test_engine_grep_rejects_unrecognized_arguments(tmp_path, engine_cache_links, extra):
    record_engine_runtime(tmp_path)
    call = {"name": "Grep", "kind": "search", "rawInput": {"pattern": "data", **extra}}
    report = audit_call(tmp_path, call, engine=True)
    assert not report["passed"]
    assert any("unrecognized engine Grep input" in hit["snippet"] for hit in report["hits"])


def test_engine_glob_checks_claudes_rewritten_root_prefix(tmp_path):
    symlink_inventory(tmp_path, {"src/cache": "/mnt/optane/mbx-targets"})
    call = {"name": "Glob", "kind": "search", "rawInput": {"path": f"{ENGINE_CWD}/src", "pattern": "src/*"}}
    report = audit_call(tmp_path, call, engine=True)
    assert not report["passed"]
    assert any("tracked symlink" in hit["snippet"] for hit in report["hits"])


def test_cyclic_and_overlong_link_chains_need_inspection(tmp_path):
    call = {"name": "Read", "kind": "read", "rawInput": {"file_path": "link0/file"}}
    symlink_inventory(tmp_path, {"link0": "link1", "link1": "link0"})
    assert not audit_call(tmp_path, call, engine=True)["passed"]
    links = {f"link{i}": f"link{i + 1}" for i in range(40)}
    symlink_inventory(tmp_path, links)
    assert audit_call(tmp_path, call, engine=True)["passed"]
    symlink_inventory(tmp_path, {**links, "link40": "src"})
    assert not audit_call(tmp_path, call, engine=True)["passed"]


def test_old_inventory_without_targets_keeps_the_strict_glob_check(tmp_path):
    (tmp_path / "pre-inventory.json").write_text(json.dumps({
        "cwd": ENGINE_CWD, "tracked_symlinks": ["CLAUDE.md"],
    }))
    call = {"name": "Glob", "kind": "search", "rawInput": {"pattern": "**/*.rs"}}
    report = audit_call(tmp_path, call, engine=True)
    assert not report["passed"]
    assert any("tracked symlink" in hit["snippet"] for hit in report["hits"])


def test_git_inventory_records_the_pinned_symlink_blob_bytes(tmp_path):
    def git(*args):
        return subprocess.check_output(["git", "-C", str(tmp_path), *args], stderr=subprocess.PIPE)

    git("init", "-q")
    (tmp_path / "first").symlink_to("source\n")
    (tmp_path / "second").symlink_to("source\n")
    (tmp_path / "source\n").write_text("data")
    git("add", "--", "first", "second", "source\n")
    git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--no-gpg-sign", "-qm", "snapshot")
    git("update-ref", "refs/mj/session-start/fixture", "HEAD")
    # A modified link on disk must not replace the target from the pinned blob.
    (tmp_path / "first").unlink()
    (tmp_path / "first").symlink_to("../outside")
    request = {"mode": "pin", "cwd": str(tmp_path), "base": "HEAD"}
    inventory = subprocess.check_output(
        [sys.executable, "-B", str(Path(__file__).with_name("inventory.py"))], input=json.dumps(request).encode(),
    )
    assert json.loads(inventory)["tracked_symlink_targets"] == {"first": "source\n", "second": "source\n"}


# Hard-won: dd934cd54: structured paths followed tracked links outside the checkout
@pytest.mark.parametrize(("name", "kind", "key", "engine"), [
    ("read_file", "read", "Path", False),
    ("write_file", "edit", "Path", False),
    ("Read", "read", "file_path", False),
    ("Write", "edit", "file_path", False),
    ("apply_patch", "edit", "patch", False),
    ("Read", "read", "file_path", True),
    ("Write", "edit", "file_path", True),
    ("Edit", "edit", "file_path", True),
    ("Grep", "search", "path", True),
    ("Glob", "search", "path", True),
])
def test_structured_file_tools_require_link_free_paths(tmp_path, name, kind, key, engine):
    if name == "Grep":
        record_engine_runtime(tmp_path)
    (tmp_path / "pre-inventory.json").write_text(json.dumps({
        "cwd": ENGINE_CWD, "tracked_symlinks": ["src/outside"],
    }))
    for path, passes in [("src/outside/data", False), ("src/outside/../data", False),
                         ("src/outside-sibling/data", True)]:
        raw = {"patch": f"*** Begin Patch\n*** Add File: {path}\n+text\n*** End Patch"} if key == "patch" else {key: path}
        if kind == "search":
            raw["pattern"] = "*.rs"
        report = audit_call(tmp_path, {"name": name, "kind": kind, "title": name, "rawInput": raw},
                            engine=engine)
        assert report["passed"] == passes, report["hits"]
        if not passes:
            assert any("tracked symlink" in finding["snippet"] for finding in report["hits"])


def test_engine_glob_checks_path_patterns_and_grep_checks_its_search_root(tmp_path):
    record_engine_runtime(tmp_path)
    (tmp_path / "pre-inventory.json").write_text(json.dumps({
        "cwd": ENGINE_CWD, "tracked_symlinks": ["src/outside"],
    }))
    call = {"name": "Glob", "kind": "search", "rawInput": {"pattern": "src/*/data"}}
    report = audit_call(tmp_path, call, engine=True)
    assert not report["passed"]
    assert any("tracked symlink" in finding["snippet"] for finding in report["hits"])
    call = {"name": "Grep", "kind": "search", "rawInput": {"pattern": "value", "glob": "*.rs"}}
    assert audit_call(tmp_path, call, engine=True)["passed"]
    call["rawInput"]["path"] = "src/outside"
    assert not audit_call(tmp_path, call, engine=True)["passed"]


def test_missing_inventory_blocks_access_outside_verified_engine_scratch(tmp_path):
    call = {"name": "Read", "kind": "read", "rawInput": {"file_path": "src/example.rs"}}
    report = audit_call(tmp_path, call, cwd=ENGINE_CWD, engine=True)
    assert not report["passed"]
    assert [finding["snippet"] for finding in report["hits"]] == [
        "path requires tracked symlink inventory: src/example.rs",
    ]
    call["rawInput"]["file_path"] = f".rql-engine-{OWN}/answer.json"
    assert audit_call(tmp_path, call, cwd=ENGINE_CWD, engine=True)["passed"]
    assert not audit_call(tmp_path, call, cwd=ENGINE_CWD)["passed"]


@pytest.mark.parametrize("prior", ["ln -s /tmp alias", f"git checkout --detach {PIN}",
                                    "python3 -c 'print(1)'"])
def test_link_handling_invalidates_subsequent_structured_paths(tmp_path, prior):
    call = {"name": "Read", "kind": "read", "rawInput": {"file_path": "alias/input"}}
    report = audit_calls(tmp_path, [shell(prior), call], cwd=writer_checkout(tmp_path))
    assert not report["passed"]
    assert any(finding["seq"] == 2 and "link handling" in finding["snippet"] for finding in report["hits"])


@pytest.mark.parametrize("name", [
    "Bash", "PowerShell", "Agent", "TaskStop", "mcp__rql__rql_query", "ListMcpResourcesTool",
    "NotebookEdit", "WebFetch", "read", "FutureTool", None,
])
def test_engine_audit_requires_the_native_tool_name(tmp_path, name):
    # Neither a plausible title nor a safe-looking input can authorize a tool.
    call = {"title": "Read file 'README.md'", "kind": "read",
            "rawInput": {"file_path": "README.md"}}
    if name is not None:
        call["name"] = name
    (tmp_path / "pre-inventory.json").write_text(json.dumps({"tracked_symlinks": []}))
    assert audit_call(tmp_path, call, cwd=ENGINE_CWD)["passed"]
    report = audit_call(tmp_path, call, cwd=ENGINE_CWD, engine=True)
    assert not report["passed"]
    assert any(hit["kind"] == "needs-inspection" for hit in report["hits"])


@pytest.mark.parametrize("name,key,escape", [
    ("Glob", "pattern", "../../*.rs"),
    ("Glob", "pattern", "/etc/*"),
    ("Grep", "glob", "safe/../../*.rs"),
    ("Grep", "glob", "/etc/*"),
])
def test_engine_audit_inspects_search_pattern_escapes(tmp_path, name, key, escape):
    record_engine_runtime(tmp_path)
    call = {"name": name, "kind": "search", "rawInput": {"pattern": "*.rs", key: escape}}
    (tmp_path / "pre-inventory.json").write_text(json.dumps({"tracked_symlinks": []}))
    report = audit_call(tmp_path, call, cwd=ENGINE_CWD, engine=True)
    assert not report["passed"]
    assert report["hits"] == [{"seq": 1, "kind": "needs-inspection", "where": "input",
                               "snippet": f"unverified {name} {key}: {escape!r}"}]


# Hard-won: 3a68d9227: Grep's content regex was audited as a path, voiding searches for path-like text
@pytest.mark.parametrize("pattern", ["/etc/passwd", "../", "include_str!\\(\"../"])
def test_engine_audit_treats_grep_pattern_as_content(tmp_path, pattern):
    record_engine_runtime(tmp_path)
    (tmp_path / "pre-inventory.json").write_text(json.dumps({"cwd": ENGINE_CWD, "tracked_symlinks": []}))
    call = {"name": "Grep", "kind": "search", "rawInput": {"pattern": pattern, "path": ENGINE_CWD}}
    assert audit_call(tmp_path, call, engine=True)["passed"]


@pytest.mark.parametrize("name", ["Glob", "Grep"])
def test_engine_audit_resolves_search_without_path_from_recorded_cwd(tmp_path, name):
    record_engine_runtime(tmp_path)
    call = {"name": name, "kind": "search", "rawInput": {"pattern": "main"}}
    assert not audit_call(tmp_path, call, engine=True)["passed"]
    (tmp_path / "pre-inventory.json").write_text(json.dumps({"cwd": ENGINE_CWD, "tracked_symlinks": []}))
    assert audit_call(tmp_path, call, engine=True)["passed"]
    call["locations"] = [{"path": f"/workspace/{OWN}/another-checkout/README.md"}]
    assert not audit_call(tmp_path, call, engine=True)["passed"]


@pytest.fixture
def engine_write_without_input():
    # Scrubbed from retired engine session 2cac763b, turn 307, seq 333.
    fixture = Path(__file__).with_name("fixtures") / "engine-write-without-input.json"
    return json.loads(fixture.read_text())


# Hard-won: 3a68d9227: an engine Write with one verified scratch target voided a live turn
@pytest.mark.parametrize(("name", "status"), [("Write", "failed"), ("Edit", "completed")])
def test_engine_certifies_location_only_scratch_write(tmp_path, engine_write_without_input, name, status):
    call = engine_write_without_input
    call.update(name=name, status=status, title=call["title"].replace("Write", name, 1),
                _meta={"claudeCode": {"toolName": name}})
    assert audit_call(tmp_path, call, cwd=ENGINE_CWD, engine=True)["passed"]


@pytest.mark.parametrize("override", [
    {"locations": []},
    {"locations": [{"path": f"{ENGINE_CWD}/answer.json"}], "title": "Write answer.json"},
    {"locations": [{"path": f"{ENGINE_CWD}/.rql-engine-{OWN}/answer.json"},
                   {"path": f"{ENGINE_CWD}/another.json"}]},
    {"title": "Write another.json"},
    {"rawOutput": {"path": f"{ENGINE_CWD}/other.json"}},
    {"content": [{"type": "diff", "path": f"{ENGINE_CWD}/other.json"}]},
    {"name": "Read", "title": "Read", "_meta": {"claudeCode": {"toolName": "Read"}}},
])
def test_engine_needs_one_unambiguous_scratch_write_location(tmp_path, engine_write_without_input, override):
    call = {**engine_write_without_input, **override}
    report = audit_call(tmp_path, call, cwd=ENGINE_CWD, engine=True)
    assert not report["passed"]
    assert [hit["kind"] for hit in report["hits"]] == ["needs-inspection"]


def test_engine_location_only_write_still_checks_tracked_symlinks(tmp_path, engine_write_without_input):
    (tmp_path / "pre-inventory.json").write_text(json.dumps({
        "cwd": ENGINE_CWD, "tracked_symlinks": [f".rql-engine-{OWN}/answer.json"],
    }))
    report = audit_call(tmp_path, engine_write_without_input, engine=True)
    assert not report["passed"]
    assert any("tracked symlink" in finding["snippet"] for finding in report["hits"])


@pytest.fixture
def saved_engine_result():
    # Scrubbed engine session 9f8e1b11, turn 47, seqs 147 and 162.
    fixture = Path(__file__).with_name("fixtures") / "engine-saved-tool-result.json"
    return json.loads(fixture.read_text())


def audit_engine_result(tmp_path, calls, *, session_id=OWN):
    symlink_inventory(tmp_path, {})
    record_engine_runtime(tmp_path)
    transcript = tmp_path / "transcript.json"
    transcript.write_text(json.dumps([{"session_id": session_id, "items": [
        {"seq": seq, "role": "tool", "body": {"call": call}}
        for seq, call in enumerate(calls, 1)
    ]}]))
    return audit_pilot(transcript, OWN, commits=[PIN], engine=True)


# Hard-won: 1d9846951: reading the engine's own saved Grep output voided a live turn
def test_engine_can_read_its_own_previously_audited_tool_result(tmp_path, saved_engine_result):
    report = audit_engine_result(tmp_path, saved_engine_result)
    assert report["passed"], report["hits"]


@pytest.mark.parametrize(("original", "replacement"), [
    ("toolu_01SavedGrep.txt", "toolu_unmatched.txt"),
    ("tool-results", "memory"),
    (f"/profiles/{OWN}/", f"/profiles/{FOREIGN}/"),
    (f"hel-aaaaaaaaaaaaaaaa-{OWN}", f"hel-aaaaaaaaaaaaaaaa-{FOREIGN}"),
    ("11111111-1111-4111-8111-111111111111", "not-a-uuid"),
    ("tool-results", "../11111111-1111-4111-8111-111111111111/tool-results"),
])
def test_engine_saved_result_enforces_path_and_tool_id(tmp_path, saved_engine_result, original, replacement):
    read = saved_engine_result[1]
    path = read["rawInput"]["file_path"].replace(original, replacement)
    read["rawInput"]["file_path"] = path
    read["locations"][0]["path"] = path
    report = audit_engine_result(tmp_path, saved_engine_result)
    assert not report["passed"]
    assert any(hit["seq"] == 2 for hit in report["hits"])


@pytest.mark.parametrize("source", ["later", "rejected", "other-session"])
def test_engine_saved_result_requires_prior_audit_in_the_same_session(tmp_path, saved_engine_result, source):
    grep, read = saved_engine_result
    read_seq = 1 if source == "later" else 2
    calls = [read, grep] if source == "later" else [grep, read]
    if source == "rejected":
        grep["rawInput"]["path"] = "/etc"
    report = audit_engine_result(tmp_path, calls, session_id=FOREIGN if source == "other-session" else OWN)
    assert not report["passed"]
    assert any(hit["seq"] == read_seq for hit in report["hits"])


@pytest.mark.parametrize("name", ["Grep", "Glob", "Write", "Edit"])
def test_engine_saved_results_are_read_only_and_disallow_search(tmp_path, saved_engine_result, name):
    read = saved_engine_result[1]
    read["name"] = name
    if name in {"Grep", "Glob"}:
        read["kind"] = "search"
        read["rawInput"] = {"path": str(Path(read["rawInput"]["file_path"]).parent), "pattern": "*"}
    else:
        read["kind"] = "edit"
    report = audit_engine_result(tmp_path, saved_engine_result)
    assert not report["passed"]
    assert any(hit["seq"] == 2 for hit in report["hits"])
