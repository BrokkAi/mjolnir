---
title: Session lifecycle
description: Create, queue, detach, checkpoint, stop, resume, import, and recover Mjolnir sessions.
---

A Mjolnir session is a durable conversation plus the target on which its coding harness runs. The dashboard may come and go: the target-side worker owns the durable prompt queue and event journal, while the per-user daemon owns connection, lifecycle, and checkpoint orchestration plus a local projection of that state.

## Session status symbols

Each terminal session row starts with a fixed status symbol. Symbols stay visible in collapsed projects, minimized session lists, narrow sidebars, and workspace previews.

| Symbol | Status |
| --- | --- |
| `◐` | Working, including reviews and background commands |
| `!` | Waiting for your input or a review decision |
| `✓` | Idle with unread activity, or a completed clean review |
| `○` | Idle with no unread activity |
| `·` | Activity not yet available |
| `?` | Disconnected or unreachable |
| `×` | Failed or lost |
| `↑` | Starting |
| `↻` | Resuming |
| `⇄` | Moving |
| `▣` | Checkpointing |
| `↓` | Stopping |
| `■` | Stopped |
| `⊗` | Destroying |

Active work and requests for input take precedence over unread activity. Reading a completed session changes its check mark to the idle circle.

## Create a session

Press **Create**, `n`, `N`, `Alt+N`, or `Alt+W` anywhere in the terminal
dashboard. The full wizard resolves four things:

1. A [profile](/profiles/) selects Codex, Claude Code, Kimi Code, Grok Build, or Muse Code and the credentials to use.
2. A project source supplies the working directory: a [bundle](/workspaces-bundles/) for a managed target, or an existing Git directory for a bare target.
3. A [target](/targets/) selects the local, container, SSH, or EC2 environment.
4. A final launch review, with optional attached directories and per-session container sizing where the target supports them.

**Create managed worktree** on the final review controls whether a bare Git
session gets a separate checkout or uses the selected directory directly. It
defaults on for primary checkouts and off for linked worktrees. Containers, VMs,
and plain directories leave it disabled. The terminal and web reviews both
offer this choice. Opening an empty workspace does not create a session.

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

While the terminal says **Opening session**, `Esc` cancels that attachment,
selecting another session switches immediately, and `Alt+Q` still quits.
Opening times out after 15 seconds. A failed or cancelled open stays stopped;
press `Enter` on the session in Sessions to retry. Cancelling attachment leaves
the agent running.

Draft saves run in the background. If the daemon does not confirm a save within
15 seconds, Mjolnir reports the uncertainty and allows exit. An unconfirmed
draft-save warning remains visible after the terminal closes; reconnect to
check the saved state.

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

**Force destroy session** is a different, irreversible action. It removes the target, managed worktree checkout, recovery archive, and session record. Mjolnir requires the session's short ID as confirmation because nothing can be read or resumed afterward.

Destroying a session leaves the managed worktree's git branch in the source repository, so any commits you made there survive. Choose **Yes, delete branch** in the confirmation if you want Mjolnir to delete the branch as well.

## Resume on a fresh target

Press **Resume** or `Alt+S` to search every non-live Mjolnir session, including
records that were previously archived by a provider. Provider archive metadata
is shown read-only. The resume wizard lets you:

- keep the original profile or choose another harness profile;
- choose a compatible target and adjust its resources;
- review or update repository origins if the archived Git history no longer exists at the configured source;
- keep the pending prompt queue or discard it before launch.

Resume provisions a fresh target and restores the verified archive; it does not revive a stale container or instance in place. The wizard checks target compatibility before provisioning.

Cross-harness resume is supported. When the new profile uses a different harness, Mjolnir condenses the canonical transcript into a size-bounded handoff. The repository state and visible conversation survive, but harness-private implementation details do not become portable history.

### Resume from a script

```text
mj resume --session <id> [--profile <id>] [--target <id>] [--queue start|discard]
mj wait --session <id>
```

`mj resume` runs the same operation as the wizard, without one. The session's
own record supplies the profile and target when the command names none, so
closing a session after capture and continuing it later is scriptable:
`mj close --session <id>`, then `mj resume --session <id>` when you want it
back. The command answers as soon as the daemon has taken the session; `mj wait`
blocks while the resume runs and reports the reason if it fails. Afterwards
`mj set-config` and `mj prompt` work as they do for any live session. The same
operation is `POST /api/v1/sessions/{id}/resume` in the
[HTTP API](/api-reference/#resume-a-stopped-session).

### Resume a local session into a container

A session that runs the agent in a directory on this machine can resume on an
isolated target when that directory is a whole Git checkout with a network
remote. Choosing such a target shows what the move will do before anything is
stopped, and asks you to confirm it:

- which remote is cloned, which branch the session continues on, and where
  `git push` will go;
- how many commits are not on the remote yet, because those travel inside the
  checkpoint;
- how many staged, unstaged, and untracked files are copied in, and their size;
- whether the checkout stays behind on this machine.

What travels is the checkout's own content: commits on no remote branch, and
staged, unstaged, and untracked files. What does not travel is anything Git
ignores—build output, `.env` files, `node_modules`—and anything outside the
checkout. Install steps and files elsewhere on the host are not migrated.

The branch rule follows the session. A Mjolnir-managed worktree arrives on its
`mj/<session>` branch. A session opened directly on your own checkout arrives on
whatever branch that checkout was on, and `git push` inside the container pushes
that branch. In the second case your checkout stays on this machine and stops
tracking the session: edits made in the container do not come back on their own,
so push the branch or move the session back.

A checkout with no network remote cannot become an isolated workspace. Add one
(`git remote add origin <url>`) or keep the session on a bare target; the picker
says which it is.

## Move a live session

Use **Move…** from the session action menu when a live session should continue
with another profile, target, or both. Move is one daemon-owned operation: it
prepares and checks the destination, interrupts the active turn only after you
confirm, captures a verified checkpoint, tears down the source, and restores
the same logical session on a fresh destination.

The web viewer and terminal confirmation show both source and destination. If
the profile changes harness, the destination receives the same bounded
transcript handoff used by cross-harness resume. A move does not migrate a
running process, installed packages, container layers, or files outside the
declared workspace. Target choices obey the current resume compatibility rules;
unsupported host/worktree combinations fail before the source is interrupted.

Queued prompts and configuration commands are listed during confirmation.
**Discard queued work** is the default and leaves the destination idle.
**Run queued work** restores the entries in their original order only after the
destination is ready. An interrupted active prompt is never replayed
automatically.

Moving a local session into a container shows the same preview and warnings
that [resuming one](#resume-a-local-session-into-a-container) does: the remote
that is cloned, the branch, the unpushed commits, the uncommitted files that are
copied in, and whether the checkout stays behind. `mj move` prints those lines
before its prompt, and with `--yes` as well.

The viewer keeps the source workspace's resource sizing and attached
directories fixed. It offers one explicit confirmation to clear inherited
resource sizing and use destination defaults; use the terminal Move wizard
when you need to change attached resources.

If destination launch fails, the session remains stopped with its verified
checkpoint and the UI offers retry or resume with the previous settings. If
queue admission fails after the destination is ready, the live destination is
kept so accepted commands are not replayed on another target. Closing the
browser or terminal does not cancel a move; use the operation cancellation
control explicitly.

For Codex, the archive includes the primary thread and child-agent results surfaced in its canonical transcript, not child agents' private rollouts. A stopped child agent cannot receive a follow-up after resume.

## Import a native harness session

The `Alt+S` picker also has an Import view for sessions created outside Mjolnir. Native sessions from all five supported harnesses can be adopted into a stopped, verified Mjolnir archive and then resumed on a configured target. Muse imports retain their native session IDs and support workspace relocation. Muse accepts one workspace root.

For scripting, select a specific native UUID or the latest session:

```sh
mj import codex --latest --bundle myapp --title "Investigate flaky tests"
mj import claude --session <native-uuid> --bundle myapp
mj import grok --latest --bundle myapp
mj import muse --session <native-uuid> --bundle myapp
```

Dashboard imports that will resume in a bare Git project also offer
**Create managed worktree**, even when there are no import warnings. The saved
choice takes effect on first resume.

Close the source harness before importing. If it changes the session during import, select it again after it stops. Unsupported native storage versions report an error rather than importing partial history.

If imported Git roots are dirty, Mjolnir warns that it will archive their complete current state; edited non-Git or scratch directories are omitted. An interactive CLI or dashboard import can acknowledge those warnings. For non-interactive use, pass `--allow-dirty` and, when applicable, `--allow-omitted-non-git`. See the [CLI reference](/cli-reference/#import-a-native-session) for every flag.

## Search and restore archived sessions

Mjolnir writes every session into
[SessionWiki](https://github.com/jbellis/sessionwiki), a separate tool that
keeps one full-text index of AI coding sessions across Claude Code, Codex, and
other harnesses. This is always on. The one setting is how long a stopped
session is kept before Mjolnir's own copy is removed:

```toml
[sessionwiki]
archive_after_days = 30
```

The Setup screen's SessionWiki page shows how much disk your sessions use and,
while you type a value for **Archive after (days)**, an estimate of what that
value would reclaim. The estimate covers checkpoints and image attachments, and
it counts every aged stopped session whether or not the index has caught up
with it yet, so it describes the policy rather than the next hourly pass.

### What gets indexed, and when

A running session is indexed from the transcript the daemon holds, and a stopped
one from its checkpoint, so a session is searchable before it has ever been
closed. The daemon indexes when it starts, when a session reaches the stopped
state, once an hour, and before a Resume search that has not synced in the last
minute. The first build walks every tool's store and can take many minutes on a
large corpus. Until it finishes, the Resume search box cannot be typed into and
reads **Indexing…**; the tabs and the list keep working, and the box opens by
itself when the build finishes. What is stored is the conversation: the prompts,
the agent's replies, the titles of the tool calls, the session title, and the
project directory. Each Mjolnir instance indexes only its own sessions, and all
of them share the tool name `mjolnir`, so one search covers every instance:

```sh
sessionwiki search "flaky migration test"
sessionwiki list --tool mjolnir
```

### Searching Resume

The Resume search box searches the index and nothing else. With the box empty,
each tab lists what it always lists. With a query, each tab lists only the
sessions the index returned, in the order the index ranked them, and each row
carries the text the search matched. That includes text that appears only inside
a transcript, and a title or project the index knows, so a session you remember
only by something said inside it is findable.

The box is closed while the index cannot answer. It reads **Indexing…** during
the first build, and names a version mismatch when the index file on disk was
written by another SessionWiki version (see [Match the `sessionwiki`
version](#match-the-sessionwiki-version)). Either way the tabs and row
navigation keep working.

### The Archived tab

`Alt+S` opens Resume with a third tab, **Archived**, listing sessions whose live
Mjolnir copy is gone but whose conversation SessionWiki still has. The pane under
the list previews the selected session's conversation. The web viewer has the
same search, Archived section, preview, and Restore button.

### Restore

Pressing Enter on an archived row, or **Restore** in the viewer, starts a new
session and hands it a compacted summary of the old conversation as hidden
first-prompt context — the same compaction Mjolnir uses when a session moves
between harnesses. The new session keeps the archived session's title. It opens
in the repository above the archived session's old managed worktree unless you
name another project directory. This is a new session, not a revival: there is
no checkpoint to restore, so the workspace starts fresh and only the
conversation carries over. An archived session with no user prompt cannot be
restored.

### What archiving deletes and keeps

With `archive_after_days = N`, the hourly job removes Mjolnir's own copy of a
stopped session older than N days, but only after confirming SessionWiki holds
its conversation. It deletes the session record, the checkpoint archive, and the
session's image attachments. It deletes the `mj/<session id>` branch only when
every commit on it is already on another branch, local or remote-tracking, that
is not itself a session branch; otherwise the branch stays, so any work the
session committed is still there. A branch whose work was squash-merged or
rebased onto another branch looks unmerged to git and is kept. A session with a
sub-agent child that is not ready to be archived waits for the next pass. Leave
`archive_after_days` unset to keep every session forever.

### Match the `sessionwiki` version

Mjolnir links SessionWiki as a library and writes into your ordinary SessionWiki
index. The `sessionwiki` command-line tool you install must be the same version.
SessionWiki drops and rebuilds its whole index when the file's schema version
differs from the one the program expects, so two programs at different versions
re-index everything each time you alternate between them — on a large corpus
that is tens of minutes per switch.

This build links the `brokk-sessionwiki` crate, version 0.29.0. Install the
matching tool, which is still named `sessionwiki`, with:

```sh
cargo install --locked brokk-sessionwiki@0.29.0
```

## Recover an orphaned worker

If the controller host crashes or its state is lost, a managed container or EC2 worker may still be alive without a matching session record. Scan for those resources:

```sh
mj recover scan
mj recover scan --json
```

The scan lists only workers this Mjolnir instance created. Two instances that share a target host, such as a QA `--instance` and your default setup, never see each other's workers unless you pass `--all-instances`, which also lists workers from older builds that carry no instance stamp. Adopting or destroying such a worker needs the same flag.

Adopt a resource after checking its reported session and target IDs:

```sh
mj recover adopt --session <session-id> --target <target-id>
```

Only older current-v1 workers without ownership markers need `--profile` and `--bundle`. To delete an orphan instead, `mj recover destroy` requires the exact session ID twice—once as `--session` and once as `--confirm`. That path destroys the managed resource and should be used only after verifying it is not recoverable.

After a Mjolnir upgrade, live workers are replaced at their next quiet point, when no prompt, shell command, or queued work is active. A continuously busy session keeps its original worker until it becomes quiet or is stopped.

Continue with [durability and recovery](/durability/) for the archive guarantees, or [troubleshooting](/troubleshooting/) when a launch, checkpoint, or resume fails.
