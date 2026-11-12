#!/usr/bin/env python3
"""Real CLI/control exchange; takes the dev mj binary as its sole argument."""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="mj-endpoint-") as root:
        config = Path(root) / "config"
        data = Path(root) / "data"
        config.mkdir()
        env = dict(os.environ, MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data))

        def run(*args):
            return subprocess.run(
                [binary, "--instance", "endpoint-e2e", *args],
                env=env, capture_output=True, text=True, check=False,
            )

        missing = run("daemon", "endpoint", "--json", "--existing")
        assert missing.returncode != 0 and not missing.stdout, missing.stderr
        assert "no running daemon" in missing.stderr, missing.stderr
        assert not (data / "daemon.json").exists()
        path = config / "config.toml"
        path.write_text('[daemon]\nruntime = "ssh"\nhost = "unused"\n')
        recursive = run("daemon", "endpoint", "--json")
        assert recursive.returncode != 0 and not recursive.stdout, recursive.stderr
        assert str(path) in recursive.stderr, recursive.stderr
        assert "choose host, docker or podman on this machine" in recursive.stderr
        path.unlink()
        try:
            started = run("daemon", "endpoint", "--json")
            assert started.returncode == 0, started.stderr
            endpoint = json.loads(started.stdout)
            assert set(endpoint) == {
                "protocol_version", "build_version", "control_address", "token", "instance_id"
            }
            existing = run("daemon", "endpoint", "--json", "--existing")
            assert existing.returncode == 0, existing.stderr
            assert json.loads(existing.stdout) == endpoint
            host, port = endpoint["control_address"].rsplit(":", 1)
            with socket.create_connection((host, int(port))) as stream:
                request = json.dumps({
                    "protocol_version": endpoint["protocol_version"], "request_id": 1,
                    "token": endpoint["token"], "action": {"action": "ping"},
                }).encode()
                stream.sendall(struct.pack("!I", len(request)) + request)

                def read(size):
                    result = b""
                    while len(result) < size:
                        chunk = stream.recv(size - len(result))
                        assert chunk, "daemon closed response"
                        result += chunk
                    return result

                response = json.loads(read(struct.unpack("!I", read(4))[0]))
                assert response["result"]["Ok"]["reply"] == "pong", response
            print("PASS: missing/existing, recursion refusal, startup JSON, authenticated Ping")
        finally:
            stopped = run("daemon", "stop")
            assert stopped.returncode == 0, stopped.stderr


if __name__ == "__main__":
    main()
