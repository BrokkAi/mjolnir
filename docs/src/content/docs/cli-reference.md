---
title: CLI reference
description: Public mj commands for the dashboard, setup, diagnosis, login, import, checkpoints, daemon control, and recovery.
---

Running `mj` without a subcommand starts the per-user daemon when necessary and opens the terminal dashboard.

```text
mj [--workspace <name>] [command]
```

`--workspace` is global and selects a named workspace for workspace-scoped commands. Hidden worker, broker, daemon-run, and desktop-bootstrap commands are internal implementation interfaces and are intentionally omitted here.

## Open a surface

| Command | Purpose |
| --- | --- |
| `mj` | Open or attach to the terminal dashboard. |
| `mj --workspace <name>` | Open a particular workspace. |
| `mj workspaces` | Open the workspace selector even when Mjolnir could auto-attach. |
| `mj app` | Open the authenticated web viewer in the separate `mj-desktop` application. |

Use `Alt+Q` to detach from the dashboard without stopping the daemon or any session. The [terminal surface](/terminal-surface/) documents its keys; the [web viewer](/web-viewer/) covers `mj app` and browser access.

## Daemon control

```text
mj daemon status
mj daemon stop
mj daemon restart
```

- `status` prints the daemon PID, version, start time, connected-client count, and viewer state. When the viewer is ready it also prints the URL and six-digit access code; otherwise it reports disabled, starting, stopped, or error state.
- `stop` gracefully stops the controller daemon. Detached workers keep running.
- `restart` gracefully replaces the daemon with the installed Mjolnir build and reconnects to existing workers.

## Setup

```text
mj setup
mj setup instructions --platform linux
mj setup instructions --platform macos
```

`mj setup` runs the interactive discovery flow. It scans harness homes and credentials, the current repository's GitHub origin, local Podman, Docker, and Apple Container runtimes, AWS CLI configuration, and concrete hosts in `~/.ssh/config`. After confirmation it writes a complete configuration. Running setup against an existing configuration replaces it rather than merging individual tables, so retain any manual settings you intend to reapply.

`setup instructions` prints coding-agent-friendly preparation steps for a Linux or macOS host.

## Diagnose the installation

```text
mj doctor
mj doctor --json
mj doctor --smoke
```

Doctor checks configuration compatibility, harness authentication, worker binaries, container runtimes, SSH targets, AWS prerequisites, and Apple Container support. Results are classified as ready, warning, fixable, or unsupported.

`--json` emits a machine-readable array. `--smoke` adds real disposable pull/run probes where a target supports them; run it after resolving ordinary fixable checks because it can download an image and provision short-lived resources.

## Authenticate a profile

```text
mj login
mj login --profile <profile-id>
mj login --profile <claude-profile> --setup-token
```

The profile may be omitted only when exactly one is configured. Mjolnir launches the appropriate authentication command for that profile. `--setup-token` is Claude-only: it mints and stores a long-lived subscription token so containers do not race the controller for a rotating OAuth refresh token.

See [profiles and harnesses](/profiles/) for home directories, credential handling, and runtime limitations.

## Import a native session

```text
mj import <harness> (--session <uuid> | --latest) [options]
```

`<harness>` is one of `claude`, `codex`, `kimi`, or `grok`. Native DeepSeek import is not available.

| Option | Meaning |
| --- | --- |
| `--session <uuid>` | Import one native session by its harness ID. Mutually exclusive with `--latest`. |
| `--latest` | Import the most recently modified native session. |
| `--bundle <id>` | Associate an existing configured repository bundle. |
| `--title <text>` | Set the title displayed in the dashboard. |
| `--allow-dirty` | Acknowledge that dirty Git roots will be archived in their complete current state. |
| `--allow-dirty-local` | Compatibility alias for `--allow-dirty`. |
| `--allow-omitted-non-git` | Acknowledge that modified non-Git or scratch directories will be omitted. |

Import never edits the harness's source transcript. It builds and verifies a Mjolnir recovery archive, creates a stopped session record, and makes that record available through `Alt+S`.

Examples:

```sh
mj import codex --latest --bundle product
mj import claude --session 018f2d00-0000-7000-8000-000000000000 \
  --bundle product --title "Finish migration"
```

## Create a checkpoint

```text
mj checkpoint --session <session-id>
```

Creates and verifies a recovery copy for an active session. It waits for a safe dispatch boundary, then lets normal work continue while the archive is packaged where supported. Mjolnir also checkpoints completed idle turns automatically, throttled to roughly one checkpoint per ten minutes.

## Recover untracked resources

```text
mj recover scan [--json]
mj recover adopt --session <id> --target <id> [--profile <id>] [--bundle <id>]
mj recover destroy --session <id> --target <id> --confirm <id>
```

- `scan` lists Mjolnir-managed workers that exist on a target but are absent from controller state.
- `adopt` probes a worker and adds it back to state. `--profile` and `--bundle` are needed only for older current-v1 workers created before ownership markers were recorded.
- `destroy` deletes an untracked managed resource. `--confirm` must repeat the exact session ID to make accidental deletion harder.

Inspect `scan` output before adopting or destroying anything. See [session recovery](/sessions/#recover-an-orphaned-worker) and [durability](/durability/) for the surrounding guarantees.

## Operator environment variables

Most behavior belongs in [configuration](/configuration/). These environment variables select filesystem locations or companion binaries before configuration loads:

| Variable | Purpose |
| --- | --- |
| `MJ_CONFIG_DIR` | Directory containing `config.toml`; overrides the platform config directory. |
| `MJ_DATA_DIR` | Root for Mjolnir's database, logs, archives, and other local state. |
| `MJ_WORKER_BINARY` | Explicit target-compatible worker binary. |
| `MJ_WORKER_DIR` | Directory searched for bundled worker binaries. |
| `MJ_WORKER_URL` | Remote worker URL fallback; it may contain `{target}` and requires `MJ_WORKER_SHA256`. |
| `MJ_WORKER_SHA256` | Required 64-character hexadecimal digest when `MJ_WORKER_URL` is set. |
| `MJ_DESKTOP_BINARY` | Explicit `mj-desktop` executable used by `mj app`. |
| `MJ_CONTROLLER_BINARY` | Explicit controller executable used by companion launchers. |
| `MJ_VOICE_WORKER` | Explicit local voice-worker executable. |
| `MJ_BIFROST_BIN` | Explicit Bifrost analysis executable used by turn review. |
| `CODEX_HOME` | Codex home used by setup discovery and native import. |
| `CLAUDE_CONFIG_DIR` | Claude Code home used by setup discovery and native import. |
| `KIMI_CODE_HOME` | Kimi Code home used by setup discovery and native import. |
| `GROK_HOME` | Grok Build home used by setup discovery and native import. |
| `DSH_HOME` | DeepSeek Harness home used by setup discovery. |
| `GH_TOKEN` / `GITHUB_TOKEN` | GitHub token available for syncing into every live target except `local-bare`. |
| `GIT_SSH_COMMAND` | SSH command used by checkpoint/archive Git operations. |
| `RUST_LOG` | Controller logging filter. |

Setting `MJ_WORKER_URL` requires `MJ_WORKER_SHA256`; an unverified downloaded worker is not accepted. A digest set without a URL is ignored.

Run `mj <command> --help` for Clap's installed-version spelling and option summary. Continue to [troubleshooting](/troubleshooting/) for logs, target diagnosis, and common command failures.
