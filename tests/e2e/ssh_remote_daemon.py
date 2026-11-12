#!/usr/bin/env python3
"""Real localhost OpenSSH control transport with separate client/remote roots."""

import signal
import argparse
import os, pathlib, socket, subprocess, tempfile, time, json

parser = argparse.ArgumentParser()
parser.add_argument("--mj", required=True)
parser.add_argument("--instance", required=True)
parser.add_argument("--scenario", choices=["control", "dispatch"], required=True)
options = parser.parse_args()
assert options.instance != "default", "a named isolated instance is required"
binary = str(pathlib.Path(options.mj).resolve())
with tempfile.TemporaryDirectory(prefix="mj-ssh-discovery-") as d:
    p = pathlib.Path(d)
    for k in ["host", "client"]:
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(p / k)],
            check=True,
        )
    (p / "authorized_keys").write_bytes((p / "client.pub").read_bytes())
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    (p / "sshd").write_text(
        f"Port {port}\nListenAddress 127.0.0.1\nHostKey {p}/host\nPidFile {p}/pid\nAuthorizedKeysFile {p}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n"
    )
    (p / "config").mkdir()
    (p / "data").mkdir()
    wrapper = p / "remote-mj"
    wrapper.write_text(
        f'#!/bin/sh\nexport MJ_CONFIG_DIR={p}/config\nexport MJ_DATA_DIR={p}/data\nexec {binary} "$@"\n'
    )
    wrapper.chmod(0o700)
    (p / "known_hosts").write_text(
        "[127.0.0.1]:" + str(port) + " " + (p / "host.pub").read_text()
    )
    args = [
        "-F",
        "/dev/null",
        "-p",
        str(port),
        "-i",
        str(p / "client"),
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "UserKnownHostsFile=" + str(p / "known_hosts"),
    ]
    (p / "client-config").mkdir()
    (p / "client-data").mkdir()
    (p / "client-config" / "config.toml").write_text(
        '[daemon]\nruntime="ssh"\nhost="127.0.0.1"\nremote_command='
        + json.dumps(str(wrapper))
        + "\nssh_args="
        + json.dumps(args)
        + "\n"
    )
    with (p / "log").open("w") as log:
        child = subprocess.Popen(
            ["/usr/sbin/sshd", "-D", "-e", "-f", str(p / "sshd")],
            stderr=log,
            start_new_session=True,
        )
        try:
            for i in range(100):
                try:
                    socket.create_connection(("127.0.0.1", port), 0.1).close()
                    break
                except OSError:
                    time.sleep(0.05)
            client_env = dict(
                os.environ,
                MJ_CONFIG_DIR=str(p / "client-config"),
                MJ_DATA_DIR=str(p / "client-data"),
            )
            if options.scenario == "dispatch":
                base = [binary, "--instance", options.instance]
                subprocess.run(
                    [
                        str(wrapper),
                        "--instance",
                        options.instance,
                        "daemon",
                        "endpoint",
                        "--json",
                    ],
                    check=True,
                    stdout=subprocess.DEVNULL,
                )
                before = json.loads(
                    subprocess.check_output(
                        [
                            str(wrapper),
                            "--instance",
                            options.instance,
                            "daemon",
                            "endpoint",
                            "--json",
                            "--existing",
                        ]
                    )
                )["instance_id"]
                for action in ["status", "restart", "status", "stop", "status"]:
                    result = subprocess.run(
                        base + ["daemon", action],
                        env=client_env,
                        text=True,
                        capture_output=True,
                        timeout=120,
                    )
                    assert result.returncode == 0, result.stderr
                    if action == "restart":
                        after = json.loads(
                            subprocess.check_output(
                                [
                                    str(wrapper),
                                    "--instance",
                                    options.instance,
                                    "daemon",
                                    "endpoint",
                                    "--json",
                                    "--existing",
                                ]
                            )
                        )["instance_id"]
                        assert before != after, (
                            "remote restart did not replace the daemon"
                        )
                    if action == "stop":
                        stopped = subprocess.run(
                            [
                                str(wrapper),
                                "--instance",
                                options.instance,
                                "daemon",
                                "endpoint",
                                "--json",
                                "--existing",
                            ],
                            capture_output=True,
                        )
                        assert stopped.returncode != 0, (
                            "remote daemon still runs after forwarded stop"
                        )
                    if action == "status":
                        assert "Client build:" in result.stdout, result.stdout
                result = subprocess.run(
                    base + ["go"], env=client_env, text=True, capture_output=True
                )
                assert result.returncode != 0
                assert (
                    "SSH daemon launches require an explicit remote path"
                    in result.stderr
                ), result.stderr
                # No client filesystem lookup may reject this opaque remote path.
                result = subprocess.run(
                    base + ["go", "--path", "/remote-only/project"],
                    env=client_env,
                    text=True,
                    capture_output=True,
                )
                assert "needs an interactive terminal" in result.stderr, result.stderr
                assert not list((p / "client-data").rglob("*.db"))
                print("PASS: remote lifecycle dispatch and explicit path handling")
            else:
                command = [binary, "--instance", options.instance, "sessions"]
                for phase in range(2):
                    result = subprocess.run(
                        command,
                        env=client_env,
                        text=True,
                        capture_output=True,
                        timeout=120,
                    )
                    assert result.returncode == 0, result.stderr
                    assert not list((p / "client-data").rglob("daemon.json"))
                    assert not list((p / "client-data").rglob("*.db"))
                    if phase == 0:
                        subprocess.run(
                            [
                                str(wrapper),
                                "--instance",
                                options.instance,
                                "daemon",
                                "restart",
                            ],
                            check=True,
                            timeout=120,
                        )
                print(
                    "PASS: SSH sessions before/after remote restart; no client metadata/store"
                )
        finally:
            subprocess.run(
                [str(wrapper), "--instance", options.instance, "daemon", "stop"]
            )
            os.killpg(child.pid, signal.SIGTERM)
            child.wait()
