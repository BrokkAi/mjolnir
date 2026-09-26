#!/usr/bin/env python3
"""Measure real submission endpoints through authoritative conversation projection.

Uses isolated stores and the reliability lab's fake harness. No live session is used.
"""

import argparse
import json
import math
import os
import pathlib
import subprocess
import re
import reliability_lab
import time
import uuid

from reliability_lab import Lab, ScenarioFailure


class LatencyLab(Lab):
    def environment(self):
        env = super().environment()
        env["RUST_LOG"] += ",mj_chat::latency=debug"
        return env


def measure_tui(lab, tui, session_id, count):
    rows = []
    logs = list((lab.data / "logs").glob("*.log"))
    for index in range(count):
        lab.wait_snapshot(
            lambda s: (lab.session(s, session_id) or {}).get("chat_phase") == "idle",
            "idle before terminal input",
        )
        screen = tui.text().splitlines()
        position = next(
            (
                (line.index("│>") + 3, y + 1)
                for y, line in enumerate(screen)
                if "│>" in line
            ),
            None,
        )
        if position is None:
            raise ScenarioFailure("terminal composer not found: " + tui.text())
        x, y = position
        tui.send(f"\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m".encode())
        offsets = {p: p.stat().st_size for p in logs}
        tui.send(f"\x1b[200~tui-latency-{index}\x1b[201~\r".encode())
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            text = ""
            for path, offset in offsets.items():
                with path.open() as source:
                    source.seek(offset)
                    text += source.read()
            match = re.search(
                r"authoritative submission frame prepared.*?elapsed_ms[=:]\s*([0-9.]+)",
                text,
            )
            if match:
                rows.append({"render_ms": float(match.group(1))})
                break
            time.sleep(0.01)
        else:
            raise ScenarioFailure(
                "no authoritative terminal frame timing: " + text[-1000:] + tui.text()
            )
    return rows


def measure(lab, session_id, surface, count):
    rows = []
    for _ in range(count):
        lab.wait_snapshot(
            lambda value: (
                (lab.session(value, session_id) or {}).get("chat_phase") == "idle"
                and (lab.session(value, session_id) or {}).get("state") == "running"
                and not (lab.session(value, session_id) or {}).get("operation")
            ),
            "idle session",
        )
        command_id = "latency-" + uuid.uuid4().hex
        text = command_id
        started = time.monotonic()
        if surface == "daemon":
            lab.daemon_request(
                {
                    "action": "submit_session_command",
                    "arguments": {
                        "session_id": session_id,
                        "command_id": command_id,
                        "command": {
                            "type": "prompt",
                            "data": {"prompt": [{"type": "text", "text": text}]},
                        },
                    },
                }
            )
        else:
            status, result = lab.request(
                "POST",
                "/api/actions",
                {
                    "action": "prompt",
                    "session_id": session_id,
                    "command_id": command_id,
                    "text": text,
                    "images": [],
                },
            )
            if status != 202:
                raise ScenarioFailure(f"submission failed: {status} {result}")
        accepted = time.monotonic()
        deadline = started + 30
        while time.monotonic() < deadline:
            status, result = lab.request("GET", f"/api/conversations/{session_id}")
            if status == 200 and any(
                e.get("command_id") == command_id for e in result.get("entries", [])
            ):
                break
            time.sleep(0.01)
        else:
            raise ScenarioFailure("authoritative prompt did not appear")
        rows.append(
            {
                "command_id": command_id,
                "accept_ms": (accepted - started) * 1000,
                "project_ms": (time.monotonic() - started) * 1000,
            }
        )
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hel", type=pathlib.Path, required=True)
    parser.add_argument("--count", type=int, default=20)
    parser.add_argument("--worker", type=pathlib.Path, required=True)
    parser.add_argument(
        "--target", choices=["localhost", "morannon-podman"], default="localhost"
    )
    parser.add_argument(
        "--surfaces",
        action="store_true",
        help="Also measure actual browser DOM and terminal frame preparation",
    )
    parser.add_argument("--history-mib", type=int, default=0)
    args = parser.parse_args()
    reliability_lab.TIMEOUT = 120.0
    remote_root = None
    remote_image = None
    session_id = None
    os.environ["MJ_WORKER_BINARY"] = str(args.worker.resolve())
    lab = LatencyLab(args.hel, "prompt-latency", 1095)
    print(f"artifacts={lab.root} runtime={lab.runtime_root}", flush=True)
    try:
        port = lab.prepare()
        if args.history_mib:
            bridge = lab.runtime_root / "fake_acp.py"
            body = bridge.read_text()
            seed = "\n".join(
                [
                    '        if text.endswith("latency-seed"):',
                    f"            for index in range({args.history_mib * 64}):",
                    '                send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "tool_call", "toolCallId": "seed-" + str(index), "title": "history fixture", "kind": "read", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "x" * 16384}}]}}})',
                    "",
                ]
            )
            body = body.replace(
                "        memory_end =", seed + "        memory_end =", 1
            )
            bridge.write_text(body)
        if args.target == "morannon-podman":
            remote_root = "/var/tmp/" + lab.runtime_root.name
            (lab.runtime_root / "gitconfig").write_text(
                '[url "file://'
                + str(lab.project)
                + '"]\n    insteadOf = https://github.com/hel-latency/fixture.git\n'
            )
            (lab.runtime_root / "Containerfile").write_text(
                "FROM ghcr.io/brokkai/mjolnir/agent-dev:latest\nUSER root\n"
                + f"COPY . {lab.runtime_root}/\nCOPY gitconfig /etc/gitconfig\n"
                + f"RUN chmod -R a+rwX {lab.runtime_root}\n"
            )
            archive = lab.root / "remote-fixture.tar"
            subprocess.run(
                ["tar", "-C", str(lab.runtime_root), "-cf", str(archive), "."],
                check=True,
            )
            subprocess.run(
                ["ssh", "-o", "BatchMode=yes", "morannon", "mkdir", "-p", remote_root],
                check=True,
            )
            subprocess.run(
                ["scp", str(archive), "morannon:" + remote_root + "/fixture.tar"],
                check=True,
            )
            subprocess.run(
                [
                    "ssh",
                    "morannon",
                    "tar",
                    "-C",
                    remote_root,
                    "-xf",
                    remote_root + "/fixture.tar",
                ],
                check=True,
            )
            remote_image = "localhost/hel-latency:" + lab.runtime_root.name
            subprocess.run(
                [
                    "ssh",
                    "morannon",
                    "podman",
                    "build",
                    "-q",
                    "-t",
                    remote_image,
                    remote_root,
                ],
                check=True,
            )
            config = lab.config / "config.toml"
            lab.git_output(
                [
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/hel-latency/fixture.git",
                ],
                lab.project,
            )
            body = config.read_text()
            body += f'\n[machines.morannon]\nkind = "ssh"\nhost = "morannon"\nbuild_cache = {{ enabled = false }}\n[targets.morannon-podman]\nkind = "podman"\nmachine = "morannon"\nimage = "{remote_image}"\npull_policy = "never"\n'
            config.write_text(body)
        tui = lab.start_tui("tui-1")
        tui.wait_for("Sessions")
        code, _ = lab.wait_daemon_status(port)
        assert lab.request("POST", "/auth/session", {"code": code})[0] == 204
        workspace = lab.snapshot()["workspaces"][0]["id"]
        status, created = lab.request(
            "POST",
            "/api/actions",
            {
                "action": "new",
                "workspace_id": workspace,
                "profile_id": "fake",
                "bundle_id": "fixture",
                "target_id": args.target,
                "title": "latency-fixture",
                **(
                    {"project_directory": str(lab.project)}
                    if args.target == "localhost"
                    else {}
                ),
            },
        )
        if status != 202:
            raise ScenarioFailure(f"create failed: {status} {created}")
        snapshot = lab.wait_snapshot(
            lambda s: any(
                x.get("chat_phase") == "idle"
                and x.get("state") == "running"
                and not x.get("operation")
                for x in s["sessions"]
            ),
            "ready fixture",
        )
        session_id = snapshot["sessions"][0]["id"]
        if args.history_mib:
            lab.submit_prompt(session_id, "latency-seed")
            time.sleep(1)
            lab.wait_snapshot(
                lambda s: (
                    (lab.session(s, session_id) or {}).get("chat_phase") == "idle"
                ),
                "history seeded",
            )
        results = {}
        for surface in ["web", "daemon"]:
            rows = measure(lab, session_id, surface, args.count)
            results[surface] = rows
            summary = {
                metric: {
                    f"p{p}": sorted(r[metric] for r in rows)[
                        math.ceil(len(rows) * p / 100) - 1
                    ]
                    for p in [50, 95]
                }
                for metric in ["accept_ms", "project_ms"]
            }
            print(surface, json.dumps(summary), flush=True)
        if args.surfaces:
            browser_output = lab.root / "browser-latency.json"
            subprocess.run(
                [
                    "node",
                    str(pathlib.Path(__file__).parent / "web/prompt-latency.cjs"),
                    lab.base_url,
                    code,
                    session_id,
                    str(args.count),
                    str(browser_output),
                ],
                check=True,
            )
            results["browser_dom"] = json.loads(browser_output.read_text())
            results["tui_frame"] = measure_tui(lab, tui, session_id, args.count)
            for surface in ["browser_dom", "tui_frame"]:
                values = sorted(row["render_ms"] for row in results[surface])
                print(
                    surface,
                    json.dumps(
                        {
                            f"p{p}": values[math.ceil(len(values) * p / 100) - 1]
                            for p in [50, 95]
                        }
                    ),
                    flush=True,
                )
        (lab.root / "latency.json").write_text(json.dumps(results, indent=2))
    finally:
        if args.target == "morannon-podman":
            try:
                sessions = lab.snapshot().get("sessions", []) if lab.base_url else []
                lab.cleanup_owned()
                for session in sessions:
                    identifier = session["id"]
                    if not re.fullmatch("[a-f0-9-]+", identifier):
                        raise ScenarioFailure("invalid owned session ID")
                    found = subprocess.run(
                        [
                            "ssh",
                            "morannon",
                            "podman",
                            "ps",
                            "-aq",
                            "--filter",
                            "label=dev.mj.session=" + identifier,
                        ],
                        check=True,
                        capture_output=True,
                        text=True,
                    ).stdout.split()
                    for container in found:
                        if not re.fullmatch("[a-f0-9]+", container):
                            raise ScenarioFailure("invalid container ID")
                        subprocess.run(
                            ["ssh", "morannon", "podman", "rm", "-f", container],
                            check=True,
                        )
                    volumes = subprocess.run(
                        [
                            "ssh",
                            "morannon",
                            "podman",
                            "volume",
                            "ls",
                            "-q",
                            "--filter",
                            "label=dev.mj.session=" + identifier,
                        ],
                        check=True,
                        capture_output=True,
                        text=True,
                    ).stdout.split()
                    for volume in volumes:
                        if not re.fullmatch("[a-zA-Z0-9_.-]+", volume):
                            raise ScenarioFailure("invalid owned volume name")
                        subprocess.run(
                            ["ssh", "morannon", "podman", "volume", "rm", volume],
                            check=True,
                        )
                    subprocess.run(
                        [
                            "ssh",
                            "morannon",
                            "rm",
                            "-rf",
                            "--",
                            ".cache/mjolnir/git/sessions/" + identifier,
                        ],
                        check=True,
                    )
                if remote_image:
                    subprocess.run(
                        ["ssh", "morannon", "podman", "rmi", remote_image], check=True
                    )
                if remote_root:
                    subprocess.run(
                        ["ssh", "morannon", "rm", "-rf", "--", remote_root], check=True
                    )
            finally:
                lab.cleanup_owned()
        else:
            lab.cleanup_owned()


if __name__ == "__main__":
    main()
