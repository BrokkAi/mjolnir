"""Exercise the real stdio server and forced-command wrapper with the stub engine."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest

from stub_engine import answer


HERE = Path(__file__).resolve().parent
SESSION_R = "a" * 32
SESSION_LOC = "b" * 32
SESSION_NONE = "c" * 32
COMMIT = "1" * 40
DIFF = "mjolnir/123"


def rpc(method, request_id, **params):
    return json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})


def call(request_id, **arguments):
    return rpc("tools/call", request_id, name="rql_query", arguments=arguments)


def payload(response):
    return json.loads(response["result"]["content"][0]["text"])


class TransportTest(unittest.TestCase):
    def setUp(self):
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        self.root = Path(root.name)
        self.manifest = self.root / "manifest.json"
        self.manifest.write_text(json.dumps({
            "page_rows": 50, "diffs": [{"key": DIFF, "intro": COMMIT}],
        }))
        self.logs = {session: self.root / f"{session}.jsonl" for session in (SESSION_R, SESSION_LOC)}
        self.config = self.root / "config.json"
        self.config.write_text(json.dumps({
            "manifest": str(self.manifest),
            "engine": "rql_mock.stub_engine:answer",
            "sessions": {SESSION_NONE: {"arm": "none"}} | {
                session: {"diff": DIFF, "intro": COMMIT, "arm": arm, "log": str(self.logs[session])}
                for session, arm in ((SESSION_R, "R"), (SESSION_LOC, "R-loc"))
            },
        }))

    def test_install_hook_stages_minimal_assignments_and_revokes_idempotently(self):
        hook = HERE / "install-assignment.py"
        pilot = self.root / "pilot.toml"
        assignments = self.root / "assignments"
        key, pin = self.root / "container-key", self.root / "container-pin"
        auth, transport = self.root / "dedicated_authorized_keys", self.root / "live-transport.json"
        source = self.root / "sshd_public_key"
        source.write_text("ssh-ed25519 dGVzdA== pilot-host\n")
        auth.write_text("# dedicated pilot authorizations\n")
        pilot.write_text(
            '[profiles.fixture]\nkind = "codex"\ncodex_profile = "review_arms_rql"\n'
            '[profiles.fixture.environment]\n'
            f'RQL_SESSION_ASSIGNMENT_DIR = {json.dumps(str(assignments))}\n'
            f'RQL_SSH_CONFIG = {json.dumps(str(HERE / "ssh_config"))}\n'
            f'RQL_SSH_KEY = {json.dumps(str(key))}\n'
            f'RQL_SSH_KNOWN_HOSTS = {json.dumps(str(pin))}\n'
            'RQL_SSH_TARGET = "ssh://pilot@host.containers.internal:22222"\n')
        podman = self.root / "podman"
        podman.write_text("""#!/usr/bin/env python3
import os, sys
verb = sys.argv[1]
if verb == "ps":
    print("container")
elif verb == "inspect":
    print("[]" if "Mounts" in sys.argv[-1] else "test-image")
else:
    assert sys.argv[:4][1:] == ["exec", "--interactive", "container"]
    os.execvp(sys.argv[4], sys.argv[4:])
""")
        ssh = self.root / "ssh"
        ssh.write_text("""#!/usr/bin/env python3
import os, re, shlex, subprocess, sys
from pathlib import Path
sid = sys.argv[-1]
assert sys.argv[-2] == "ssh://pilot@host.containers.internal:22222"
key = sys.argv[sys.argv.index("-i")+1]
assert Path(key).stat().st_mode & 0o777 == 0o600
assert Path(os.environ["RQL_SSH_KNOWN_HOSTS"]).read_text() == \
    "[host.containers.internal]:22222 ssh-ed25519 dGVzdA==\\n"
public = subprocess.check_output(["ssh-keygen", "-y", "-f", key], text=True).strip().split()
lines = Path(os.environ["RQL_AUTHORIZED_KEYS"]).read_text().splitlines()
entry, = [line for line in lines if line.endswith(" rql-pilot-" + sid)]
forced = re.fullmatch(r'restrict,command="([^"]+)" (.+)', entry)
assert forced[2].split()[:2] == public[:2]
command = shlex.split(forced[1])
assert command[-1] == sid
os.execve(command[0], command, {**os.environ, "SSH_ORIGINAL_COMMAND": sid})
""")
        podman.chmod(0o700)
        ssh.chmod(0o700)
        environment = {**os.environ, "PATH": str(self.root) + os.pathsep + os.environ["PATH"],
                       "RQL_PILOT_CONFIG": str(pilot), "RQL_AUTHORIZED_KEYS": str(auth),
                       "RQL_TRANSPORT_CONFIG": str(transport), "RQL_SSH_HOST_PUBLIC_KEY": str(source)}
        r, none = self.root / "R.json", self.root / "none.json"
        identity = {"manifest_sha256": hashlib.sha256(self.manifest.read_bytes()).hexdigest(),
                    "engine_inputs_sha256": "9" * 64}
        r.write_text(json.dumps({"session_id": SESSION_R, "arm": "R", "diff": DIFF, "intro": COMMIT,
                                 "manifest": str(self.manifest), "log": str(self.logs[SESSION_R]), **identity}))
        none.write_text(json.dumps({"session_id": SESSION_NONE, "arm": "none"}))

        def invoke(*args):
            result = subprocess.run([hook, *args], env=environment, text=True, capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            return result.stdout

        for _ in range(2):
            self.assertEqual(json.loads(invoke(r)), {"session_id": SESSION_R, "tools": ["rql_query"]})
            self.assertEqual(json.loads(invoke(none)), {"session_id": SESSION_NONE, "tools": []})
            sessions = json.loads(transport.read_text())["sessions"]
            self.assertEqual(set(sessions), {SESSION_R})
            self.assertEqual({key: sessions[SESSION_R][key] for key in identity}, identity)
            self.assertEqual(len(auth.read_text().splitlines()), 2)
            for session, arm in ((SESSION_R, "R"), (SESSION_NONE, "none")):
                self.assertEqual(json.loads((assignments / f"{session}.json").read_text()),
                                 {"session_id": session, "arm": arm})
        self.assertEqual(invoke("--revoke", r), "")
        self.assertEqual(invoke("--revoke", r), "")
        self.assertEqual(json.loads(transport.read_text())["sessions"], {})
        self.assertEqual(auth.read_text(), "# dedicated pilot authorizations\n")

    def run_server(self, session, *requests, original=None):
        environment = os.environ.copy()
        environment.pop("PYTHONPATH", None)
        environment["SSH_ORIGINAL_COMMAND"] = session if original is None else original
        return subprocess.run(
            [HERE / "serve-rql.sh", self.config, session],
            env=environment,
            input="\n".join(requests) + "\n", text=True, capture_output=True, timeout=15,
        )

    def exchange(self, session, *requests):
        process = self.run_server(session, *requests)
        self.assertEqual(process.returncode, 0, process.stderr)
        return [json.loads(line) for line in process.stdout.splitlines()]

    def run_client(self, session, arm, *requests, preflight=False):
        if arm is not None:
            assignment = self.root / f"{session}.json"
            assignment.write_text(json.dumps({"session_id": session, "arm": arm}))
        # The command runs through the actual forced-command script, with only
        # SSH's argv and SSH_ORIGINAL_COMMAND supplied by this local stand-in.
        ssh = self.root / "ssh"
        ssh.write_text('#!/bin/sh\nfor session do :; done\n'
                       'SSH_ORIGINAL_COMMAND="$session" exec "$TEST_FORCED" "$TEST_CONFIG" "$session"\n')
        ssh.chmod(0o700)
        environment = {**os.environ, "RQL_SESSION_ASSIGNMENT_DIR": str(self.root),
                       "PATH": str(self.root) + os.pathsep + os.environ["PATH"],
                       "RQL_SSH_CONFIG": str(HERE / "ssh_config"),
                       "TEST_FORCED": str(HERE / "serve-rql.sh"), "TEST_CONFIG": str(self.config)}
        for name in ("MJ_SESSION_ID", "RQL_SSH_KEY", "RQL_SSH_KNOWN_HOSTS", "RQL_SSH_TARGET"):
            environment.pop(name, None)
        if session is not None:
            environment["MJ_SESSION_ID"] = session
        if arm in {"R", "R-loc"}:
            key = self.root / "key"
            key.touch(mode=0o600)
            environment.update(RQL_SSH_KEY=str(key), RQL_SSH_KNOWN_HOSTS="known_hosts",
                               RQL_SSH_TARGET="writer@invalid")
        overlay = tomllib.loads((HERE / "review_arms_rql.config.toml").read_text())["mcp_servers"]["rql"]
        command = ([sys.executable, "-B", HERE / "client.py", "--preflight"] if preflight
                   else [overlay["command"], *overlay["args"]])
        process = subprocess.run(command, env=environment, input="\n".join(requests) + "\n",
                                 text=True, capture_output=True, timeout=5)
        self.assertEqual(process.returncode, 0, process.stderr)
        return [json.loads(line) for line in process.stdout.splitlines()]

    def test_none_catalog_stays_connected_without_ssh(self):
        initialize, tools, ping = self.run_client(
            SESSION_NONE, "none", rpc("initialize", 1), rpc("tools/list", 2), rpc("ping", 3),
        )
        self.assertEqual(initialize["result"]["serverInfo"]["name"], "rql_mock")
        self.assertEqual(tools["result"], {"tools": []})
        self.assertEqual(ping["result"], {})
        self.assertEqual(self.run_client(SESSION_NONE, "none", preflight=True),
                         [{"session_id": SESSION_NONE, "tools": []}])
        [host] = self.exchange(SESSION_NONE, rpc("tools/list", 1))
        self.assertEqual(host["result"], {"tools": []})

    def test_assigned_rql_arms_connect_and_list_exactly_one_tool(self):
        for session, arm in ((SESSION_R, "R"), (SESSION_LOC, "R-loc")):
            with self.subTest(arm=arm):
                self.assertEqual(self.run_client(session, arm, preflight=True),
                                 [{"session_id": session, "tools": ["rql_query"]}])

    def test_missing_record_replies_empty_immediately_and_leaves_reconnect_possible(self):
        initialize, tools = self.run_client(
            SESSION_R, None, rpc("initialize", 1), rpc("tools/list", 2), rpc("ping", 3),
        )
        self.assertEqual(initialize["result"]["serverInfo"]["name"], "rql_mock")
        self.assertEqual(tools["result"], {"tools": []})
        self.assertEqual(self.run_client(SESSION_R, "R", preflight=True),
                         [{"session_id": SESSION_R, "tools": ["rql_query"]}])

    def test_profile_discovery_replies_empty_without_session_id(self):
        _, tools, ping = self.run_client(
            None, None, rpc("initialize", 1), rpc("tools/list", 2), rpc("ping", 3),
        )
        self.assertEqual(tools["result"], {"tools": []})
        self.assertEqual(ping["result"], {})

    def test_mcp_overlay_lists_one_tool_and_negotiates_protocol(self):
        config = tomllib.loads((HERE / "review_arms_rql.config.toml").read_text())
        self.assertEqual(list(config["mcp_servers"]), ["rql"])
        init, tools = self.exchange(
            SESSION_R, rpc("initialize", 1, protocolVersion="2025-06-18"), rpc("tools/list", 2),
        )
        self.assertEqual(init["result"]["protocolVersion"], "2025-06-18")
        [tool] = tools["result"]["tools"]
        self.assertEqual(tool["name"], "rql_query")
        self.assertEqual(tool["inputSchema"]["properties"]["page"]["default"], 1)

    def test_pages_cover_all_rows_in_engine_order(self):
        pages = [payload(res) for res in self.exchange(
            SESSION_R, call(1, query="paged"), call(2, query="paged", page=2),
            call(3, query="paged", page=3), call(4, query="paged", page=4),
            call(5, query="empty"),
        )]
        self.assertEqual([(p["page"], p["total"], p["pages"], len(p["rows"])) for p in pages[:3]],
                         [(1, 123, 3, 50), (2, 123, 3, 50), (3, 123, 3, 23)])
        expected = answer(COMMIT, "paged")
        self.assertEqual([row for p in pages[:3] for row in p["rows"]], expected["rows"])
        self.assertEqual([page["coverage"] for page in pages[:4]], [expected["coverage"]] * 4)
        self.assertEqual(pages[3]["status"], "page_out_of_range")
        self.assertEqual(pages[4], {"status": "ok", "rows": [], "total": 0, "page": 1, "pages": 0,
                                    "coverage": {"complete": True, "open": []}})

        self.manifest.write_text(json.dumps({
            "page_rows": 60, "diffs": [{"key": DIFF, "intro": COMMIT}],
        }))
        [response] = self.exchange(SESSION_R, call(6, query="paged", page=3))
        self.assertEqual(payload(response)["rows"], answer(COMMIT, "paged")["rows"][120:])

    def test_location_arm_holds_every_span_in_its_original_row(self):
        reduced = [payload(res) for res in self.exchange(
            SESSION_LOC, call(1, query="paged"), call(2, query="paged", page=2),
            call(3, query="paged", page=3),
        )]
        self.assertEqual([row for page in reduced for row in page["rows"]], [
            {"sites": [
                {"path": "src/source.rs", "start_line": i + 1, "end_line": i + 2},
                {"path": "src/target.rs", "start_line": i + 11, "end_line": i + 13},
                {"path": "src/other.rs", "start_line": i + 21, "end_line": i + 21},
            ]}
            for i in range(123)
        ])
        self.assertEqual([page["total"] for page in reduced], [123, 123, 123])
        coverage = {"complete": False, "open": [
            {"site": {"path": "src/dispatch.rs", "start_line": 4, "end_line": 8}},
            {"site": {"path": "src/depth.rs", "start_line": 12, "end_line": 12}},
        ]}
        self.assertEqual([page["coverage"] for page in reduced], [coverage] * 3)
        records = [json.loads(line) for line in self.logs[SESSION_LOC].read_text().splitlines()]
        self.assertEqual([record["coverage"] for record in records], [coverage] * 3)
        self.assertEqual([record["payload"] for record in records], reduced)

    def test_open_answer_with_zero_rows_delivers_and_logs_coverage(self):
        [response] = self.exchange(SESSION_R, call(1, query="open-empty"))
        delivered = payload(response)
        coverage = answer(COMMIT, "open-empty")["coverage"]
        self.assertEqual(delivered, {"status": "ok", "rows": [], "total": 0, "page": 1,
                                     "pages": 0, "coverage": coverage})
        self.assertIs(delivered["coverage"]["complete"], False)
        self.assertEqual(len(delivered["coverage"]["open"]), 2)
        [record] = [json.loads(line) for line in self.logs[SESSION_R].read_text().splitlines()]
        self.assertEqual(record["coverage"], coverage)
        self.assertEqual(record["payload"], delivered)

    def test_unknown_session_is_refused(self):
        res = self.run_server("f" * 32, rpc("tools/list", 1))
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("unknown session", res.stderr)
        self.assertEqual(res.stdout, "")
        self.assertTrue(all(not log.exists() for log in self.logs.values()))

    def test_client_cannot_select_another_session_or_supply_a_shell_command(self):
        for original in (SESSION_LOC, SESSION_R + " ; echo no"):
            with self.subTest(original=original):
                res = self.run_server(SESSION_R, rpc("tools/list", 1), original=original)
                self.assertEqual(res.returncode, 64)
                self.assertIn("does not match", res.stderr)
                self.assertEqual(res.stdout, "")
        self.assertTrue(all(not log.exists() for log in self.logs.values()))

    def test_success_and_errors_write_one_record_each_with_the_delivered_hash(self):
        responses = self.exchange(
            SESSION_R, call(1, query="demo"), call(2, query="parse-error"),
            call(3, query="engine-error"), call(4, query="demo", page=True),
        )
        records = [json.loads(line) for line in self.logs[SESSION_R].read_text().splitlines()]
        self.assertEqual(len(records), len(responses))
        for record, response in zip(records, responses):
            with self.subTest(id=response["id"]):
                delivered = response["result"]["content"][0]["text"]
                self.assertEqual(record["answer_sha256"], hashlib.sha256(delivered.encode()).hexdigest())
                self.assertEqual(record["payload"], payload(response))
                self.assertEqual(record["coverage"], payload(response).get("coverage"))
                self.assertEqual((record["session"], record["diff"], record["intro"], record["arm"]),
                                 (SESSION_R, DIFF, COMMIT, "R"))
                self.assertTrue(record["time"])
                self.assertGreaterEqual(record["latency_ms"], 0)
        self.assertEqual([r["status"] for r in records],
                         ["ok", "parse_error", "engine_error", "invalid_arguments"])
        self.assertEqual([r["total"] for r in records], [3, None, None, None])
        self.assertEqual([r["query"] for r in records], ["demo", "parse-error", "engine-error", "demo"])
        self.assertEqual(records[0]["page"], 1)
        self.assertEqual([r["result"]["isError"] for r in responses], [False, True, True, True])
        self.assertEqual(payload(responses[1])["message"], "Stub parser rejected the query.")
        self.assertEqual(payload(responses[2])["message"], "The engine could not produce a valid answer.")
        self.assertEqual(records[2]["engine_diagnostic"], "Stub engine could not answer.")

    # Hard-won: 8c9d945: an R-loc engine error disclosed row diagnostics.
    def test_location_arm_engine_error_keeps_diagnostics_host_side(self):
        from server import Server

        details = "rows[3].fields.binding: src/private.rs does not contain the claimed expression"
        # Use a real file because the writer delivery audit takes an flock.
        with self.logs[SESSION_LOC].open("w") as output:
            server = Server(SESSION_LOC, {"intro": COMMIT, "diff": DIFF, "arm": "R-loc"}, 50,
                            lambda *args, **kwargs: {"status": "engine_error", "message": details}, output)
            response = server.call({"name": "rql_query", "arguments": {"query": "demo"}})
            response.pop("_after_write")(True)
            self.assertEqual(payload({"result": response}),
                             {"status": "engine_error", "message": "The engine could not produce a valid answer."})
        self.assertEqual(json.loads(self.logs[SESSION_LOC].read_text())["engine_diagnostic"], details)

    def test_large_jsonrpc_answer_streams_without_truncating_or_blocking_requests(self):
        # Both input and output exceed a pipe buffer before the final ping. subprocess.run
        # drains stdout while feeding stdin so any implementation stall times out.
        request = "q" * (96 * 1024)
        large, after, ping = self.exchange(
            SESSION_R, call(1, query="large"), call(2, query=request), rpc("ping", 3),
        )
        self.assertEqual(payload(large)["rows"], answer(COMMIT, "large")["rows"])
        self.assertGreater(len(large["result"]["content"][0]["text"].encode()), 64 * 1024)
        self.assertEqual(payload(after)["rows"], answer(COMMIT, request)["rows"])
        self.assertEqual(ping, {"jsonrpc": "2.0", "id": 3, "result": {}})

    # Hard-won: a19cfbf: a response logged before a failed write counted as exposure.
    def test_closed_writer_does_not_log_large_response_as_delivered(self):
        process = subprocess.Popen([HERE / "serve-rql.sh", self.config, SESSION_R],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True,
                                   env={**os.environ, "SSH_ORIGINAL_COMMAND": SESSION_R})
        process.stdout.close()
        process.stdin.write(call(1, query="large") + "\n")
        process.stdin.flush()
        process.stdin.close()
        process.wait(timeout=15)
        records = [json.loads(line) for line in self.logs[SESSION_R].read_text().splitlines()]
        self.assertEqual(len(records), 1)
        self.assertIs(records[0]["delivered"], False)


if __name__ == "__main__":
    unittest.main()
