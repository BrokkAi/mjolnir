# Sync project memory at turn boundaries through one atomic store per side


This ExecPlan is a living document maintained according to `.agents/PLANS.md`. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.


## Purpose / Big Picture


Project memory is the set of Markdown files (an index, `MEMORY.md`, plus topic files) that agents write so later sessions on the same project remember preferences and constraints. Each session edits a private copy, the replica, under its profile home at `projects/hel-<key16>-<session-id>/memory`; harnesses such as Claude Code read that path directly. The project's shared copy, canonical, lives on the daemon host under `<data dir>/projects/<project key>/memory`.

On 2026-11-26 a day of memory cleanup was silently lost. Two defects caused it. Every worker launch, including an in-place quota Move, staged canonical over the replica and its baseline (`stage_memory_replica` in `mj-worker-staging/src/worker_binary/project_memory.rs`, applied by `copy_overlay` in `mj-worker/src/worker_runtime/reload_commit.rs`). And memory reached canonical only inside a checkpoint (`mj-controller/src/controller/checkpoint/latched.rs`), which for a long-lived session happens rarely.

After this change, a session's memory edits reach canonical within one turn, running sessions receive other sessions' edits by their next turn start, and no launch, reload, restart or restore can overwrite a session's replica. To see it: in an isolated instance, edit and delete memory in one session and watch canonical change after the turn; start a turn in a second session and watch its replica receive the change; force an in-place profile Move and watch the first session's replica stay untouched.

A previous attempt (local commits 0dfdf9884..e15ed5af4, never pushed; plan `project-memory-saved-at-turn-end.md` in those commits) reached the same behaviour with about 3,400 lines of consistency machinery across three stores, and six review rounds kept finding new interleavings. This plan starts over from master with a small set of simplifying assumptions, proposed by the advisor session and adopted for that reason.


## Simplifying assumptions


These assumptions are the design. Every mechanism below depends on one of them, and anything they do not need is left out.

First, whole trees move. Memory is small (the startup index is capped at 25 KiB, `STARTUP_INDEX_BYTES` in `mj-core/src/project_memory.rs`, and whole trees are hundreds of kilobytes), so every transfer and every write is a whole tree, and no per-file synchronization state exists anywhere.

Second, the replica changes only inside the agent's tool calls. The worker writes the replica only if it still equals the snapshot the delivered tree was merged from. A foreground sub-agent runs inside its parent's tool call, so it is covered. Background sub-agents, background shells, and a write landing in the milliseconds of an apply are out of scope; the equality check turns almost all of them into a skipped delivery rather than an overwritten edit.

Third, a session synchronizes only at its own turn start (alongside prompt submission, not before it), its own turn end, and attach. That bounds staleness to one turn in both directions. There is no per-project watch and no published dirty flag; asking the worker is cheap because it answers "in sync" without sending trees.

Fourth, exactly one fact crosses between the daemon's store and the worker's store: "canonical absorbed snapshot X". Canonical owns it.


## Ownership rule


The replica has two writers: the agent, and the worker's `Deliver` handler. Staging, in-place reload, worker restart and checkpoint restore create the replica only when it is absent. Canonical has one writer: the daemon's commit, a conditional atomic file replacement. The worker record has one writer: the worker.


## Progress


- [x] (2026-11-27) Slice 1: seed only an absent replica, keep memory out of staged overlays, and capture its baseline in the checkpoint; seed materialization uses Slice 2's `render_memory_tree`.
- [x] (2026-11-27 01:48Z) Slice 2: canonical uses an atomically replaced file and a read-only rendered directory; checkpoint sync and project-store merges commit through it.
- [ ] Slice 3: `Snapshot` and `Deliver`, and the turn-start, turn-end and attach triggers.
- [ ] Isolated-instance acceptance on a release build (in-place Move needs release: dev builds overflow a Tokio stack during harness preparation, a pre-existing issue).
- [ ] Review, commit, push.


## Surprises & Discoveries


- From the previous attempt: checkpoints already capture the replica (`mj-checkpoint/src/checkpoint/capture.rs`) and ordinary resume runs profile staging and archive restore in concurrent lanes (`mj-controller/src/controller/resume.rs`), so today a restored session's replica depends on which lane finishes last. Slice 1 removes the dependency because staging no longer writes the replica.
- From the previous attempt: `RelayRequest` and `ProjectMemoryLaunchConfig` use `deny_unknown_fields`, and some restart paths keep an older worker binary while rewriting its `launch.json`. New launch fields therefore break older workers; derive new paths from existing fields instead.
- Canonical merges, conflict resolution, and legacy migration drop empty and whitespace-only documents so older workers can install synchronized trees. The shared tree-to-directory renderer also materializes the staging seed.
- Native checkpoint archives contain files, not directories. Capture adds a hidden marker for an empty replica or baseline so restore preserves the distinction between an absent tree and an empty one.
- From the previous attempt: in-place Moves cannot be exercised end to end on dev builds because of a pre-existing Tokio stack overflow in harness preparation (`mj-harness-install/src/harness_install.rs` Node probe); use release builds for that acceptance step.


- Legacy trees can contain empty documents and hidden entries. Empty and whitespace-only documents are excluded from merged and migrated canonical trees; `render_memory_tree(root, tree, read_only)` still accepts any tree. Migration leaves hidden entries in the backup without importing them; existing redirect metadata is beside the rendered directory. The shared renderer also materializes the staging seed.
- The shared renderer uses confined directory and file handles for cleanup and permission changes; a planted symlink cannot redirect its writes or chmod operations.
- The old checkpoint tests used the temporary directory itself as the memory root. The canonical file belongs beside the memory root, so the tests now give each case a `memory` child to keep files isolated.


## Decision Log


- Decision: start over from master instead of finishing the previous attempt. Rationale: its fragility came from three stores that no interruption-safe ordering could keep consistent; the assumptions above reduce that to one atomic store per side and one crossing fact. Date: 2026-11-26.
- Decision: no git on workers or the daemon. Rationale: a checkout into a live directory is not crash-atomic with HEAD and the index, so delivery would still need an intent record, and git adds its version, user configuration, hooks, stale lock files and `safe.directory` as failure sources on every target. One renamed file gives the daemon its atomic commit. Date: 2026-11-26.
- Decision: keep the existing merge (`mj_core::project_memory::merge_trees`) and conflict resolver, provided the sync always gets a tree back when the utility model fails (fall back to the replica-wins tree the merge already computes). Rationale: turn-boundary sync shrinks divergence from days to one turn; count conflicts before deciding whether the model is still needed. Date: 2026-11-26.


- Decision: no command for editing canonical directly; the rendered directory is read-only and editing any session's replica is the supported path. Rationale: the user's choice; a replica edit reaches canonical within one turn. Date: 2026-11-26.


- Decision: retain `ProjectMemorySnapshot::version()` as the file version. Compare the committing session's `absorbed` entry separately under the same lock, allowing other sessions to acknowledge unchanged trees without false conflicts; the commit retains all their entries. Date: 2026-11-27.
- Decision: preserve the existing sibling redirect file. Project merges lock source and destination in path order, recheck redirects, commit destination, then atomically publish the redirect; missing source acknowledgements are copied to destination, where an existing destination acknowledgement takes precedence. Date: 2026-11-27.


- Decision: migrate each project on its first canonical access and render only after a commit; do not scan projects at daemon startup. Rationale: canonical reads already perform migration, and the rendered directory is never read back, so a stale view is harmless until the next commit. Date: 2026-11-26.


## Outcomes & Retrospective


Slice 1 preserves existing replica edits and deletions across in-place reload and carries the baseline through checkpoint restore. Slice 2 provides `read_canonical` for staging and `read_canonical_state`/`commit_canonical` for the turn protocol. Slice 3, acceptance and review remain.


## Context and Orientation


Staging: `mj-worker-launch/src/worker_launch.rs::stage_worker_files_for` prepares a staged profile for every launch kind, including the in-place reload candidate (`WorkerFilePreparation::StageCandidate`, used by `mj-controller/src/controller/move_session.rs`). It calls `stage_memory_replica`, which copies canonical into the staged replica and baseline and lists both as shared paths, so every launch and every reload overlays canonical onto the live replica. `staged_memory_index` feeds the memory index into staged instructions for Kimi, OpenCode and Grok (`mj_core::session_instructions::delivered_by_staging`).

Worker: `ProjectMemoryLaunchConfig` (`mj-core/src/worker_launch.rs`) gives the worker the replica `root` and `baseline_root`. The worker serves `ProjectMemorySnapshot`, `InstallProjectMemorySnapshot`, `ReplaceProjectMemoryTree` and paged `ProjectMemoryTransfer` (`mj-worker/src/worker_runtime/unix/requests.rs`). The harness turn boundary is `DurableRelay::settle_harness_turn` and prompt completion is `record_command_completed` (`mj-worker/src/relay.rs`, `mj-worker/src/relay/commands.rs`).

Daemon: `StandaloneSession::sync_project_memory` (`mj-session-runtime/src/session_manager/standalone.rs`) runs the merge and install inside a checkpoint barrier only. `merge_and_swap_canonical` and the resolver are in `mj-session-runtime/src/project_memory_merge.rs`. `ProjectMemoryStore` and `replace_tree` (`mj-core/src/project_memory.rs`) write canonical file by file under an in-process lock. Project-store merges (`merge_canonical_stores`) redirect one project's canonical into another's.

Protocol: `RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs`; per-version notes sit beside the constants.


## Plan of Work


Slice 1: the replica survives every launch. Staging stops writing the replica and baseline into any staged profile or reload candidate. It stages a canonical seed instead, at a location derived from existing fields (`baseline_root.with_file_name(".hel-memory-seed")`, computed by one helper in `mj-core/src/worker_launch.rs`), and installs it directly, never through the overlay; no launch field is added. A worker function `ensure_project_memory_replica`, run at worker startup and when applying an in-place reload, copies the seed into replica and record only when the replica is absent, and always removes the seed. For this slice, the existing baseline directory is the record; if the replica exists but the baseline is absent, the worker copies the replica into the baseline. Slice 3 replaces this directory-based record with the record file described below. Checkpoint capture archives the baseline beside the replica, so a restore brings both back. `staged_memory_index` reads the seed. The in-place reload test pins the original defect: an unsynced edit and an unsynced deletion survive a reload, marked `// Hard-won:` with the fix commit. This slice ends the destruction even before any sync change.

Slice 2: canonical becomes one file. Per project, canonical is `<data dir>/projects/<key>/memory.json` = `{ version, tree, absorbed: { session id: snapshot id } }`, written by temp file, fsync and rename under a short cross-process file lock, conditional on `version` and on the committing session's `absorbed` entry being unchanged since it was read. A commit replaces the tree and that session's entry together. After every commit, the daemon renders the tree into the existing `memory/` directory, read-only, and never reads that directory back. Each project's first canonical access migrates an existing directory into the file; daemon startup does not scan projects or repair rendered views. A stale rendered view is harmless until the next commit. Staging's seed reads the file. The existing checkpoint sync commits through the file with today's protocol for workers that predate slice 3. `merge_canonical_stores` becomes two file commits, destination then redirect. Editing any session's replica is the supported way to change memory by hand; the user declined a dedicated editing command.

Slice 3: `Snapshot` and `Deliver`. The worker keeps one record file beside the replica, `.hel-memory-record.json` in the baseline directory, holding `{ base: tree, base_version: optional canonical version, handout: optional { id, tree }, incoming: optional tree }`, written by temp file and rename. `handout` is the snapshot the worker last gave the daemon, kept until the daemon proves canonical absorbed it; `incoming` exists only while a delivery is being written, so worker startup can finish it.

`Snapshot { absorbed, head_version }`: if `handout.id == absorbed`, set `base = handout.tree`, set `base_version = none`, and clear `handout`. If a handout is still unresolved and differs from base, return that same handout again. If the replica equals base and `base_version` equals `head_version`, answer `InSync`. Otherwise store a new handout `{ fresh id, replica }` and return base and handout.

The daemon then computes `merged = merge(base, canonical tree, handout.tree)`. If the handout differs from base it commits `{ tree: merged, absorbed[session] = handout.id }`: on a version mismatch it re-reads and merges again; on an `absorbed` mismatch it starts over with a new `Snapshot`. Then it sends `Deliver { id, tree: merged, version }`.

`Deliver`: answer `Stale` unless `id` is the pending handout. If the replica no longer equals the handout, set `base = handout.tree` and `base_version = none`, clear the handout, write nothing, and answer `Dirty`; the daemon runs another round, which picks up the later edits and the foreign changes together. Otherwise write `incoming`, replace the replica, set `base = merged` and `base_version = version`, and clear `handout` and `incoming`.

Triggers: the session actor runs one sync at a time, sharing its slot with checkpoint syncs, at turn start (scheduled alongside prompt submission), at turn end (prompt completion and harness-turn settlement), and on attach. If the worker can report an open tool call from a single owner of that fact, `Snapshot` and `Deliver` answer `Busy` while one is open; otherwise the equality check decides alone. Workers below the new protocol keep the checkpoint-only path.

Why interruptions are safe: base advances only to a handout canonical has absorbed, or to a tree just written over an unedited replica, so no edit is dropped. A handout carrying edits is never discarded: any daemon rolls it forward, and the `absorbed` condition lets exactly one commit win. A lost `Deliver`, a daemon handoff or crash after the rename is resolved by the next `Snapshot` seeing `absorbed == handout.id`. A crash before the rename re-merges the same handout. A worker crash mid-delivery is finished at startup from `incoming`. No step holds handoff admission: every step either completes atomically or is redone from durable state.


## Validation and Acceptance


Tests follow AGENTS.md "Which tests to add": tricky conditions and integration only. Slice 1: the in-place reload test above, and checkpoint capture and restore round-tripping the record. Slice 2: concurrent conditional commits from two sessions serialize and neither is lost; a commit interrupted before the rename leaves the previous file intact; migration of an existing directory. Slice 3: an agent edit between `Snapshot` and `Deliver` yields `Dirty` and the next round delivers both the edit and the foreign change; a lost `Deliver` is promoted by the next `Snapshot`; a crash before the canonical rename re-merges the same handout without duplication; a worker crash mid-delivery is finished at startup; a foreign deletion of a note this session edited and already pushed is not resurrected.

Acceptance in an isolated instance (`--instance memsync`, with MJ_CONFIG_DIR and MJ_DATA_DIR unset), release build for the Move step: turn-end push; turn-start delivery to a second session; in-place Move preserving unsynced edits; daemon stop and restart between the canonical commit and `Deliver` ending converged; the deleted-note race.


## Limitations


Legacy SSH host-bare sessions whose whole profile home is a symlink have a pre-existing gap: `stage_worker_files_for` calls `unlink_host_bare_profile_home` on `RefreshProfileOnly`, leaving the old replica and baseline under the symlink target and seeding the new home from canonical. No migration is included in Slice 1.

A background writer landing within milliseconds of a delivery's replica write can be overwritten. Restoring a checkpoint captured while a handout was pending, after the original timeline moved on, re-merges that handout against newer canonical and can bring back a note deleted since; checkpoints sync before capture, so this needs a failed pre-capture sync first. Older workers keep checkpoint-only synchronization until their next worker restart.


## Interfaces and Dependencies


Relay: `ProjectMemorySnapshot` gains `{ absorbed, head_version }` and its reply gains `InSync`; new paged `ProjectMemoryDeliver { id, tree, version }` with replies `Delivered`, `Dirty`, `Stale` (and `Busy` if adopted); both gated on a new protocol version. No new launch fields. No database migration.


Revision note (2026-11-26): initial plan for the restart, from the advisor session's design (session 91233b0e) and the user's direction to simplify through assumptions.


Revision note (2026-11-27): record Slice 1 implementation, empty-directory checkpoint handling, and the pre-existing legacy SSH host-bare symlink gap.
