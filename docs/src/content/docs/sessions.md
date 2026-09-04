---
title: Session lifecycle
description: Create, queue, detach, checkpoint, stop, resume, import, and recover Mjolnir sessions.
---

A Mjolnir session is a durable conversation plus the target on which its coding harness runs. The dashboard may come and go: the target-side worker owns the durable prompt queue and event journal, while the per-user daemon owns connection, lifecycle, and checkpoint orchestration plus a local projection of that state.

## Create a session

Press `Alt+N` anywhere in the terminal dashboard. The wizard resolves four things:

1. A [profile](/profiles/) selects Codex, Claude Code, Kimi Code, Grok Build, or DeepSeek Harness and the credentials to use.
2. A project source supplies the working directory: a [bundle](/workspaces-bundles/) for a managed target, or an existing Git directory for a bare target.
3. A [target](/targets/) selects the local, container, SSH, or EC2 environment.
4. A final launch review, with optional attached directories and per-session container sizing where the target supports them.

Provisioning runs in the background. The new row appears immediately in Sessions, its status changes as each launch stage completes, and a failure remains visible with useful diagnostics. `Alt+X` cancels an in-flight launch without blocking the rest of the dashboard.

Per-session CPU, memory, and attachment choices live in Mjolnir's state database, not `config.toml`. Container edits made later through **F2 → Container settings** take effect when that container is next created.

## Work, queue, and cancel

Type in Prompt and press `Enter`. If the harness is already answering, another submitted prompt joins that session's durable queue. The queue continues in order even after every terminal and browser client disconnects.

The prompt surface also understands:

- `!command` to run `bash -lc` independently inside the session workspace.
- `/model` and `/effort` to change the harness settings. A change submitted during a turn queues with the other work.
- `/fast`, `/plan`, and `/implement` when the active harness exposes the corresponding mode.
- `/review` for an independent review of the completed turn; see [turn review](/turn-review/).
- Agent-advertised slash commands, which appear in completion beside Mjolnir's local commands.

Press `Esc` to cancel the active agent turn or shell command. This does not stop the worker, delete queued prompts, or detach the client. Use `Alt+X` only for a lifecycle operation such as launch, resume, or stop.

## Detach and reattach

`Alt+Q` detaches the current terminal client. Active turns, shell commands, and queued prompts keep running under the daemon.

Run `mj` again to reattach. Mjolnir selects the workspace and opens the session whose agent spoke most recently. You can also reconnect through the authenticated [web viewer](/web-viewer/).

Stopping the daemon is different from detaching:

```sh
mj daemon stop
```

The daemon shuts down gracefully, but detached workers keep running. Starting `mj` again reconnects the controller.

## Checkpoints

After a completed turn becomes idle, Mjolnir creates a recovery checkpoint when the previous one is roughly ten minutes old. “Idle” includes agent-initiated follow-up work: Mjolnir waits for the worker to become quiet instead of capturing a moving conversation.

Force a checkpoint when you need a known recovery point:

```sh
mj checkpoint --session <session-id>
```

Mjolnir verifies the archive byte-for-byte against the target's SHA-256 and verifies its internal manifest and payload hashes before using it. Credentials and live GitHub tokens are not written into checkpoints.

## Stop safely

Select a live session, press `F2`, and choose **Stop session**. A normal stop:

1. Freezes dispatch at a safe boundary.
2. Captures and verifies a current recovery archive.
3. Terminates the owning process group or remote worker.
4. Retires the session's managed worktree, container, or instance only after the worker has stopped.
5. Leaves the session record and verified archive available to resume.

If checkpoint creation or verification fails, normal Stop refuses teardown. The failure dialog lets you retry. **Force stop** is offered only when an existing recovery archive is present and passes verification again; it then removes the current target without making a new checkpoint. Work newer than that archive may be lost, while the verified older archive remains resumable. If the existing archive cannot be verified, force stop changes nothing.

**Force destroy session** is a different, irreversible action. It removes the target, managed worktree, recovery archive, and session record. Mjolnir requires the session's short ID as confirmation because nothing can be read or resumed afterward.

## Resume on a fresh target

Press `Alt+S` to search every non-live Mjolnir session. The resume wizard lets you:

- keep the original profile or choose another harness profile;
- choose a compatible target and adjust its resources;
- review or update repository origins if the archived Git history no longer exists at the configured source;
- keep the pending prompt queue or discard it before launch.

Resume provisions a fresh target and restores the verified archive; it does not revive a stale container or instance in place. The wizard checks target compatibility before provisioning.

Cross-harness resume is supported. When the new profile uses a different harness, Mjolnir condenses the canonical transcript into a size-bounded handoff. The repository state and visible conversation survive, but harness-private implementation details do not become portable history.

For Codex, the archive includes the primary thread and child-agent results surfaced in its canonical transcript, not child agents' private rollouts. A stopped child agent cannot receive a follow-up after resume.

## Import a native harness session

The `Alt+S` picker also has an Import view for sessions created outside Mjolnir. Native Claude Code, Codex, Kimi Code, and Grok Build sessions can be adopted into a stopped, verified Mjolnir archive and then resumed on a configured target. DeepSeek native import is not available.

For scripting, select a specific native UUID or the latest session:

```sh
mj import codex --latest --bundle myapp --title "Investigate flaky tests"
mj import claude --session <native-uuid> --bundle myapp
```

If imported Git roots are dirty, Mjolnir warns that it will archive their complete current state; edited non-Git or scratch directories are omitted. An interactive CLI or dashboard import can acknowledge those warnings. For non-interactive use, pass `--allow-dirty` and, when applicable, `--allow-omitted-non-git`. See the [CLI reference](/cli-reference/#import-a-native-session) for every flag.

## Recover an orphaned worker

If the controller host crashes or its state is lost, a managed container or EC2 worker may still be alive without a matching session record. Scan for those resources:

```sh
mj recover scan
mj recover scan --json
```

Adopt a resource after checking its reported session and target IDs:

```sh
mj recover adopt --session <session-id> --target <target-id>
```

Only older current-v1 workers without ownership markers need `--profile` and `--bundle`. To delete an orphan instead, `mj recover destroy` requires the exact session ID twice—once as `--session` and once as `--confirm`. That path destroys the managed resource and should be used only after verifying it is not recoverable.

After a Mjolnir upgrade, live workers are replaced at their next quiet point, when no prompt, shell command, or queued work is active. A continuously busy session keeps its original worker until it becomes quiet or is stopped.

Continue with [durability and recovery](/durability/) for the archive guarantees, or [troubleshooting](/troubleshooting/) when a launch, checkpoint, or resume fails.
