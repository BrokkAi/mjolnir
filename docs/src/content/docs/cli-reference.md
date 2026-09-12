---
title: CLI reference
description: Public mj commands for the dashboard, setup, diagnosis, login, import, checkpoints, daemon control, and recovery.
---

Running `mj` without a subcommand starts the per-user daemon when necessary and opens the terminal dashboard. If no workspace exists, it creates one using the current directory name and leaves it ready for an explicit new session. Otherwise it opens the requested workspace or the most recently opened workspace. The terminal surface requires at least 80 columns.

```text
mj [--workspace <name>] [command]
```

`--workspace` is global and selects a named workspace for workspace-scoped commands. Hidden worker, broker, daemon-run, and desktop-bootstrap commands are internal implementation interfaces and are intentionally omitted here.

## Open a surface

| Command | Purpose |
| --- | --- |
| `mj` | Open or attach to the terminal dashboard. |
| `mj --workspace <name>` | Open a particular workspace. |
| `mj workspaces` | Open the workspace manager in the dashboard. |
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

`mj setup` runs the interactive discovery flow. It scans harness homes and credentials, the current repository's GitHub origin, local Podman, Docker, and Apple Container runtimes, AWS CLI configuration, and concrete hosts in `~/.ssh/config`. Installed harness commands are detected even before their first login; setup reports the login needed to initialize their profile. After confirmation it adds newly discovered profiles, repository bundles, and targets while preserving existing entries and preferences. Repeated discovery reuses matching entries. When a target has different settings, setup offers to keep it or add a separate target; existing sessions keep their original configuration. A conflicting edit made while setup is open stops the write and asks you to rerun setup.

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

`<harness>` is one of `claude`, `codex`, `kimi`, `grok`, `deepseek` (alias `dsh`), or `muse`.

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

## Move a live session

```text
mj move --session <id> [--target <target-id>] [--profile <profile-id>]
        [--queue discard|start] [--clear-resources] [--yes] [--json]
```

Move keeps the logical session, workspace, transcript, and recoverable
repository state while rebuilding its execution environment. At least one of
`--target` or `--profile` is required; an omitted selector keeps its current
value. The destination must satisfy the same compatibility checks as resume.
Moving between harnesses creates a bounded transcript handoff rather than
copying harness-private process state.

Move inherits the source resource sizing and attached directories by default.
`--clear-resources` explicitly removes inherited sizing so the destination
uses its configured defaults; attached directories remain part of the fixed
workspace selection.

An interactive invocation prepares the destination and asks for confirmation.
`--yes` confirms the interruption for unattended use, but it does not choose
what to do with queued work. If commands are pending, unattended use must pass
`--queue discard` or `--queue start`; discard is the default only in an
interactive confirmation. For `start`, commands are accepted in their
original order after destination readiness. The interrupted active prompt is
never replayed.

Human output reports the operation ID and phase progress. `--json` suppresses
progress on stdout and emits one final object containing the operation ID,
session ID, resolved profile and target, outcome, and any recovery guidance.
Failures return a nonzero status. Ctrl-C requests cancellation from the daemon;
disconnecting a client does not cancel a detached move.

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

## Drive a session from another agent

```text
mj new --profile <id> --target <id> [--bundle <id>] [--project-directory <path>]
       [--workspace-id <id>] [--title <text>] [--model <name>] [--effort <name>]
       [--idempotency-key <key>] [--prompt-file <path>] [<prompt>|-] [--json]
mj prompt --session <id> [<text>|-] [--prompt-file <path>] [--wait] [--timeout <seconds>] [--json]
mj wait --session <id> [--turn <turn-id>] [--timeout <seconds>] [--json]
mj transcript --session <id> [--after-seq <seq>] [--limit <count>] [--json]
mj diff --session <id> [--json]
mj export --session <id> [--kind patch|branch|bundle|file] [--branch <name>]
           [--path <workspace-relative path>] [--out <path>] [--json]
mj sessions [--session <id>] [--json]
mj close --session <id>
mj cancel-turn --session <id>
mj api-info [--json]
```

These commands run one Mjolnir session as a subagent: `mj new` starts it with a
first prompt and prints its id, `mj wait` blocks until the turn ends and prints
the outcome, the turn number, the elapsed time, and the agent's final message,
and `mj prompt --wait` does both for the next prompt. A prompt comes from the
positional argument, from `--prompt-file`, or from standard input when the
argument is `-`.

`mj export` writes a patch, a bundle, or one workspace file (`--kind file
--path <path>`) to `--out`, or to standard output when no file is named;
`--kind branch` pushes the session's work and reports the branch and remote
instead. `mj transcript` pages by `--after-seq`, so a caller that
remembers the last `seq` it read sees only what is new.

Every command takes `--json` and then prints the API response unchanged. They
are clients for the [HTTP API](/api-reference/), which documents the routes,
the wait outcomes, the export preconditions, and the bearer token these
commands read. `mj api-info` prints the base URL and the token file.

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
| `DSH_HOME` | DSH home used by setup discovery and native import. |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME` | Muse configuration and external native session storage roots, respectively. |
| `DSH_HOME` | DeepSeek Harness home used by setup discovery. |
| `GH_TOKEN` / `GITHUB_TOKEN` | GitHub token available for syncing into every live target except `local-bare`. |
| `GIT_SSH_COMMAND` | SSH command used by checkpoint/archive Git operations. |
| `RUST_LOG` | Controller logging filter. |

Setting `MJ_WORKER_URL` requires `MJ_WORKER_SHA256`; an unverified downloaded worker is not accepted. A digest set without a URL is ignored.

Run `mj <command> --help` for Clap's installed-version spelling and option summary. Continue to [troubleshooting](/troubleshooting/) for logs, target diagnosis, and common command failures.
