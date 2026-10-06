---
name: mj
description: Configure Mjolnir (mj) for the user or drive its coding sessions from localhost. Use for requests to configure mj profiles, targets, review, delegation, or interface preferences; to start and steer sessions; or to diagnose the daemon.
---

# Configure and drive Mjolnir

Mjolnir (`mj`) runs coding sessions on local or remote targets. This skill is
how an agent running on the user's localhost configures Mjolnir and starts
and steers sessions.

## Configure Mjolnir for the user

For requests such as "configure mj to review with this model" or "add an SSH
target", read [the configuration reference](references/configuration.md).
It describes the supported settings and TOML examples for this build. Search
its headings for the relevant section rather than guessing keys.

Mjolnir's user configuration is `config.toml` in `$MJ_CONFIG_DIR` when set.
Otherwise use the platform configuration directory and the `MJ_INSTANCE`
subdirectory described in the reference. Localhost sessions receive the
owning daemon's `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, and named instance so file
edits and `mj` commands address the same instance. The harness's staged home
(`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, etc.) holds the agent's own settings, not
the user's Mjolnir configuration. Do not confuse its `config.toml` with mj's.

Read the existing file, make only the requested changes, and preserve its
other settings and comments. Check the file again before writing and reconcile
any concurrent changes. Use the file's current schema version;
do not rewrite a configuration from a newer mj version. Prefer adding a
new named profile or target over changing one used by an active session.

Run `mj doctor --json` before and after editing. Distinguish configuration
errors introduced by the change from existing authentication or runtime
problems; report those without making unrelated changes. The daemon reloads
configuration automatically. Changes to launch settings apply to future
sessions, while daemon startup settings may need a restart; consult the
reference and do not restart a daemon just to apply an ordinary preference.
Summarize the edited path and settings and any remaining diagnostics.

To rerun automatic agent and repository discovery, use `mj setup`. For host prerequisites,
`mj setup instructions --platform linux` or `--platform macos` prints an
agent preparation guide. `mj set-config` changes one live session's harness
settings (model, effort, etc.); it does not edit the user's mj defaults.

## Check availability first

```sh
command -v mj
mj api-info
```

`mj api-info` prints the API base URL and the file holding the bearer token.
Both commands must succeed for CLI orchestration.

When `mj` is unusable, use the `mj-agents` MCP tools if your harness lists
them. When neither is available, say once that session orchestration is not
reachable from here, and continue with the task by yourself.

## The mj-agents MCP server

Some sessions are given an MCP server named `mj-agents`. It delegates work to
child sessions in the same target and filesystem:

- `list_profiles` — the profiles and models a child may use.
- `spawn` — start a child; returns `child_session_id` at once, before the child
  has started running.
- `list_agents` — this parent's children and their status.
- `send_input` — durably queue follow-up input to one child, including while it
  starts. The queue receipt is not delivery confirmation; do not resend it.
  `wait` and `list_agents` show pending input and delivery failures.
- `wait` — call without arguments to watch every child that is not stopped. It
  returns as soon as one has a new report, when nothing is unfinished, or when
  this harness's wait window ends. A wait may end before work is done, and
  another wait is normal. Each finish is
  reported once; `send_input` can start another turn and produce another
  report. Status `reported` means new reports are in `output`;
  `nothing_to_wait_for` means no new report or unfinished child;
  `still_running` means the wait window ended first. Call `wait` again later to
  collect reports that become ready. A prompt may remind you to call `wait`,
  but does not include child output.
- `interrupt` — stop only the child's current turn. With no active turn it
  returns immediately; queued input remains queued.
- `close` — stop a child and keep its conversation.

Prefer these tools when they are listed; they work where the CLI cannot reach
the daemon.

## Commands

The session and workspace API commands below take `--json` to print the API
response unchanged. A session is named with `--session <id>`. For the flags
of any command, append `--help` to it rather than guessing.

Sessions and workspaces:

- `mj workspaces list` — the workspaces a session can be created in.
- `mj workspaces create` — create a workspace, or select the existing one with
  that name.
- `mj sessions` — the sessions the daemon holds; add `--session` for one.
- `mj new` — create a session with a first prompt and print its id.
- `mj suspend` — save a recovery copy and release the environment for Resume.
- `mj destroy` — permanently remove a session, its environment, and recovery archive; `--delete-branch` also removes its managed branch.
- `mj resume` — continue a session that was suspended. It keeps its id, its
  transcript, and its work.

Running a turn:

- `mj prompt` — send a prompt; `--wait` also waits for the turn to end.
- `mj wait` — block until the turn ends and print the outcome, the turn number,
  the elapsed time, and the agent's final message.
- `mj interrupt-turn` — interrupt the current turn while keeping the environment.
- `mj set-config` — apply a session configuration setting, such as the model.
- `mj models` — the models and efforts a profile offers.

Reading a session:

- `mj transcript` — page the transcript; `--after-seq` returns only what is new.
- `mj events` — follow durable session events as line-delimited JSON.
- `mj usage` — recorded token usage and coverage.
- `mj elicitations` — structured input requests a session is waiting on.
- `mj respond` — answer one of those requests.

Moving work in and out:

- `mj diff` — unified diff of the session's work.
- `mj export` — the work as a patch, a pushed branch, a git bundle, or one file.
- `mj put-file` — upload one file into an idle session's workspace.

Diagnosis:

- `mj doctor` — platform and configuration prerequisites.
- `mj daemon status` — daemon PID, version, start time, and client count.

## A delegation, end to end

```sh
mj workspaces create delegated
id=$(mj new --workspace delegated --profile work --target local --json "Port the parser to the new API" | jq -r .session_id)
mj wait --session "$id" --timeout 900
mj diff --session "$id"
mj suspend --session "$id"
```

`mj new` prints the id, `mj wait` blocks until the turn ends, and
`mj prompt --wait` does both for every prompt after the first. A prompt comes
from the positional argument, from `--prompt-file`, or from standard input when
the argument is `-`.

## Rules

- Read the session's own output before reporting on it. `mj wait` gives you the
  agent's final message; `mj diff` gives you what actually changed.
- One session, one job. Give a child a self-contained task and a way to report.
- A session that has work in it is not disposable: `mj destroy` permanently removes
  its environment and recovery archive. Prefer `mj suspend`. Keeping a managed
  branch does not preserve work held only in the environment.
- Report the session id in anything you tell the user, so they can open it.
