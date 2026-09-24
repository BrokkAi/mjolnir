---
name: mj
description: Drive Mjolnir coding sessions from inside an agent. Use when you need to start another coding session, send it a prompt, wait for its turn to end, read its transcript, look at its diff, export its work, or close it, and when you need to check whether the Mjolnir daemon is healthy.
---

# Drive Mjolnir sessions

Mjolnir (`mj`) runs coding sessions on local or remote targets. This skill is
how you, an agent running inside one session, start and steer other sessions.

## Check availability first

```sh
command -v mj
mj api-info
```

`mj api-info` prints the API base URL and the file holding the bearer token.
Both commands must succeed. They often fail inside containers and on remote
targets, because the worker there cannot reach the daemon on the user's
machine.

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
- `send_input` — send follow-up input to one child.
- `wait` — block until the named children finish their current turn. Status
  `complete` means the reports are in `output`; `still_running` means the
  timeout came first. That is not a failure: call `wait` again with the same
  `child_session_ids`.
- `interrupt` — stop the child's current turn.
- `close` — stop a child and keep its conversation.

Prefer these tools when they are listed; they work where the CLI cannot reach
the daemon.

## Commands

Every command takes `--json` and then prints the API response unchanged. A
session is named with `--session <id>`. For the flags of any command, append
`--help` to it rather than guessing.

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
