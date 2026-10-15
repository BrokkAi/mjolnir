# Windows controller to Linux SSH bare target (#1238)

Validated on 2026-10-15 with a Windows 11 controller and the Windows OpenSSH
client (`C:\Windows\System32\OpenSSH\ssh.exe`, OpenSSH_for_Windows 9.5p2). The
daemon was started from PowerShell, so `ssh` and `scp` resolved to Windows
OpenSSH rather than Git for Windows' MSYS build. Connection sharing is
Unix-only, so every `ssh` and `scp` ran on its own connection through the
unshared lease.

## Setup

- Linux host: a Debian bookworm container running `sshd` and published on
  `127.0.0.1:2238`. The container had user `dev`, Git, Node.js 22, and a
  checkout at `/home/dev/project`. That checkout's `origin` was
  `dev@localhost:/home/dev/upstream.git`. SSH bare targets require a network
  remote, and the check for it runs on the host.
- Worker: a static `x86_64-unknown-linux-musl` `mj-worker`, built in a
  `rust:1.96.0-bookworm` container from the same working tree and passed with
  `MJ_WORKER_BINARY`. `mj doctor` reported that it matched the controller.
- Instance: `--instance win1238`. Machine `lab` used `kind = "ssh"`,
  `identity_file`, and `extra_args = ["-p", "2238", "-o",
  "UserKnownHostsFile=..."]`. Target `lab-bare` was `kind = "bare"`. Profile
  `claude` was the user's Claude home.

## Flow and result

1. `mj doctor`: SSH access, free space on the host, and worker availability
   were all ready.
2. `mj new --target lab-bare --project-directory /home/dev/project`, with a
   prompt to create and commit `hello.txt`. The session provisioned an
   isolated clone at `/home/dev/project/.mj/clones/<id>`, staged the worker
   and profile, attached the relay over `ssh`, and finished the Claude turn
   with the commit. The source checkout was unchanged.
3. `mj daemon stop` and a restart: the detached worker kept running, and the
   new daemon reattached to it.
4. `mj checkpoint`: the recovery copy was captured on the host, downloaded
   with `scp`, and verified.
5. `mj suspend --acknowledge-unpublished-work`: the worker, clone, profile
   stage, and processes were removed from the host.
6. `mj resume`: the archive and restore spec were uploaded with `scp`, and the
   session was restored. In a second turn the agent showed the earlier commit
   and remembered the file it had created.
7. `mj destroy`: the host had no worker, profile, or clone directories left,
   and no `dev` processes.

## Windows defects found and fixed

- Remote POSIX paths were judged by Windows rules. `Path::is_absolute`
  rejected `/home/dev/project` when a session was admitted. The same check
  rejected the remote Git root, the remote home, mbx and tool-cache paths, and
  stored project locations.
- `Path::join` and `PathBuf::push` inserted `\` into target paths. Examples:
  the isolated clone (`/home/dev/project\.mj\clones\<id>`), the Git exclude
  file (the host gained files literally named
  `\home\dev\project\.git\info\exclude`), and the project-memory replica, which
  made checkpoint capture fail with "project memory replica is outside the
  harness home". Other affected paths were the subagent MCP server and
  mailbox hook commands, nested Muse/OpenCode homes, container workspaces,
  network clone directories, move staging, and tool-cache directories. SSH
  bare and EC2 worker and profile roots are relative to the remote home, so
  `mj_core::target_path::join` now keeps POSIX separators for any base without
  a Windows drive or UNC prefix.
- Windows `scp` cannot write a file that another process holds open. The
  checkpoint download now keeps only the temporary file's reserved name
  (`TempPath`), not an open handle.
- `canonicalize` returns `\\?\C:\...`, and `scp` reads that as the host
  `\\?\C` ("hostname contains invalid characters"). `scp` arguments now name
  controller paths without the verbatim prefix.

## Not covered

- Connection sharing on Windows. It stays Unix-only by design.
- Git for Windows' MSYS `ssh`/`scp`. That build comes first on PATH in Git
  Bash shells, but it was not used here.
- SSH Podman/Docker and EC2 targets. Their path fixes come from the same audit
  but were not run live from Windows.
