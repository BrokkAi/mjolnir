#!/usr/bin/env python3
"""Compare sequential profile transfers to a real loopback SSH Podman target."""

import argparse
import getpass
import hashlib
import json
import os
import pathlib
import re
import shlex
import shutil
import socket
import subprocess
import time

from reliability_lab import Lab


def ssh_target(root, git_config, log):
    home = root / "remote"
    home.mkdir()
    for key in ("host", "client"):
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / key)],
            check=True,
        )
    (root / "authorized_keys").write_bytes((root / "client.pub").read_bytes())
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    shell = root / "remote-shell"
    shell.write_text(
        f'#!/bin/sh\ncd {shlex.quote(str(home))}\nexec /bin/sh -c "$SSH_ORIGINAL_COMMAND"\n'
    )
    shell.chmod(0o700)
    storage = subprocess.check_output(
        ["podman", "info", "--format", "{{.Store.ConfigFile}}"], text=True
    ).strip()
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    (root / "sshd.conf").write_text(
        f"""Port {port}
ListenAddress 127.0.0.1
HostKey {root}/host
PidFile {root}/pid
AuthorizedKeysFile {root}/authorized_keys
StrictModes no
UsePAM no
PasswordAuthentication no
ForceCommand {shell}
SetEnv HOME={home} XDG_RUNTIME_DIR={runtime} CONTAINERS_STORAGE_CONF={storage} GIT_CONFIG_GLOBAL={git_config}
Subsystem sftp /usr/lib/openssh/sftp-server
"""
    )
    (root / "known_hosts").write_text(
        f"[127.0.0.1]:{port} " + (root / "host.pub").read_text()
    )
    extra_args = [
        "-F", "/dev/null", "-p", str(port),
        "-o", "IdentitiesOnly=yes",
        "-o", "StrictHostKeyChecking=yes",
        "-o", f"UserKnownHostsFile={root}/known_hosts",
    ]
    sshd = subprocess.Popen(
        ["/usr/sbin/sshd", "-D", "-e", "-f", str(root / "sshd.conf")],
        stderr=log,
    )
    try:
        deadline = time.monotonic() + 5
        while True:
            try:
                socket.create_connection(("127.0.0.1", port), 0.1).close()
                break
            except OSError:
                if sshd.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("isolated sshd did not start")
                time.sleep(0.05)
    except BaseException:
        sshd.kill()
        sshd.wait()
        raise
    return sshd, home, extra_args


def transfer_log(lab):
    return [
        line
        for logfile in sorted((lab.data / "logs").glob("mj-daemon-*.log"))
        for line in logfile.read_text().splitlines()
        if "send profile content" in line and "bytes_offered" in line
    ]


def metric(line, key):
    return int(re.search(rf"\b{key}=(\d+)", line)[1])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mj", type=pathlib.Path, required=True)
    parser.add_argument("--worker", type=pathlib.Path, required=True)
    parser.add_argument("--base-image", default="ghcr.io/brokkai/mjolnir/session-base:latest")
    args = parser.parse_args()
    os.environ["MJ_WORKER_BINARY"] = str(args.worker.resolve())
    lab = Lab(args.mj, "profile-transfer", 1, watchdog=False)
    print(f"artifacts={lab.root}", flush=True)
    root = lab.runtime_root
    image = f"localhost/mj-profile-transfer:{root.name}"
    image_built = False
    sshd = None
    sessions = []

    def mj(*argv, check=True):
        # Older binaries passed through --mj may still read Brokk Observe
        # settings; keep the operator's configuration away from fixtures.
        env = {key: value for key, value in lab.environment().items() if not key.startswith("OBSERVE_")}
        env["OBSERVE_CONFIG"] = str(root / "observe-unconfigured")
        return subprocess.run(
            [lab.hel, "--instance", "send-only-missing", *argv],
            env=env, text=True, capture_output=True, timeout=180, check=check,
        )

    try:
        lab.prepare()
        (lab.profile / "rules").mkdir()
        payload = os.urandom(1024 * 1024)
        (lab.profile / "rules" / "random-payload.bin").write_bytes(payload)
        private_login = b"{}\n"
        (lab.profile / "auth.json").write_bytes(private_login)
        # The network identity stays realistic, but neither the Podman host nor
        # the container leaves loopback to obtain this repository.
        remote = "https://github.com/mj-profile-transfer/fixture.git"
        lab.git_output(["remote", "add", "origin", remote], lab.project)
        git_config = root / "remote-gitconfig"
        git_config.write_text(
            f'[user]\nname = Profile Transfer\nemail = test@example.invalid\n'
            f'[url "file://{lab.project}"]\ninsteadOf = {remote}\n'
        )

        context = root / "build"
        context.mkdir()
        for name in ("project", "cache", "bin"):
            shutil.copytree(root / name, context / name, symlinks=True)
        for name in ("fake_acp.py", "remote-gitconfig"):
            shutil.copy2(root / name, context)
        (context / "Containerfile").write_text(
            f"FROM {args.base_image}\n"
            f"USER root\nCOPY . {root}/\nCOPY remote-gitconfig /etc/gitconfig\n"
            f"RUN chmod -R a+rwX {root}\n"
        )
        subprocess.run(["podman", "build", "--pull=never", "-q", "-t", image, context], check=True)
        image_built = True

        with (lab.root / "sshd.log").open("w") as log:
            sshd, remote_home, extra_args = ssh_target(root, git_config, log)
        config = lab.config / "config.toml"
        fixture_bin = str(root / "bin")
        config.write_text(config.read_text().replace(
            json.dumps(fixture_bin), json.dumps(fixture_bin + ":/usr/bin:/bin")
        ))
        with config.open("a") as output:
            output.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")
            output.write(
                f'\n[machines.loopback]\nkind = "ssh"\nhost = "127.0.0.1"\n'
                f'user = {json.dumps(getpass.getuser())}\nidentity_file = {json.dumps(str(root / "client"))}\n'
                f'extra_args = {json.dumps(extra_args)}\nbuild_cache = {{ enabled = false }}\n'
                f'[targets.loopback]\nkind = "podman"\nmachine = "loopback"\n'
                f'image = {json.dumps(image)}\npull_policy = "never"\n'
            )
        mj("workspaces", "create", "Profile transfer")
        store = remote_home / ".cache/mjolnir/profile-content"
        cached = []
        for label in ("cold", "warm"):
            result = mj(
                "--workspace", "Profile transfer", "new", "--profile", "fake", "--target", "loopback", "--bundle", "fixture",
                "--title", label, "--no-github-credentials", "--no-github-events",
                "--subagents", "none", "--review", "off",
                "--cpus", "2", "--memory-gib", "4", "--json",
            )
            session = json.loads(result.stdout)["session_id"]
            sessions.append(session)
            mj("wait", "--session", session, "--timeout", "120")
            cached.append({entry.name for entry in store.iterdir() if not entry.name.startswith(".")})

        lines = transfer_log(lab)
        assert len(lines) == 2, lines
        cold, warm = lines
        print("\n".join(lines))
        assert metric(cold, "bytes_sent") > 1024 * 1024, lines
        assert metric(warm, "bytes_sent") < 8192, lines
        # Session-specific instructions can change. Every warm object body
        # sent must have produced a new digest, while the large body is reused.
        first, second = cached
        assert hashlib.sha256(payload).hexdigest() in first & second
        assert metric(warm, "objects_sent") == len(second - first), lines
        assert metric(warm, "objects_reused") > 0, lines
        secret_hash = hashlib.sha256(private_login).hexdigest()
        assert secret_hash not in second
    except subprocess.CalledProcessError as error:
        if error.stdout or error.stderr:
            print(error.stdout, error.stderr, flush=True)
        raise
    finally:
        for session in sessions:
            mj("destroy", "--session", session, check=False)
        mj("daemon", "stop", check=False)
        if sshd is not None:
            sshd.terminate()
            sshd.wait()
        if image_built:
            containers = subprocess.check_output(
                ["podman", "ps", "-aq", "--filter", f"ancestor={image}"], text=True
            ).split()
            if containers:
                subprocess.run(["podman", "rm", "-f", *containers], check=True)
            subprocess.run(["podman", "image", "rm", image], check=True)
        if (lab.data / "logs").is_dir():
            shutil.copytree(lab.data / "logs", lab.root / "daemon-logs")
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
