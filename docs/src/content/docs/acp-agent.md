---
title: ACP agent
description: Run Mjolnir as the agent a program speaks to, so an ACP client gets managed sessions on local, container, SSH, and EC2 targets.
---

`mj acp` makes Mjolnir look like a coding agent to any program that speaks the
[Agent Client Protocol](https://agentclientprotocol.com/). The program starts
`mj acp` instead of a harness such as `codex` or `claude-agent-acp`, and every
session it creates is a Mjolnir session: it runs on a configured target, appears
in `mj sessions` and the web viewer, is indexed for search, and can be steered,
checkpointed, and resumed like any other.

This is for programs that drive an agent themselves — a scheduler, a bot, or an
editor that accepts a custom agent command. If you drive agents by hand, the
[terminal surface](/terminal-surface/) is what you want.

## Start it

Point the program's agent command at `mj acp`:

```text
mj acp [--profile <id>] [--target <id>] [--bundle <id>]
```

| Flag | Meaning |
| --- | --- |
| `--profile <id>` | The [profile](/profiles/) whose account and harness run the session. Omitted follows your saved default. |
| `--target <id>` | The [target](/targets/) the session runs on: this machine, a container, an SSH host, or an EC2 instance. Omitted follows your saved default. |
| `--bundle <id>` | The [bundle](/workspaces-bundles/) to provision on a managed target. Without one, the working directory the client submits becomes the project, which is what a local target needs. |

Every flag is optional, and an omitted one resolves the same way `mj new`
resolves it, so `mj acp` on its own is a valid agent command:

```sh
mj acp
mj acp --profile codex-work --target builder --bundle product
```

The command is hidden from `mj --help` because a person does not run it; a
program does. It connects to your daemon and starts it if it is not running, so
the first session may take a moment while the daemon comes up. `mj api-info`
reports the daemon it reached.

## What a session becomes

`session/new` creates an ordinary Mjolnir session and answers with its id, so the
id the client sees is the id `mj sessions` prints. Nothing about the session is
special because a program started it: it is durable, it is checkpointed, it
appears in the viewer, and its transcript is searchable in the session index.
[Session lifecycle](/sessions/) covers exploring, suspending, resuming, and
exporting one.

The client's id is also the only key it needs. A prompt may name only a session
the same `mj acp` process created, so one client cannot drive sessions that
happen to live in the same daemon.

## Supported methods

| ACP method | What this agent does |
| --- | --- |
| `initialize` | Answers ACP v1 and claims no optional capabilities. |
| `session/new` | Creates a Mjolnir session and returns its id. |
| `session/prompt` | Runs one turn, emits the turn's answer, and answers with a stop reason. |
| `session/cancel` | Interrupts the turn; the prompt answers `cancelled`. |
| `session/update` | The one notification this agent sends, carrying the turn's final message as an `agent_message_chunk`. |

These are deliberately outside it:

- **No authentication methods.** Signing in belongs to the profile on the
  controller; run `mj login --profile <id>` there.
- **No `fs/*` or `terminal/*` requests to the client.** The workspace, the
  shell, and the files belong to the session's own worker on its target, so the
  client is never asked to supply them — which is what makes running somewhere
  other than the client's machine work.
- **No session modes or configuration options.** Model and reasoning effort are
  not selectable through this interface yet; set them on the session afterwards
  from a surface that can change them.
- **No streaming.** The answer arrives as one message when the turn ends rather
  than token by token.

A prompt contributes its text. Attachments and resource links are ignored rather
than refused, because a resource link names something in the workspace the
session already runs in and its agent can read it directly. A prompt that
carries no text at all is refused, since there would be nothing to run.

## How a turn ends

| The turn | The stop reason the client sees |
| --- | --- |
| finished | `end_turn` |
| cancelled by the client, or interrupted | `cancelled` |
| failed, hit a quota limit, timed out, or its session stopped | `refusal` |

Only a finished turn reports `end_turn`. A program that treats `end_turn` as
success depends on that, so a failed or quota-limited turn is never dressed up
as a finished one.

A turn that stops to ask the client a question is handled differently, because
this adapter has no person to ask. Mjolnir interrupts the turn and answers the
prompt with an error naming the question, so a program fails visibly with a
reason instead of waiting for an answer that is never coming.

## When the program exits

Closing the pipe stops the turns the program can no longer see, and the adapter
exits. Sessions are not destroyed: they are durable, so a turn interrupted this
way leaves a session that `mj` can resume, and the work it had checkpointed is
still there. See [Durability and recovery](/durability/).
