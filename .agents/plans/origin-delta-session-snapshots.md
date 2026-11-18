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
- [ ] Milestone 6: `mj purge-untracked`; retire legacy-worktree creation, branch-deletion options, `mj move-sources`, the Move file picker and other dead capture code.
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
  Rationale: the snapshot is defined relative to origin.
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

- Decision: each session gets `.mj/keep/` at the top of its checkout, a Mjolnir-owned scratch directory exposed to the agent as `MJ_KEEP_DIR` and excluded from Git through `.git/info/exclude`, as `.mj/agents/` already is.
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
  Rationale: the user asked to tie compaction to harness compaction. A numeric age or count would be a speculative cap, which `AGENTS.md` forbids. The full tool output remains in the agent's own files on the target and in the suspend snapshot.
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
  - For each suspended session whose archive was written in an old format, it rewrites the archive without its untracked-file payloads and saved operation state, keeping everything else byte for byte.
  - It verifies the new archive, replaces the file atomically, and updates the stored SHA-256 in the same transaction.
  - It also deletes Move sources that were retained only to hold excluded untracked files.
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

- **Bundle:** a file produced by `git bundle create` that carries commits and refs. The negative form `--not --remotes=origin` excludes everything reachable from origin's remote-tracking refs, so the bundle carries only what origin lacks.
- **Remote-tracking refs:** the `refs/remotes/origin/*` refs that record what origin had at the last fetch.
- **Stash commit:** the commit that `git stash create` builds from the index and the tracked working tree.


## Plan of Work

### Milestone 1: git-native snapshot capture, restore and inventory

**Scope.** Add a new module, `mj-checkpoint/src/snapshot.rs`, whose functions run Git in a checkout and produce or consume a workspace snapshot. They go through the existing `GitCommandRunner` abstraction in `mj-checkpoint/src/archive/git.rs`, so the same code runs locally and on targets. This milestone adds the functions and their tests only; nothing calls them yet. At its end, a test can capture a checkout, restore it into a fresh clone of the same origin, and observe equal branches, tags, stash list, HEAD, index and tracked working files, with no untracked files.

**Capture**, for one checkout:

1. Best-effort `git fetch --prune origin`. On failure, record that the remote-tracking refs may be stale and continue.
2. Read HEAD: a symbolic branch name, or a detached commit.
3. Select refs:
   - Managed checkout: every ref under `refs/heads`, `refs/tags` and `refs/notes`, plus the stash list.
   - Raw checkout: only the refs whose value differs from the launch record (Milestone 2 writes that record), plus HEAD, plus stash entries absent from the launch record.

   The stash list is read with `git stash list --format=%H%x00%gs` so the messages can be recreated.
4. Check for an in-progress operation: `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `rebase-merge`/`rebase-apply`, `BISECT_LOG`.
   - Managed checkout: run the matching abort (`git merge --abort`, `git rebase --abort`, `git cherry-pick --abort`, `git revert --abort`, `git am --abort`, `git bisect reset`).
   - Raw checkout: skip step 5 and set `working_state_skipped: true`.
5. Run `git stash create` (with no message). Empty output means a clean working state. Otherwise the output is the stash commit, whose first parent is HEAD and whose second parent records the index.
6. For each linked worktree registered with this repository whose directory lies inside this checkout (read from `git worktree list --porcelain`), repeat steps 2, 4 and 5 in that worktree. Record its path relative to the checkout. For a raw checkout, only worktrees absent from the launch record count.
7. Write one bundle with `git bundle create <file> <selected refs> <stash commits> <worktree HEADs and stash commits> --not --remotes=origin`. The non-ref commits are passed as temporary refs under `refs/mj/snapshot/<n>`, created before the bundle and deleted afterwards, because `git bundle` only names refs. When nothing remains after the exclusion, Git refuses to create an empty bundle; record "no bundle" instead.
8. Return a `WorkspaceSnapshot` value containing:
   - the origin URL and push URLs;
   - HEAD;
   - the selected refs with their object IDs;
   - the stash list with messages;
   - the stash commit, if any;
   - the nested worktrees;
   - `working_state_skipped`;
   - whether the remote-tracking refs were stale;
   - the bundle path.

**Restore**, into a target directory:

1. Clone origin. An optional local seed (the clone cache on the target, or a source repository) may be passed for speed with `--reference-if-able`, followed by `git repack -a -d` and removal of the alternates file so the clone is independent. Correctness never depends on the seed.
2. Fetch the bundle, if there is one, into temporary refs.
3. Create every saved ref at its object ID.
4. Rebuild the stash list with `git stash store -m <message> <commit>`, oldest first.
5. Check out HEAD: the branch, or the detached commit.
6. If there is a stash commit, run `git stash apply --index <commit>`.
7. Recreate nested worktrees with `git worktree add` at their recorded relative paths and apply their stash commits the same way.
8. Delete the temporary refs.

When an object the snapshot names is missing, because neither the bundle nor origin has it, fail with a message naming the ref and suggesting that origin may have rewritten history.

**Inventory**, for warnings:

1. List untracked non-ignored paths with `git ls-files --others --exclude-standard -z`, excluding the directories of nested linked worktrees, which are captured.
2. Report any in-progress operation, and whether this checkout is raw.
3. Repeat for each nested worktree.

The suspend warnings in Milestone 3 present this result.

**Tests** are integration tests against real Git; `AGENTS.md` lists Git as a boundary that warrants them. They go in a `#[cfg(test)] mod tests` block in `mj-checkpoint/src/snapshot.rs`. Each test uses a bare repository in a temporary directory as origin. They cover:

- a pushed branch, which yields no bundle while its ref is recorded;
- an unpushed commit and a new tag;
- a three-entry stash list whose messages survive;
- staged and unstaged edits to the same file, restored with the staged part still staged;
- a nested linked worktree with an unpushed detached commit and an unstaged edit;
- a merge with conflicts, aborted in a managed checkout and left alone in a raw one;
- a raw checkout where a user branch that existed at launch is excluded;
- stale remote-tracking refs after origin becomes unreachable (rename the bare directory), still producing a restorable snapshot while origin is back;
- origin force-pushed after capture so a recorded object is gone, with a clear error.

### Milestone 2: suspend and resume on the new snapshot

**Scope.** Suspend captures the repository half of the archive with Milestone 1's `capture_workspace_snapshot` instead of `CloneFrom`, `DeltaFrom`, `MetadataOnly` and the untracked tar. Resume restores it with `restore_workspace_snapshot`. At the end of this milestone, suspending and resuming a managed-clone session, a raw session (in place and onto another target) and a legacy-worktree session all work, and old archives still resume.

**Archive format.** Add a new capture mode, `CheckpointRepositoryCapture::OriginDelta`, and a new archive schema version in `mj-checkpoint/src/archive.rs`. The archive stores the `WorkspaceSnapshot` metadata in `RepositoryMetadata` (new fields beside the existing `saved_refs` and `stash_stack`) and the bundle as a payload.

**Spec version.** Raise `CHECKPOINT_EXPORT_PROTOCOL_VERSION` to 3 for specs that request `OriginDelta`. The controller already learns the worker's version before exporting (see the version repair in `mj-controller/src/controller/checkpoint/staging.rs`, around line 190). A session whose worker predates this change keeps receiving a version-2 spec, and its archive is in the old format. That is acceptable because old formats stay readable, and the next suspend after the worker's automatic idle upgrade writes the new format.

**Raw launch record.** For a raw session, record the checkout's refs (`git for-each-ref --format=%(refname)%00%(objectname)`), its stash list and its linked worktrees when the session launches. Store the record as a file in the repository's common Git directory, at `mj/sessions/<session-id>/launch`, written by the worker at launch. Mjolnir already writes under `refs/mj/` in user repositories for review pins, so this adds no new kind of side effect. Suspend reads the file. Destroy deletes it, and suspend keeps it. When the file is missing (a raw session launched before this change), capture treats every ref as unchanged except HEAD's branch and includes all stash entries. Record that in the Decision Log if it proves wrong in practice.

**Resume paths** (`mj-controller/src/controller/resume.rs`):

- **Raw session on its own raw target:** do not restore the repository half. The checkout is authoritative; this matches today's behavior.
- **Raw session on another target (raw-to-workspace conversion):** provision a managed clone of origin there and restore the snapshot into it. Delete the code that snapshots the host checkout at resume time (`raw_checkout_snapshot`, `conversion_checkpoint`, around lines 2057–2110). When the raw checkout has no origin, the resume dialogs do not offer other targets for that session and state the reason.
- **Workspace to raw:** create a new managed clone on the raw target and restore into it. `WorkspaceToRawConversion` in `worktree.rs` (around line 981) currently creates a linked worktree on `mj/<session>`; change it to create a clone.
- **Old archives** (`CloneFrom`, `DeltaFrom`, `RemoteWorkspace`, `MetadataOnly`, any schema version): keep the existing readers in `git.rs`, with one change for linked-worktree records (`ManagedCheckoutKind::Worktree`). Resume them into a fresh managed clone instead of re-adding a worktree:
  1. Clone origin.
  2. If the archive's base commit is missing from that clone, fetch it once from the source repository on the same target; for these old sessions the base often is not on origin.
  3. Apply the archived delta.
  4. Check out the saved commit on the saved branch name, or as a detached HEAD if that name is already a branch in the new clone with a different value.

  Old untracked payloads are restored. Saved in-progress operation state is dropped, and a resume notice says so.

**Fail visibly.** Remove the missing-source short-circuit in `remove_managed_worktree_checkout` (`worktree.rs`, around line 2777) for clones: a clone is independent of its source, so it is removed regardless. Linked-worktree removal still needs the source repository.

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

**Recheck at the capture cut.** After the barrier is held, the controller runs the inventory again. When untracked paths exist and `discard_untracked` is false, suspend fails with a refusal listing them. The CLI names the flag in that refusal, as it does today for the publication flag (`mj-cli/src/api_commands.rs`, around line 1357). An in-progress operation is reported in the warning and does not need separate consent, because the dialog already says it will be aborted. This recheck is what makes consent safe: files created after the preview are never silently dropped without consent.

**Surfaces.**

- **CLI:** `mj suspend --session <id> [--discard-untracked] [--without-checkpoint] [--json]`.
- **Terminal UI:** the suspend dialog (`Confirmation::SuspendSession` in `mj-tui/src/dialogs.rs`) gains the untracked list (truncated with a count) and the operation warning. Its buttons are "Cancel" and "Continue anyway" when there is something to warn about, and "Cancel" and "Suspend session" otherwise. The "suspend without dialog when idle" shortcut in `actions.rs` (around line 2018) applies only when the preflight finds nothing to warn about.
- **Web viewer:** replace the native `confirm()` with a custom dialog built from the existing Destroy dialog's shell (`viewer.js`, around lines 6533–6583), with the same content and buttons.
- **Move:** `mj move` and the terminal and web Move dialogs run the same preflight. They require the same consent through `--discard-untracked` or "Continue anyway", since a cross-target Move loses untracked files exactly as suspend does. In-place profile switches skip the warning because they keep the environment.

**The keep directory.** Create `.mj/keep/` when a session launches. Add `/.mj/keep/` to `.git/info/exclude` with the same helper that excludes `/.mj/agents/` (see `mj-core/src/subagent.rs`, around line 1140), and export `MJ_KEEP_DIR` to the harness. The preflight reports the directory's total size. On suspend of a managed checkout it is listed among the things that will not be kept, under the same consent; Milestone 5 carries it on Move.

**Dry run.** `mj suspend --dry-run` and `mj move --dry-run` call the preflight plus a size estimate and change nothing:
- The Git delta size comes from `git rev-list --objects --disk-usage <selected refs and stash commits> --not --remotes=origin`.
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
    receipts, results) in `$MJ_KEEP_DIR` (`.mj/keep/` in the checkout). It
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
- The checkpoint taken by bundle export. Export of a live session computes Milestone 1's snapshot bundle directly through the worker, without the barrier and without touching the checkpoint row. Export of a stopped session reads its suspend snapshot.
- Checkpoint-based agent restart: `mj-daemon/src/daemon/restart.rs` around line 449, `lifecycle.rs` around line 183, and the restart intent's checkpoint fields in `mj-store/src/database/session_restart.rs`. Restart stops the harness and starts it again in the same worker from its native state, as worker upgrades already do (`mj-controller/src/controller/worker_restart.rs`). First read why restart used checkpoint-and-restore: the likely reason is model or harness switches (`/model`). Keep that case working through the harness's own resume; record what you find in Surprises & Discoveries.

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

**Tool-output compaction.** Call `compact_materialized_transcript_through` in two places:

- at suspend, after the archive is verified (it already runs there);
- when the daemon's projection ingests a completed tool card that carries `_meta.jetbrains.air.contextCompaction` (Claude, Codex) with a successful completion. The boundary is the position of the last completed turn that ended before that card's start event. Exclude cards that failed or reported nothing to compact, cards replayed from history, and child sessions' events. Items whose tool call was not completed by the boundary are left alone.

Add Muse (`_meta.contextCompaction`) only after confirming the pinned 0.24.0 wire shape; record the result. Kimi, Grok, OpenCode, DSH and Antigravity compact only at suspend until their signals are wired in a later change.

Compaction no longer depends on a checkpoint holding a copy, so make sure SessionWiki indexing does not need the full output. It uses the output for counts and error excerpts; compute those when an item is ingested and store them with the item.

**Tests.**

- A tricky-condition test for the compaction boundary: a compaction card that starts while an earlier tool call is still running must not compact that call.
- A cross-version test that a worker snapshot written with a non-zero recovery floor still decodes.
- Remove tests of removed behavior.

### Milestone 5: Move on the shared snapshot with direct transport

**Scope.** A cross-target Move becomes the following sequence:

1. Quiesce the source with the suspend barrier, stopping children as today.
2. Capture with the same code as suspend: Milestone 1 for the repository plus the existing native capture. The result is one archive.
3. Transfer the archive straight to the destination.
4. Restore with the same code as resume.
5. Wait for destination readiness, then retire the source.

The archive is not installed on the daemon as the session's checkpoint; the daemon copy remains whatever the last suspend produced. Delete `mj-worker/src/move_workspace.rs`, the untracked inventory, picker and exclusions in `mj-core/src/move_workspace.rs` and their UI, the daemon-side workspace staging under `data_dir()/moves`, the rsync legs in `transfer.rs`, and Move's separate handoff archive. Quota and login switches on the same environment keep their in-place path unchanged.

**Transport.** Implement `mj-controller/src/controller/move_session/transport.rs` with one function that copies an archive file from a source target to a destination target and verifies its SHA-256 there. Choose the route in this order:

1. **Same host.** When both targets resolve to the same machine (the same local host, or equal SSH host identity), run one host-local command that pipes the source's export into the destination's import through the engines' `exec -i` wrappers. No byte leaves the host.
2. **Direct between hosts.**
   1. Generate an ed25519 keypair on the daemon.
   2. Append its public key to the destination's `~/.ssh/authorized_keys` with `restrict,from="<source address>",command="<worker import command for this operation>"`.
   3. When the destination is a persistent EC2 Podman host whose security group does not admit the source, add an ingress rule for the source's address as a /32 (use the EC2 helpers in `mj-cli/src/ec2.rs`, around lines 878–918, moved into a shared crate if needed).
   4. From the source host, run `ssh` to the destination with that key and stream the archive.
   5. If the source cannot reach the destination, try the reverse direction: install the key on the source and pull.
   6. Remove the key and the security-group rule on every exit path.
3. **Daemon stream.** When neither direction connects, run `ssh source export | ssh destination import` from the daemon without writing to its disk. Report the route in the move's progress and in its completion notice.

Record which route each move used in the move's durable state, so a daemon restart resumes the same transfer or restarts it cleanly.

**Keep directory on Move.** After the archive, transfer `.mj/keep/` as a tar stream over the same route, leaving out the paths named by `--skip`. Unpack it into the destination checkout's `.mj/keep/` before the agent starts. The keep directory never enters the archive, so it is never stored on the daemon. When the route is the daemon stream it does pass through the daemon's network, and the move's progress shows its size so the user can cancel and rerun with `--skip`.

**Persisted state.** The new `MoveOperation` shape replaces the old one. At startup, an old-format in-flight move is rolled back to its source using the existing rollback, which works because the source is kept until destination readiness, and a notice tells the user to run the move again.

**Target cache.** After a successful suspend, the worker's archive file on the target is kept in a cache directory under the target's worker root, named `<sha256>.hel.zip`, instead of being deleted after the verified download. Resume on the same target checks that directory for the checkpoint's SHA-256 and uses that file when present; otherwise it uploads from the daemon. The startup and periodic cleanup that already sweeps worker roots (`mj-controller/src/controller/checkpoint/leftovers.rs`) deletes cache files older than 48 hours. Restore also fetches the snapshot's bundle into the target's clone cache, if one exists, so later clones of that origin find those objects locally; losing them to Git garbage collection is harmless.

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
- For each selected session that is suspended, it takes that session's lifecycle admission, so a concurrent Resume, Destroy or suspend cannot race the rewrite. If the session's archive uses an old schema version and holds untracked-file payloads or saved operation state, it streams the archive into a new file without those entries and verifies it with the archive reader.
- It then installs the new file atomically and updates the `session_checkpoints` SHA-256 in one transaction, and deletes the old file only after that transaction commits.
- It deletes Move sources retained only for excluded untracked files, through the existing retained-source cleanup. After that, `mj move-sources` has nothing left to manage and is removed in this milestone.
- It skips sessions that are running or have a new-format archive, saying so.
- Output is one line per session with the bytes reclaimed, then a total.

A crash between installing the new file and committing leaves the old row pointing at the old file, which still exists; the startup reconciliation in `mj-controller/src/controller/checkpoint/archives.rs` removes the unreferenced new file. Add one integration test that purges a checked-in old-format fixture and resumes it.

**Keep.** The reader-side code for old archives, and the live review, diff and push code in `git.rs`. Update the bundled agent command guide, which a test checks against the CLI subcommands, wherever it mentions removed flags.


## Concrete Steps

Run all commands from the repository root, `/home/jonathan/Projects/mjolnir` or a clone of it. Build on the dev profile. Run `cargo test` outside the restricted sandbox, because the suite uses loopback sockets.

After Milestone 1:

    cargo test -p mj-checkpoint snapshot::
    cargo clippy -p mj-checkpoint --all-targets -- -D warnings

Expected: every `snapshot::tests::*` test passes. The pushed-branch test asserts that no bundle file exists.

For each later milestone, build the CLI and use an isolated instance. Never use the default instance:

    cargo build -p mj-cli
    target/debug/mj --instance snapshot-test daemon start
    ... scenario below ...
    target/debug/mj --instance snapshot-test daemon stop

Run the affected crates' tests with filters, for example `cargo test -p mj-controller lifecycle::` or `cargo test -p mj-tui dialogs`, plus `cargo clippy -p <crate> --all-targets -- -D warnings` for each edited crate. When a test fails, rerun only that test after fixing it.


## Validation and Acceptance

Use a scratch repository whose origin is a bare repository on the same machine, so local targets can reach it. Start a session in a managed clone in the `snapshot-test` instance. Inside it:

1. Create branch `feature`, commit, and push it.
2. Make one more, unpushed commit.
3. Run `git stash push -m keepme` on an edit.
4. Stage one edit, leave another unstaged, and create `notes.txt` untracked.

Then:

- `target/debug/mj --instance snapshot-test suspend --session <id>` fails with a message naming `notes.txt` and `--discard-untracked`. The session keeps running.
- With `--discard-untracked` the command succeeds. `target/debug/mj --instance snapshot-test sessions` shows the session as suspended. Its archive's repository bundle contains only the unpushed commit and the stash commits; inspect it with `git bundle list-heads`.
- Resume the session on a second local target, for example a Podman target. In the new clone: `git log -1` shows the unpushed commit on `feature`, `git stash list` shows `keepme`, `git diff --cached` and `git diff` show the staged and unstaged edits, and `notes.txt` is absent.
- Start a merge with a conflict in the session and suspend. The warning says the merge will be aborted. After resume, the merge is gone and the pre-merge state is intact.
- Suspend a raw session, resume it on another target, and check that the user's own unrelated branches did not travel. Resume it back on its raw target and check that the checkout was not modified by the resume.
- Resume a legacy linked-worktree session suspended by the previous release (keep one before upgrading the test instance). It resumes in a managed clone at its saved commit.
- Run `mj --instance snapshot-test suspend --session <id> --without-checkpoint` on a session whose target was stopped by hand. It completes with a warning about unreachable resources, and the session shows the age of the checkpoint Resume will use.
- Move a session between two local targets. The move's progress reports the same-host route, and the daemon's data directory gains no `moves/` payload.
- In the web viewer, choose "Suspend session…" on a session with an untracked file. The custom dialog lists it and offers "Continue anyway".


## Idempotence and Recovery

Capture changes nothing in a raw checkout except a best-effort `git fetch` and the temporary `refs/mj/snapshot/*` refs, which are deleted afterwards and removed again by the next capture if a crash left them. In a managed checkout, the operation abort is the one destructive step; the warning names it, and it happens only after the user consents.

Suspend is resumable from durable intent as today. A retried suspend recaptures, and the new archive replaces the old one only after verified installation. Restore always works in a fresh directory and publishes it only on success, so a retry starts clean. Move transfers use per-operation names and verify the hash at the destination before anything on the source is retired. The temporary key and security-group rule are removed on every exit path; a crash leaves the key restricted to one import command and the source's address, and the next move to that host removes stale Mjolnir keys by their comment.


## Artifacts and Notes

The Git commands at the heart of capture and restore, shown for one managed checkout with HEAD on `feature`:

    git fetch --prune origin                      # best effort
    git stash create                              # -> <stash-commit> or empty
    git update-ref refs/mj/snapshot/0 <stash-commit>
    git stash list --format=%H%x00%gs             # stash entries to keep
    git bundle create snap.bundle refs/heads/feature refs/tags/v1 refs/mj/snapshot/0 \
        <stash entry refs> --not --remotes=origin
    git update-ref -d refs/mj/snapshot/0

    git clone <origin> work && cd work
    git fetch ../snap.bundle 'refs/*:refs/mj/restore/*'
    git update-ref refs/heads/feature <head-commit>
    git stash store -m keepme <stash-entry-commit>
    git checkout feature
    git stash apply --index <stash-commit>


## Interfaces and Dependencies

No new external dependencies. Git 2.30 or newer is assumed on targets for `--reference-if-able` and `git stash create` behavior; the existing harness images already ship newer Git.

In `mj-checkpoint/src/snapshot.rs`, define:

    pub struct WorkspaceSnapshot {
        pub origin: String,
        pub push_urls: Vec<String>,
        pub head: SnapshotHead,                    // Branch(String) | Detached(String)
        pub refs: BTreeMap<String, String>,        // full ref name -> object id
        pub stash: Vec<SnapshotStashEntry>,        // newest first: commit, message
        pub working_state: Option<String>,         // `git stash create` commit
        pub working_state_skipped: bool,           // raw checkout with an operation in progress
        pub worktrees: Vec<SnapshotWorktree>,      // relative path, head, working_state
        pub remote_refs_stale: bool,
        pub bundle: Option<PathBuf>,
    }

    pub enum CheckoutOwnership { Managed, Raw { launch: Option<LaunchRecord> } }

    pub fn capture_workspace_snapshot(
        runner: &dyn GitCommandRunner,
        checkout: &Path,
        ownership: &CheckoutOwnership,
        bundle_path: &Path,
    ) -> Result<WorkspaceSnapshot>;

    pub fn restore_workspace_snapshot(
        runner: &dyn GitCommandRunner,
        snapshot: &WorkspaceSnapshot,
        destination: &Path,
        seed: Option<&Path>,
    ) -> Result<()>;

    pub struct WorkspaceInventory {
        pub untracked: Vec<PathBuf>,
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

The suspend request gains `discard_untracked: bool` and `without_checkpoint: bool` and loses `acknowledge_unpublished_work` and `ignore_nested_repositories`. This applies to the HTTP body (`mj-server/src/server/api/turns.rs`), `SessionLifecycleRequest::Suspend` (`mj-client/src/daemon.rs`), and the CLI flags `--discard-untracked` and `--without-checkpoint`. The destroy request loses `delete_branch`. Both suspend and Move accept `dry_run: bool` (CLI `--dry-run`), and Move accepts `skip: Vec<PathBuf>` (CLI `--skip <path>`, repeatable, relative to `.mj/keep/`). The new route `GET /api/v1/sessions/{session_id}/suspend-preflight` returns:

    {
      "untracked": [{"path": ..., "bytes": ...}],
      "operations": [...],
      "raw": bool,
      "keep_bytes": ...,
      "interrupting": bool,
      "subagents_not_handed_back": [...]
    }

Workers export `MJ_KEEP_DIR` to the harness, set to the checkout's `.mj/keep/`.

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
