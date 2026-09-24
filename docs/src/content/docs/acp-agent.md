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
mj acp [--profile <id>] [--target <id>] [--bundle <id>] [--on-exit keep|suspend|destroy]
```

| Flag | Meaning |
| --- | --- |
| `--profile <id>` | The [profile](/profiles/) whose account and harness run the session. Omitted follows your saved default. |
| `--target <id>` | The [target](/targets/) the session runs on: this machine, a container, an SSH host, or an EC2 instance. Omitted follows your saved default. |
| `--bundle <id>` | The [bundle](/workspaces-bundles/) to provision on a managed target. Without one, the working directory the client submits becomes the project, which is what a local target needs. |
| `--on-exit <policy>` | What happens to the sessions this process created when it exits: `keep` (the default), `suspend`, or `destroy`. See [When the program exits](#when-the-program-exits). |

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

The adapter exits when the program closes its side of the pipe. Any turn still
running is interrupted first, because nothing can watch or steer it any more.
What happens to the sessions next is the exit policy's choice, and the policy
covers every session this `mj acp` process created, however its turns ended:
finished, refused, failed, cancelled, or cut off when the pipe closed.

| `--on-exit` | Each session is left | Use it for |
| --- | --- | --- |
| `keep` (default) | exactly as it was, live on its target | a program whose sessions a person follows up on |
| `suspend` | checkpointed, with its worker and target released; `mj resume --session <id>` brings it back | a scheduler that wants its runs kept but not running |
| `destroy` | removed, with its workspace and recovery archive; the branch is kept | a one-shot scheduler that takes its answer from the turn |

A one-shot scheduler — one that starts `mj acp` for a single prompt, reads the
answer, and exits — should use `destroy`, or `suspend` if a person may want to
look at a run later. With `keep`, such a scheduler leaves a live session, worker,
and workspace behind every run, and has no id left to clean them up with once the
adapter is gone.

`suspend` and `destroy` wait for the daemon to finish, so the adapter can take a
few minutes to exit after the pipe closes: a suspension checkpoints the
workspace first. A program should close standard input and then wait for the
process to exit rather than kill it. `SIGTERM`, `SIGINT`, and `SIGHUP` also run
the policy; `SIGKILL` cannot, and leaves sessions as `keep` would. Under `keep`
a signal ends the adapter where it stands, as it always has.

`destroy` waits for an interrupted turn to stop before it removes a session. A
turn that does not stop within a minute is not destroyed, so no work is removed
while it is still being written.

The adapter exits with status 0 when every session was retired as asked. When
any was not — the daemon refused, the operation failed, or it did not finish in
time — the adapter exits with a non-zero status and an error on standard error
that names each such session, what went wrong, and the state it was left in. A suspension is refused
for a clone whose Git work is not verified as pushed, since releasing it could
lose that work; `mj suspend --session <id> --acknowledge-unpublished-work`
releases it once you have checked. A failed suspension leaves the session live, and a session
that was not destroyed is still there; either way `mj sessions --session <id>`
shows it and `mj suspend` or `mj destroy` retries. A creation still in flight
when the program leaves is waited for, so its session is retired too; a creation
the daemon refused made no session, so there is nothing to retire.

Sessions kept or suspended are durable: a turn interrupted by the pipe closing
leaves a session that `mj` can resume, and the work it had checkpointed is still
there. See [Durability and recovery](/durability/).
