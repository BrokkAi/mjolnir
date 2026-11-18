# Origin-relative session snapshots, suspend-only checkpoints, and direct moves

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir (the `mj` command) runs coding agents in sessions. Suspending a session saves what is needed to continue it later, stops the agent, and releases the machine it ran on. The saved copy is called a checkpoint. Today checkpoints are too large and taken too often:

- they copy untracked files and every local Git ref with its history;
- they are retaken automatically while a session sits idle;
- moving a session between machines pushes the whole repository history through the daemon's disk twice.

After this change, a suspended session's workspace is saved as the smallest Git delta against the repository's `origin` remote. A session that pushed everything saves an almost empty delta. Restoring clones origin and applies the delta. Suspend becomes the only operation that takes a checkpoint.

Moving a session between machines uses the same snapshot. The snapshot goes straight from the old machine to the new one, and streams through the daemon only when the two machines cannot reach each other, saying so in the move's progress. Untracked files are never saved. Suspend warns about them; the user either chooses "Continue anyway" (terminal and web UI) or passes `--discard-untracked` (CLI). A new `mj suspend --without-checkpoint` stops a session without taking a new checkpoint, for when checkpointing itself is broken. `mj destroy` keeps its meaning of permanently removing a session.

To see it working, use an isolated test instance named `snapshot-test`:

1. Start a session in a managed clone.
2. Push one branch, make one unpushed commit, and stash a change.
3. Leave one staged edit, one unstaged edit and one untracked file.
4. Run `mj --instance snapshot-test suspend --session <id>`. It refuses and names the untracked file.
5. Rerun it with `--discard-untracked`. It succeeds, and the Git part of the snapshot is a few kilobytes.
6. Resume the session on a different target. It shows the same branch and commit, the stash list, and the staged and unstaged edits, without the untracked file.


## Progress

- [x] (2026-11-18) Read-only audit of suspend capture, checkpoint triggers, lifecycle verbs and their surfaces, moves, managed checkouts, transcript coupling and harness compaction signals. User decisions recorded in the Decision Log.
- [ ] Milestone 1: git-native snapshot capture, restore and inventory in `mj-checkpoint`, with integration tests against real Git.
- [ ] Milestone 2: suspend and resume use the new snapshot; raw launch-ref record; resume conversions; old-archive and legacy-worktree restore onto a fresh managed clone.
- [ ] Milestone 3: suspend warnings and consent on every surface; `suspend --without-checkpoint`; consolidation of the lifecycle verbs.
- [ ] Milestone 4: suspend is the only checkpoint producer; periodic copies, the explicit checkpoint action, export side effects and checkpoint-based agent restart are removed; new journal-retention and tool-output-compaction triggers.
- [ ] Milestone 5: Move on the shared snapshot, with direct transport and the target-side snapshot cache.
- [ ] Milestone 6: `mj purge-untracked`; retire legacy-worktree creation, branch-deletion options, the Move file picker and other dead capture code.
- [x] (2026-11-18) Asked session 5992a212 whether Move needs `--with-untracked`. Its answer led to the `.mj/keep/` decision instead.


## Surprises & Discoveries

- Observation: Destroy is permanent removal, not "suspend without a checkpoint" as originally assumed.
  Evidence: `force_destroy_session` in `mj-controller/src/controller/lifecycle.rs` (around line 1420) calls `crate::database::delete_session` and removes every checkpoint archive of the session. Prompt history, attachments and the stored transcript cascade away with the row. Child sessions are destroyed recursively.

- Observation: a managed clone keeps its branches only inside the clone. Today they survive suspend only because the checkpoint bundles them.
  Evidence: the clone is created with `git clone --local --dissociate` (`mj-controller/src/controller/worktree.rs`, around line 2195), and suspend removes it with `rm -rf` (around line 2787). Nothing pushes the clone's branches anywhere or fetches them back into the source repository.

- Observation: Move ships the whole history through the daemon.
  Evidence: `mj-worker/src/move_workspace.rs` (around line 271) runs `git bundle create --all HEAD` plus the stash commits. `mj-controller/src/controller/move_session/transfer.rs` then rsyncs the result to `data_dir()/moves/...` on the daemon (around line 1219), and from there to the destination host (around line 1369).

- Observation: automatic checkpoints wait for a completed turn and idle admission rather than running on a fixed timer, yet still need most of the live-copy machinery.
  Evidence: a prompt can arrive during the copy, which explains the prestage, early-release and two-phase packing code in `mj-controller/src/controller/checkpoint/latched.rs` and `staging.rs`. Busy deferrals once leaked 3,325 prestage directories of about 260 MB each (commit 7f0d41b9d).

- Observation: t3code is not a reference-only transcript design. It keeps a full copy of messages and turn items in its own SQLite projection, as Mjolnir does.
  Evidence: `~/Projects/t3code` at 856c84e4b, `ProjectionStore.ts` around lines 2921 and 2996.

- Observation: `git stash create` returns nothing when the only change is a staged gitlink (submodule) update, even with `-c diff.ignoreSubmodules=none`.
  Evidence: real-Git reproduction during Milestone 1. `git diff --cached --raw` shows `:160000 160000 <old> <new> M sub`, yet both `git stash create` calls print an empty line. The snapshot therefore records each submodule's index gitlink explicitly and restores it with `git update-index --cacheinfo`.

- Observation: a harness context-compaction event does not mark an exact boundary of discarded context.
  Evidence: the pinned Claude bridge's compaction boundary can carry a preserved recent segment (`compactMetadata.preservedSegment`). The Claude and Codex bridges report compaction as tool cards whose `_meta.jetbrains.air.contextCompaction` has start and completion events, but no list of discarded items.


## Decision Log

- Decision: a workspace snapshot is a git-native delta against `origin`, as small as Git allows. It records ref names and their object IDs even when origin already holds the objects, but never copies an object that origin has.
  Rationale: a session that pushed everything should produce an almost empty delta, and restore should be "clone origin, then apply". This also fixes the main problem with today's launch-commit deltas: they re-copy pushed work and restore only where the source repository exists.
  Date/Author: 2026-11-18, user.

- Decision: untracked and ignored files are never captured. When suspend finds untracked files that are not ignored, it warns and requires consent: "Continue anyway" in the terminal and web UI, `--discard-untracked` on the CLI.
  Rationale: agents litter checkouts with files nobody wants carried along. Tracked state is what matters.
  Date/Author: 2026-11-18, user.

- Decision: the snapshot keeps local branches, tags, notes, the stash list, and linked worktrees nested inside the checkout, each as an origin delta.
  Rationale: the user said "we really should not throw away stashes", and agents use nested worktrees for parallel lanes.
  Date/Author: 2026-11-18, user.

- Decision: a merge, rebase, cherry-pick, revert, `am` or bisect that is in progress in a managed checkout is aborted before capture (for example `git merge --abort`). The warning says that any conflict resolution done so far is lost. A raw checkout is never aborted: when it has an operation in progress, the snapshot records HEAD and refs only, without uncommitted changes, and the warning says those changes will not follow the session to another target.
  The abort needs its own explicit consent: `--abort-operation`, or a dialog that showed that operation, checked against what capture finds. The operation is detected and aborted before HEAD and refs are read, so the snapshot describes the post-abort state.
  Rationale: this case is rare, and starting from a clean state is simpler than preserving operation state; the existing `mj-checkpoint/src/archive/git/operation.rs` is about 850 lines. A raw checkout belongs to the user, so Mjolnir must not change its state.
  Date/Author: 2026-11-18, user.

- Decision: working state is captured with Git's own stash commands.
  - `git stash create` captures staged and unstaged tracked changes as a commit, without touching the working tree or the stash list.
  - `git stash apply --index <commit>` restores the changes onto the saved HEAD.
  - `git stash store` rebuilds the stash list.

  Rationale: no custom patch format. Applying onto the exact HEAD the commit was made from cannot conflict.
  Date/Author: 2026-11-18, user.

- Decision: an unreachable origin never blocks suspend. Capture runs `git fetch --prune origin` on a best-effort basis; if that fails, it uses the remote-tracking refs it already has.
  Rationale: older refs only make the delta larger, which beats aborting. Restore fails visibly only if origin has since deleted objects the delta left out.
  Date/Author: 2026-11-18, user.

- Decision: managed checkouts (Mjolnir-created clones) require an `origin` remote.

  This applies to new sessions. Existing managed clones were allowed to start from a source repository without an origin, and some legacy linked-worktree archives depend on commits that exist only in the local source repository. Those sessions keep working on their own target: capture and restore treat the source repository's path as their origin, through `origin_override`, so they round-trip there but cannot move to another target.
  Rationale: the snapshot is defined relative to origin. Existing sessions must not be stranded by the upgrade (`AGENTS.md`, Upgrades).
  Date/Author: 2026-11-18, user.

- Decision: raw checkouts (a directory the user owns, used in place) behave as follows.
  - At launch, Mjolnir records the checkout's refs and stash list.
  - At suspend, the snapshot holds HEAD, the refs the session created or moved since launch, the stash entries added since launch, worktrees the session created, and the tracked working state. Objects are delta'd against origin.
  - Resuming on the same raw target uses the checkout as it is and never applies the snapshot.
  - Resuming elsewhere, a "resume conversion", creates a managed clone there and applies the snapshot.

  The snapshot is taken at suspend, not at resume as today's conversion does.
  Rationale: the user wants raw sessions resumable on other targets without sweeping in the user's unrelated branches or stashes. Snapshotting at suspend gives the state the agent last saw, and does not require the raw target to be reachable at resume time.
  Date/Author: 2026-11-18, user and plan author.

- Decision: resuming a workspace session onto a raw target creates a new managed clone on that target, never a linked worktree in the user's repository.
  Rationale: the user does not support resuming into an existing worktree; linked worktrees are being retired.
  Date/Author: 2026-11-18, user.

- Decision: legacy linked-worktree sessions are no longer created. Suspended ones resume onto a fresh managed clone at their saved commit, on their saved branch name, or on a detached HEAD if that name already exists.
  Rationale: the user has many such sessions suspended.
  Date/Author: 2026-11-18, user.

- Decision: Destroy stays as permanent removal. A new `suspend --without-checkpoint` stops the session without a new checkpoint.
  - It keeps the record, the history and any earlier checkpoint.
  - It completes with warnings when the target is unreachable.
  - It shows how old the copy that Resume will use is.
  - It replaces the terminal UI's "Discard changes since checkpoint" (internally `ForceStop`), the internal `suspend_session_without_checkpoint` for sessions with nothing to save, and the "Ignore and Suspend" nested-repository escape.

  Rationale: the user wants both a permanent-removal verb and an escape hatch for broken checkpointing. This also folds four stop paths into one.
  Date/Author: 2026-11-18, user.

- Decision: suspend is the only checkpoint producer. Removed:
  - automatic recovery copies;
  - the explicit checkpoint action;
  - the checkpoint that bundle export takes as a side effect;
  - checkpoint-and-restore in agent restart;
  - Move's handoff archives;
  - resume-time conversion archives.

  Rationale: idle-gated periodic copies are not close to free. They still need the live-copy machinery, and every copy re-uploads the full agent transcript. Agent restarts happen often because of upgrades. The user accepts that losing a target's disk permanently loses work since the last suspend; daemon restarts and worker crashes are unaffected because the worker's journal survives them.
  Date/Author: 2026-11-18, user.

- Decision: the daemon keeps one snapshot file per session, the latest. The target keeps a cache copy named by its SHA-256, which is deleted after 48 hours. Resume on the same target uses the cache copy when present, and otherwise uploads from the daemon.
  Rationale: the snapshot file is dominated by the agent's transcript, which can reach gigabytes, and the daemon's uplink is slow. A cache miss is harmless. The daemon copy must not expire, because a suspended session can only resume from it.
  Date/Author: 2026-11-18, user.

- Decision: Move uses the same snapshot as suspend and transfers it directly between targets.
  - On the same host it is a host-local pipe.
  - Between hosts, the daemon installs a one-time key on the destination, restricted to the import command and the source's address, and opens an EC2 security-group rule when needed. It probes both directions and then pushes or pulls.
  - When neither direction works, the daemon streams the snapshot without writing it to disk, and the move's progress says so.

  Quota and login switches on the same target keep their in-place path, which moves no workspace.
  Rationale: the user wants to avoid the daemon's slow uplink. SSH agent forwarding was rejected because it would let a target use every key the daemon holds.
  Date/Author: 2026-11-18, user and plan author.

- Decision: agents must be told plainly that Move and suspend lose untracked files, and how to avoid it. The places that say so:
  - the refusal message itself: "N untracked files will not be kept: <paths>. `git add` (or commit) anything you want to keep, or pass `--discard-untracked`";
  - `mj move --help` and `mj suspend --help`;
  - the bundled agent skill `mj-core/assets/skills/mj/SKILL.md`;
  - the CLI reference `docs/src/content/docs/cli-reference.md` and `docs/src/content/docs/sessions.md`.

  The same rules also go into every session's staged agent instructions file (the `AGENTS.md` or `CLAUDE.md` that Mjolnir writes into the harness profile), so agents see them before they create anything. Move requires the same consent as suspend.
  Rationale: the user wants agents using Mjolnir's tooling to understand that Move will lose data unless they stage it, and asked for the rules to be in the `AGENTS.md` Mjolnir creates.
  Date/Author: 2026-11-18, user.

- Decision: each session gets `.mj/keep/<session-id>/` at the top of its checkout, a Mjolnir-owned scratch directory exposed to the agent as `MJ_KEEP_DIR` and excluded from Git through `.git/info/exclude`, as `.mj/agents/` already is. The per-session subdirectory means two raw sessions sharing a checkout never carry each other's scratch, and Mjolnir never treats a pre-existing user directory as its own. (Where the rest of this plan says `.mj/keep/`, it means the session's own subdirectory.)
  - A cross-target Move carries it to the destination by direct transfer.
  - Suspend never saves it.
  - Suspend's preflight lists it with its size among the things that will not be kept, under the same consent as untracked files.
  - In a raw checkout it simply stays on disk with everything else.

  There is no `--with-untracked`.
  Rationale: session 5992a212, which runs many moves, reported that its real losses were git-ignored agent scratch under `.agents/scratch/` (receipts, JSON results, a harness). Neither the old nor the new design carried those, and untracked non-ignored files are rare for it. The user rejected `--with-paths <glob>` (it brings back per-file capture, and every caller must remember it) and documenting `git add -f` (scratch ends up committed). The user also said agents often create gigabytes of scratch, so it must never go into the daemon's checkpoints.
  Date/Author: 2026-11-18, user.

- Decision: `mj move` gains `--skip <path>` (repeatable; a path under `.mj/keep/` to leave behind). Both `mj move` and `mj suspend` gain `--dry-run`, which prints, without changing anything:
  - what would be kept and how large it is: the Git delta, the agent's session state including transcripts, and for Move each top-level entry of `.mj/keep/`;
  - what would be lost: untracked files with sizes, `.mj/keep/` on suspend, and an in-progress operation;
  - for Move, which transfer route would be used.

  Rationale: the user asked for a way to see sizes and leave large scratch behind before a move over a slow link.
  Date/Author: 2026-11-18, user.

- Decision: an automatic Move (the weekly-quota profile switch) never drops files. It runs only in place, keeping the environment, which is how it already behaves. When a profile switch would rebuild the environment, the automatic move does not happen; the session keeps its existing quota notice and waits for the reset as it does today.
  Rationale: nobody is present to consent to data loss in an automatic operation. The user confirmed that same-target-only is the desired behavior.
  Date/Author: 2026-11-18, user.

- Decision: the transcript redesign is out of scope.
  Rationale: the user's condition was to include it only if it is intertwined with this work. Investigation found it loosely coupled; the snapshot format and transfer do not depend on how transcripts are stored. The motivating example, t3code, keeps its own transcript copy anyway. The two real couplings, journal retention and tool-output compaction, are handled in Milestone 4.
  Date/Author: 2026-11-18, user's condition, plan author's finding.

- Decision: tool-output compaction in the daemon's transcript projection (`mj-store/src/database/materialized.rs`, `compact_materialized_transcript_through`) is triggered in two ways.
  - At suspend, for every harness.
  - Earlier, when a harness reports a successful context compaction. Claude and Codex report one today; Muse is added after its pinned wire shape is verified.

  The compaction boundary is the last completed turn that ended before the compaction started. It excludes unfinished tool calls, failed or no-op compactions, replayed events and child-session events. There is no age-based or count-based trigger.
  Rationale: the user asked to tie compaction to harness compaction. A numeric age or count would be a speculative cap, which `AGENTS.md` forbids.

  Accepted loss: the snapshot's canonical transcript is built from this projection. So once a tool call before a harness compaction is compacted, Mjolnir keeps only its summary (tool, target, result and diffstat) everywhere it keeps the transcript. The full body survives only where the harness's own native history kept it, and that history travels in the snapshot. These calls are already out of the agent's context. The user accepted the summary-only view for them when choosing this trigger.
  Date/Author: 2026-11-18, user and plan author.

- Decision: the worker's relay journal is retained until the daemon acknowledges it, no longer also until a checkpoint covers it. This depends on the Milestone 4 verification that nothing needs journal history below the acknowledgement.
  Rationale: the journal lives on the target, so it does not survive the failure a checkpoint exists for, and the daemon's store already holds everything it has acknowledged. Under suspend-only checkpoints, the current rule would keep a session's whole journal until it is suspended. The investigation flagged two places to check first: the command ledger's retry deduplication, and native-history evidence in `mj-worker/src/relay/native_history.rs`.
  Date/Author: 2026-11-18, plan author.

- Decision: suspend no longer asks for publication consent; `--acknowledge-unpublished-work` and `acknowledge_unpublished_work` are removed from suspend.
  Rationale: the snapshot now carries every unpushed commit, so suspend no longer risks unpublished work. The publication marker still gates automatic archival of old sessions. Per the user's standing preference, renamed or removed API fields get no aliases.
  Date/Author: 2026-11-18, plan author.

- Decision: `mj destroy --delete-branch` and the "Destroy and delete branch" buttons are removed.
  Rationale: only legacy linked worktrees used them. Those branches stay in the user's repository, where the user can delete them with Git.
  Date/Author: 2026-11-18, plan author.

- Decision: at upgrade, an in-flight Move recorded in the old format is rolled back to its source, and the user gets a notice to run the move again.
  Rationale: the source is kept until the destination is ready, so rollback is safe and can restart from durable state. Keeping the old move executor alive would mean two move mechanisms.
  Date/Author: 2026-11-18, plan author.

- Decision: old archive formats stay readable. Their untracked-file payloads are still restored, because those files are the user's data. Saved in-progress operation state is dropped with a notice.
  Rationale: suspended sessions from older releases must resume without user intervention (`AGENTS.md`, Upgrades).
  Date/Author: 2026-11-18, plan author.

- Decision: add `mj purge-untracked [--session <id> | --all] [--dry-run]` to reclaim space once the user no longer needs old untracked payloads.
  - For each suspended session whose archive was written in an old format, it rewrites the archive without its untracked-file payloads, keeping everything else byte for byte.
  - It verifies the new archive, replaces the file atomically, and updates the stored SHA-256 in the same transaction.
  - It rewrites archive payloads only. Move sources retained by earlier releases are whole environments and stay under the explicit `mj move-sources` cleanup. Old managed-clone archives also bundle original stash commits, and a `git stash -u` or `-a` entry's third parent carries untracked files the purge cannot strip without rewriting history. The purge reports those as retained ("N stash entries still hold untracked files"), so its result never implies they were removed.
  - It prints the space reclaimed per session. `--dry-run` reports without changing anything.

  Rationale: the user asked for "some kind of purge untracked tool to clean up my archives once I'm sure I don't need that stuff". Old untracked payloads stay restorable until the user explicitly purges them.
  Date/Author: 2026-11-18, user.

- Decision: the user approved the plan author's decisions on publication consent, `--delete-branch` removal, rolling back in-flight old-format moves during the upgrade to the release that ships this work, the raw launch-record location, in-place agent restart, `--without-checkpoint` with no earlier checkpoint, and the suspend-preflight endpoint.
  Date/Author: 2026-11-18, user.


## Outcomes & Retrospective

Nothing implemented yet.


## Context and Orientation

This section explains the parts of the repository involved, for a reader who has never seen it.

**Daemon and worker.** The daemon is the long-running control process (`mj daemon-run`; crates `mj-daemon` and `mj-controller`). It owns the SQLite store (`mj-store`), session records, lifecycle operations, and the HTTP API (`mj-server`). A worker (`mj-worker`, one per session) runs on the session's target and owns the agent's process. It keeps a durable event journal called the relay journal, from which a reconnecting daemon replays events. The agent program itself (Claude Code, Codex, OpenCode, Kimi Code, Grok, Antigravity, DSH or Muse) is called the harness. It keeps its own native history files under the worker's home directory on the target.

**Target.** A target is where a session runs. It can be:

- a bare local directory;
- a local Podman, Docker or Apple container;
- a remote host reached over SSH, bare or with containers;
- a persistent EC2 Podman host, which is an ordinary SSH Podman target;
- a Nitro microVM.

The central command wrapper that runs a program "inside" any target is in `mj-core/src/targets.rs` (around line 3691). SSH connections are daemon-owned, shared and leased per command (`mj-core/src/targets/ssh.rs`).

**Checkout kinds.** A session's working directory is one of three kinds:

- **Raw checkout:** a directory the user owns, used in place. Mjolnir never deletes it.
- **Managed clone:** created by Mjolnir with `git clone --local --dissociate` from the user's source repository. Its `origin` is then set to the source repository's real origin URL (`mj-controller/src/controller/worktree.rs`, around line 2170). Records: `ManagedWorktree` with `kind: ManagedCheckoutKind::Clone` in `mj-core/src/state.rs` (around line 1127).
- **Legacy linked worktree:** a `git worktree` of the user's repository on a branch named `mj/<session-id>` (`ManagedCheckoutKind::Worktree`). New sessions no longer get these, but suspended records still exist.

Suspend already removes managed clones and linked worktrees after the checkpoint is verified (`retire_managed_worktree` and `remove_managed_worktree_checkout` in `worktree.rs`, called from `mj-controller/src/controller/environment.rs`, around lines 368–482). There is one exception: removal is skipped when the source repository is missing (`worktree.rs`, around line 2777).

**Checkpoint archive.** A checkpoint is a single ZIP-like archive file. It is written on the target by a worker subcommand: the controller sends a JSON "spec" and the worker runs `export_from_spec_reader` or `capture_from_spec_reader` in `mj-worker/src/checkpoint.rs`. The archive is then copied to the daemon's `sessions` directory (`mj-controller/src/checkpoint_transfer.rs`), hash-verified and installed. The store keeps one row per session in `session_checkpoints`: archive path, SHA-256, creation time, and the event frontier, which is the journal ordinal the archive covers.

An archive has two halves:

- **Native and canonical session state:** the harness's native history files, the canonical transcript, queue, goals, notifications, attachments and child reports. Captured by `mj-checkpoint/src/checkpoint/capture.rs`, `native_scan.rs` and `harness_paths.rs`. This plan does not change this half, except where it is transferred and cached.
- **Repository state:** `mj-checkpoint/src/archive/git.rs` (about 3,900 lines, of which about 1,400 are live review, diff and push features unrelated to checkpoints).

How the repository half is captured is chosen per repository by `CheckpointRepositoryCapture` in `mj-checkpoint/src/checkpoint.rs`, which the controller selects in `mj-controller/src/controller/checkpoint/layout.rs`:

- managed clones use `CloneFrom(launch base)`;
- linked worktrees use `DeltaFrom(base)`;
- managed network workspaces use `RemoteWorkspace`, which already bundles `HEAD --not --remotes=origin` (`git.rs`, around line 1806);
- raw checkouts use `MetadataOnly`, which saves no tracked edits.

Untracked non-ignored files are tarred by `collect_untracked_tar` (`git.rs`, around line 1893). Local refs and stashes are collected around lines 1614–1780. Nested linked worktrees are captured around lines 2011–2321 and restored around 2632–2918. In-progress operations are handled in `git/operation.rs`. Repository metadata, including saved refs and the stash stack, is `RepositoryMetadata` in `mj-core/src/archive.rs` (around line 100). The archive's schema version constants are in `mj-checkpoint/src/archive.rs` (around lines 1118–1142 and 1947). The controller-to-exporter spec has a version, `CHECKPOINT_EXPORT_PROTOCOL_VERSION` (currently 2), and the spec structs use `deny_unknown_fields`, so the controller must send an older worker the spec version that worker understands.

**Checkpoint producers today.** Only the first is suspend:

1. Suspend: `mj-controller/src/controller/lifecycle.rs`, around lines 115–143 and 304.
2. Automatic recovery copies: `mj-daemon/src/recovery.rs`, driven by observations in `mj-daemon/src/daemon/snapshot.rs`.
3. The explicit checkpoint action: `mj-daemon/src/daemon/actions.rs` (around line 540) and `mj-controller/src/controller/checkpoint/capture.rs`.
4. Bundle export from a live session: `mj-daemon/src/server_runtime/api.rs`, around lines 3392 and 4527.
5. Move and quota or login moves: `lifecycle.rs`, around lines 216–255.
6. Agent restart: `mj-daemon/src/daemon/restart.rs` (around line 449) and `lifecycle.rs` (around line 183).
7. Raw-to-workspace resume conversion: `mj-controller/src/controller/resume.rs`, around lines 2057–2110; the conversion kinds are in `worktree.rs` around line 960.

The live-copy pipeline lives in `mj-controller/src/controller/checkpoint/`:

- `latched.rs`, `lease.rs`, `staging.rs`, `validate.rs`: prestage, early release, two-phase packing and revalidation.
- `barrier.rs`: the worker barrier that freezes command dispatch while a consistent cut is taken.
- `workspace_lease.rs`: idle reservation for file uploads and worker upgrades, unrelated to checkpoints; keep it.
- `leftovers.rs` and `archives.rs`: cleanup and reference reconciliation.

**Lifecycle verbs and surfaces.**

- Daemon: `mj-daemon/src/daemon/session_lifecycle.rs` routes `SessionLifecycleRequest` from `mj-client/src/daemon.rs` (around line 513). That request covers Suspend, Discard, DestroyStopped and ForceDestroy. Older direct actions also exist (around lines 1199–1244 of the same file).
- HTTP API: `mj-server/src/server/api/routes.rs` (around lines 65–82) and `mj-server/src/server/api/turns.rs` (around lines 448–567).
- CLI: `mj-cli/src/main.rs` (around lines 196–232) and `mj-cli/src/api_commands.rs` (`SuspendArgs` around line 907, `DestroyArgs` around line 925, output around lines 2058–2104).
- Terminal UI: commands in `mj-tui/src/actions.rs` (around lines 959, 1338 and 2011–2033); dialogs in `mj-tui/src/dialogs.rs` (around line 356) and `mj-tui/src/dialogs/render.rs` (around lines 1433–1727).
- Web viewer: `mj-server/src/web/viewer.js`. The menu is around line 2390. The Destroy dialog is around lines 6533–6583. The suspend confirmation, around line 10161, uses the browser's `confirm()`, which cannot label a button "Continue anyway".
- Other entry points that suspend in bulk or automatically: workspace deletion (`mj-daemon/src/daemon/close_workspace.rs`), `mj daemon stop --suspend-sessions` (`mj-cli/src/main.rs`, around lines 329–339 and 805–858), and `mj acp --on-exit` (`mj-cli/src/acp.rs`, around lines 68–88 and 627–634).

**Move.**

- Cross-target moves: `mj-controller/src/controller/move_session/transfer.rs` (about 1,700 production lines) and `mj-worker/src/move_workspace.rs` (about 650 production lines). The untracked-file picker and exclusions are in `mj-core/src/move_workspace.rs`.
- Move operations are persisted as JSON with `deny_unknown_fields` (`mj-core/src/state/session_move.rs`, around line 299).
- In-place quota and login switches keep the environment and move no workspace bytes.

**Retention couplings.**

- Journal retention: the worker deletes journal history only below `retained_through = min(acknowledged_through, recovery_floor_ordinal)` (`mj-core/src/relay/snapshot.rs`, around line 2097; garbage collection in `mj-worker/src/relay/journal.rs`, around line 1002). The recovery floor advances only when a checkpoint completes (`mj-core/src/relay/snapshot/apply.rs`, around lines 1206–1264).
- Tool-output compaction: the daemon compacts large tool output in its transcript projection only for items a verified checkpoint covers (`mj-store/src/database/materialized.rs`, around line 985, called from `mj-controller/src/controller/checkpoint/staging.rs`, around line 526). It was added because one session's projection reached 635 MiB, 561 MB of it tool output (commit 84aedcb88).
- Harness compaction signals: the Claude and Codex bridges report context compaction as tool cards titled "Compact conversation", with `_meta.jetbrains.air.contextCompaction` provenance and start and completion events. Muse uses `_meta.contextCompaction`. A transcript item's `position` is the journal ordinal that first created it (`mj-core/src/transcript.rs`, around line 554).

**Git terms used here.**

- **Pack:** the file format Git uses to ship objects. `git pack-objects --revs` reads tip commits, then `--not` and exclusions, on standard input, and writes every object reachable from the tips but not from the exclusions. A non-thin pack is self-contained, and `git index-pack` loads it into another repository. A **bundle** (`git bundle create`) is a pack plus a list of refs, and needs refs to name its tips, so it is used only for user-facing export.
- **Remote-tracking refs:** the `refs/remotes/origin/*` refs that record what origin had at the last fetch.
- **Stash commit:** the commit that `git stash create` builds from the index and the tracked working tree.


## Plan of Work

### Milestone 1: git-native snapshot capture, restore and inventory

**Scope.** Add a new module, `mj-checkpoint/src/snapshot.rs`, whose functions run Git in a checkout and produce or consume a workspace snapshot. They go through the existing `GitCommandRunner` abstraction in `mj-checkpoint/src/archive/git.rs`, so the same code runs locally and on targets. This milestone adds the functions and their tests only; nothing calls them yet. At its end, a test can capture a checkout, restore it into a fresh clone of the same origin, and observe equal branches, tags, stash list, HEAD, index and tracked working files, with no untracked files.

**Capture** works on a whole session at once. A session's export layout (`session_export_layout`) can hold several independent repositories, so `capture_session_snapshot` takes every repository and runs step 1 across all of them before the first abort or other change in any of them. Consent items and refusals are keyed by repository ID plus path. The steps below describe one repository and the linked worktrees nested inside it (from `git worktree list --porcelain`, recorded by path relative to the checkout; for a raw checkout, only worktrees absent from the launch record count):

1. **Consent and operations, for every checkout before anything is read or changed.** An abort in a nested worktree can move a shared branch, and during a rebase or bisect HEAD sits on a temporary commit. So all of this happens before any HEAD or ref is read.
   1. Build the inventory:
      - the untracked non-ignored paths of every checkout, with nested worktree directories excluded;
      - the stash entries with a third parent, which `git stash -u` or `-a` creates to hold untracked or ignored files;
      - the in-progress operation of each checkout: `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `rebase-merge`/`rebase-apply`, `BISECT_LOG`.
   2. Check it against the caller's `CaptureConsent`:
      - Untracked items are paths and stash untracked payloads. `UntrackedConsent::None` covers none of them, `Any` (the CLI flag) covers all, and `Shown(items)` (a dialog) covers only what the dialog displayed. An item that has since disappeared is fine; a new one is not.
      - In a managed checkout, each operation must be covered by `OperationConsent::Any` or by a matching `Shown` entry. It is identified by checkout path, kind and identity: a digest of the current HEAD commit and branch, plus the contents, inode numbers and nanosecond modification times of its state files. Git rewrites them through a lockfile rename, so a merge aborted and restarted between the same commits still gets a new identity. The state files are (`MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, the rebase directory's `head-name`/`onto`/`orig-head`, `sequencer/head` and `sequencer/todo`, `BISECT_START` and the first line of `BISECT_LOG`). An operation that ended and was replaced by another of the same kind while the dialog was open therefore does not inherit the consent.
      - If anything is not covered, capture changes nothing and fails with `SnapshotRefused::Untracked(items)` or `SnapshotRefused::OperationInProgress(path, kind)`.
      - Hidden tracked edits keep Move's existing guard (`mj-worker/src/move_workspace.rs`, around line 98). If any index entry has `assume-unchanged` or `skip-worktree` set, `git stash create` could miss its edits, so capture refuses with `SnapshotRefused::HiddenTrackedEdits(paths)`. There is no consent override; the message says to clear those flags first.
      - Submodules keep today's guard, with no consent override (see `mj-checkpoint/src/checkpoint/submodules.rs`). If a checked-out submodule has any status entry (untracked files included, as today's guard checks), or if any submodule commit the snapshot needs is not reachable from that submodule's remote-tracking refs, capture refuses. The commits it needs are the gitlink in HEAD, the gitlink in the index and the checked-out commit. A staged gitlink to an unpushed submodule commit is therefore caught even when the submodule looks clean. Reachability is checked against the remote restore will actually use. Capture records each submodule's effective URL (`git config submodule.<name>.url` in the superproject), checks against the remote-tracking refs of the submodule remote whose URL matches it, and refuses if none matches. Restore sets that URL before initializing, so a commit pushed only to a fork that restore would not consult is caught. The comparison with remote URLs happens locally, unredacted. Only the URL run through `redact_origin_credentials` is persisted and restored, so a token embedded in a submodule URL never enters a snapshot, cache or destination. with `SnapshotRefused::DirtySubmodule(path)`. A parent's pack and stash carry only the gitlink, never the submodule's own work. The message says to commit and push inside the submodule, or to discard the change.

      Capture itself is the single decision point, so nothing can appear between a separate check and the capture.
   3. Managed checkouts: run each covered abort (`git merge --abort`, `git rebase --abort`, `git cherry-pick --abort`, `git revert --abort`, `git am --abort`, `git bisect reset`).
   4. Raw checkouts: never abort. Mark the checkout `working_state_skipped`, and remember its pre-operation position for step 3:
      - rebase: the branch named in `rebase-merge/head-name` or `rebase-apply/head-name`. If that file says `detached HEAD`, the position is the commit in `orig-head`, recorded as detached;
      - bisect: the branch or commit in `BISECT_START`;
      - a cherry-pick or revert sequence: the commit in `.git/sequencer/head` when it exists. That is where `--abort` returns, and the current tip may already include applied steps. Otherwise, HEAD as it is;
      - merge or am: HEAD is still the branch.

      Resuming elsewhere then lands where an abort would have returned.
2. **Learn what origin has.**
   - **Managed checkout:** best-effort `git fetch --prune origin`, since Mjolnir owns this clone's refs.
   - **Raw checkout:** never fetch. The user's remote-tracking refs are theirs, and `--prune` could delete the last reference to commits. Read origin's heads and tags with `git ls-remote origin` instead, and use the SHAs that exist locally as exclusions in step 6.

   In either case, when origin cannot be reached, exclude only the existing `refs/remotes/origin/*`, never local tags (a local tag may be unpublished), and set `remote_refs_stale`. This is the user's accepted risk: using old refs is better than refusing to suspend. If origin force-pushed since the last fetch, the delta can omit objects origin no longer has, and a later restore then fails visibly at its missing-object check. Suspend's result and the session's notice say "origin was unreachable; commits origin has since removed may not be restorable", so the user can resume and push, or retry suspend once origin is reachable.
3. **Pin the cut.** Read every ref once with `git for-each-ref`, plus each checkout's HEAD: a branch name or a detached commit, or the pre-operation position from step 1. Select the refs to keep:
   - **Managed checkout:** every ref under `refs/heads`, `refs/tags` and `refs/notes`, plus the durable Mjolnir refs under `refs/mj/` that the pre-push hook and review read (reviewed, overridden and push-base). Use the predicate today's clone capture applies for "durable Mjolnir refs" (`RepositoryMetadata::saved_refs`). Also the stash list.
   - **Neither kind keeps `refs/stash` as a ref.** It points at the original stash commit, which may have a `-u` or `-a` payload. The stash list, with its sanitized replacements, owns stash capture and restore.
   - **Raw checkout:** the refs whose value differs from the launch record (Milestone 2 writes that record), plus the branch each captured checkout's HEAD is on (primary and nested) even when it is unchanged, plus stash entries absent from the launch record.

   The stash list is read with `git stash list --format=%H%x00%gs`, so the messages can be recreated.
4. **Working state.** Run `git stash create` in each checkout not marked `working_state_skipped`. Empty output means a clean state; otherwise the result is a commit whose first parent is HEAD and whose second records the index. For each kept stash entry with a third parent, write a replacement without it, `git commit-tree <stash>^{tree} -p <stash>^1 -p <stash>^2 -m <message>`. It keeps the entry's tracked content; its untracked payload was consented to in step 1.
5. **Verify the cut.** Each working-state commit's first parent must equal its checkout's pinned HEAD, and a second `for-each-ref` must match the pinned set. If anything moved (possible during a barrier-free export), start again from step 3 after a short backoff; the change in state is what ends the loop. If nothing is stable after a few attempts, fail with a clear error. Under the suspend barrier, the first pass succeeds.
6. **Pack the delta.** After the pack is written, capture lists the untracked items again, and refuses if anything has appeared that the consent does not cover. A raw checkout can be written by the user or by another session, whose processes this session's freeze does not stop. For a raw checkout, the guarantee is this: everything the snapshot omits was either consented to or still sits untouched in the raw checkout, which Mjolnir never deletes.

   Then the pack itself: Run `git pack-objects --revs <basename>`, which writes `<basename>-<hash>.pack` and its `.idx` straight to disk (a pack can be larger than memory should hold), feeding it on standard input:
   - the pinned SHAs: kept refs, every checkout's HEAD commit (attached or detached, primary and nested), working-state commits, and kept (or replaced) stash commits. A raw session that started on a branch with commits origin lacks, and only edited tracked files, depends on those commits;
   - a `--not` line;
   - the exclusions, chosen by what step 2 learned:
     - **Origin reachable:** exactly the SHAs origin advertises (`git ls-remote origin`, heads and tags) that exist locally, and never stale remote-tracking refs. A raw checkout's `refs/remotes/origin/*` may point at history origin has force-pushed away. A fresh clone fetches origin's heads and tags, so their history need not travel.
     - **Origin unreachable:** the existing `refs/remotes/origin/*` only, with `remote_refs_stale` set.

     For a managed checkout, the two rules coincide in practice, because it has just fetched with `--prune`.

   The result is a non-thin pack containing only what origin lacks; it may be empty. Packing from SHAs writes no ref at all. Two raw sessions sharing a Git common directory, or an export overlapping a suspend, therefore cannot disturb each other, and a crash leaves nothing to clean up.
7. **Return a `WorkspaceSnapshot`:** the origin URL and push URLs, both passed through the existing `redact_origin_credentials` so no embedded token enters a snapshot, cache or destination, HEAD, the kept refs with their object IDs, the stash list (commits and messages), the working-state commit, the nested worktrees, `working_state_skipped`, `remote_refs_stale`, and the pack path.

**Restore**, into a target directory:

1. Establish the base clone. Restore takes a `RestoreBase`:
   - `Clone { origin, seed }` clones origin, then runs `git fetch --tags origin`, so every tag the exclusions relied on is present.
   - `Provisioned { allow_origin_fetch }` uses a clone the caller already prepared and skips both.

   History-cutoff workspaces (`--no-later-history`, see `HistoryCutoff` in `mj-core/src/targets.rs`) deliberately have no origin remote. Their resume already provisions and verifies the cutoff, so they restore with `Provisioned { allow_origin_fetch: false }`, capture with `origin_override`, and never fetch later history. Their capture uses `ExclusionSource::Commits(<cutoff base>)`: it excludes only what the provisioned base will contain, never origin's refs, so a session commit that was also published on origin is still packed. Keep the existing cutoff resume regression passing. An optional local seed (the clone cache on the target, or a source repository) may be passed for speed with `--reference-if-able`, followed by `git repack -a -d` and removal of the alternates file so the clone is independent. Correctness never depends on the seed.
2. Set `remote.origin.pushurl` to each saved (redacted) push URL. For each restored branch whose upstream was recorded (`branch.<name>.remote` and `branch.<name>.merge`), set it again so ordinary `git pull` and `git push` keep working; the destination's credential helper supplies credentials, as today's network-workspace restore does, so a workspace that reads from upstream and pushes to a fork keeps doing so.
3. Copy the pack into `.git/objects/pack/` and run `git index-pack` on that file. Then run the missing-object check below. Every commit it reports missing, whether a saved tip or an interior parent the pack omitted because origin had it, gets one `git fetch origin <sha>`, and the check runs again. This covers a branch the session pushed and that was later deleted on origin, including a local unpushed commit built on it: the snapshot deliberately keeps only refs for what origin had, and many servers still serve a commit by SHA. After the refs and stash entries are created, verify the whole object graph: `git rev-list --objects --missing=print` over every saved tip, HEAD, working-state commit and stash commit must report no missing object (no line starting with `?`). Plain `--objects` lists blob IDs without checking that the blobs exist, and checking only the tips is not enough, because origin may have rewritten history so that an excluded parent, tree or blob is gone. On failure, nothing is reported as restored, so a Move never retires its source over an incomplete destination.
4. Create every saved ref at its object ID.
5. Rebuild the stash list with `git stash store -m <message> <commit>`, oldest first.
6. Point HEAD at the saved position, with `git symbolic-ref HEAD refs/heads/<branch>` or `git update-ref --no-deref HEAD <commit>`. Then run `git reset --hard <saved head commit>`, so the index and working tree match the saved commit. Without the reset, a saved branch that is origin's default branch at a commit different from origin's tip would leave the clone's old files in place, where they would look like local changes.
7. If there is a working-state commit, run `git stash apply --index <commit>`. Then set each recorded index gitlink with `git update-index --cacheinfo 160000,<sha>,<path>`. Do this for every mode-160000 index entry, initialized or not, because a staged gitlink can exist without a checkout, and because `git stash create` omits a change that consists only of a staged gitlink update. Then initialize exactly the submodules that were initialized at capture, at every depth and parent before child, each with `git -C <parent> submodule update --init -- <child>`. Never use `--recursive`, which would also initialize nested submodules the session never had, and resume would fail if their remotes were unreachable. Then, and check out each one's recorded commit detached (`git -C <path> checkout --detach <commit>`), fetching it from the submodule's remote first if needed. Capture records each initialized submodule's actual checked-out commit, which can differ from both the committed and the staged gitlink, and the guard already ensured it is reachable from the submodule's remote. Initializing only after the index is applied keeps a staged submodule update from being undone.
8. Recreate nested worktrees with `git worktree add` at their recorded relative paths. Position, reset and apply their working state the same way.

When an object the snapshot names is missing, because neither the pack nor origin has it, fail with a message naming the ref and suggesting that origin may have rewritten history.

**A raw checkout without an `origin` remote** still captures: the snapshot records HEAD, has `origin: None`, and holds no pack and no working state. Resuming in place needs nothing more. Restore refuses a snapshot without an origin with a clear error, and the resume dialogs offer only the original target for such a session. A managed checkout without an origin is an error at capture.

**Inventory**, for warnings:

1. List untracked non-ignored paths with `git ls-files --others --exclude-standard -z`, excluding the directories of nested linked worktrees, which are captured.
2. Report any in-progress operation, and whether this checkout is raw.
3. Repeat for each nested worktree.

The suspend warnings in Milestone 3 present this result.

**Tests** are integration tests against real Git; `AGENTS.md` lists Git as a boundary that warrants them. They go in a `#[cfg(test)] mod tests` block in `mj-checkpoint/src/snapshot.rs`. Each test uses a bare repository in a temporary directory as origin. They cover:

- a pushed branch, which yields an empty pack while its ref is recorded;
- an unpushed commit and a new tag;
- a three-entry stash list whose messages survive;
- staged and unstaged edits to the same file, restored with the staged part still staged;
- a nested linked worktree with an unpushed detached commit and an unstaged edit;
- a merge with conflicts, aborted in a managed checkout and left alone in a raw one;
- a raw checkout where a user branch that existed at launch is excluded;
- a raw checkout whose remote-tracking ref for a branch deleted on origin still exists after capture;
- a raw checkout on a pre-existing unpushed branch with only working changes, restored onto a fresh clone;
- stale remote-tracking refs after origin becomes unreachable (rename the bare directory), still producing a restorable snapshot while origin is back;
- origin force-pushed after capture so a recorded object is gone, with a clear error.
- a clean managed checkout whose only unpushed commit is reachable from a detached HEAD;
- a saved branch that is origin's default branch, one commit ahead of origin's tip, which must restore with a clean `git status`;
- a managed checkout mid-rebase without consent, which must refuse and leave the rebase intact;
- a raw checkout mid-rebase, which must record the original branch as HEAD;
- a raw checkout without an origin remote, which must capture HEAD only and be refused by restore;
- a `git stash -u` entry, whose tracked part restores while its untracked file is gone and required consent;
- a pack larger than 64 KB, written and loaded through files;
- an origin URL with an embedded token, which must not appear in the snapshot;
- a dirty submodule, which must be refused;
- an inventory taken during a conflicted merge, which must succeed without running `git stash create`;
- a dialog-style `Shown` consent for one file while a second appears, which must be refused naming only the new file, with nothing changed;
- a nested worktree in the middle of a multi-commit cherry-pick, whose abort moves its branch; the snapshot records the post-abort tip;
- an origin workspace with a separate push URL, which keeps it after restore;
- history reachable on origin only through a tag, which is excluded from the pack.

### Milestone 2: suspend and resume on the new snapshot

**Scope.** Suspend captures the repository half of the archive with Milestone 1's `capture_workspace_snapshot` instead of `CloneFrom`, `DeltaFrom`, `MetadataOnly` and the untracked tar. Resume restores it with `restore_workspace_snapshot`.

The new capture must never become active without its consent checks, so this milestone also lands:
- the request fields `discard_untracked` and `abort_operations` (shapes under Interfaces), and the CLI flags `--discard-untracked` and `--abort-operation`;
- the inventory recheck at the capture cut described in Milestone 3, which refuses when untracked files, a nonempty `.mj/keep/` on a managed checkout, or an operation is found without the matching consent.

Until Milestone 3 adds the dialogs, the terminal and web UIs show that refusal as an ordinary suspend failure. That is safe, because nothing is lost.

**Freeze workspace writers before the final capture.** The relay barrier stops command dispatch, but a process the agent started earlier, such as a background command, a build or a dev server, can keep writing to the checkout. A file it creates after the consent check would then be deleted with the checkout, unseen.
1. Under the barrier, after the early consent check, the worker writes a freeze record to its durable state before sending any signal. The record holds the host's boot ID, the record time (in the same boot-relative clock as process start times: `/proc/<pid>/stat` field 22 on Linux, the kernel process start time on macOS), and the process groups the worker owns for the session (the harness and the background commands it tracks).

   Ownership is checked against the session's process marker, the environment marker Mjolnir already places on every process of a session. `mj-worker/src/work_facts.rs` and its Linux and macOS readers already find it to attribute processes to sessions (`/proc/<pid>/environ` on Linux, `ps -E` on macOS). Group P counts as the session's only if both hold:
   - the host's boot ID matches the record;
   - at least one live member of P carries this session's marker.

   A numeric group ID can be reused by an unrelated group, even one whose leader has since exited while its members live on, but that group's processes do not carry this session's marker. The check therefore stays correct when the original leader has exited, when members forked and exited, and after a worker crash at any point after the record was written. A different boot ID means every frozen process died with the old boot. A group with no live marked member is never signaled. Exactly as with today's process attribution, a process that wipes its environment loses the marker; it is then not provably the session's, and is left alone rather than risk signaling an unrelated process.

   The worker then stops each group with `killpg(SIGSTOP)`, repeatedly. After each signal it enumerates the group's current members by group ID (`/proc` on Linux, `ps -g` on macOS) and signals again while any live member is not yet stopped. A stopped process cannot fork, so the loop ends, even when a member forked a replacement and exited during the race. Resume and teardown also signal whole groups with `killpg` after the same ownership check, so every member is covered whenever it joined. Every signal, including the repeated `SIGSTOP`, is preceded by the marker check.
2. The loop above is what confirms that every live member has actually stopped before capture begins. The wait can be cancelled: a process in uninterruptible sleep (state `D`, for example blocked on an unavailable filesystem) may never stop. While waiting, the suspend's progress names the processes that have not stopped, and the user's cancel, or `suspend --without-checkpoint`, has the daemon record its decision and release or retire the freeze as in step 4. There is no timeout guessing how long is too long. Sending the signal only queues it, and a process running on another CPU can still write. On Linux, poll `/proc/<pid>/stat` until each live member's state is `T`; on macOS, poll `ps -o stat=`. A member that exited (state `Z`, a zombie whose frozen parent cannot reap it) or has disappeared counts as quiescent, because it can no longer write. The poll waits only for live writers, so it always ends. Only then does the final inventory and capture run, so the consent check and the saved cut see the same workspace.
3. Capture runs, as the single consent decision point.
4. **The daemon alone decides whether the frozen groups resume or are retired,** and it decides from its durable suspend intent. The worker never resumes or kills them on its own judgment, so a lost acknowledgement can never split the decision.
   - **Any failure before the daemon has verified and installed the archive** is reported to the daemon, which records the failed suspend and tells the worker to resume (`SIGCONT`). Failures include a consent refusal, a Git or I/O error, a transfer or verification error, and cancellation. The session then continues untouched.
   - **Once the daemon has verified and installed the archive,** it records the teardown decision in the same transaction that already gates target deletion, and tells the worker to terminate the groups, process groups first and files second.
   - **If the connection drops or the worker restarts while groups are frozen,** the worker keeps them frozen and, on reconnect, asks the daemon for the decision. The daemon's pending suspend intent, which already resumes after a daemon restart, answers resume or retire. The freeze record is cleared only when that answer has been carried out.
   - **The consent the user gave** (the `Shown` lists or the `Any` flags, and `without_checkpoint`) is persisted with the accepted suspend intent, in the same write that records the session as closing before the HTTP acceptance. A daemon restart before the final capture rechecks against exactly that consent, never a default. This changes a persisted record; classify the migration as compatibility requires.
   - **A user waiting on a stalled suspend** sees it as "suspending". They can retry, or run `suspend --without-checkpoint`, whose decision is retire. The processes are never left frozen without an owner.

Processes that left the worker's process groups (daemonized with `setsid`) are outside this guarantee, exactly as they are outside today's teardown; say so in the code comment. Add tricky-condition tests: a background command creates a file after the preflight, and suspend without consent for it must refuse and then resume the command; a capture that fails with an injected Git error must also resume the frozen groups. At the end of this milestone, suspending and resuming a managed-clone session, a raw session (in place and onto another target) and a legacy-worktree session all work, and old archives still resume.

**Plain directories.** Sessions opened in a directory without Git (`PlainDirectory` in `session_export_layout`, `RepositoryMetadata::plain_directory`) keep their existing metadata-only treatment. Suspend never runs the Git inventory, consent check or capture on them, and their transcript is saved as today.

**Archive format.** Add a new capture mode, `CheckpointRepositoryCapture::OriginDelta`, and a new archive schema version in `mj-checkpoint/src/archive.rs`. An older daemon's archive reader rejects unknown schema versions. Once a suspend writes the new format and removes the checkout, an older daemon could open the session but never resume it. So the store migration that ships with this milestone is **breaking**: advance the migration revision and raise the minimum compatible read and write revision in the same transaction, with that reason beside it, and test it with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. Milestone 5's `MoveOperation` change raises the floor again, under its own revision. The archive stores the `WorkspaceSnapshot` metadata in `RepositoryMetadata` (new fields beside the existing `saved_refs` and `stash_stack`) and the pack as a payload.

**Spec version and older workers.** Raise `CHECKPOINT_EXPORT_PROTOCOL_VERSION` to 3 for specs that request `OriginDelta`.

A session whose running worker predates this change must not fall back to the old capture. That capture stores untracked files and full clone history, which is exactly what the user excluded, and can mean gigabytes of upload. An old worker also cannot freeze its writers. So suspending such a session first upgrades its worker through the existing atomic idle replacement (`mj-controller/src/controller/worker_restart.rs`, `IdleWorkspaceLease`), after interrupting any turn as suspend already does, and then proceeds exactly as for a current worker: consent, freeze, capture.

There is one suspend path, and no version-2 spec ever writes a new archive. If the replacement cannot happen, because the worker is not idle or the swap fails, suspend fails with that reason and the session keeps running. `--without-checkpoint` remains available.

**Raw launch record.** For a raw session, record the checkout's refs (`git for-each-ref --format=%(refname)%00%(objectname)`), its stash list and its linked worktrees when the session launches. Store the record as a file in the repository's common Git directory, at `mj/sessions/<session-id>/launch`, written by the worker at launch. Mjolnir already writes under `refs/mj/` in user repositories for review pins, so this adds no new kind of side effect. Suspend reads the file. Destroy deletes it, and suspend keeps it. When the file is missing (a raw session launched before this change), capture keeps every local ref, the same set as a managed checkout (heads, tags, notes and durable `refs/mj/` refs, never `refs/stash`), plus all stash entries. Treating them as unchanged would drop the session's own branches when it is converted to another target, and objects are still delta'd against origin, so the cost is small.

**Resume paths** (`mj-controller/src/controller/resume.rs`):

- **Raw session on its own raw target:** do not restore the repository half. The checkout is authoritative; this matches today's behavior.
- **Raw session on another target (raw-to-workspace conversion):** provision a managed clone of origin there and restore the snapshot into it.
  - A raw session suspended by an earlier release has a `MetadataOnly` archive without tracked edits, and its conversion has always read them from the raw checkout at resume. Keep that behavior for those archives, but produce the snapshot with Milestone 1's `capture_workspace_snapshot`, run on the raw checkout at resume time (with no launch record and `UntrackedConsent::Any`), instead of the old code. The old conversion also carried untracked files. The new one does not, but nothing is lost: the raw checkout is left untouched and keeps them. The resume notice lists the untracked files that stayed behind in the raw checkout.
  - Then delete the old resume-time snapshot code (`raw_checkout_snapshot`, `conversion_checkpoint`, around lines 2057–2110).
  - When the raw checkout has no origin, the resume dialogs do not offer other targets for that session and state the reason.
- **Workspace to raw:** create a new managed clone on the raw target and restore into it. `WorkspaceToRawConversion` in `worktree.rs` (around line 981) currently creates a linked worktree on `mj/<session>`; change it to create a clone.
- **Old archives** (`CloneFrom`, `DeltaFrom`, `RemoteWorkspace`, `MetadataOnly`, any schema version): keep the existing readers in `git.rs`, with one change for linked-worktree records (`ManagedCheckoutKind::Worktree`). Resume them into a fresh managed clone instead of re-adding a worktree:
  1. Clone origin. If the archive's origin is empty, which the old collector recorded when `remote.origin.url` was unset, clone the source repository on the same target instead. That is these sessions' only upstream.
  2. If the archive's base commit is missing from that clone, fetch it once from the source repository on the same target; for these old sessions the base often is not on origin.
  3. Apply the archived delta.
  4. Check out the saved commit on the saved branch name, or as a detached HEAD if that name is already a branch in the new clone with a different value.

  Old untracked payloads are restored. Saved in-progress operation state is dropped, and a resume notice says so.

**Fail visibly.** A clone is independent of its source, so retiring it depends only on the verified origin-relative snapshot, never on the source repository. Remove the missing-source short-circuit in `remove_managed_worktree_checkout` (`worktree.rs`, around line 2777). Also remove the separate prerequisite in its caller `retire_managed_worktree`, which runs `git cat-file` in the source repository for the launch commit (`worktree.rs`, around lines 2748–2761). For clones, the prerequisite becomes the session's verified, installed snapshot. Linked-worktree removal still needs the source repository.

**Tests.**

- One end-to-end test in an isolated instance, run as described under Validation: suspend and resume a managed-clone session across two local targets, bare and Podman.
- A compatibility test that resumes a checked-in old-format archive fixture of each capture kind, including a legacy linked worktree, into a fresh managed clone. Such a fixture is a persisted-format reader test, which `AGENTS.md` allows; mark it `// Hard-won` only if it catches a defect after the code was believed done.

### Milestone 3: warnings, consent and lifecycle verbs

**Scope.** Every way of suspending a session shows the untracked-file and in-progress-operation warnings and requires consent, and `suspend --without-checkpoint` exists. At the end, the CLI, terminal UI and web UI behave as described under Validation.

**Preflight.** Add a daemon operation and an HTTP route, `GET /api/v1/sessions/{session_id}/suspend-preflight`. It runs Milestone 1's inventory on the target in a background task and returns:

- the untracked paths;
- any in-progress operation per checkout;
- whether the checkout is raw;
- the existing interruption and unfinished-children facts.

The terminal UI and web UI must call it off their event loops (`AGENTS.md`: no blocking I/O on render loops). They show "Checking workspace…" until it returns.

**Consent.** Add `discard_untracked: bool` to the suspend request: the HTTP body in `turns.rs`, `SessionLifecycleRequest::Suspend` in `mj-client`, and the older direct daemon action. Remove `acknowledge_unpublished_work` and `ignore_nested_repositories` from suspend.

**Consent is checked at the capture cut against what was approved.** The request carries the consent the user actually gave:
- **CLI flags approve blindly:** `--discard-untracked` sends `UntrackedConsent::Any`, and `--abort-operation` sends `OperationConsent::Any`. The person or agent typing the flag accepts whatever is there.
- **Dialogs approve exactly what they displayed:** `Shown` with the listed untracked paths, including `.mj/keep/` as one entry when it was shown, and `Shown` with the listed (checkout, operation) pairs.

Milestone 1's capture checks untracked paths and operations against that consent inside the barrier, and refuses with the uncovered items before changing anything. The controller checks a managed checkout's `.mj/keep/` (not Git state, so not part of capture) in the same step, immediately before capture under the barrier: if `.mj/keep/` is nonempty and neither `Any` nor a `Shown` list containing it covers it, suspend refuses.

On refusal:
- the CLI names the uncovered items and the flag, as it does today for the publication flag (`mj-cli/src/api_commands.rs`, around line 1357);
- the terminal and web UIs show the dialog again with the new inventory.

Aborting an operation needs its own consent because it discards conflict resolution. This is what makes consent safe: nothing the user did not see is discarded without a blanket flag.

**Surfaces.**

- **CLI:** `mj suspend --session <id> [--discard-untracked] [--abort-operation] [--without-checkpoint] [--dry-run] [--json]`.
- **Terminal UI:** the suspend dialog (`Confirmation::SuspendSession` in `mj-tui/src/dialogs.rs`) gains the untracked list and the operation warning. When the list is longer than fits, it shows the first entries and "and N more (v to view all)", which opens a scrollable view of the full list. The button reads "Discard N files and continue", so the count is always visible. Its buttons are "Cancel" and "Continue anyway" when there is something to warn about, and "Cancel" and "Suspend session" otherwise. The "suspend without dialog when idle" shortcut in `actions.rs` (around line 2018) applies only when the preflight finds nothing to warn about.
- **Web viewer:** replace the native `confirm()` with a custom dialog built from the existing Destroy dialog's shell (`viewer.js`, around lines 6533–6583), with the same content and buttons.
- **Move:** `mj move` and the terminal and web Move dialogs run the same preflight. They require the same consent through `--discard-untracked` and `--abort-operation`, or "Continue anyway", since a cross-target Move loses untracked files exactly as suspend does. In-place profile switches skip the warning because they keep the environment.

**The keep directory.** Create `.mj/keep/<session-id>/` when a session launches, using the existing path-containment checks that guard Mjolnir's own paths inside a checkout (as for `.mj/agents/`; see `mj-controller/src/controller/subagents.rs`, around lines 111–140, which refuses symlinked roots). A tracked symlink in an ancestor can therefore never redirect it outside the checkout or into another session's directory. Add `/.mj/keep/` to `.git/info/exclude` with the same helper that excludes `/.mj/agents/` (see `mj-core/src/subagent.rs`, around line 1140), and export `MJ_KEEP_DIR` to the harness. The preflight reports the directory's total size. On suspend of a managed checkout it is listed among the things that will not be kept, under the same consent; Milestone 5 carries it on Move.

**Dry run.** `mj suspend --dry-run` and `mj move --dry-run` call the preflight plus a size estimate and change nothing:
- While an operation is in progress, the dry run reports the operation, the untracked items and the sizes it can measure, and says "Git size estimate unavailable during <operation>". It does not run `git stash create`, which fails on an unmerged index and could not estimate the post-abort state anyway.
- Otherwise, the Git delta size comes from Milestone 1's read-only `estimate_delta`. It sizes the sanitized stash replacements, as capture packs them, so a `git stash -u` full of scratch does not inflate the estimate. It reuses capture's exact exclusion selection: advertised SHAs when origin is reachable, remote-tracking refs otherwise. It runs `git rev-list --objects --disk-usage <selected refs, every HEAD, stash commits and working-state commit> --not <those exclusions>`, so the estimate and the capture cannot disagree. The working-state commit is made by running `git stash create`. That writes unreachable objects but changes no ref, index or file, so the estimate includes the staged and unstaged content capture would save; a pushed branch with large tracked edits does not look empty.
- The session-state size is the sum of the native and canonical files that capture would read.
- Move also reports each top-level entry of `.mj/keep/` with its size, minus any `--skip` paths, and the transfer route Milestone 5's probe would choose.

The output is one line per item with human-readable sizes, followed by the list of things that would be lost.

**Agent-facing text.** The refusal message from suspend and Move reads: "N untracked files will not be kept: <first paths>… `git add` (or commit) anything you want to keep, or pass `--discard-untracked`". Ignored files are never kept, and the message says so.

Put the full rule into four places:
- every session's staged instructions file;
- `mj move --help` and `mj suspend --help`;
- the bundled agent skill `mj-core/assets/skills/mj/SKILL.md`, near its `mj destroy` and Move entries;
- `docs/src/content/docs/cli-reference.md` and `docs/src/content/docs/sessions.md`.

For the staged instructions file, add a new asset, `mj-core/assets/session-state.md`, appended for every harness by `append_staged_instructions` in `mj-worker-staging/src/worker_binary/install.rs`, beside the existing external-attachments guidance. Its text:

    ## Keeping your work across suspend and move

    When this session is suspended or moved, Mjolnir keeps only its Git state:
    branches, tags, stashes, commits, staged and unstaged changes to tracked
    files, and linked worktrees inside the checkout. Untracked files are lost
    unless you `git add` them first. Git-ignored files are never kept.

    Put scratch you will need after a move to another machine (evidence,
    receipts, results) in `$MJ_KEEP_DIR`. It
    travels with Move but is not saved when the session is suspended. Never put
    build outputs or other rebuildable files there.

Also remove the skill's mention of `--delete-branch`.

**Bulk paths:** workspace deletion aggregates the preflights of its sessions into its confirmation. `mj daemon stop --suspend-sessions` gains `--discard-untracked`. `mj acp --on-exit suspend` sends `discard_untracked: false`; on refusal it leaves the session running for a person to handle, as it already does on other refusals.

**Without checkpoint.** Add `without_checkpoint: bool` to the suspend request. In the controller, replace `prepare_discard`/`force_stop` (`lifecycle.rs`, around lines 1171 and 1211) and `suspend_session_without_checkpoint` (around line 945) with one path:

1. Stop the worker, its children and the environment, using `DestroyCleanup::BestEffort` semantics, so it completes with warnings when the target is unreachable (see `destroy_session_with_cleanup`).
2. Keep the session record, its history and the existing checkpoint row, if any.
3. Settle the session as `Stopped`.

Resume uses the existing checkpoint. When there is none, the session is shown as "Suspended without a checkpoint: nothing was saved, so it cannot be resumed". Resume is not offered; Destroy remains. A managed clone's branches went with the removed clone, so there is nothing to resume from. The confirmation states the age of the checkpoint Resume would use, or warns that there is none.

Remove the terminal UI's "Discard changes since checkpoint…" and "Ignore and Suspend" actions. The failure dialog "Suspension could not complete" now offers "Retry suspension" and "Suspend without checkpoint…".

**Destroy.** Destroy keeps its semantics and current wording. Remove `--delete-branch`, `delete_branch` and the "Destroy and delete branch" buttons.

**Protocol.** Advance the daemon protocol constant in `mj-client/src/daemon.rs` (currently 106) because request shapes change. Follow the existing daemon/client version negotiation; there are no aliases for removed fields.

**Tests.**

- Golden tests for the CLI refusal and success output, and for the terminal UI dialog render with untracked files, with an operation, and with a raw checkout. Extend existing golden cases where they exist.
- A tricky-condition test for consent staleness: the preflight shows nothing, a file appears before the capture cut, and suspend without `discard_untracked` refuses.

### Milestone 4: suspend is the only checkpoint producer

**Scope.** Remove every other producer and the machinery that exists only for live copies, and move the two retention rules onto new triggers. At the end, a running session never takes a checkpoint, the journal and transcript projection still stay bounded as described below, and the checkpoint directory is materially smaller.

**Remove the producers.**

- Automatic recovery copies: `RecoveryCoordinator` and the snapshot observations that feed it (`mj-daemon/src/recovery.rs`, `mj-daemon/src/daemon/snapshot.rs`). `mj-daemon/src/daemon/continuation.rs` borrows `AUTO_CHECKPOINT_INTERVAL` as the ceiling of its quota-retry backoff; give it its own named constant with a comment explaining the value.
- The explicit checkpoint action: the daemon action, the HTTP route, the terminal UI and web commands, and the views (`actions.rs` around line 540, `views.rs` around line 361, controller `checkpoint/capture.rs`).
- The checkpoint taken by bundle export. Export of a live session never runs suspend's preparation: no consent check, no abort, no fetch and no stash. It calls Milestone 1's read-only pin helper, which pins refs and HEADs with the same stability check, through the worker without the barrier and without touching the checkpoint row. A scratch file or an in-progress merge therefore never blocks an export. The export keeps its current range (the session's commits beyond its base, as today's export computes it), independent of the snapshot's origin exclusions. A session that pushed everything still exports its work. It builds the user-facing `git bundle` from the pinned refs over that range. Export of a stopped session reads its suspend snapshot plus a clone of origin for the part origin holds.
- Checkpoint-based agent restart: `mj-daemon/src/daemon/restart.rs` around line 449, `lifecycle.rs` around line 183, and the restart intent's checkpoint fields in `mj-store/src/database/session_restart.rs`. Restart stops the harness and starts it again in the same worker from its native state, as worker upgrades already do (`mj-controller/src/controller/worker_restart.rs`). First read why restart used checkpoint-and-restore: the likely reason is model or harness switches (`/model`). Keep that case working through the harness's own resume; record what you find in Surprises & Discoveries.

  An upgrade can find a Restart already accepted and past its checkpoint phase: the old harness is stopped, and its recovery archive is referenced only by the persisted restart intent. Keep a reader and completion path for those old intents, restoring from the referenced archive exactly as today, so the accepted restart finishes. Delete only the path that creates new checkpoint-based restarts. Add an upgrade regression.

**Remove the live-copy machinery.** In `mj-controller/src/controller/checkpoint/`, remove:

- prestage and refresh, early release, two-phase packing, and archive reuse (`ReuseUnchangedArchive`);
- `AdvanceRecoveryFloor`-after-copy, the live revalidation in `validate.rs`, and the routine busy-deferral flavors in `barrier.rs`.

Keep:

- the barrier and close hold that give suspend a consistent final cut (they fixed real races, commits 634af0a36, 15e0e5b35 and 225d25ed0);
- `workspace_lease.rs`, moved out of the checkpoint module to `mj-controller/src/controller/idle_lease.rs`, since it serves uploads and upgrades;
- `layout.rs`, which diff, file reads and push also use;
- atomic, verified installation and crash reconciliation of the single suspend archive.

Leave `checkpoint_operations` rows in the store unwritten rather than dropping the table; no migration is needed.

**Journal retention.**

1. Verify first, and write the findings into Surprises & Discoveries: does any reader need journal events below the daemon's acknowledgement? Look at the command ledger pruning in `mj-worker/src/relay/journal.rs` (around lines 1002–1041), the native-history evidence in `mj-worker/src/relay/native_history.rs` (around lines 27–41), and the empty-session detection described there.
2. If nothing does, make `retained_through` equal `acknowledged_through` and delete the recovery-floor fields' advancement. Keep reading the stored fields, so older snapshots decode.
3. If a specific record is needed, give that record its own retention instead of holding the whole journal.

**Tool-output compaction.** Suspend builds the snapshot's canonical transcript from the daemon's projection. Compacting the projection on a harness signal therefore also removes full tool bodies from later snapshots. Once journal retention follows acknowledgement, the relay no longer holds them either. This plan accepts that, and says so in the Decision Log: for tool calls before a successful harness compaction, which are already out of the agent's context, Mjolnir keeps the summary (tool, target, result and diffstat). The full body survives only where the harness's own native history kept it, and that history travels in the snapshot.

Call `compact_materialized_transcript_through` in two places:

- at suspend, after the archive is verified (it already runs there);
- when the harness reports a successful compaction and also tells Mjolnir what it kept. A completion alone is not enough evidence that earlier turns were evicted: Claude, for example, can preserve a recent segment.
  - **Claude:** extend the `_claude/sdkMessage` subscription (`mj-worker/src/acp/launch.rs`, around line 279, currently filtered to `result`) to include `system/compact_boundary`. Bound the compaction before the start of its `compactMetadata.preservedSegment`.
  - **Codex:** first verify from the pinned bridge and CLI source that a completed `contextCompaction` item replaces all prior tool output with a summary, keeping only recent user messages. If so, the completion card is enough; otherwise Codex compacts only at suspend.

  The boundary is the position of the last completed turn that ended before both the compaction's start and any preserved segment. Exclude cards that failed or reported nothing to compact, cards replayed from history, and child sessions' events. Items whose tool call was not completed by the boundary are left alone.

Add Muse (`_meta.contextCompaction`) only after confirming the pinned 0.24.0 wire shape and what it keeps; record the result. Kimi, Grok, OpenCode, DSH and Antigravity compact only at suspend until their signals are wired in a later change.

Compaction no longer depends on a checkpoint holding a copy, so make sure SessionWiki indexing does not need the full output. It uses the output for counts and error excerpts; compute those when an item is ingested and store them with the item.

**Tests.**

- A tricky-condition test for the compaction boundary: a compaction card that starts while an earlier tool call is still running must not compact that call.
- A cross-version test that a worker snapshot written with a non-zero recovery floor still decodes.
- Remove tests of removed behavior.

### Milestone 5: Move on the shared snapshot with direct transport

**Scope.** A cross-target Move becomes the following sequence:

1. Quiesce the source with the suspend barrier, stopping children as today, and freeze its workspace writers exactly as suspend does (Milestone 2), confirming they have stopped. The freeze covers capture, the archive transfer and the separate `.mj/keep/` transfer, and lasts until the daemon's durable decision to retire the source (destination ready) or roll back (resume the frozen groups). A writer therefore cannot add to `.mj/keep/` after its tar stream finished.
2. Capture with the same code as suspend: Milestone 1 for the repository plus the existing native capture. The result is one archive.
3. Transfer the archive straight to the destination.
4. Restore with the same code as resume.
5. Wait for destination readiness, then retire the source.

The archive is not installed on the daemon as the session's checkpoint; the daemon copy remains whatever the last suspend produced. Delete `mj-worker/src/move_workspace.rs`, the untracked inventory, picker and exclusions in `mj-core/src/move_workspace.rs` and their UI, the daemon-side workspace staging under `data_dir()/moves`, the rsync legs in `transfer.rs`, and Move's separate handoff archive. Quota and login switches on the same environment keep their in-place path unchanged.

**Transport.** Implement `mj-controller/src/controller/move_session/transport.rs` with one function that copies an archive file from a source target to a destination target and verifies its SHA-256 there. Choose the route in this order:

1. **Same host.** When both targets resolve to the same machine (the same local host, or equal SSH host identity), run one host-local command that pipes the source's export into the destination's import through the engines' `exec -i` wrappers. No byte leaves the host.
2. **Direct between hosts.**
   1. Generate an ed25519 keypair on the daemon for this transfer. Copy the private key over the daemon's existing SSH connection into the operation's stage directory on the initiating host (the source for a push, the destination for a pull), with mode 0600. Record its path in the move's durable state before writing it, and delete it on every exit path and in startup reconciliation, like the other grants. That host's `ssh` client uses it with `-i` and `IdentitiesOnly=yes`. The key is useful only for one restricted command from one address.
   2. Give the initiating host a verified host key for its peer. The daemon already trusts both hosts' keys from its own connections, so it writes the peer's key, under a `HostKeyAlias` naming the peer, into a temporary known-hosts file in the same stage directory, recorded and cleaned up like the private key. The `ssh` client runs with `-o UserKnownHostsFile=<file> -o StrictHostKeyChecking=yes -o HostKeyAlias=<alias>`, so it never prompts and never trusts an unknown key.
   3. Append its public key to the destination's `~/.ssh/authorized_keys` with `restrict,from="<source address>",command="<worker import command for this operation>"`.
   4. When the host being connected to (the destination for a push, the source for a pull) is a persistent EC2 Podman host whose security group does not admit the connecting host, add an ingress rule for the connecting host's address as a /32. This follows the shared-grant rules below (use the EC2 helpers in `mj-cli/src/ec2.rs`, around lines 878–918, moved into a shared crate if needed).
   5. From the source host, run `ssh` to the destination with that key and stream the archive.
   6. If the source cannot reach the destination, try the reverse direction. Install the key on the source host, restricted to the worker *export* command for this operation's archive and to the destination's address, give the destination the source's host key as in step 2, and open the source's security group for the destination as in step 4. Then run `ssh` from the destination to the source and pull, piping into the destination's import. A pushed transfer's key is restricted to import, and a pulled transfer's key to export.
   7. Remove the key, the known-hosts file and the security-group rule on every exit path.

   Before installing either resource, persist its ownership in the move's durable state: the host and the key's comment (`mj-move-<operation-id>`), and the EC2 security group, rule and address.
   - **Keys** are per move (each has its own line and comment), so each move removes its own.
   - **A security-group rule can be shared:** AWS rejects a duplicate rule, so two concurrent moves between the same hosts need the same one. A SQLite transaction cannot roll back an AWS call, so the grant has durable states, and every AWS call happens between two committed state changes.
     - **Install:** in one transaction, the move records itself as a user of the rule. If the rule is `removing`, the move does not register: it retries after a short backoff until the removal has finished and the record is gone, then installs afresh. This serializes acquisition against an in-flight revoke. If the rule has no other users, the move marks it `installing` and becomes its installer. If another move is installing it, this move registers as a user but uses the rule only once the record says `installed`, waiting with backoff. If the installer's AWS call reports failure, the record is not dropped on that evidence alone, because the rule may have been created with the response lost. The installer queries AWS for a rule with Mjolnir's description. If one exists, the state becomes `installed`. If AWS confirms none exists, the record is dropped and a waiting move retries as installer. If the query itself fails, the record stays `installing`. A daemon background task retries the query with backoff until AWS answers, as does startup reconciliation. A move waiting on an unresolved grant does not wait indefinitely: it takes the daemon-stream route, reported as such, and releases its registration. The record's state is the only thing any move consults, and every change to it happens in one transaction, so acquisition, installation and removal are serialized. Mjolnir then authorizes the rule with a description naming Mjolnir (`mj-move-grant`), and in a second transaction marks it `installed`.
     - **Pre-existing rules:** if AWS reports a duplicate, the rule already existed. If its description is not Mjolnir's, it belongs to the user: record it as `external`, use it, and never revoke it.
     - **Release:** in one transaction, the move removes itself as a user. If it was the last user of a Mjolnir-created rule, it marks the rule `removing`. It then revokes the rule and, in a second transaction, deletes the record.
     - **Startup reconciliation** queries AWS for each recorded rule. An `installing` rule becomes `installed` if a rule with Mjolnir's description exists, and is otherwise dropped. A `removing` rule is revoked again if present and then dropped. Users whose moves are finished, or that resumed through another route, are released as above.
   - **At startup,** the daemon releases the records of moves that are finished, or that resumed through a different route, by the same rule, deleting key lines by comment.

   This follows the `AGENTS.md` rule that daemon background work must be resumable from durable state.
3. **Daemon stream.** When neither direction connects, run `ssh source export | ssh destination import` from the daemon without writing to its disk. Report the route in the move's progress and in its completion notice.

Record which route each move used in the move's durable state, so a daemon restart resumes the same transfer or restarts it cleanly.

**Keep directory on Move.** After the archive, transfer `.mj/keep/` as a tar stream over the same route. Export and extraction apply the same containment checks: refuse symlinked ancestors, and extract only into the verified `.mj/keep/<session-id>/` with no path escaping it. leaving out the paths named by `--skip`. Skipped paths are deleted when a managed source is retired; in a raw checkout they stay on disk. `--dry-run`, the CLI confirmation and the Move dialogs say this, listing each skipped path with its size, and passing `--skip` is the consent. Unpack it into the destination checkout's `.mj/keep/` before the agent starts. The keep transfer must succeed at both ends: the source `tar` and the destination importer must both exit 0, and the importer must report the byte count the source sent. Only then may destination readiness allow the managed source to be retired. Otherwise the move fails and the source, with its scratch, stays intact. A `tar` that skips an unreadable file still produces a valid archive, so the exit status is what catches it. The keep directory never enters the archive, so it is never stored on the daemon. When the route is the daemon stream it does pass through the daemon's network, and the move's progress shows its size so the user can cancel and rerun with `--skip`.

**Persisted state.** The new `MoveOperation` shape replaces the old one. Older binaries decode `MoveOperation` with `deny_unknown_fields` and treat a decode failure as no intent, so an older daemon opening the store could overlook an active move and the grants it owns. The change is therefore a **breaking** store revision. Advance the migration revision and raise the minimum compatible read and write revision in the same transaction, classified with that reason beside it as `AGENTS.md` requires. Test it with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. At startup, an old-format in-flight move is recovered according to its durable phase.
Ownership is decided by the existing recovery classification, not by the phase name alone. Today's recovery already handles the crash window where the destination's session is durably `Running` while the operation still says `ResumingDestination`, and finishes forward there. Keep that classifier and its finish-forward steps as the reader for old records, and delete the rest of the old executor:
- **The source still owns the session:** roll back to the source with the existing rollback, and send a notice telling the user to run the move again.
- **The destination owns the session** (including that crash window, `StartingQueue`, and later): finish forward. Retire the source if it is not already gone, complete the exactly-once queue admission, and settle the operation.

Add upgrade regressions for each case, setting the persisted destination owner and the Move phase together.

**Target cache.** After a successful suspend, the archive file is kept on the machine that hosts the target, named `<sha256>.hel.zip`, instead of being deleted after the verified download. It goes in host-owned storage that outlives the session's environment: for a bare target, Mjolnir's cache directory on that host; for a container target, the container host's Mjolnir cache directory, outside the container that suspend removes. Copy it out of the container before the container is retired. A replacement worker finds it by host and SHA-256. The copy is published atomically: written to a temporary name in the same directory, synced, then renamed to `<sha256>.hel.zip`. An interrupted copy therefore never sits at the final path. Resume on the same target checks that directory for the checkpoint's SHA-256, verifies the file's hash, and uses it only if the hash matches. Otherwise it deletes the file and uploads from the daemon. Cleanup of this host cache directory deletes files older than 48 hours, even on a host that receives no further snapshots. Record each cached copy in its own row of a new table, `host_snapshot_cache` (session ID, host, SHA-256, path, written_at). Destroy removes that session's cached copies as part of its cleanup, with warnings for any host it cannot reach, so a permanently removed session leaves no archive behind; each copy belongs to exactly one session. The row is written before the copy, deleted only after the file is removed, and independent of the session's checkpoint row, so copies superseded by a later suspend, a purge or a Destroy are still tracked. Adding the table is a compatible migration: older daemons ignore it, and it changes no existing data. The daemon schedules a removal at `written_at` plus 48 hours, rebuilds those timers from the rows at startup, and removes overdue copies immediately. The removal itself extends the existing sweep in `mj-controller/src/controller/checkpoint/leftovers.rs`. Restore also loads the snapshot's pack into the target's clone cache, if one exists, so later clones of that origin find those objects locally; losing them to Git garbage collection is harmless.

**Tests.**

- An end-to-end test moving a session between a local bare target and a local Podman target, which takes the same-host route.
- A transport test that drives more than 64 KB through the pipe route (`AGENTS.md`, Subprocess Rules).
- A tricky-condition test that the temporary key and security-group rule are removed when the transfer fails midway. Use a hand-written fake for the AWS calls.

### Milestone 6: retire legacy paths

**Scope.** Delete code that no remaining path uses. At the end, `cargo clippy --workspace --all-targets -- -D warnings` reports no dead code, and the old readers still pass their compatibility tests.

**Remove.**

- Linked-worktree creation for new sessions, if any path still creates one.
- The branch-disposition variants that only served linked-worktree deletion: `Delete`, `DeleteIfMerged`, and the branch choice in the UIs. Keep `KeepWhenDirty` only if automatic lost-session cleanup still uses it.
- New-capture code for untracked tars, all-refs clone history, nested-repository rejection, operation capture (`git/operation.rs`), and the old linked-worktree lane capture.

**Add `mj purge-untracked [--session <id> | --all] [--dry-run]`.**
- It is a daemon operation and an HTTP route, run as a supervised background task; the CLI waits for it and prints the result.
- For each selected session that is suspended, it takes that session's lifecycle admission, so a concurrent Resume, Destroy or suspend cannot race the rewrite. If the session's archive uses an old schema version and holds untracked-file payloads, it streams the archive into a new file in the same old format. Each `untracked_tar_path` payload, for the repository and for each linked worktree, is replaced with an empty tar, and its manifest descriptor and digest are updated, because the old reader requires those entries to exist. Saved operation state is left alone, since restore ignores it. It then verifies the new file with the existing archive reader.
- It writes the new archive under a new unique name, then updates both `archive_path` and the SHA-256 of the `session_checkpoints` row in one transaction. It deletes the old file only after that transaction commits.
- It rewrites archive payloads only. Move sources retained by earlier releases are whole environments the user may still want to inspect, so they stay under `mj move-sources`, which keeps its explicit operation ID and `--yes`. Milestone 5 stops creating new retained sources; `mj move-sources` stays for the ones that exist.
- It skips sessions that are running or have a new-format archive, saying so.
- Output is one line per session with the bytes reclaimed, then a total.

A crash between installing the new file and committing leaves the old row pointing at the old file, which still exists; the startup reconciliation in `mj-controller/src/controller/checkpoint/archives.rs` removes the unreferenced new file. Add one integration test that purges a checked-in old-format fixture and resumes it.

**Keep.** The reader-side code for old archives, and the live review, diff and push code in `git.rs`. Update the bundled agent command guide, which a test checks against the CLI subcommands, wherever it mentions removed flags.


## Concrete Steps

Run all commands from the repository root, `/home/jonathan/Projects/mjolnir` or a clone of it. Build on the dev profile. Run `cargo test` outside the restricted sandbox, because the suite uses loopback sockets.

After Milestone 1:

    cargo test -p brokk-mj-checkpoint snapshot::
    cargo clippy -p brokk-mj-checkpoint --all-targets -- -D warnings

Expected: every `snapshot::tests::*` test passes. The pushed-branch test asserts that the pack holds no objects.

For each later milestone, build the CLI and use an isolated instance. Never use the default instance:

    cargo build -p brokk-mjolnir        # the mj binary; mj-cli is the directory, brokk-mjolnir the package
    target/debug/mj --instance snapshot-test daemon start
    ... scenario below ...
    target/debug/mj --instance snapshot-test daemon stop

Run the affected crates' tests with filters, for example `cargo test -p brokk-mj-controller lifecycle::` or `cargo test -p brokk-mj-tui dialogs` (Cargo package names carry a `brokk-` prefix), plus `cargo clippy -p <crate> --all-targets -- -D warnings` for each edited crate. When a test fails, rerun only that test after fixing it.


## Validation and Acceptance

Use a scratch repository whose origin is a bare repository on the same machine, so local targets can reach it. Start a session in a managed clone in the `snapshot-test` instance. Inside it:

1. Create branch `feature`, commit, and push it.
2. Make one more, unpushed commit.
3. Run `git stash push -m keepme` on an edit.
4. Stage one edit, leave another unstaged, and create `notes.txt` untracked.

Then:

- `target/debug/mj --instance snapshot-test suspend --session <id>` fails with a message naming `notes.txt` and `--discard-untracked`. The session keeps running.
- With `--discard-untracked` the command succeeds. `target/debug/mj --instance snapshot-test sessions` shows the session as suspended. Its archive's repository pack contains only the unpushed commit and the stash commits; inspect it with `git verify-pack -v <pack>`.
- Resume the session on a second local target, for example a Podman target. In the new clone: `git log -1` shows the unpushed commit on `feature`, `git stash list` shows `keepme`, `git diff --cached` and `git diff` show the staged and unstaged edits, and `notes.txt` is absent.
- Start a merge with a conflict in the session and suspend. The warning says the merge will be aborted. After resume, the merge is gone and the pre-merge state is intact.
- Suspend a raw session, resume it on another target, and check that the user's own unrelated branches did not travel. Resume it back on its raw target and check that the checkout was not modified by the resume.
- Resume a legacy linked-worktree session suspended by the previous release (keep one before upgrading the test instance). It resumes in a managed clone at its saved commit.
- Run `mj --instance snapshot-test suspend --session <id> --without-checkpoint` on a session whose target was stopped by hand. It completes with a warning about unreachable resources, and the session shows the age of the checkpoint Resume will use.
- Move a session between two local targets. The move's progress reports the same-host route, and the daemon's data directory gains no `moves/` payload.
- In the web viewer, choose "Suspend session…" on a session with an untracked file. The custom dialog lists it and offers "Continue anyway".


## Idempotence and Recovery

Capture changes nothing in a raw checkout except a best-effort `git fetch` and the unreachable objects that `git stash create` and the stash replacements write; Git's garbage collection removes those. It writes no refs, so a crash leaves nothing to clean up. In a managed checkout, the operation abort is the one destructive step; the warning names it, and it happens only after the user consents.

Suspend is resumable from durable intent as today. A retried suspend recaptures, and the new archive replaces the old one only after verified installation. Restore always works in a fresh directory and publishes it only on success, so a retry starts clean. Move transfers use per-operation names and verify the hash at the destination before anything on the source is retired. The temporary key and security-group rule are recorded in the move's durable state before they are installed, and removed on every exit path. After a daemon crash, startup reconciliation removes any that are still recorded. Until then, a leftover key is restricted to one import command and the source's address.


## Artifacts and Notes

The Git commands at the heart of capture and restore, shown for one managed checkout with HEAD on `feature`:

    git fetch --prune origin                      # best effort
    git for-each-ref --format='%(refname) %(objectname)'   # pin the cut
    git stash create                              # -> <work> or empty
    git stash list --format=%H%x00%gs             # stash entries to keep
    git ls-remote --tags origin                   # origin tags, when reachable
    printf '%s\n' <feature> <v1> <work> <stash...> --not <origin refs and tags> \
        | git pack-objects --revs snap        # writes snap-<hash>.pack and .idx

    git clone <origin> work && cd work && git fetch --tags origin
    git config --add remote.origin.pushurl <push-url>   # per saved push URL
    cp ../snap-<hash>.pack .git/objects/pack/ && git index-pack .git/objects/pack/snap-<hash>.pack
    git update-ref refs/heads/feature <head-commit>
    git stash store -m keepme <stash-entry-commit>
    git symbolic-ref HEAD refs/heads/feature && git reset --hard <head-commit>
    git stash apply --index <work>
    git rev-list --objects --missing=print <every saved tip> | grep '^?' && exit 1   # full graph present


## Interfaces and Dependencies

No new external dependencies. Git 2.30 or newer is assumed on targets for `--reference-if-able` and `git stash create` behavior; the existing harness images already ship newer Git.

In `mj-checkpoint/src/snapshot.rs`, define:

    pub struct WorkspaceSnapshot {
        pub origin: Option<String>,                // None only for a raw checkout without origin
        pub push_urls: Vec<String>,
        pub head: SnapshotHead,                    // Branch { name, commit } | Detached(commit); every checkout's HEAD carries its commit
        pub refs: BTreeMap<String, String>,        // full ref name -> object id
        pub stash: Vec<SnapshotStashEntry>,        // newest first: commit, message
        pub working_state: Option<String>,         // `git stash create` commit
        pub working_state_skipped: bool,           // raw checkout with an operation in progress
        pub worktrees: Vec<SnapshotWorktree>,      // relative path, head, working_state, initialized submodules
        pub submodules: Vec<SnapshotSubmodule>,    // every initialized submodule at any depth: path, index gitlink, checked-out commit
        pub remote_refs_stale: bool,
        pub pack: Option<PathBuf>,                  // non-thin pack of what origin lacks
    }

    pub enum CheckoutOwnership { Managed, Raw { launch: Option<LaunchRecord> } }

    // The caller loads the raw launch record (versioned JSON in
    // <git common dir>/mj/sessions/<session-id>/launch) and passes it in.
    impl LaunchRecord {
        pub fn read(runner: &dyn GitCommandRunner, checkout: &Path, session_id: &str) -> Result<Option<LaunchRecord>>;
    }

    pub enum UntrackedItem { Path { repository: String, path: PathBuf }, Stash { repository: String, commit: String, message: String } }  // path relative to the repository; stash = its -u/-a payload
    pub enum UntrackedConsent { None, Any, Shown(Vec<UntrackedItem>) }
    pub struct ShownOperation { pub repository: String, pub path: PathBuf, pub kind: GitOperationKind, pub identity: String }  // path "" = primary checkout; identity = digest of HEAD and the operation's state files
    pub enum OperationConsent { None, Any, Shown(Vec<ShownOperation>) }
    pub struct CaptureConsent { pub untracked: UntrackedConsent, pub operations: OperationConsent }

    pub enum ExclusionSource { Origin, Commits(Vec<String>) }   // Commits = a history cutoff's base, what a Provisioned restore base contains
    pub struct RepositoryInput { pub id: String, pub checkout: PathBuf, pub ownership: CheckoutOwnership, pub origin_override: Option<String>, pub exclusions: ExclusionSource, pub pack_path: PathBuf }

    pub fn capture_session_snapshot(
        runner: &dyn GitCommandRunner,
        repositories: &[RepositoryInput],
        consent: CaptureConsent,
    ) -> Result<SessionSnapshot>;   // one WorkspaceSnapshot per repository; SnapshotRefused::{Untracked, OperationInProgress, DirtySubmodule, HiddenTrackedEdits}, keyed by repository and path

    pub enum RestoreBase { Clone { origin: String, seed: Option<PathBuf> }, Provisioned { allow_origin_fetch: bool } }

    pub fn restore_workspace_snapshot(
        runner: &dyn GitCommandRunner,
        snapshot: &WorkspaceSnapshot,
        destination: &Path,
        base: RestoreBase,
    ) -> Result<()>;

    pub struct UntrackedPath { pub path: PathBuf, pub bytes: u64 }

    pub struct WorkspaceInventory {
        pub untracked: Vec<UntrackedPath>,
        pub untracked_stashes: Vec<SnapshotStashEntry>,   // entries whose -u/-a payload will not be kept
        pub operation: Option<GitOperationKind>,   // Merge | Rebase | CherryPick | Revert | Am | Bisect
        pub raw: bool,
        pub worktrees: Vec<(PathBuf, WorkspaceInventory)>,
    }

    pub fn workspace_inventory(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        ownership: &CheckoutOwnership,
    ) -> Result<WorkspaceInventory>;

    pub fn record_launch(runner: &dyn GitCommandRunner, checkout: &Path, session_id: &str) -> Result<()>;

In `mj-checkpoint/src/checkpoint.rs`, add `CheckpointRepositoryCapture::OriginDelta { ownership: CheckoutOwnership }` and raise `CHECKPOINT_EXPORT_PROTOCOL_VERSION` to 3 for specs that use it.

The suspend request gains `discard_untracked` (`"any"` or `{"shown": [items]}`, where each item is `{"repository": ..., "path": ...}` or `{"repository": ..., "stash": <commit>}`; absent for none), `abort_operations` (`"any"` or `{"shown": [{"repository", "path", "kind", "identity"}]}`) and `without_checkpoint: bool` and loses `acknowledge_unpublished_work` and `ignore_nested_repositories`. This applies to the HTTP body (`mj-server/src/server/api/turns.rs`), `SessionLifecycleRequest::Suspend` (`mj-client/src/daemon.rs`), and the CLI flags `--discard-untracked`, `--abort-operation` and `--without-checkpoint`. The destroy request loses `delete_branch`. Both suspend and Move accept `dry_run: bool` (CLI `--dry-run`), and Move accepts `skip: Vec<PathBuf>` (CLI `--skip <path>`, repeatable, relative to `.mj/keep/`). The new route `GET /api/v1/sessions/{session_id}/suspend-preflight` returns:

    {
      "untracked": [{"repository": ..., "path": ..., "bytes": ...}],
      "untracked_stashes": [{"commit": ..., "message": ...}],   // `git stash -u`/`-a` payloads that will not be kept
      "operations": [{"repository": ..., "path": ..., "kind": ..., "identity": ...}],
      "raw": bool,
      "keep_bytes": ...,
      "interrupting": bool,
      "subagents_not_handed_back": [...]
    }

Workers export `MJ_KEEP_DIR` to the harness, set to the checkout's `.mj/keep/<session-id>/`.

In `mj-controller/src/controller/move_session/transport.rs`, define:

    pub enum TransferRoute { SameHost, Direct { pulled: bool }, DaemonStream }

    pub fn transfer_archive(
        source: &TargetLocator,
        source_path: &Path,
        destination: &TargetLocator,
        destination_path: &Path,
        sha256: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<TransferRoute>;


Revision note (2026-11-18): initial plan, written after a read-only audit by seven investigators and three rounds of user decisions recorded in the Decision Log.

Revision note (2026-11-18, later): added the agent-facing data-loss rules (refusal text, help, skill, docs and a staged-instructions asset). Added the `.mj/keep/` directory, which Move carries and suspend never saves, after session 5992a212 reported losing ignored agent scratch. Added `--dry-run` for suspend and Move and `--skip` for Move, and confirmed that automatic quota moves stay on the same target. Added `mj purge-untracked` and recorded the user's approval of the plan author's decisions.

Revision note (2026-11-18, review): addressed the plan review of b9f97fb41:
- Capture detects and aborts an in-progress operation before reading HEAD and refs, and the abort needs its own consent (`--abort-operation`).
- A detached HEAD goes into the delta, and durable `refs/mj/` review refs are kept.
- Restore resets the index and working tree to the saved commit before applying the stash.
- The consent recheck lands together with the capture switch in Milestone 2 and also covers `.mj/keep/`.
- Old raw archives keep converting from the raw checkout at resume, through the new capture.
- Raw checkouts without an origin capture HEAD only.
- Old in-flight moves are recovered by their durable phase.
- `--skip` deletion is explicit.
- The target cache lives in host-owned storage.
- Temporary transfer grants are recorded before installation and reconciled at startup.
- The dry-run estimate includes the working state.

Revision note (2026-11-18, review 2):
- Consent now records exactly what a dialog showed, as `Shown` lists, while CLI flags give blanket `Any` consent. Capture checks it against what it finds, as the single decision point.
- Sessions with an older worker capture with the daemon's own staged worker binary instead of falling back to the old untracked-including format.
- A reverse (pull) transfer gets a key restricted to export.
- Cargo package names in commands corrected to `brokk-mj-*`.

Revision note (2026-11-18, review 3):
- Capture now:
  - checks consent and aborts operations in every checkout before reading any ref;
  - pins a verified cut;
  - packs objects with `git pack-objects` from SHAs instead of bundling through temporary refs;
  - excludes history origin offers through tags;
  - keeps `git stash -u`/`-a` entries without their untracked payload, under consent.
- Restore reapplies push URLs.
- Old in-flight moves are classified by the existing recovery classifier, including the case where the destination is already running.
- A shared EC2 security-group rule's lifetime is decided in one store transaction across moves.

Revision note (2026-11-18, review 4):
- Capture:
  - writes the pack to disk with `pack-objects <basename>`;
  - redacts remote credentials;
  - keeps the dirty-submodule guard;
  - leaves the inventory working during a conflicted operation.
- Restore verifies the full object graph.
- The keep directory is per session (`.mj/keep/<session-id>/`).
- Consent and preflight carry typed `git stash -u` items, and the terminal dialog can show the full list.
- Old raw conversions leave untracked files in the untouched raw checkout, with a notice.
- Export keeps its current range.
- Old restart intents complete.
- The compaction loss is accounted for explicitly.
- EC2 grants have durable install and removal states and recognize pre-existing rules.
- The `MoveOperation` change is a breaking store revision.
- Cache expiry runs on idle hosts.
- Purge updates the archive path with the digest.
- Declined (override recorded): carrying full history for raw checkouts without origin.

Revision note (2026-11-18, review 5):
- Workspace writers are frozen with `SIGSTOP`, and resumed on refusal, around the final capture.
- Restore fetches deleted-branch tips by SHA.
- Existing managed sessions without a network origin use the source repository as `origin_override`.
- The submodule guard covers untracked files.
- Export uses a read-only pin helper and never runs suspend's preparation.
- Harness compaction is used only where the harness reports what it kept: Claude's preserved segment, and Codex after verification.
- Direct transfers get a verified peer host key.
- Purge no longer deletes retained Move sources, and `mj move-sources` stays.
- Raw checkouts are read with `ls-remote` instead of being fetched into.
- EC2 grant acquisition waits on `installing` and `removing`.
- Cache copies are tracked in their own table.

Revision note (2026-11-18, review 6):
- Raw exclusions use only refs origin advertises when it answers.
- `refs/stash` is never a generic ref.
- Every HEAD carries its commit.
- Restore fetches missing prerequisites by SHA, and checks blobs with `--missing=print`.
- The writer freeze lasts until the daemon confirms installation, is released on every other exit, and is recovered after a worker crash.
- Older workers are upgraded before suspend, leaving one capture path.
- The keep-directory transfer must succeed at both ends before the source is retired.
- The purge decision no longer mentions retained Move sources.

Revision note (2026-11-18, review 7):
- The daemon's durable suspend intent alone decides whether frozen writers resume or are retired, which also covers lost acknowledgements and worker restarts.
- The worker confirms writers are stopped before capturing.
- Move holds the same freeze through the keep-directory transfer.
- The first write of the new archive format raises the store compatibility floor.
- The hidden-tracked-edits guard is kept.
- A raw rebase that started from a detached HEAD uses `orig-head`.
- Offline captures report the stale-refs risk to the user.
- The host cache publishes atomically and verifies the hash before use.
- Initialized submodules are restored.

Revision note (2026-11-18, review 8):
- The submodule guard verifies every needed gitlink commit, including staged ones.
- Freeze records carry birth identities, which are verified before any signal.
- The quiescence poll ignores exited and zombie members.
- The exact consent is persisted with the suspend intent.
- Submodules are initialized after the saved index is applied.
- The CLI build command uses the `brokk-mjolnir` package.

Revision note (2026-11-18, review 9):
- A process group stays the session's while any recorded member is alive.
- Restore initializes only the nested submodules that were recorded at capture.
- Raw sessions without a launch record keep every local ref.
- The quiescence wait is cancellable and names stragglers.
- Clone retirement depends on the verified snapshot rather than the source repository.
- Staged gitlinks are recorded for every gitlink entry.
- Operation consent is bound to an operation's identity.
- A raw cherry-pick or revert sequence uses `sequencer/head`.
- The dry run uses capture's exclusions.
- Purge keeps the old format by writing empty payloads.

Revision note (2026-11-18, review 10):
- History-cutoff workspaces restore onto a caller-provisioned clone that never fetches later history.
- The freeze loops `killpg(SIGSTOP)` until membership is stable, then records it.
- Legacy archives without an origin clone from the source repository.
- `.mj/keep/` creation, export and extraction use path containment.
- Destroy removes a session's host cache copies.
- Raw sessions keep every captured worktree's branch.
- Submodule availability is checked against the URL restore will use.
- Operation identity is carried through consent types and the API.
- Reverse transfers open the source's security group.

Revision note (2026-11-18, review 11):
- The freeze record (boot ID, record time and group IDs) is written before the first signal.
- Group ownership follows from POSIX group-ID rules: no live process with that PID started after the record. So recovery is safe at any crash point without per-member identities.

Revision note (2026-11-18, review 12):
- Capture decides consent once across every repository in a session before any abort.
- Submodule URLs are redacted.
- A post-pack re-inventory defines a raw checkout's guarantee.
- Plain directories keep the metadata-only path.
- An ambiguous EC2 install is reconciled by querying AWS.
- Purge reports untracked payloads still held in old stash commits.
- Operation identity includes HEAD.
- Restore takes a `RestoreBase` in the interface.

Revision note (2026-11-18, review 13): freeze ownership is checked against the session's existing process marker as well as the boot ID, because a reused group ID whose new leader has exited would pass a PID-only test.

Revision note (2026-11-18, review 14):
- Consent, preflight and refusals are keyed by repository and path.
- Cutoff workspaces exclude only their cutoff base.
- Restore reapplies branch upstream configuration.
- Operation identity includes state-file inode numbers and modification times.
- Ambiguous EC2 grants are retried live, and waiting moves take the daemon stream.
- The estimate sizes sanitized stash entries.
- Freeze ownership uses the session process marker.
