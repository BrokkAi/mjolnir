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
| `↓` | Suspending, or a sub-agent stopping because its parent is suspending |
| `■` | Suspended |
| `⊗` | Destroying |

A failure takes precedence over an unreachable worker, which takes precedence over a request for input, which takes precedence over unread activity. Reading a completed session changes its check mark to the idle circle.

With the ASCII symbol set (**Setup → Advanced → Symbols**, or automatically on
a terminal without UTF-8) the same states read `*` working, `!` waiting, `+`
unread, `-` idle, `.` unknown, `?` unreachable, `x` failed, `^` starting, `~`
resuming, `<>` moving, `#` checkpointing, `v` suspending, `=` suspended, and `X`
destroying.

## Needs attention

A session needs attention when it is waiting for you rather than working. There are four reasons, from most to least urgent:

1. `×` **Failed**: the session, its last stop or move, or its review failed.
2. `?` **Unreachable**: the worker cannot be reached, so nothing shown about the session is current.
3. `!` **Needs input**: the agent asked a question, or a review produced findings to answer.
4. `✓` **Unread**: the turn finished and nobody has read what it said.

A session that is working, starting, suspending, or already read needs no attention.

Each workspace tab carries the most urgent symbol among its own sessions and how many of them are flagged, as in `Default !2`. A tab with nothing flagged carries no badge. A folded project heading and a minimized Sessions pane carry the same badge.

`prefix+o` opens the next session that needs you, in that order, and switches workspace when the next one is on another tab. `prefix+a` marks every session read, which clears only the `✓` reason; questions, failures, and unreachable sessions stay flagged. See [Sessions that need you](/terminal-surface/#sessions-that-need-you).

## Create a session

Press **Create**, `n`, `N`, or `prefix+c` anywhere in the terminal
dashboard. The full wizard resolves four things:

1. A [profile](/profiles/) selects Codex, Claude Code, Kimi Code, Grok Build, or Muse Code and the credentials to use.
2. A [target](/targets/) selects the local, container, SSH, or EC2 environment.
3. A project source supplies the working directory: a [bundle](/workspaces-bundles/) for a managed target, or an existing Git directory for a bare target.
4. A final launch review, with optional attached directories and per-session container sizing where the target supports them.

When only one target is available and it has no size to set, the wizard
chooses that target and skips the target step. For example, on a host without
Podman or Docker, `localhost` is the only target. The step numbers then count
three steps, and the review still shows the target. A container or EC2 target
keeps its step even when it is the only one, because you set its size there.

**Create isolated checkout** on the final review controls whether a bare Git
session gets a separate checkout or uses the selected directory directly. It
defaults on for primary checkouts and off for linked worktrees. Containers, VMs,
and plain directories leave it disabled. The terminal and web reviews both
offer this choice. Opening an empty workspace does not create a session.

Provisioning runs in the background. The new row appears immediately in Sessions, its status changes as each launch stage completes, and a failure remains visible with useful diagnostics. `prefix+shift+c` cancels an in-flight launch without blocking the rest of the dashboard.

Per-session CPU, memory, and attachment choices live in Mjolnir's state database, not `config.toml`. Container edits made later through the command palette (`prefix+:`) → **Container settings** take effect when that container is next created.

## Work, queue, and cancel

Type in Prompt and press `Enter`. If the harness is already answering, another submitted prompt joins that session's durable queue. The queue continues in order even after every terminal and browser client disconnects.

The prompt surface also understands:

- `!command` to run `bash -lc` independently inside the session workspace.
- `/model` and `/effort` to change the harness settings. A change submitted during a turn queues with the other work.
- `/fast`, `/plan`, and `/implement` when the active harness exposes the corresponding mode.
- `/review` for an independent review of the completed turn; see [turn review](/turn-review/).
- Agent-advertised slash commands, which appear in completion beside Mjolnir's local commands.

Press `Esc` to cancel the active agent turn or shell command. This does not stop the worker, delete queued prompts, or detach the client. Use `prefix+shift+c` only for a lifecycle operation such as launch, resume, or stop.

### When a turn goes quiet

A turn normally ends when the harness answers. If the harness bridge process exits, its connection closes, or the worker restarts, Mjolnir reports a failed turn within seconds, with the reason in `mj wait` and in the transcript. Optional stall bounds and key-enabled turn classification can also end Mjolnir’s tracked turn, as described below.

Silence is different. A turn can send nothing at all for a long time and be perfectly healthy, because a twenty-minute build produces no protocol traffic. Unless the turn classifier confidently identifies a request for your input, Mjolnir reports the silence and leaves the decision to you. Once a running turn has been quiet for a minute, `mj sessions --session <id>` prints `running, no harness activity for about N minutes`, `mj wait` says the same in its timeout message, and the session row in the terminal and web surfaces shows a `Quiet` clock beside the turn and step clocks. If you decide the turn is not coming back, end it with `mj interrupt-turn` or `Esc`.

There is one case Mjolnir cannot recover from: an adapter that finished the work — wrote its final message, made its commit — and then failed to send the reply. That work exists in the workspace and in the harness's own session files, but never reaches Mjolnir's transcript, so the turn stays running until you end it. If you would rather have Mjolnir end such turns automatically, set `MJ_TURN_STALL_TIMEOUT_MS` to a number of milliseconds of silence to allow; the turn then fails with the reason `harness_inactive`. It is off by default because the same setting will also end healthy turns that are merely slow.

Mjolnir automatically uses TypeSafe's Jev classifier to distinguish a question from ongoing work. By default, it sends bounded recent prompt and assistant text, tool titles, and activity counts through Mjolnir's public Cloudflare proxy to TypeSafe after a minute of silence and after a completed reply. No API key is required. With `TYPESAFE_API_KEY` set, or a key in `~/.secrets/typesafe_api_key`, requests go directly to TypeSafe using your key; the daemon forwards that key to container and SSH workers. During a running turn, a confident user-input verdict marks the turn as waiting for you (`mj wait` returns `input_required`); the harness may still be running. After a completed reply, Jev also considers sessions whose harness still lists background tasks. A confident finished or user-input verdict shows the agent as idle, while preserving the task list, stop controls, and protections against replacing a worker that owns background work. A confident background-work verdict retains the background activity status when tasks are recorded, or shows “expecting the agent to continue” otherwise. New foreground activity or changed task inventory invalidates the inference. Low confidence, rate limits, and API failures preserve the existing activity and retry after one minute, backing off to five minutes; conclusive decisions remain until the evidence changes. The proxy does not log request content and limits requests per client IP; users sharing an IP share that allowance.

Jev checks are recorded in local diagnostic logs, including scores, thresholds, timing, classifier questions, exact submitted JSON, and the actual application outcome. A positive assessment and an accepted continuation are recorded separately. Ordinary checks and retries do not add transcript messages. When the classifier identifies a running turn as waiting for input, the transcript shows: **Classifier: The agent appears to be waiting for you. The harness may still be running.**

Exact inputs contain conversation text and live runtime facts. They are stored in `jev-decisions/decisions.*.jsonl` under the daemon data directory and each worker root, with four rotating 8 MiB segments per owner. HTTP authentication headers and configured TypeSafe keys are not recorded. The hosted proxy does not log request content. Details expire through rotation; no permanent audit database or history backfill is created.

## Session actions

The **⋯** button on a session row, `.` on the Sessions pane, and `prefix+.`
open the session's action menu. The menu groups its actions under dividers:

- **Content**: **Changed files** lists the files the session's checkout has
  changed, with the branch and its distance from upstream.
- **Organize**: **Rename…**, **Pin…**, and **Unpin**.
- **Lifecycle**: **Container settings** (container sessions only), **Move…**,
  **Suspend…**, and **Restart**.
- After a plain divider: **Copy session ID** and **Destroy…**.

The menu leaves out an action that does not apply to the session. An action
that applies but cannot run yet stays in the menu, greyed, with the reason.

**Copy session ID** puts the session's full ID on the clipboard, for commands
such as `mj wait --session <id>`. The footer confirms the copy with the ID's
first eight characters. The command palette (`prefix+:`) lists the same action
under the session's name.

## Detach and reattach

`prefix+q` detaches the current terminal client. Active turns, shell commands, and queued prompts keep running under the daemon.

Run `mj` again to reattach. Mjolnir selects the workspace and opens the session whose agent spoke most recently. You can also reconnect through the authenticated [web viewer](/web-viewer/).

While the terminal says **Opening session**, `Esc` cancels that attachment,
selecting another session switches immediately, and `prefix+q` still quits.
Opening times out after 15 seconds. A failed or cancelled open stays suspended;
press `Enter` on the session in Sessions to retry. Cancelling attachment leaves
the agent running.

A session whose target failed is never opened on its own: the startup pick
skips it, and selecting it shows a **Session failed** band with the recorded
error instead of an attach that cannot finish. `Enter` on it asks whether to
read its transcript or recover it, and **Delete session** removes it. A daemon
notice about a session in another workspace starts with that workspace's
name, and the palette's **Recent messages** keeps the whole text of every
notice, wrapped, so a long failure is readable after the footer cut it off.

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

## Suspend safely

Select a live session, press `prefix+:`, and choose **Suspend session**. Suspending a session:

1. Freezes dispatch at a safe boundary.
2. Captures and verifies a current recovery archive.
3. Stops its Mjolnir sub-agents, if it has any (see below).
4. Terminates the owning process group or remote worker.
5. Retires the session's managed clone, container, or instance only after the worker has stopped.
6. Leaves the session record and verified archive available to resume.

If checkpoint creation or verification fails, suspension refuses teardown and
reports the failure. The session and its sub-agents keep running. Retry
suspension after resolving it. When a recovery copy
exists, the terminal also offers **Discard changes since checkpoint…**. A second
confirmation shows the copy's timestamp and explains that newer work may be lost.
The daemon verifies the selected recovery copy again before releasing the environment.
Discarding changes stops the session's sub-agents the same way, just before
the session goes back to that copy.

### Sub-agents and suspend

A session that uses Mjolnir's sub-agents gets each child's work back as the
report the child hands back. Suspending the session suspends only the session
itself:

- Each active sub-agent is stopped and removed, without a recovery copy of its
  own. Its conversation is put into SessionWiki first, so
  `mj sessions --session <child-id>` still finds it.
- While a sub-agent stops, its row and its conversation say "Stopping".
- A sub-agent that already handed back its report is stopped without a
  warning, since its report already reached the session.
- When a sub-agent has not handed back yet, Mjolnir says so: "2 sub-agents
  have not handed back; suspending stops them". The terminal and the web
  viewer ask for confirmation first when they can see a sub-agent still at
  work; the terminal names up to three of them, then counts the rest
  ("Sub-agents "Alpha", "Bravo", "Charlie" and 2 more have not handed
  back"). `mj suspend` prints the warning when the suspend is accepted, and
  the API returns it in its answer.
- The sub-agents stop only after the session's recovery copy is verified, so
  a suspend that fails before that point leaves them running.
- A sub-agent that cannot be stopped normally, for example because its target
  cannot be reached, is removed anyway. It never makes the session's suspend
  fail.

When the session resumes, one line in its conversation lists the sub-agents
the suspend stopped, and its agent is told on its first prompt: which
sub-agents were stopped, what each was working on, whether each had handed
back, and that the work not handed back was lost. The agent can start them
again with `spawn` if it still needs that work. The agent is told once; if the
session is suspended again before its next prompt, it is not told again. If a
suspend or a discard fails after it stopped sub-agents and the session is
still running, the session is told at once in the same way, without waiting
for a resume.

**Destroy session…** is a separate irreversible action in both terminal and web
interfaces. It removes the environment, managed checkout, and recovery archive,
so work that was not pushed or exported is lost. The conversation is archived,
not deleted: `mj sessions --session <id>` then lists the session as `archived`,
and `mj resume --wiki <id>` starts a new session from its conversation (see
[Search and restore archived sessions](#search-and-restore-archived-sessions)).
New managed clones have no branch
in the source repository; their branches survive only in a published remote or
the recovery archive. Older linked worktrees keep their managed branch by default
and offer a choice to delete it.

**Interrupt turn** leaves the environment available for further prompts.
**Close pane** only dismisses a viewer; it does not interrupt or suspend a session.

## Resume on a fresh target

`prefix+g` and the **Open** button open the session dialog on the running
sessions. Press `→` once for the **Mjolnir** tab, which lists every non-live
Mjolnir session, including records that were previously archived by a provider.
Provider archive metadata is shown read-only. Pressing Enter on a row opens the
resume wizard, which lets you:

- keep the original profile or choose another harness profile;
- choose a compatible target and adjust its resources (when only one target
  suits the session and it has no size to set, the wizard chooses it and
  skips that step);
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
`mj suspend --session <id>`, then `mj resume --session <id>` when you want it
back. The command answers as soon as the daemon has taken the session; `mj wait`
blocks while the resume runs and reports the reason if it fails. Afterwards
`mj set-config` and `mj prompt` work as they do for any live session. The same
operation is `POST /api/v1/sessions/{id}/resume` in the
[HTTP API](/api-reference/#resume-a-suspended-session).

To test native recovery for one selected session, use **Restart session** in
the terminal command palette, or suspend and resume that session with the
commands above. This stops its worker and starts a new one, which attempts to
reload the recorded native session. Restarting only the Mjolnir daemon leaves
detached workers running and does not exercise native reload. If Codex or
Claude reports that a never-prompted native session is missing, Mjolnir warns
and opens a new empty native session under the same Mjolnir session ID. A
native session with recorded use is not replaced this way. For worker
attribution during diagnosis, workers launched by current Mjolnir versions
carry their owning `MJ_INSTANCE` in the process environment; workers launched
before that marker was added acquire it after a supported session restart.

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

The branch rule follows the checkout. A managed clone arrives on its selected
or default branch. A session opened directly on your own checkout arrives on
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
confirm, captures a verified checkpoint, and restores the same logical session
on the destination. Like the resume wizard, the Move wizard skips its target
step when only one target suits the session and it has no size to set.

How much is rebuilt depends on what changes. When the target, the attached
directories, and the resource allocation all stay the same, Move replaces only
the harness, in place: the container or bare worker root, the workspace,
untracked files, and build caches are kept, and only the old worker daemon and
profile home are removed before the new profile is staged and the harness state
is restored. The confirmation says so: "Only the harness and profile are
replaced; the environment and workspace are kept." When the target changes,
Move tears the source down and rebuilds the environment from the checkpoint,
which does not migrate a running process, installed packages, container layers,
or files outside the declared workspace. Either way the old harness process
stops, so running process memory is lost.

If an in-place swap fails, or the daemon restarts while it runs, Mjolnir does
not retry in place. It releases the environment and leaves the session suspended
with its verified checkpoint, and the UI offers retry or resume with the
previous settings.

The web viewer and terminal confirmation show both source and destination. If
the profile changes harness, the destination receives the same bounded
transcript handoff used by cross-harness resume. Target choices obey the current
resume compatibility rules; unsupported host/worktree combinations fail before
the source is interrupted.

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

If destination launch fails, the session remains suspended with its verified
checkpoint and the UI offers retry or resume with the previous settings. If
queue admission fails after the destination is ready, the live destination is
kept so accepted commands are not replayed on another target. Closing the
browser or terminal does not cancel a move; use the operation cancellation
control explicitly.

For Codex, the archive includes the primary thread and child-agent results surfaced in its canonical transcript, not child agents' private rollouts. A stopped child agent cannot receive a follow-up after resume.

## Import a native harness session

The `prefix+g` picker also has an Import view for sessions created outside Mjolnir. Native sessions from all five supported harnesses can be adopted into a suspended, verified Mjolnir archive and then resumed on a configured target. Muse imports retain their native session IDs and support workspace relocation. Muse accepts one workspace root.

The Import view and `mj import` read the home of each enabled profile. A session Mjolnir runs writes its native history into its own staged home instead, on this machine as on every other target, so it never appears in the Import view, and your harness's own resume command, such as `codex resume` or `claude --resume`, does not list it either. Find it on the **Mjolnir** or **Archived** tab. See [Native session history](/profiles/#native-session-history).

For scripting, select a specific native UUID or the latest session:

```sh
mj import codex --latest --bundle myapp --title "Investigate flaky tests"
mj import claude --session <native-uuid> --bundle myapp
mj import grok --latest --bundle myapp
mj import muse --session <native-uuid> --bundle myapp
```

Dashboard imports that will resume in a bare Git project also offer
**Create isolated checkout**, even when there are no import warnings. The saved
choice takes effect on first resume, which then makes the session's clone
under `.mj/clones/<session id>`.

Close the source harness before importing. If it changes the session during import, select it again after it stops. Unsupported native storage versions report an error rather than importing partial history.

If imported Git roots are dirty, Mjolnir warns that it will archive their complete current state; edited non-Git or scratch directories are omitted. An interactive CLI or dashboard import can acknowledge those warnings. For non-interactive use, pass `--allow-dirty` and, when applicable, `--allow-omitted-non-git`. See the [CLI reference](/cli-reference/#import-a-native-session) for every flag.

## Search and restore archived sessions

Mjolnir writes every session into
[SessionWiki](https://github.com/jbellis/sessionwiki), a separate tool that
keeps one full-text index of AI coding sessions across Claude Code, Codex, and
other harnesses. This is always on. The one setting is how long a suspended
session is kept before Mjolnir's own copy is removed:

```toml
[sessionwiki]
archive_after_days = 30
```

The Setup screen's SessionWiki page shows how much disk your sessions use and,
while you type a value for **Archive after (days)**, an estimate of what that
value would reclaim. The estimate covers checkpoints and image attachments, and
it counts every aged suspended session whether or not the index has caught up
with it yet, so it describes the policy rather than the next hourly pass.

### What gets indexed, and when

A running session is indexed from the transcript the daemon holds, and a suspended
one from its checkpoint, so a session is searchable before it has ever been
closed. The daemon indexes when it starts, when a session reaches the suspended
state, once an hour, and before a Resume search that has not synced in the last
minute. The first build walks every tool's store and can take many minutes on a
large corpus. Until it finishes, the Resume search box cannot be typed into and
reads **Indexing…**; the tabs and the list keep working, and the box becomes
typable as soon as the build finishes, though moving the keyboard focus there
still takes `/` or a click. What is stored is the conversation: the prompts,
the agent's replies, the titles of the tool calls, the session title, and the
project directory. Native sessions from every enabled profile home are indexed
too, including Kimi Code, Grok Build, and Muse, under the tool names
`kimi-code`, `grok-build`, and `muse`. Each Mjolnir instance indexes only its
own sessions, and all of them share the tool name `mjolnir`, so one search
covers every instance:

```sh
sessionwiki search "flaky migration test"
sessionwiki list --tool mjolnir
```

Search finds the session; two commands read inside one, which is how an agent
works through a hit without pulling a whole transcript into its context.
`sessionwiki grep --json "<text>" <id>` prints one JSON line per matching
message, each with its index and a bounded window of text around the match.
`sessionwiki show <id> --jsonl` prints the whole session as one JSON object per
message, in order, so `sed` and `jq` can select and truncate the part worth
reading.

### Session metadata in the index

Mjolnir tags each of its own indexed sessions with the target template it ran
on, the harness profile it last used, and the harness kind, as
`mj-target:<target>`, `mj-profile:<profile>` and `mj-harness:<harness>`. The
tags are written on every sync and replaced when a session moves or changes
profile, and `sessionwiki list` and `sessionwiki tags` show them like any other
tag. They exist because the archive job destroys Mjolnir's own record of a
session, so without them nothing would be left to say where an archived session
ran. The Archived tab reads them for its PROFILE and TARGET columns, and Restore
opens on the same profile and target. A target that has since been removed from
`config.toml` is shown exactly as it was recorded, and Restore falls back to the
first profile and target.

### Searching the session dialog

On the **Live** tab the search box matches running sessions by name, ID, and
workspace immediately from the dashboard's own state. It also searches the
index in the background so the other tabs show their match counts before you
switch tabs. On the three history tabs the index supplies the results. With the
box empty,
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

The dialog's last tab, **Archived**, lists sessions whose live Mjolnir copy is
gone but whose conversation SessionWiki still has. The pane under
the list previews the selected session's conversation. The web viewer has the
same search, Archived section, preview, and Restore button.
Scroll the preview with the wheel or by clicking and dragging its scrollbar.
When the search has transcript hits, use the up and down arrows beside the hit
count to move to the previous or next hit; `N` and `n` work from the list too.

### Restore

Pressing Enter on an archived row, or **Restore** in the viewer, starts a new
session and hands it a compacted summary of the old conversation as hidden
first-prompt context — the same compaction Mjolnir uses when a session moves
between harnesses. The new session keeps the archived session's title. It opens
in the source repository of the archived session's old managed checkout unless you
name another project directory. This is a new session, not a revival: there is
no checkpoint to restore, so the workspace starts fresh and only the
conversation carries over. An archived session with no user prompt cannot be
restored.

### What archiving deletes and keeps

With `archive_after_days = N`, the hourly job removes Mjolnir's own copy of a
suspended session older than N days, but only after confirming SessionWiki holds
its conversation. It deletes the session record, the checkpoint archive, and the
session's image attachments. For independent clones it does so only when the
verified checkpoint has no uncommitted files or stashes and every saved commit
and ref has been confirmed on the configured push remote. A pushed branch can
be archived even when it has not merged. Unknown publication status keeps the
checkpoint. Older linked-worktree sessions retain their branch under the legacy
rule. A session with a
sub-agent child that is not ready to be archived waits for the next pass. Leave
`archive_after_days` unset to keep every session forever.

### Match the `sessionwiki` version

Mjolnir links SessionWiki as a library and writes into your ordinary SessionWiki
index. The `sessionwiki` command-line tool you install must be the same version.
SessionWiki drops and rebuilds its whole index when the file's schema version
differs from the one the program expects, so two programs at different versions
re-index everything each time you alternate between them — on a large corpus
that is tens of minutes per switch.

This build links the `brokk-sessionwiki` crate, version 0.30.1. Install the
matching tool, which is still named `sessionwiki`, with:

```sh
cargo install --locked brokk-sessionwiki@0.30.1
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

After a Mjolnir upgrade, live workers are replaced at their next quiet point, when no prompt, shell command, or queued work is active. A continuously busy session keeps its original worker until it becomes quiet or is suspended.

Continue with [durability and recovery](/durability/) for the archive guarantees, or [troubleshooting](/troubleshooting/) when a launch, checkpoint, or resume fails.

## Automatic continuation

Mjolnir can continue work the agent has explicitly left unfinished when your earlier messages already request it. For example, if you asked for an implementation and tests, “Implemented; shall I run tests?” can trigger a continuation without another reply from you.

This is enabled by default. Uncheck **Enabled** under **Setup → Continuation** to disable it, or set:

```toml
[continuation]
enabled = false
```

Continuation also needs Jev: with `[jev] enabled = false` it does not run, whatever `[continuation]` says, and the Continuation row in Setup reads **Off · Jev is off (Privacy)**.

The session shows **Checking continuation** while Jev checks the conversation. A continuation appears as **Continuing requested work automatically · 1 of 3**. The diagnostic logs contain the evidence and outcome. There are at most three automatic continuations between your messages. New input or interrupting the session cancels a pending check. Automatic turn review waits until the continuation chain settles.

Mjolnir checks every turn that ends, including a turn the agent starts on its own, for example when a background task it was waiting for finishes.

Continuation supplies no new approval. It does not resolve missing information, genuine decisions, plan approvals, credentials, or external blockers. It skips child sessions, failed turns, sessions waiting on a question, and unsupported older workers. A classifier failure or uncertain result leaves the session waiting normally.

When a background command, a sub-agent, or an active goal will still move the session on, a continuation waits. Mjolnir checks again when that work stops, or when Jev judges that the background work is idle, such as a command that only sleeps.

When a turn ends at a subscription limit, Mjolnir schedules a continuation for one minute after the limit resets. This also applies to turns the agent started on its own, and to sessions with background work still running. If the usage limit stopped a Codex goal, Mjolnir resumes the goal instead. A goal that has used up its own token budget is never continued automatically; resume it yourself.

The check uses user messages since the last context reset and recent assistant replies, including earlier exchanges that establish what “go ahead” refers to. It excludes tool history and generated prompts. Evidence has size limits; required user history is never clipped to fit. TypeSafe processes this text, through the public Jev proxy when no local TypeSafe key is configured. The proxy does not log or store message bodies.
