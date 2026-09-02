# Mjolnir

Mjolnir (`mj`) is a terminal control plane for coding agents. It runs many long-lived
agent sessions — Codex, Claude Code, Kimi Code, Grok Build, and DeepSeek Harness — in disposable
isolated environments, keeps them working while you are away, and gives you one
dashboard for their sessions, quotas, and credentials. Agents connect through
the [Agent Client Protocol](https://agentclientprotocol.com) (ACP).

Mjolnir 2.0 is a new product generation: the session control plane replaces
the 1.x interactive client. The last 1.x release remains available at the
[v1.17.0 tag](https://github.com/BrokkAi/mjolnir/releases/tag/v1.17.0).

## Why Mjolnir

Running one coding agent in one terminal works. Running six of them across two
Codex accounts and a Claude account, on three machines, overnight, does not.
Mjolnir exists for the second case.

- **Sessions survive everything.** Prompts queue durably on the target and keep
  executing in order while your terminal is closed or your laptop is off. Every
  session records a hash-chained event journal. Recovery archives are verified
  end to end before Mjolnir tears anything down, and crashed or wedged workers are
  detected and restarted automatically.
- **Full-access mode without fear.** Isolated targets run the agent in its
  unrestricted mode — no permission prompts — because the blast radius is a
  disposable container or instance, not your machine.
- **Your credentials stay canonical.** Each profile keeps one credential set on
  your machine. Mjolnir copies a minimal allowlist into each target, reconciles
  rotating OAuth tokens across every live session within about a minute, and
  structurally excludes credentials from event streams and recovery archives.
- **One view of capacity.** Sessions, per-profile quota and usage, and host
  capacity in one dashboard — and on your phone through the persistent Mjolnir daemon.
- **Agents can operate it.** `mj doctor --json` and `mj setup instructions`
  are designed so your coding agent can converge a host to session-ready by
  looping on machine-readable checks.

## Goals

1. Run many concurrent, long-lived agent sessions and make them durable:
   detached execution, verified recovery archives, resume onto a fresh target.
2. Make unrestricted agent modes safe by pairing them with disposable,
   isolated environments.
3. Keep provisioning minimal and deterministic: per-harness allowlists,
   SHA-256-verified workers and archives, no snowflake state in targets.
4. Give one control plane across harnesses and profiles: sessions, quotas,
   credentials, and remote control in one place.
5. Fail loudly. A failed checkpoint leaves the session usable and says so;
   retired formats are rejected, never half-converted.
6. Stay operable by both humans (TUI, web) and coding agents (JSON output,
   scriptable CLI).

## Non-goals

- **Mjolnir is not an agent.** It does not write code, plan, or pick models. It
  manages harnesses that do.
- **No privileged host setup.** Mjolnir will not install container runtimes, edit
  `subuid`/`subgid`, create AWS launch templates or security groups, or make
  SSH hosts reachable. You (or your agent, with your credentials) do that;
  `mj doctor` verifies it and prescribes the exact remediation.
- **No wholesale environment transfer.** SSH and GPG keys, shell dotfiles,
  editor configuration, package-registry credentials, cloud configuration, and
  toolchain state are never copied into targets.
- **Not a team server.** One controller process owns a session store, enforced
  by an OS-backed lock. The web server is a personal remote control with one
  viewer credential, not a multi-user service.
- **Not an orchestration platform.** Containers are unnamed disposable
  templates, rebuilt from checkpoints rather than upgraded in place. There is
  no scheduler and no load-based admission; overcommit is your call.
- **No compatibility shims.** Old relay protocols and archive schemas are
  rejected with a clear error instead of being partially converted.

## Harnesses and targets

| Harness | Credentials & quota | Checkpoint/restore of native state |
|---|---|---|
| Codex | yes | yes |
| Claude Code | yes | yes |
| Kimi Code | yes | yes |
| Grok Build | yes | yes |
| DeepSeek Harness | credentials yes; usage-priced, no subscription quota | yes |

The set is extensible by design: these five are reference integrations, not a
closed list. A new ACP-speaking harness needs a launch recipe or bridge, its
credential file shapes and login command, its home environment variable, a
checkpoint allowlist for native session state, and optionally a quota reader.
Issues and pull requests for new harnesses are welcome.

| Target | Kind | Where it runs | Agent mode |
|---|---|---|---|
| Local Git worktree | `local-bare` | your machine | your configured approvals |
| Podman container | `local-podman` | Linux, WSL2 | unrestricted |
| Docker container | `local-docker` | Linux, WSL2 | unrestricted |
| Apple container | `apple-container` | macOS 26+, Apple silicon | unrestricted |
| SSH machine | `ssh-bare` | a Linux host you name | guardian or unrestricted |
| Podman over SSH | `ssh-podman` | a Linux host you name | unrestricted |
| EC2 instance | `aws-ec2` | your AWS account | unrestricted |

The controller (the `mj` binary you run) supports Linux and macOS. Windows is
not supported; use WSL2.

## Install

```console
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | sh
```

This downloads a verified release into `~/.local/bin` — no Rust toolchain
needed. Each desktop release ships the headless `mj` controller, its separate
`mj-desktop` application, the voice worker, and the static musl session workers
that Mjolnir uploads into disposable targets. `mj` itself does not load native
desktop libraries. Run `mj doctor` next. The installer also supports `--prefix`
and `--version`; see `--help`.

npm works too:

```console
npm install -g @brokkai/mjolnir
```

As does building the static headless executable from source:

```console
cargo build --release --target x86_64-unknown-linux-musl
./target/x86_64-unknown-linux-musl/release/mj
```

Use `aarch64-unknown-linux-musl` instead on ARM64 Linux. For local development,
plain `cargo run` builds and runs the controller for the current host.

The desktop application is a separate native build. On x86-64 GNU/Linux,
install the WebKitGTK development package for your distribution and run:

```console
cargo build --release -p brokk-mjolnir -p brokk-mj-desktop \
  --target x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/mj app
```

Use the corresponding host target on ARM64 Linux or macOS. Installing from
crates.io likewise requires both `brokk-mjolnir` and `brokk-mj-desktop` when
you want `mj app`; headless installations need only `brokk-mjolnir`.

For container targets, pull the published multi-arch agent image (public, no
authentication):

```console
podman pull ghcr.io/brokkai/mjolnir/agent-dev:latest
# or
docker pull ghcr.io/brokkai/mjolnir/agent-dev:latest
```

It includes Rust, cargo-nextest, Node, OpenJDK 25, Git, GitHub CLI, the Codex
and Claude ACP bridges, and pinned DeepSeek Harness plus `dsh-acp-server`
packages.
It also bakes in Playwright's Chromium system libraries and the Chromium
headless shell (in `PLAYWRIGHT_BROWSERS_PATH=/ms-playwright`), so headless
browser tests run without a privileged install or a run-time download,
and the profiling tools `perf`, `cargo-flamegraph`, `samply`, and `heaptrack`
(`perf` also needs the host's `kernel.perf_event_paranoid` to be 1 or lower, or
`--cap-add SYS_ADMIN` on the container).
Coverage runs in a session too: the image carries the `llvm-tools-preview`
component, `cargo-llvm-cov` at the version `.github/workflows/coverage.yml`
pins, and `lcov` for `genhtml`.
See [docs/src/content/docs/custom-images.md](docs/src/content/docs/custom-images.md)
to build your own.

## Quickstart

1. Run `mj`. The first run creates a named workspace and opens a plain-terminal setup dialog: it finds your
   local harness homes, checks that credentials look present, detects the
   current GitHub repository, configures each usable local container runtime
   as its own target, and writes `config.toml` after you confirm.
2. Run `mj doctor` (or `mj doctor --json`) and fix what it reports, until it
   is clean. Log in to any profile that needs it with
   `mj login --profile <id>`.
3. Press `Alt+N` to create a session — that works from anywhere, including
   the prompt — and pick a profile, a repository bundle, and a target. From
   the Sessions pane, plain `n` does the same. Focus returns to the prompt;
   send your first message.
4. Detach whenever you like (`Alt+Q`). The session keeps running and your
   queued prompts keep executing. Reattach by running `mj` again, or open the
   daemon-owned web viewer shown by `mj daemon status`.

## The terminal surface

Mjolnir's TUI is one screen. From top to bottom: **Sessions**, the **transcript**
of the conversation you are in, the **Prompt** composer, **Targets**, **Quota**,
and a footer that names the keys that apply right now. Nothing is behind a
navigation step, so you can read an agent's output while seeing what your other
agents are doing and how loaded your machines are.

Mjolnir opens on the session whose agent spoke most recently, with the cursor in
Prompt.

`Tab` moves the keyboard down the layout — Sessions, Prompt, Targets, Quota —
and `Shift+Tab` reverses it. Once the support panes are collapsed the ring is
the two panes that are still lists. The transcript is not a Tab stop: read it
with the mouse wheel or `PageUp`/`PageDown` from wherever you are.

`Alt+G` is a two-position dial: panes open, or panes collapsed for the
conversation. Collapsed, Targets and Quota become one summary row each — host
names with CPU load, EC2 fleets with how many machines they are running,
profile names with weekly quota remaining — and the session list shrinks to a
fixed grid, one line per session, unless your terminal has more rows than
half its columns, in which case the list stays a list.

`Alt+G` always leaves the keyboard in Prompt: asking for room around the
conversation and asking to work in it are the same gesture.

Tab leaves the dial where you set it.

A few keys answer from everywhere, including while you are typing in Prompt:
`F2` opens the command palette, `F3` the workspace picker, `F4` the web
viewer, `F5` refreshes the Targets and Quota panes, `Alt+N` creates a session,
`Alt+S` resumes one, `Alt+A` marks everything read, `Alt+X` cancels whatever
the selected session is in the middle of, `Alt+G` turns the pane dial, and
`Alt+Q` detaches this terminal client — the daemon and the sessions it runs
keep working. Each of these has one spelling: a command you can reach from
anywhere has no plain-letter alias as well.

`F2` is the way to reach a command you have no key for. It lists the selected
session's own commands first — rename it, edit its container settings, stop it
— under a heading naming that session, then the commands for the pane you are
in, then everything that works anywhere, each with the key that runs it. Type
to filter by name or description, `Up`/`Down` to move, `Enter` to run, `Escape`
to close. Commands that cannot run right now stay in the list, greyed, with the
reason.

In Prompt, `Ctrl+R` searches your prompt history, as in a shell, and `Alt+T`
switches the transcript between rendered and raw. Inside the search, `Alt+R`
cycles which history it reads. Every other `Ctrl` key in Prompt is a text
editing key, as in a shell.

`Alt` chords need Option to act as Meta in macOS terminals (iTerm2:
Preferences, Profiles, Keys, "Left Option key: Esc+"; Terminal.app: "Use Option
as Meta key"), and inside tmux a short `escape-time`, for example
`set -sg escape-time 10` in `~/.tmux.conf`. Without those the terminal reports
`Alt+N` as `Escape` then `n`. The command palette on `F2` and the key reference
below are the fallbacks: every chord is also a line in both.

`F1` opens the key reference, and so does `?` from any pane. It lists every key
this screen answers, greying the ones that do not apply where you are; `Escape`,
`F1`, or `?` closes it and puts back whatever it opened over. The footer is
generated from the same list, so it names only the keys that apply right now —
`Alt-X cancel launch`, for instance, appears only while the selected session is
starting or stopping.

The footer reads in three groups, separated by a vertical bar: what the pane
you are in answers, the `Alt` chords that answer anywhere, then the function
keys. A narrow terminal drops hints from the left-hand groups first; the
function keys stay, because they are the way to the palette and the reference.

The panes take plain keys, because the composer is a separate focus and never
sees them. A plain letter is always pane-local: everything reachable from
anywhere is a chord. On Sessions: `Enter` opens the selection, `Space` and
`1`–`9` collapse and expand projects; a session's own commands are on `F2`. On
Targets and Quota: `Enter` or `e` opens that row's actions, and `F5` refreshes
both panes from anywhere.
Every list also takes the arrow keys, `j`/`k`, `Ctrl+N`/`Ctrl+P`, and
`Home`/`End`.

`Escape` belongs to the conversation: it cancels a running turn or a shell
command, and closes a dialog. It never detaches.

In an attached TUI or the phone viewer, start a message with `!` to run the
rest as `bash -lc` inside that session's target. Shell commands run in the
session workspace without blocking an active agent turn. Their bounded live
output is saved in the transcript and included once as hidden context on the
next prompt submitted after the command finishes. Press Escape in the TUI, or
use the shell's Cancel button in the viewer, to stop it.

Configuration lives at `~/.config/mjolnir/config.toml` (the platform-equivalent
directory elsewhere). The first-run dialog writes a working single-target
setup; everything beyond that is edited in TOML. A minimal example:

```toml
version = 1

[profiles.codex-1]
kind = "codex"
home = "/home/me/.codex"

[profiles.claude-1]
kind = "claude"
home = "/home/me/.claude"

[bundles.myapp]
primary_repo = "myapp"

[[bundles.myapp.repositories]]
id = "myapp"
github = "your-org/myapp"        # or: local = "/home/me/src/myapp"
destination = "myapp"

[targets.podman]
kind = "local-podman"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
# Optional: auto (default), always, newer, missing, or never. Auto launches from
# the cached image; the daemon refreshes remote latest tags hourly in the
# background. Versioned tags stay cached and digest references stay pinned.
# pull_policy = "auto"

# Docker uses the same fields:
# [targets.docker]
# kind = "local-docker"
# image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

`version` is the config schema version. A file written by a *newer* Mjolnir still
loads: the settings this build understands keep working, and the config becomes
read-only, so the older build refuses to save and never downgrades the file.
`mj doctor` reports that state. Update Mjolnir, or change settings with the newer
build, to make it writable again.

Profiles point at harness home directories on your machine — run as many
profiles per harness as you have accounts. Bundles describe the repositories a
session checks out (multi-repository bundles give agents a virtual monorepo).
Mjolnir-owned worker and bridge commands use non-login shells. On raw local, SSH,
and EC2 targets, Mjolnir makes one bounded login-shell probe when each worker starts
and carries only its discovered `PATH` into the non-login runtime; an explicit
`environment.PATH` in the profile takes precedence. Later profile changes take
effect after the worker restarts or the session resumes. Agent-requested shell
commands still run as `bash -lc` and intentionally use the session user's login
environment. If automatic discovery is insufficient, set a target-side ACP
bridge path with the profile's `executable` key or set an explicit search path
under `[profiles.<id>.environment]` with `PATH = "..."`.
Target prerequisites and full option lists are covered in
[docs/PODMAN.md](docs/PODMAN.md), [docs/DOCKER.md](docs/DOCKER.md),
[docs/SSH.md](docs/SSH.md), and
[docs/AWS.md](docs/AWS.md).

### Web viewer and Tailscale

The daemon starts the authenticated web viewer by default. Run
`mj daemon status` for its URL and six-digit login code. Without Tailscale it
serves HTTP only on `127.0.0.1:3765`.

`mj app` opens that viewer in the sibling `mj-desktop` executable. The main
`mj` process remains headless and works without GUI libraries. On Linux the
desktop executable uses the system WebKitGTK runtime; install
`libwebkit2gtk-4.1-0` on Debian/Ubuntu or the equivalent package for your
distribution if it is not already present.

When the local Tailscale node has MagicDNS and HTTPS Certificates enabled, Mjolnir
automatically requests the node's trusted `ts.net` certificate and serves HTTPS
on all interfaces at the same port. Certificate issuance runs in the background
and may take about 30 seconds the first time; certificates renew daily without a
daemon restart. If HTTPS Certificates are unavailable, the status output keeps
the viewer loopback-only and explains how to enable them. After changing the
tailnet setting, run `mj daemon restart`.

The historical configuration section remains `[phone]`. Explicit certificate
configuration takes precedence over automatic Tailscale detection:

```toml
[phone]
# Set false to disable the web viewer entirely.
enabled = true
bind = "127.0.0.1:3765"
# Set false to keep the viewer loopback-only without probing Tailscale.
tailscale_detect = true
# tls_cert = "/path/to/cert.pem"
# tls_key = "/path/to/key.pem"
```

## Security and isolation model

- Execution policy is selected by target, then translated into each harness's
  own controls. Containers and EC2 targets run unconstrained. Named raw SSH
  targets (`ssh-bare`) explicitly select `permissions = "guardian"` to preserve
  configured approvals or `permissions = "yolo"` for unconstrained execution.
  A local worktree (`local-bare`) also preserves the profile and harness's
  configured approval behavior. Codex, Claude Code, and Grok Build expose
  guardian modes; Kimi Code and DeepSeek Harness do not, so Mjolnir shows a
  prominent warning when guardian permissions cannot be enforced on a target.
- Harness homes are copied by allowlist, not wholesale. For Claude Code, for
  example: credentials, settings, `CLAUDE.md`, `skills/`, and `plugins/` — no
  transcripts, history, or caches. Mjolnir sets `CODEX_HOME`, `CLAUDE_CONFIG_DIR`,
  `KIMI_CODE_HOME`, or `GROK_HOME` in the target. Skill edits on your machine
  propagate to live sessions within about a minute.
- Credentials travel only between the controller and a session's worker. They
  are never written to the event journal or recovery archives. When the
  controller's `gh` is authenticated, Mjolnir continuously pushes its active
  GitHub token to every live non-local session, including raw SSH targets.
  The token is not stored in archives.
- Rotating OAuth logins are single use, so a container and the controller that
  reach the same expiry instant both spend the same refresh token: one wins and
  the other session's turn dies with an expired session. For Codex profiles the
  daemon rotates the login ahead of expiry and pushes the new file, so container
  copies never arrive at that instant. Claude Code has no early refresh, so
  store a long-lived token instead with `mj login --profile <id> --setup-token`;
  new and resumed sessions of that profile run with `CLAUDE_CODE_OAUTH_TOKEN`
  set, and a token that does not rotate cannot lose the race. It covers model
  requests only, not Remote Control or claude.ai connectors, which Mjolnir
  sessions do not use.
- A repository configured with `local` is served to workers through a
  per-session Git protocol bridge over the session's own transport: `git
  fetch` and fast-forward `git push origin` operate on your checkout with no
  inbound port and no writable mount. Force pushes, ref deletion, and receive
  hooks are disabled; pushes to a dirty checked-out branch are rejected. Git
  LFS is not supported through the bridge.
- Attached directories reject symbolic links, so an attachment cannot escape
  its source or destination tree.
- The daemon's web viewer requires a six-digit code exchanged for a signed
  session cookie. It binds only to loopback unless explicit TLS is configured
  or automatic Tailscale detection obtains a trusted `ts.net` certificate.

## Durability

Mjolnir saves a recovery copy automatically after completed turns when the session
is idle (at most every ten minutes), and `mj checkpoint --session <id>`
forces one. "Idle" includes work the agent starts on its own: when Claude Code
picks a task back up after a background command finishes, the session shows as
running and a recovery copy waits until that work ends, and the composer's Esc
cancels only a prompt you sent. Recovery archives are verified end to end; a
normal Stop writes and verifies the archive before any teardown, and refuses
teardown if verification fails. Explicit force-destroy is the data-loss escape
hatch.

A stopped session resumes by provisioning a fresh target from its archive,
with its pending prompt queue intact (resume asks whether to keep or discard
it). A session recorded under one harness can be resumed under another; Mjolnir
condenses the transcript into a size-bounded handoff for the new harness.

If Mjolnir or its host crashes, workers and their queued prompts keep running.
`mj recover scan` finds managed containers and instances that are no longer
tracked; `mj recover adopt` reconnects one as a tracked session.

After Mjolnir itself is upgraded, each running session's worker is replaced with
the new one at the session's next quiet moment - no prompt running, no terminal
or background command alive, nothing queued - because replacing a worker ends
the agent process with it. A session that is never quiet keeps the worker it
started with until it is stopped.

## License

Mjolnir is licensed under `GPL-3.0-only`.
