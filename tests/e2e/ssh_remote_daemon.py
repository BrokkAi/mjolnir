#!/usr/bin/env python3
"""Real localhost OpenSSH control transport with separate client/remote roots."""

import signal
import argparse
import fcntl, os, pathlib, pty, re, select, shlex, socket, struct, subprocess, tempfile, termios, time, json

parser = argparse.ArgumentParser()
parser.add_argument("--mj", required=True)
parser.add_argument("--instance", required=True)
parser.add_argument("--scenario", choices=["control", "http", "dispatch", "first-launch"], required=True)
options = parser.parse_args()

class Terminal:
    """A real pseudo-terminal around the client binary. reedline asks the
    terminal for the cursor position, and waits until something answers."""

    def __init__(self, argv, env):
        self.output = b""
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(argv[0], argv, env)
        # A full-screen dashboard needs a real size; a new pty is 0x0.
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    def pump(self, timeout):
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if not ready:
            return
        try:
            data = os.read(self.fd, 65536)
        except OSError:
            return
        self.output += data
        if b"\x1b[6n" in data:
            os.write(self.fd, b"\x1b[1;1R")
        # The dashboard waits for the terminal's background colour.
        if b"\x1b]11;?" in data:
            os.write(self.fd, b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?1;2c")

    def expect(self, text, seconds=120):
        end = time.time() + seconds
        while text.encode() not in self.output:
            assert time.time() < end, f"timed out waiting for {text!r}: {self.screen()[-1500:]}"
            self.pump(0.2)
        # Later expectations look only at what follows this one.
        self.output = self.output.split(text.encode(), 1)[1]

    def send(self, text):
        os.write(self.fd, text.encode())

    def screen(self):
        return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", self.output.decode(errors="replace"))

    def drain(self):
        """Read what the exited child left in the pty; reads end with an error
        once the buffered output is gone."""
        while True:
            ready, _, _ = select.select([self.fd], [], [], 0.2)
            if not ready:
                return
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            self.output += data

    def wait_exit(self, seconds=60):
        """Whether the process exited, reaping it when it did."""
        end = time.time() + seconds
        while time.time() < end:
            done, _ = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.pid = None
                self.drain()
                return True
            self.pump(0.2)
        return False

    def kill(self):
        if self.pid is None:
            return
        try:
            os.killpg(os.getpgid(self.pid), signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(self.pid, 0)


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
    if options.scenario == "http":
        (p / "config" / "config.toml").write_text(
            'version = 23\n[phone]\nenabled=false\n'
        )
    else:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
            route.connect(("192.0.2.1", 9))
            advertised = route.getsockname()[0]
        assert advertised != "0.0.0.0" and not advertised.startswith("127."), (
            "SSH HTTP scenario requires a specific non-loopback host address"
        )
        subprocess.run(
            [
                "openssl",
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(p / "key.pem"),
                "-out",
                str(p / "cert.pem"),
                "-days",
                "1",
                "-subj",
                "/CN=ssh-http-test",
                "-addext",
                f"subjectAltName=IP:{advertised}",
            ],
            check=True,
            capture_output=True,
        )
        (p / "config" / "config.toml").write_text(
            f'version = 23\n[phone]\nbind="{advertised}:0"\ntailscale_detect=false\n'
            + "tls_cert="
            + json.dumps(str(p / "cert.pem"))
            + "\n"
            + "tls_key="
            + json.dumps(str(p / "key.pem"))
            + "\n"
        )
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
        # Hard-won: acf349c6b: a user's shared ControlMaster connection took
        # over the tunnel's forward, so its listening line never appeared.
        "-o",
        "ControlMaster=auto",
        "-o",
        "ControlPath=" + str(p / "cm"),
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
            # An existing shared master, as a user's ssh_config would create.
            subprocess.run(
                ["ssh", "-M", "-N", "-f", "-o", "ControlPersist=yes", *args, "127.0.0.1"],
                check=True,
                timeout=60,
            )
            if options.scenario == "control":
                subprocess.run(
                    ["cargo", "test", "-p", "brokk-mj-client",
                     "real_ssh_endpoint_discovery", "--", "--ignored", "--nocapture"],
                    env=dict(client_env, MJ_SSH_DISCOVERY_PLACEMENT=str(
                        p / "client-config" / "config.toml")),
                    check=True,
                )
            if options.scenario == "first-launch":
                # The prompt takes only a host name, so an ssh in front of PATH
                # supplies the key, port and host key this sshd needs, and
                # names the remote executable the wrapper stands for.
                shim = p / "shim"
                shim.mkdir()
                # This harness's ControlPath has no host in it, so every host
                # would reuse the shared master; the shim connects directly.
                direct = [
                    a
                    for i, a in enumerate(args)
                    if not a.startswith(("ControlMaster=", "ControlPath="))
                    and not (a == "-o" and args[i + 1].startswith(("ControlMaster=", "ControlPath=")))
                ]
                # The daemon command quotes each word, then quotes the whole
                # line again, so the remote `mj` is the word 'mj' in that form.
                (shim / "ssh").write_text(
                    "#!/usr/bin/env python3\n"
                    "import os, sys\n"
                    f"args = {['/usr/bin/ssh', *direct]!r} + sys.argv[1:]\n"
                    "args[-1] = args[-1].replace(\"'\\\\''mj'\\\\''\", "
                    f"\"'\\\\''{wrapper}'\\\\''\", 1)\n"
                    f"open({str(p / 'ssh-commands.log')!r}, 'a').write(args[-1] + '\\n')\n"
                    f"if 'daemon' in args[-1] and {str(wrapper)!r} not in args[-1]:\n"
                    "    sys.exit('shim: the remote mj was not rewritten; refusing to run the installed mj')\n"
                    "os.execv(args[0], args)\n"
                )
                (shim / "ssh").chmod(0o700)
                fresh_config = p / "fresh-config"
                fresh_data = p / "fresh-data"
                fresh_config.mkdir()
                fresh_data.mkdir()
                (p / "config" / "config.toml").write_text("version = 23\n[phone]\nenabled = false\n")
                env = dict(
                    os.environ,
                    MJ_CONFIG_DIR=str(fresh_config),
                    MJ_DATA_DIR=str(fresh_data),
                    PATH=f"{shim}:{os.environ['PATH']}",
                    TERM="xterm-256color",
                )
                # The runner's own instance must not reach the client under test.
                env.pop("MJ_INSTANCE", None)
                def launch(*arguments, environment=env):
                    return Terminal(
                        [binary, "--instance", options.instance, "--no-splash", *arguments],
                        environment,
                    )

                def assert_no_local_daemon(data):
                    assert not list(data.rglob("daemon.json")), "a local daemon started"
                    assert not list(data.rglob("*.sqlite3")), "a local store was created"

                # Ctrl-C at the menu leaves; it must not take the default and
                # start a local daemon. `simple` and `go` prepare against the
                # daemon, so the question has to come before they run.
                for arguments in ([], ["simple", str(p)], ["go", "--path", "/remote-only"]):
                    terminal = launch(*arguments)
                    try:
                        terminal.expect("Choose 1 or 2")
                        terminal.send("\x03")
                        assert terminal.wait_exit(), terminal.screen()[-1500:]
                    finally:
                        terminal.kill()
                    assert_no_local_daemon(fresh_data)
                    assert not (fresh_config / "config.toml").exists()

                # A usage error comes before a question whose answer is saved.
                terminal = launch("--workspace", "nope", "go", "--path", str(p))
                try:
                    assert terminal.wait_exit(), terminal.screen()[-1500:]
                    assert "omit --workspace" in terminal.screen(), terminal.screen()
                    assert "Choose 1 or 2" not in terminal.screen(), terminal.screen()
                finally:
                    terminal.kill()
                assert_no_local_daemon(fresh_data)
                assert not (fresh_config / "config.toml").exists()

                # Hard-won: round 4 review: after a failed attempt tokio's handler
                # stays installed, and Ctrl-C at the next prompt did nothing.
                terminal = launch()
                try:
                    terminal.expect("Choose 1 or 2")
                    terminal.send("2\r")
                    terminal.expect("SSH destination")
                    terminal.send("no-such-host.invalid\r")
                    terminal.expect("Could not use the daemon on no-such-host.invalid")
                    terminal.expect("SSH destination")
                    terminal.send("\x03")
                    assert terminal.wait_exit(), terminal.screen()[-1500:]
                finally:
                    terminal.kill()
                assert_no_local_daemon(fresh_data)

                # Neither is a configuration copied from another computer, nor a
                # standalone `mj setup`, which writes its marker without one.
                (fresh_data / "setup-state").write_text("complete\n")
                copied_config = p / "copied-config"
                copied_config.mkdir()
                (copied_config / "config.toml").write_text(
                    'version = 14\n[phone]\nenabled = false\n\n[targets.docker]\nkind = "docker"\nmachine = "local"\nimage = "ubuntu:24.04"\n'
                )
                terminal = launch(environment=dict(env, MJ_CONFIG_DIR=str(copied_config)))
                try:
                    terminal.expect("Where should it run?")
                    terminal.send("\x03")
                    assert terminal.wait_exit(), terminal.screen()[-1500:]
                finally:
                    terminal.kill()
                assert_no_local_daemon(fresh_data)

                terminal = launch()
                try:
                    terminal.expect("Where should it run?")
                    terminal.expect("Choose 1 or 2")
                    terminal.send("2\r")
                    # A host that cannot be used saves nothing and asks again.
                    terminal.expect("SSH destination")
                    terminal.send("no-such-host.invalid\r")
                    terminal.expect("Could not use the daemon on no-such-host.invalid")
                    assert not (fresh_config / "config.toml").exists()
                    terminal.expect("SSH destination")
                    terminal.send("127.0.0.1\r")
                    terminal.expect("This computer will use the daemon on 127.0.0.1")
                    saved = (fresh_config / "config.toml").read_text()
                    assert 'runtime = "ssh"' in saved and 'host = "127.0.0.1"' in saved, saved
                    # The dashboard starts on the saved placement: the remote
                    # daemon answers, and quitting ends the process. Only then
                    # is the absence of a local daemon or store meaningful.
                    assert (p / "data" / "daemon.json").exists()
                    terminal.expect("No live session")
                    terminal.send("\x02q")
                    assert terminal.wait_exit(), terminal.screen()[-1500:]
                except BaseException:
                    for log in [*fresh_data.rglob("*.log"), *(p / "data").rglob("*.log")]:
                        print(f"--- {log}\n" + "\n".join(log.read_text(errors="replace").splitlines()[-25:]))
                    raise
                finally:
                    terminal.kill()
                assert_no_local_daemon(fresh_data)
                # Hard-won: first-launch review: a client with no instance asked
                # the remote for `--instance default`, which is a different
                # store from the remote's own plain `mj`. It must send none.
                (p / "ssh-commands.log").write_text("")
                result = subprocess.run(
                    [binary, "daemon", "status"],
                    env=env,
                    text=True,
                    capture_output=True,
                    timeout=120,
                )
                assert result.returncode == 0, result.stderr
                assert "Daemon instance: the remote's own\n" in result.stdout, result.stdout
                sent = (p / "ssh-commands.log").read_text().splitlines()
                assert sent and not any("--instance" in command for command in sent), sent
                print("PASS: first launch asks, rejects a bad host, saves a working one")
            elif options.scenario == "dispatch":
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
                        assert "Daemon host: 127.0.0.1\n" in result.stdout, result.stdout
                        assert f"Daemon instance: {options.instance}\n" in result.stdout, result.stdout
                        assert f"Client config: {p}/client-config/config.toml\n" in result.stdout, result.stdout
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
                        command, env=client_env, text=True, capture_output=True, timeout=120
                    )
                    assert result.returncode == 0, result.stderr
                    assert not list((p / "client-data").rglob("daemon.json"))
                    assert not list((p / "client-data").rglob("*.db"))
                    assert not list((p / "client-data").rglob("api-token*"))
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
                if options.scenario == "http":
                    # The viewer is disabled throughout. Both sessions calls use the
                    # HTTP port forwarded alongside control, and restart rediscovers it.
                    result = subprocess.run(
                        [binary, "--instance", options.instance, "api-token"],
                        env=client_env,
                        text=True,
                        capture_output=True,
                        timeout=120,
                    )
                    assert result.returncode == 0, result.stderr
                print(
                    "PASS: SSH sessions before/after remote restart; no client metadata/store"
                )
        finally:
            subprocess.run(["ssh", "-O", "exit", *args, "127.0.0.1"], capture_output=True)
            if options.scenario == "control":
                subprocess.run([str(wrapper), "--instance", "ssh-discovery-e2e",
                                "daemon", "stop"])
            subprocess.run(
                [str(wrapper), "--instance", options.instance, "daemon", "stop"]
            )
            os.killpg(child.pid, signal.SIGTERM)
            child.wait()
