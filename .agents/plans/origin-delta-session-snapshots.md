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
- [x] Milestone 1: git-native snapshot capture, restore and inventory in `mj-checkpoint`, with integration tests against real Git. Landed with the managed-clone slice at 6afca5eea (2026-11-20).
  - [x] (2026-11-19) Implemented in worktree `snapshot-m1` through commit 7b2e5aa5a: capture, restore and inventory with authoritative blockers, the structural teardown guard, shallow refusal, branch-owner normalization with displaced refs, abort prediction including autostash, and origin queries from the source repository. 112 `snapshot::` integration tests and all-target clippy pass.
  - [x] (2026-11-19) Simplified on the advisor's recommendation (commit d0b259f7f, net -12,707 lines). 36 `snapshot::` tests and 56 generated round-trip seeds pass.
  - [x] (2026-11-20) Milestone 2 vertical slice for managed clones landed on master at 6afca5eea, from worktree `m2-takeover`: the original phase A and B commits (f0bd08368, 980cc504d), then five review rounds triaged by the plan author. The earlier 20-round child branch is kept as `refs/mj/archive/m2-slice-round20` for reference only. Breaking store revision 144 (master took 142 for checkpoint mail and 143 for permission forms) and archive schema 14 (13 is mailbox; an OriginDelta archive may also carry mail). Export protocol 3. Decisions made while landing it are in the Decision Log entry dated 2026-11-20 "landing the managed-clone slice".
  - [x] Next slice: the canonical session carries native-use evidence and pending mailbox events (#1616), under its own archive schema and breaking store revision.
    - [x] (2026-11-20) Pending mailbox events landed ahead of the managed-clone slice, as archive schema 13, relay protocol 75 and breaking store revision 142; see `.agents/plans/1616-pending-mail-in-checkpoints.md`.
    - [x] (2026-11-21) Native-use evidence slice: worker-owned `NativeSessionUseEvidence` fetched at the checkpoint cut (relay protocol 80), stored on `CanonicalSessionSnapshot` (archive schema 15, included in `content_matches`, breaking floor-only store revision 146), carried on `RestoredRelaySeed` while preserving `LoadBeforeReplace` before `SessionOpened`, and falling back to `current_native_session_received_prompt` only for legacy schema `<= 14` archives. Landed on master at `89ae5671e`.
- [ ] Milestone 2: suspend and resume use the new snapshot; raw launch-ref record; resume conversions; old-archive and legacy-worktree restore onto a fresh managed clone.
  - [x] (2026-11-21) Raw sessions slice: worker writes `mj/sessions/<session-id>/launch` once at first launch (`SESSION_RAW_LAUNCH_RECORD_ENV`, `!restarting`, atomic no-clobber), raw captures without a launch record keep every local ref and stash, `CheckpointRepositoryCapture::OriginDeltaRaw` (export protocol 3, gated on relay protocol 78 so older workers fall back to `MetadataOnly`) captures raw Git checkouts under the relay barrier without managed-clone quiescence, archive validation allows originless in-place-only raw snapshots, in-place raw resume leaves the checkout untouched, cross-target resume refuses originless raw archives with a dedicated message, and Destroy removes the launch record on local and SSH bare targets while Suspend keeps it. Landed on master at `7305b8792`.
  - [x] (2026-11-21) History-cutoff sessions slice: `WorkspaceSnapshotMetadata.history_cutoff` records `origin_url: None` and empty `branch_tracking` even if an `origin` remote was added locally, `ExclusionSource::Commits` packs against the cutoff commits without querying/fetching `origin` (and refuses initialized submodules before teardown so locally created submodule commits cannot be lost or require network fetches), restore fails visibly on missing prerequisites without fetching later history while restoring local refs, stashes, working state, and `index_gitlinks`, `CheckpointRepositoryCapture::OriginDeltaCutoff` (relay protocol 79, older workers keep `RemoteWorkspace` fallback), `restore_repositories_from_archive` streams `GitOriginDeltaPack` directly from the archive file, and resume provisions cutoff workspaces via `backend_session_bundle` + `apply_session_history_cutoff` like new sessions. Landed on master at `ac8bb6b1e` (container-side `OriginDeltaSuspend` quiescence tracked as a separate follow-up).
  - [x] (2026-11-21) Origin-seeded provisioning & cross-target OriginDelta resume slice: `create_managed_clone_with_snapshot` seeds directly from `snapshot.metadata.origin_url` when `source_repository` is absent, `retire_managed_worktree` / `remove_managed_worktree_checkout` retire managed clones without requiring `source_repository`, `restore_checkpoint_with_notices` resolves `primary_repository_root` and surfaces `restore_workspace_snapshot` notices back to `resume_notices`, `bundle_from_manifest_for_session` and `plan_raw_to_workspace_from_origin_delta` allow resuming origin-backed `OriginDelta` archives onto container/remote targets without touching the original checkout or writing a conversion archive, and `WorkspaceToRaw` on `OriginDelta` creates a managed clone rather than reusing a linked worktree. Landed on master at `3cd573357`.
  - [x] (2026-11-21) Legacy linked worktrees slice: shared `checkpoint_archive_uses_origin_delta` predicate in `mj-checkpoint`; archival eligibility gate in `sessions_ready_to_archive_from_state` fails closed on unreadable existing archives and denies the age-only exemption to `OriginDelta` linked-worktree checkpoints; scoped linked-worktree removal in `remove_managed_worktree_checkout` replaces repository-wide `git worktree prune` with single-admin-dir cleanup (preserving locked registrations and siblings on both Local and SSH); `OriginDelta` suspend enabled for local linked worktrees with `origin_override = source_repository` fallback (skipping `selected_refs` and `refs/heads/mj/*` from shared-source `ls-remote` exclusions so session commits travel in the pack independently); in-place resume of an `OriginDelta` linked-worktree archive upgrades the session to a fresh `ManagedCheckoutKind::Clone`; `plan_workspace_to_raw` skips source origin refresh and base-commit resolution when `origin_delta` is true. Landed on master at `84c98871c`.
  - [x] (2026-11-21) Old-archive conversion slice: legacy restores drop saved Git operation state with a visible notice while preserving `conflicted_worktree_tar` as unstaged edits (propagated through all restore callers into `resume_notices`), retired local legacy linked-worktree archives resume onto fresh `ManagedCheckoutKind::Clone` checkouts under `.mj/clones/<id>` (persisted before acceptance, fetching any missing local-only base once from `source_repository`, detaching HEAD rather than overwriting colliding branches in `saved_refs`, repairing nested linked worktrees after staging rename with cleanup on failure, and refusing cross-target resumes when an older checkout still exists on disk or its base is missing from origin), and legacy `Attached` raw conversions capture an ephemeral origin-delta snapshot directly from the untouched raw checkout against its selected remote (`ExclusionSource::Remote`, leaving untracked files behind with a notice, restoring `record.checkpoint` on every exit path, and structurally forbidding raw capture when retiring a managed checkout). Landed on master at `4a324804e`.
  - [x] (2026-11-21) Worker-version upgrade & legacy lifecycle intent slice: `OriginDeltaSuspend::applies` no longer falls back to version-2 capture for older workers lacking `suspend-worker.json`; `upgrade_worker_for_origin_delta_suspend` in `worker_restart.rs` upgrades older live workers in place before quiescence/capture (cancelling an active turn first when interrupting, rolling back pre-stop intents on failure, marking post-stop failures with `WorkerRestartLeftNoWorker`, recovering interrupted mid-swap intents on retry, and dropping swap admission right after durable spawn); plus upgrade regressions covering legacy `Closing`/`Destroying` intents and stopped sub-agents. Landed on master at `fc5f08ec4`.
  - [ ] Remaining in Milestone 2: container/SSH target quiescence (`OriginDeltaSuspend` generalization beyond local bare checkouts).
  - [ ] Declined review findings to revisit: one recovery path for an interrupted resume after publication and for a missing checkout (remove the receipt-owned checkout, restore again); nested worktree locks; an accepted suspend coinciding with a harness startup failure (worker and controller); a notice when the pack relied on stale cached origin refs; refreshing tracking refs after an origin change (SystemGit's GIT_SSH_COMMAND overrides core.sshCommand, #1466); `update = none` submodules; a stopped clone's suspend retry getting a live-clone 409.
- [ ] Milestone 3: suspend warnings and consent on every surface; `suspend --without-checkpoint`; consolidation of the lifecycle verbs.
- [ ] Milestone 4: suspend is the only checkpoint producer; periodic copies, the explicit checkpoint action, export side effects and checkpoint-based agent restart are removed; new journal-retention and tool-output-compaction triggers.
  - [x] (2026-11-19) Journal retention, startup-receipt pins and their legacy migration, and adoption after pruning implemented in worktree `suspend-only-m4a` through commit f30ad6477. Adoption waits for quiescence with no interrupt, installs a current operational cut with a history notice, and uses ordinary reattach for genesis journals. Relay protocol 74, daemon protocol 108, no migration. Worker and controller adoption tests and clippy on 8 crates pass. The container end-to-end test is ignored pending #1603.
  - [x] (2026-11-20) Reviewed clean at 92a796d8d. It removes the automatic checkpoint coordinator and the explicit checkpoint action, makes retention follow the ACK with receipt and native-use pins, and implements adoption as designed.
  - [x] (2026-11-20) Landed on master at dc508d58f after two upstream merges, each reviewed clean. Daemon protocol 111 (110 for the checkpoint removal), relay protocol 74 (73 for retention), relay state 28.
  - [ ] Tool-output compaction at suspend and on the harness compaction signal is still deferred.
  - [ ] Follow-up from a plan review (2026-11-20): check whether `goal::recover` and `goal::resolve` make the resume reservation visible to the relay before the harness request is sent. If `NativeGoalResumeSent` is only enqueued, a quiescent adoption arriving over another connection could take its cut in that window. Fix it, if real, by making the relay the single owner that records the reservation before dispatch.
- [ ] Milestone 5: Move on the shared snapshot, with direct transport and the target-side snapshot cache.
  - [x] (2026-11-21) Slice 5C (Direct Move transport, framed archive+`.mj/keep/` stream, flocked `authorized_keys`, and durable EC2 grant state machine): `mj-checkpoint/src/move_stream.rs` (length-prefixed archive + symlink-safe `.mj/keep/` tar with `--skip`, SHA-256 + byte-count verification, rollback on archive persist failure), `mj-worker/src/authorized_keys.rs` (`flock`-locked atomic install/remove/reconcile matching exact managed key shape), `mj-worker-bin` `export-move-stream`/`receive-move-stream`/`manage-move-key` subcommands, `mj-controller/src/controller/move_session/transport.rs` (`SameHost`, `Direct` push/pull with `-o ControlMaster=no -o ControlPath=none` and `StrictHostKeyChecking=yes` + `HostKeyAlias`, and `DaemonStream` via `open_ssh_session_without_marker` + `ProcessStreamPipe`, plus `SshNitro` microVM engine support), and `transport/ec2_grant.rs` (durable `Installing`/`Installed`/`Removing`/`External` shared SG grant manager with per-key revoke serialization and live vs startup reconciliation). Wiring into `move_session.rs` follows in Slice 5D.
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

- Observation: `git clone` checks that a relative local path exists *before* applying `insteadOf`, so cloning a rewritten relative URL fails, while `git ls-remote` and `git fetch` with the same configuration succeed.
  Evidence: real-Git reproduction during Milestone 1: `git -c url.file://<B>.insteadOf=../alias.git clone ../alias.git <dest>` fails with "repository ../alias.git does not exist", even with `--no-local`. Restore therefore initializes and fetches instead of cloning.

- Observation: rewrite pins keyed on real URLs collide between fetch and push. Git applies `insteadOf` to both `url` and explicit `pushurl`, and picks the first rule of equal length.
  Evidence: real-Git reproduction during Milestone 1, with fetch A→B by `insteadOf` and push A→A by `pushInsteadOf`. Restoring with the pin A→B plus `pushurl` A and its identity pin gives fetch=B and push=B. Push destinations are therefore restored as synthetic aliases.

- Observation: Git applies `insteadOf` once per invocation, so passing an already rewritten URL back to Git rewrites it again.
  Evidence: real-Git reproduction during Milestone 1, with rules A→B and B→(missing) C. `git ls-remote A` succeeds; `git remote get-url` prints B; `git ls-remote B` fails at C. Capture therefore queries by remote name, and restore pins synthetic aliases to effective destinations.

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

- Decision: an unreachable origin never blocks suspend. Capture queries origin on a best-effort basis (originally `git fetch --prune origin`; now `git ls-remote origin` in the source plus an objects-only fetch of missing tips). Only a failed `ls-remote` falls back to the remote-tracking refs it already has. Once the advertisement succeeds, its locally present tips are the exclusions, even if downloading the missing tips fails; that download only makes the delta smaller.
  Rationale: older refs only make the delta larger, which beats aborting. Restore fails visibly only if origin has since deleted objects the delta left out.
  Date/Author: 2026-11-18, user.
- Decision (2026-11-19, the user asked for simplification after 16 review rounds; advisor session consulted): keep the origin-relative delta, but stop modelling Git configuration and topology, which caused most findings. Milestone 1 is restructured by these cuts:
  1. Restore only applies a delta to a clone that the existing session provisioner (`create_managed_clone`) made during this resume attempt. Restore's own init and fetch go, along with every destination check, rollback and retry rule. History-cutoff workspaces are provisioned like new sessions. The delta is applied before the staging rename, and `git worktree repair <nested paths>` runs after it.
  3. The snapshot records its pack's prerequisites: its boundary commits and any saved tips outside the pack. Restore checks them with `cat-file --batch-check`, fetches the missing ones from origin by SHA in one fetch, and fails naming them. This replaces the missing-object recovery loop and the old-Git branches.
  5. Positions are literal. HEAD is recorded as Git reports it and refs as read. Pre-operation positions, branch owners and displaced refs go.
  7. There is no stability loop. A managed capture has stopped every writer. A raw capture packs exactly what one ref read returned.
  8. Raw checkouts skip every teardown audit, with one fixed notice.
  9. Operation consent is keyed by checkout and kind. Abort prediction goes: capture takes a fresh inventory after the abort and refuses then. `deleted_refs` and tag cleanup go. Replace refs, grafts and shallowness become rows of the one presence audit.
  A seeded round-trip property test replaces most hand-written cases. Its properties: restore equals the source minus declared losses, restore works after the source clone is deleted, a fully pushed session gives an empty pack, and a raw capture leaves the source unchanged. A reviewer finding must then be a generator operation or a property violation. One managed-clone, network-origin path goes through Milestone 2 before Milestone 1 gets more review.
  The user decided the three remaining cuts on 2026-11-19:
  - Cut 2, accepted. The snapshot carries no configuration except origin's URL and the upstreams that name origin. Rewrite rules, push URLs and push evidence beyond origin, worktree-scoped remotes, module URL rules, URL anchoring and the checks built on them go. Configuration an agent changed inside its clone does not survive suspend.
  - Cut 4, rejected. Deltas against origin are kept for local-path origins too, so the origin-survival checks stay: an origin inside the teardown set, a route through it, alternates dependencies, shallow local origins, and local mirrors of uninitialized submodules.
  - Cut 6, accepted. Restore brings submodules up with `git submodule update --init -- <recorded paths>`, parent first, and reports a failure as a notice rather than a failed resume. The bespoke bootstrap, its overlay and rollback, and the staged-pin probe for uninitialized submodules go. At capture, an initialized submodule passes when it is clean and the root's own selection for it is empty: its refs, HEAD, stashes and the parent's current pins, minus its origin's tips. Otherwise a managed capture refuses.
  Untracked files in a raw checkout are reported in a notice, not put to consent, because the source checkout stays.
  Rationale: two owners built the same fact (the provisioner and the snapshot both configured clones), capture predicted what restore later rediscovered, and positions reconciled state the design itself had invented.
  Date/Author: 2026-11-19, plan author on the advisor's recommendation, at the user's request.
- Decision: capture queries origin from the source repository with its own configuration. Restore copies rewrite rules and remotes and runs ordinary fetches, submodules included. (Cut 2 later removed the copying of rewrite rules and push URLs; restore installs only origin's URL. The rest of this decision stands.) It copies no transport policy, sets no `GIT_PROTOCOL_FROM_USER=0`, and capture uses no scratch repository. If a resume host cannot reach origin as the source did, resume fails visibly and that is accepted. This supersedes the transport-policy copying, the capture-side emulation of restore's policy, and the scratch-repository procedure of reviews 72–85.
  Rationale: checking origin under restore's exact configuration drew a new review finding in each of about twelve rounds: Git's precedence, worktree-scoped keys, shallow and partial clones, other promisors, `core.sshCommand`. The user ruled that an origin resume cannot reach is simply unrecoverable, and that no more palatable failure is needed. Nothing is lost: the snapshot holds every commit origin did not advertise, and a Move keeps its source until the destination verifies.
  Date/Author: 2026-11-18, user.

- Decision: managed checkouts (Mjolnir-created clones) require an `origin` remote. This is enforced when the session is created (`create_managed_clone` today accepts a source without `remote.origin.url`), not first discovered at suspend. A new isolated session on a repository without an origin is refused before launch with a choice offered in the same surface: "This repository has no origin, which isolated sessions need. Run in this checkout directly instead, or add an origin." The surface offers "Run in this checkout" as an action, so the user is never stuck.

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
  Rationale: the snapshot now carries every unpushed commit, so suspend no longer risks unpublished work. The publication marker still gates automatic archival of old sessions. Its assessor (`mj-controller/src/controller/publication.rs`) reads only the legacy archive collector today. Under cut 2 (Decision Log, 2026-11-19), a new-format snapshot records only origin's configured URL, with no push URLs or push evidence. So when the assessor gains a new-format reader, it compares that recorded origin URL with the live configured `remote.origin.url` and queries origin alone. A snapshot counts as published only under the legacy assessor's rules (`mj-controller/src/controller/publication.rs`), applied to the snapshot's saved state: every checkout's working state is clean (`has_uncommitted_state` false, no working-state commit), its stash list is empty, no recorded index gitlink differs from HEAD's, and every initialized submodule's saved HEAD equals its parent's pin; every saved tag and notes ref has a ref of the same name at the same object on origin, so a local lightweight tag on a pushed commit stays unpublished; and every other saved ref and HEAD commit is contained in a head origin advertises. Staged or unstaged edits, stash entries, changed submodule positions and unpublished tag or notes names exist only in the checkpoint, so they keep the session unpublished, as they do for legacy archives. A local branch name whose commits origin already contains is not preserved, by the legacy rule's design: the work itself is recoverable from origin, as it is for legacy archives today. Until that reader lands, every assessor entry, including the stopped-clone refresh (`refresh_stopped_clone_publication_for_worktree`) that the daemon runs at resume, reports `Unknown` for an OriginDelta archive, because the legacy reader would judge it by legacy fields the new writer leaves empty. Automatic archival therefore never deletes an OriginDelta checkpoint of an independent clone. The vertical slice lands this gate with its first OriginDelta writer. Archival eligibility (`sessions_ready_to_archive_from_state` in `mj-sessionwiki`) also archives legacy managed worktrees by age alone, because their branch lives on in the source repository. That exception must not cover an OriginDelta checkpoint, whose working state and stashes exist only in the archive. Before Milestone 2 writes OriginDelta for legacy linked worktrees, eligibility requires the same current, Published, clean and stash-free assessment for every OriginDelta checkpoint, whatever its checkout kind. That is sufficient, not a weaker guess: restore fetches only from origin, so a saved tip that origin advertises is recoverable whatever a separate push URL or `pushInsteadOf` destination holds, and recoverability is what archival needs before it deletes a checkpoint. The legacy assessor, with its push-destination distinction and test, stays for legacy archives. The earlier per-destination push-evidence design is superseded.
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

- Decision: suspend and off-target Moves stop all of the session's processes before the final capture. A refused or failed capture leaves the checkout intact and restarts the agent in place, without its background processes. Same-target Moves are always in place and stop nothing.
  Rationale: a background writer could otherwise create files after consent that are then deleted with the checkout. Pausing with SIGSTOP and resuming on refusal kept generating edge cases in review: group-ID reuse, crash recovery, paused jobs, detached processes. The user chose the simpler order teardown already uses, processes first and files second, and said that for same-target moves "if something writes into the checkout it's no big deal", since nothing is removed. Refusals at the capture cut are rare because the preflight runs first.
  Date/Author: 2026-11-18, user.

- Decision: remote URLs are restored as configured, with the source checkout's repository-local rewrite rules copied in. A restored checkout behaves like a fresh clone of the same origin on the destination host. This supersedes the pinned and synthetic-alias scheme of plan review rounds 33 to 56.
  Rationale: each refinement of the exact-reproduction scheme exposed another Git rewrite edge case: equal-length ties, prefix capture after `set-url`, implicit push URLs from `pushInsteadOf`, and fetch/push collisions. The last one was fatal: `git submodule update --init` passes the alias to a separate `git clone` that cannot see the superproject's rewrite rules. The simple principle keeps everything Git users expect (submodules, `set-url`, default pushes), carries the source checkout's own configuration, and lets the destination host's configuration apply as it does to any clone there. Restore's missing-object check fails visibly if a host routes origin somewhere lacking the objects. This matches the user's preference for a lightweight design over over-engineered machinery.
  Date/Author: 2026-11-18, plan author.

- Decision: the user approved the plan author's decisions on publication consent, `--delete-branch` removal, rolling back in-flight old-format moves during the upgrade to the release that ships this work, the raw launch-record location, in-place agent restart, `--without-checkpoint` with no earlier checkpoint, and the suspend-preflight endpoint.
  Date/Author: 2026-11-18, user.


- Decision (2026-11-20, landing the managed-clone slice): suspend uses OriginDelta only when the session's worker is alive and has published its quiescence capability (`suspend-worker.json`) and the clone has an origin; an accepted suspend keeps OriginDelta even after its worker exits. Otherwise the shipped capture runs, so older workers and originless clones suspend as before the upgrade.
  Rationale: refusing those sessions strands users after an upgrade; the shipped capture is no regression. The worker-upgrade path in "Spec version and older workers" remains the target design.
  Date/Author: 2026-11-20, plan author.
- Decision (2026-11-20, landing the managed-clone slice): publication consent keeps the shipped behavior in this milestone: the TUI confirmation, the API's early 409 and the controller's post-checkpoint check. An OriginDelta suspend takes no publication assessment, so the controller never refuses it, but clients still show the confirmation. Removing the prompt moves to Milestone 3.
  Rationale: a consent-free rule for local clones removed the protection the hard-won daemon test from f11450124 pins for clones that fall back to the legacy capture, and no client can tell the two captures apart.
  Date/Author: 2026-11-20, plan author.
- Decision (2026-11-20): every publication assessor reports Unknown for an OriginDelta archive until a new-format reader exists, so automatic archival never deletes one. The pack is written against exactly the recorded prerequisites, not every exclusion. Restore removes the seed's push URL and tracking refs only when the redacted seed origin differs from the saved one, and does not refetch tracking refs.
  Date/Author: 2026-11-20, plan author.
- Decision (2026-11-21, raw sessions slice): raw Git checkouts capture `CheckpointRepositoryCapture::OriginDeltaRaw` directly under the relay checkpoint barrier (gated on relay protocol 78, falling back to `MetadataOnly` on older workers until the worker-upgrade slice lands), without running managed-clone quiescence (`OriginDeltaSuspend` / open-file check).
  Rationale: raw checkouts are never deleted on suspend, never abort operations or require untracked consent, and pack what one ref read returns (Cut 7 & Cut 8). Capturing under the relay barrier works identically on local bare and SSH bare targets without extra worker restarts.
  Date/Author: 2026-11-21, implementer.

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

**Scope.** Add the Git snapshot library in `mj-checkpoint/src/snapshot.rs`. It uses `GitCommandRunner`
to capture the session's Git state as a delta and apply it to a clone made by the session provisioner.
The library works with multiple repositories and both checkout ownership modes. The first Milestone 2
caller covers one independent local managed clone. It uses the read-only
`require_managed_snapshot_without_consent` preflight and captures after the worker stops the writers.

`capture_session_snapshot` takes `RepositoryInput` entries and `SessionCaptureConsent` and returns
a `SessionSnapshot` keyed by repository ID. `capture_workspace_snapshot` wraps the single-repository
case. `session_inventory` and `workspace_inventory` return `WorkspaceInventory` entries with typed
`SnapshotRefused` blockers and remedies. `CheckoutOwnership` distinguishes managed and raw checkouts.
`CheckoutKey` identifies a repository and the relative path of its primary or nested checkout.
`restore_workspace_snapshot` applies one `WorkspaceSnapshot` and returns notices.
`repair_workspace_snapshot` fixes linked worktree paths after publication.

**Capture** takes the following steps.

1. Inventory the entire session and check blockers and consent before the first abort. Managed
   consent covers nonignored untracked files and embedded repositories, untracked stash payloads,
   and applicable `.mj/keep/` files. `CaptureConsent` accepts blanket approval or the items shown.
   Shown file items carry file identity. Shown operations match `GitOperation` by checkout and
   `GitOperationKind`. Abort consented managed operations with Git, then inventory again and validate
   the resulting state against the whole session's teardown set. The second inventory can refuse.
   Tracked keep files, hidden tracked edits and intent-to-add entries can block capture.
   Raw operations stay in progress and their tracked working state is skipped.
2. Take the cut with `pin_workspace_cut`. Read refs once and use that enumeration for the commit
   of each symbolic branch HEAD. Keep a detached HEAD as read, including during a raw operation.
   Include all discovered nested linked worktrees inside the checkout. Keep managed local heads,
   tags, notes and durable `refs/mj/` refs, subject to the narrower selection for a legacy checkout
   with shared Git administration. Save stashes separately. Capture uses a single cut with no
   stability retry. If a working-state stash has a different parent, save that parent as detached.
3. Query origin from the source repository using its Git configuration. `remote_objects` runs
   `git ls-remote`, uses advertised heads and tags that peel to locally available commits, and tries
   to fetch missing advertised objects without writing refs or `FETCH_HEAD`. Only a failed
   advertisement uses cached origin tracking refs. `origin_tracking_objects` selects unambiguous
   fetch-refspec mappings, and capture issues a notice for stale exclusions or full history.
   An explicit `ExclusionSource::Commits` supplies the cutoffs instead.
4. Use `git stash create` on a private index to save staged and unstaged tracked edits. Strip any
   untracked parent from saved stash entries while keeping their tracked contents and messages.
   Record each checkout's index gitlinks separately, and record the path and HEAD of each initialized
   submodule. For managed capture, an initialized submodule must have clean tracked state, no active
   operation, and an empty object delta from its local refs, HEAD, stashes and the parent's current
   committed and index gitlinks against its origin. Uninitialized pins are left to Git at restore.
5. Feed the saved HEADs, refs, stashes and working-state commits to `git pack-objects --revs` with the
   exclusions. Keep a single non-thin pack, omitting the artifact when it contains zero objects.
   Record the boundary commits and saved tips left outside the pack as prerequisites.
   `WorkspaceSnapshot` holds that optional pack and `WorkspaceSnapshotMetadata` with the object
   format, gitlinks, positions, stash list, worktree paths and locks, and state flags and notices.
   The saved configuration is the credential-redacted origin URL and upstreams naming origin.

The managed teardown checks cover Git state the snapshot cannot represent. The presence audit
reports replace refs, grafts, shallow state, unrepresented worktree registrations and leftover
module administration directories. Presence alone is enough for those rows. Keep the separate
origin-survival check for local origins and uninitialized submodule mirrors, including the route
through the teardown roots, recursive alternates and shallow local origins. A mirror's URL is
resolved for this check, and its staged pin is left unprobed.

Raw capture skips teardown checks and issues a fixed notice that the checkout is kept and untracked
files and unsupported Git state stay behind. Given a `LaunchRecord` from `record_launch`, it
selects created or moved local refs, includes each saved symbolic HEAD's branch, and keeps newly
added stash entries. Without a launch record it keeps every local ref and the stash list, as Milestone 2's raw launch record specifies; the current code keeps only the checked-out branches and is aligned when raw sessions are wired. Raw capture leaves refs,
index, files and stashes alone. Objects-only fetches and synthetic stash objects may write objects.
A raw checkout with no origin or override is in-place-only. It saves HEAD and status metadata
without a transferable pack, and in-place resume uses the checkout itself.

**Restore** applies the snapshot in the provisioner's staging clone.

1. `create_managed_clone_with_snapshot` uses the ordinary independent clone path, then calls
   `restore_workspace_snapshot` before publication. The provisioner seeds the clone from the session's
   source repository when that exists on the target, and otherwise clones the snapshot's recorded
   origin, so a moved or deleted source checkout, or a resume on another target, depends only on
   origin and the pack. The vertical slice covers only the first case; the origin-seeded path is
   remaining Milestone 2 work. Restore uses the saved origin URL, retaining
   same-origin provisioner settings. When the URL differs or is absent it installs the saved URL,
   removes the old push URL and tracking refs, and fetches tracking refs. A shallow clone is
   unshallowed. Install and index the optional pack. A history-cutoff workspace (`--no-later-history`)
   is captured with `ExclusionSource::Commits` against its cutoff base and records no origin, even if
   the agent added an origin remote, so the pack holds everything after the base. Restore onto the
   cutoff-provisioned clone therefore installs no origin, fetches nothing, and fails visibly on a
   missing prerequisite rather than exposing later history.
2. Batch-check prerequisites with `git cat-file --batch-check` and send missing SHAs in one
   objects-only fetch from origin. Recheck and name any still missing. Also verify connectivity
   from all saved tips and fetch any remaining partial-clone objects in a separate batch.
3. Write saved local refs and restore their origin upstreams. Keep clone-only refs unless they
   conflict with a saved name. Rebuild the stash list oldest first, preserving repeated entries
   and messages. Attach or detach HEAD as recorded, reset to the saved commit and apply the
   working-state stash with `--index`.
4. Restore index gitlinks for checkouts whose working state was captured. Run
   `git submodule update --init -- <path>` parent first for recorded paths, then check and detach
   each initialized module at its saved commit. Report initialization or saved-commit errors as
   notices. Restore nested worktrees with the same HEAD, working-state and submodule steps and
   restore their locks. After the provisioner renames the staging checkout, run
   `git worktree repair` on the saved paths. A resume recovering its own published checkout
   uses its publication receipt and finishes repair there.

**Tests.** `seeded_git_round_trip_after_source_deletion_and_raw_immutability` generates real Git histories,
mixing staged and unstaged edits, additions and removals, commits, stashes, tags, branches, notes and pushes,
in SHA-1 and SHA-256 repositories with managed or raw ownership and nested worktrees. It serializes the
snapshot, deletes the source clone, restores, and compares refs, stash contents and messages, HEADs, exact
indexes and tracked files. Declared losses include untracked and ignored files, stash third parents,
raw launch-baseline state, active raw-operation edits, symbolic ref aliases, module-local state and extra
configuration. Git owns submodule directory placeholders. The only allowed baseline additions are
origin tracking refs, origin tags and the clone's initial branch. The raw invariant compares refs, indexes,
stashes and `FETCH_HEAD`. Companion seeds cover empty packs, published submodule positions and restore notices.
Hand-written real-Git cases still cover consent and abort ordering, raw operations and ref preservation,
stash payloads and duplicate entries, origin exclusions and SHA recovery, seed origin and ref conflicts,
credentials and SSH, shallow and partial clones, nested worktree repair, and teardown and submodule edges.

### Milestone 2: suspend and resume on the new snapshot

**Scope.** Suspend captures the repository half of the archive with Milestone 1's `capture_workspace_snapshot` instead of `CloneFrom`, `DeltaFrom`, `MetadataOnly` and the untracked tar. Resume restores it with `restore_workspace_snapshot`.

The new capture must never become active without its consent checks, so this milestone also lands:
- the request fields `discard_untracked` and `abort_operations` (shapes under Interfaces), and the CLI flags `--discard-untracked` and `--abort-operation`;
- the inventory recheck at the capture cut described in Milestone 3, which refuses when untracked files, a nonempty `.mj/keep/` on a managed checkout, or an operation is found without the matching consent.

Until Milestone 3 adds the dialogs, the terminal and web UIs show that refusal as an ordinary suspend failure. That is safe, because nothing is lost.

**Stop the session's processes before the final capture.** The relay barrier stops command dispatch, but a process the agent started earlier, such as a background command, a build or a dev server, can keep writing to the checkout. A file it created after the consent check would be deleted with the checkout, unseen. So suspend stops all of the session's processes *before* the final capture, the same order teardown already uses: processes first, files second. That applies to suspend and to Moves to another target, which both remove the source checkout. A same-target Move keeps the environment in place and stops nothing (Milestone 5). The sequence:

1. **Early consent check.** Run the preflight inventory against the request's consent, changing nothing. Nearly every refusal happens here, while the session is still running untouched.
2. **Stop the session's sub-agents, then interrupt the turn and stop every session process.** A managed sub-agent has its own worker and marker, and by default works in the parent's checkout. So suspend first closes child admission and stops the parent's known child sessions, each by its own worker's teardown. Today that happens only after the checkpoint is verified; it now happens before the quiescence check, so the children are gone before the parent's processes are stopped and the open-file check runs. Process stopping This uses today's teardown ownership in `mj-worker/src/worker_stop.rs` and the existing process-group teardown, extended to cover:
   - the harness and the process groups the worker tracks;
   - the worker's descendants. On Linux the worker sets `PR_SET_CHILD_SUBREAPER` at startup. It then also owns reaping adopted orphans: one reaper reaps any exited child the worker did not launch through its own subprocess helpers, and leaves the exit statuses of its own launched children to their existing waits, so adopted processes never pile up as zombies. Launch and reaping are serialized the way the microVM guest reaper already does it (`mj-worker/src/microvm/guest_agent.rs`, around lines 195–206). Every spawn holds a shared lock across `spawn` and registration of the child's PID, and the reaper takes the lock exclusively before deciding that an exited PID is an unregistered orphan. A child that exits before it is registered therefore never has its status taken by the reaper (new in this milestone; today only a microVM test sets it), so a descendant that daemonizes is reparented to the worker and stays in its tree. Sessions whose processes started under an older worker lack that, and rely on the marker and the open-file check;
   - every process on the host carrying this session's marker (`MJ_SESSION_ID`), which the worker's process observer in `mj-worker/src/work_facts.rs` already finds, including `setsid`-detached ones.

   Each process is verified before it is signaled, by its start time and its marker or tree membership. On Linux this goes through a `pidfd` (`pidfd_open`, then `pidfd_send_signal`), so verification and signal refer to the same process. macOS has no pidfd, so the identity is re-read immediately before `kill`; say so in a code comment. The worker repeats enumerate-and-terminate until an enumeration finds no live attributable process. A `/proc` scan alone cannot prove that, though: a parent can fork and exit between reading the directory and reading its state. So on Linux, quiescence of the worker's own tree is decided by the kernel. Because the worker is a subreaper, every orphaned descendant becomes its child, so once `waitid(P_ALL, …, WEXITED | WNOHANG | WNOWAIT)` reports `ECHILD` the worker has no descendants at all. The worker runs its own helper subprocesses for capture only after that point. The marker scan covers session processes outside the tree, which predate the subreaper or run on macOS. Quiescence holds when the worker has no children and a marker scan then finds no marked process. A process in uninterruptible sleep (state `D`) can delay that indefinitely, so the wait is cancellable, and the suspend's progress names the processes still running. There is no guessed timeout.
3. **Open-file check for a managed checkout.** Once they have exited, scan for any remaining process whose working directory, root or open file descriptors lie inside the checkout or `.mj/keep/` (`/proc/<pid>/cwd`, `root` and `fd` on Linux, `lsof +D` on macOS). Such a process is not provably the session's, and it may be a writer. If one exists, suspend refuses: "Mjolnir cannot prove these processes belong to this session; stop them, then retry", naming PIDs and command lines. A raw checkout is never deleted and the user's own tools may have files open, so the check does not apply there. The raw checkout stays in place, so its untracked files are reported in a notice rather than put to consent, and nothing is re-checked after packing.
4. **Final capture,** the single consent decision point, now against a workspace nothing writes to.
5. **Outcome.** The daemon records it from its durable suspend intent:
   - **Success:** the daemon verifies and installs the archive, then retires the checkout, as today.
   - **Refusal or failure,** whether consent, a Git or I/O error, a transfer error, or cancellation of the wait: nothing was deleted, and the checkout is intact. The daemon restarts the agent in place, the way a worker restart already does (Milestone 4 makes in-place restart archive-free), but only once the original harness has exited. If the wait was cancelled while the original harness was still alive (for example stuck in state `D`), the restart is deferred until the worker observes that process gone. Two harnesses never share the checkout and native history, and the session shows "restarting after the previous agent exits" until then. The session is running again, minus its background processes, and the refusal or error tells the user what happened and what to do.
   - **A daemon restart or worker reconnect midway** continues from the durable suspend intent: processes already stopped stay stopped, and capture is retried or the agent restarted according to the recorded outcome.
   - **The consent the user gave** (the `Shown` lists or the `Any` flags, and `without_checkpoint`) is persisted with the accepted suspend intent, in the same write that records the session as closing before the HTTP acceptance. A daemon restart before the final capture rechecks against exactly that consent, never a default. This changes a persisted record; classify the migration as compatibility requires.

**Suspends accepted by the previous release.** An upgrade can find a session that the old daemon left `Closing` or `Destroying`. Such a record carries none of the new consent fields. The old daemon persisted `Closing` *before* the relay received Close, so the relay may be in any state. The new daemon recognizes these legacy intents by the absence of the new consent fields, and finishes them by relay state:
- **`Idle` or `Running` relay** (Close never reached the worker): nothing has been sealed or captured, so the suspend continues on the new path with no consent. That means the worker upgrade if needed, the preflight, stopping processes, and capture. If untracked files or an operation need consent, it is refused like any no-consent suspend: the session keeps running untouched, and the user gets a notice that the suspend requested before the upgrade needs their consent. Nothing is lost, and no old writer is needed.
- **A sub-agent left `Closing` or `Destroying`:** the previous release's close router checks sub-agent ownership before anything else. It synchronizes the child's conversation and tears it down without any archive. The new daemon routes these records the same way, through the existing archive-free sub-agent completion (`mj-controller/src/controller/lifecycle/subagent.rs`), never through the managed suspend recovery, which would reject them for lacking a checkpoint.
- **Sealed relay (`Closing` or `Closed`), or a session already `Destroying`:** the previous release verified and installed the archive *before* it sent Close, so a sealed legacy intent always has its installed checkpoint. `recover_interrupted_close_managed`'s sealed branch waits out `Closing` and finishes teardown from that installed checkpoint, with no export. The daemon keeps only that sealed branch and the installed-archive verification it uses. The capture branch, including `ReuseUnchangedArchive` and the version-2 export spec, is not needed for legacy intents, since unsealed ones take the new path above. Mark the kept branch in code as the legacy-intent completion path, removable only in a release whose upgrade floor guarantees no older daemon can have left a sealed legacy intent.

An accepted suspend is completed rather than repeated or dropped, as the `AGENTS.md` upgrade rules require. This is the single exception to "no new old-format archive", limited to suspends accepted before the upgrade. Add an upgrade regression for each relay state (`Idle`, `Running`, `Closing`, `Closed`) and for `Destroying`, each built from records exactly as the previous release writes them, with the archive already installed in the sealed cases.

**Handoff admission.** Stopping processes and capturing are worker-owned durable steps, and waiting for processes to exit can take indefinitely long. So the daemon holds lifecycle handoff admission only for its bounded control transitions: accepting the suspend and persisting its intent and consent, sending the stop or capture request, and recording the outcome. A replacement daemon finds the durable suspend intent at startup, reattaches, asks the worker where it is (stopping, captured, failed), and continues. This follows the `AGENTS.md` rule that handoff waits only for daemon-owned bounded work.

Add tricky-condition tests:
- a `setsid`-detached background command writing into the checkout is stopped before capture;
- a background command that creates a file after the preflight is stopped first, and if that file is not covered by consent, suspend refuses and the agent restarts in place with the checkout intact;
- a capture that fails with an injected Git error leaves the checkout intact and the agent restarted.

At the end of this milestone, suspending and resuming a managed-clone session, a raw session (in place and onto another target) and a legacy-worktree session all work, and old archives still resume.

**Plain directories.** Sessions opened in a directory without Git (`PlainDirectory` in `session_export_layout`, `RepositoryMetadata::plain_directory`) keep their existing metadata-only treatment. Suspend never runs the Git inventory, consent check or capture on them, and their transcript is saved as today.

**Archive format.** Add a new capture mode, `CheckpointRepositoryCapture::OriginDelta`, and a new archive schema version in `mj-checkpoint/src/archive.rs`. An older daemon's archive reader rejects unknown schema versions. Once a suspend writes the new format and removes the checkout, an older daemon could open the session but never resume it. So the store migration that ships with this milestone is **breaking**: advance the migration revision and raise the minimum compatible read and write revision in the same transaction, with that reason beside it, and test it with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. Milestone 5's `MoveOperation` change raises the floor again, under its own revision. The next slice's schema bump (see Pending mailbox events below), not this first OriginDelta one, carries the worker-owned, versioned native-use evidence (identity, opening ordinal, used/unused/unknown) in the archive's canonical session. That replaces the transcript-based `current_native_session_received_prompt()` predicate, so the checkpoint asks the evidence's owner instead of recomputing the fact, and lets Milestone 4's conservative adoption marker be removed. Archives in older formats, including OriginDelta archives written before that slice, carry no evidence. For those only, restore keeps reading the old predicate as their legacy reader, so an old archive made before the first prompt, or just after `/clear`, can still replace an absent native session. The shipped 2.71.0 reader has `deny_unknown_fields` on `CanonicalSessionSnapshot` (`mj-core/src/archive.rs`, around lines 224–226), so the field can only arrive with a breaking revision, which that slice provides. The archive stores the `WorkspaceSnapshot` metadata in `RepositoryMetadata` (new fields beside the existing `saved_refs` and `stash_stack`) and the pack as a payload.

**Pending mailbox events (#1616).** A mailbox message the worker accepted before the checkpoint barrier, but had not yet delivered to a harness turn, is lost on suspend today. The daemon has already marked its outbox row accepted, so it never sends the message again, yet the canonical state and the restore seed carry no payload for it (`pending_mailbox_events` in `mj-core/src/relay/snapshot/apply.rs`; the canonical and seed queue shapes in `mj-core/src/archive.rs` and `mj-core/src/relay.rs`). The canonical session therefore carries every event the relay owner has accepted but not delivered, payloads included: `pending_mailbox_events` and any events a hook holds under an unacknowledged `mailbox_hook_lease`, which return to pending at the cut. It also carries the relay's mailbox command receipts and delivered-event keys, and the relay-owned `mailbox_prompt_contexts` of retained recovery prompts. A worker that crashed after claiming a mailbox event for a prompt, but before sending it, moves the claimed message text to the recovery command there, and that text must travel with the queued recovery prompt. Restore reinstates both, so undelivered events arrive in their original order, and a daemon retry of a command ID whose acceptance it never recorded is deduplicated by the resumed relay instead of delivered twice. There is no separate preservation path: the worker is the owner of accepted mail, and the checkpoint copies its state. Like the native-use evidence, this needs the breaking archive schema, because the shipped reader denies unknown fields.

These two canonical-session fields are built as the next slice after the managed-clone vertical slice lands, with their own archive schema version and breaking store revision. If both land before the same release, users see one breaking upgrade. Test them as a tricky condition: a message accepted just before the suspend barrier, one leased to an interrupted hook, one the worker delivered before the daemon recorded its acceptance (so the daemon retries the same command ID after resume), and one claimed by a recovery prompt queued at the cut are each delivered exactly once.

**Spec version and older workers.** Raise `CHECKPOINT_EXPORT_PROTOCOL_VERSION` to 3 for specs that request `OriginDelta`.

A session whose running worker predates this change must not fall back to the old capture. That capture stores untracked files and full clone history, which is exactly what the user excluded, and can mean gigabytes of upload. An old worker also cannot stop the session's processes the way the new suspend requires. So suspending such a session first runs the read-only preflight against the request's consent, which interrupts nothing, and only if that passes upgrades its worker through the existing atomic idle replacement (`mj-controller/src/controller/worker_restart.rs`, `IdleWorkspaceLease`), after interrupting any turn as suspend already does, and then proceeds exactly as for a current worker: consent, stop the processes, capture.

There is one suspend path, and no version-2 spec ever writes a new archive. If the replacement cannot happen because the worker is not idle, suspend fails with that reason and the session keeps running. If the swap fails after the old worker has stopped (`WorkerRestartLeftNoWorker`), suspend first restarts a worker in place from the current binary through the existing worker recovery, and the session keeps running. If that also fails, the daemon records and shows that the session has no live worker, with its checkout intact, and offers Retry and `--without-checkpoint`. In every case the checkout is untouched.

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
  2. If the archive's base commit is missing from that clone, fetch it once from the source repository, which exists only on the session's original target; for these old sessions the base often is not on origin. Such a session can therefore resume on another target only when its base is on origin. Otherwise the resume dialogs offer only the original target and say why: "this older session's starting commit exists only in <source repository> on <target>; resume it there, push, then move it". This is the same rule as for raw checkouts without an origin, so the user always has a stated path forward.
  3. Apply the archived delta.
  4. Check out the saved commit on the saved branch name, or as a detached HEAD if that name is already a branch in the new clone with a different value.

  Old untracked payloads are restored. Saved in-progress operation state is dropped, and a resume notice says so, but not the work inside it. Old archives store the working files of unmerged paths in the operation payload and leave those paths out of the staged and unstaged patches. The legacy reader therefore writes those files into the working tree as unstaged changes before discarding the operation metadata, so a partial conflict resolution survives, as ordinary edits to tracked files. The notice names those paths.

**Fail visibly.** A clone is independent of its source, so retiring it depends only on the verified origin-relative snapshot, never on the source repository. Remove the missing-source short-circuit in `remove_managed_worktree_checkout` (`worktree.rs`, around line 2777). Also remove the separate prerequisite in its caller `retire_managed_worktree`, which runs `git cat-file` in the source repository for the launch commit (`worktree.rs`, around lines 2748–2761). For clones, the prerequisite becomes the session's verified, installed snapshot. Linked-worktree removal still needs the source repository. Removing a legacy linked worktree (`remove_managed_worktree_checkout`) removes only the selected registration, never a repository-wide `git worktree prune`, which could delete a prunable sibling's admin directory and the submodule repositories inside it that this session's presence audit never inspected. It runs `git worktree remove --force <path>`, or deletes only that entry's admin directory when its checkout is already gone. It honors the entry's lock as `git worktree prune` does: when `.git/worktrees/<name>/locked` exists, teardown keeps that admin directory, even if the checkout path is missing (for example on a disconnected mount), and reports that the locked registration was kept.

**Tests.**

- One end-to-end test in an isolated instance, run as described under Validation: suspend and resume a managed-clone session across two local targets, bare and Podman.
- A compatibility test that resumes a checked-in old-format archive fixture of each capture kind, including a legacy linked worktree, into a fresh managed clone. Such a fixture is a persisted-format reader test, which `AGENTS.md` allows; mark it `// Hard-won` only if it catches a defect after the code was believed done.

### Milestone 3: warnings, consent and lifecycle verbs

**Scope.** Every way of suspending a session shows the untracked-file and in-progress-operation warnings and requires consent, and `suspend --without-checkpoint` exists. At the end, the CLI, terminal UI and web UI behave as described under Validation.

**Preflight.** Add a daemon operation and an HTTP route, `GET /api/v1/sessions/{session_id}/suspend-preflight`. It runs Milestone 1's inventory on the target in a background task and returns:

- the untracked paths;
- any in-progress operation per checkout;
- whether the checkout is raw;
- the existing interruption and unfinished-children facts;
- every unconditional blocker, each with its kind, path and remedy: a shallow repository, structural teardown leftovers, intent-to-add entries and the initialized-submodule rules. Blockers that only appear after a consented abort are not predicted; capture's fresh inventory after the abort refuses then (cut 9).

When any blocker is present, every surface (CLI, terminal UI, web UI, and Move's preflight) shows the blockers and their remedies and offers no "Continue anyway" or abort consent until they are cleared.

The terminal UI and web UI must call it off their event loops (`AGENTS.md`: no blocking I/O on render loops). They show "Checking workspace…" until it returns.

**Consent.** Add `discard_untracked: bool` to the suspend request: the HTTP body in `turns.rs`, `SessionLifecycleRequest::Suspend` in `mj-client`, and the older direct daemon action. Remove `acknowledge_unpublished_work` and `ignore_nested_repositories` from suspend.

**Consent is checked at the capture cut against what was approved.** The request carries the consent the user actually gave:
- **CLI flags approve blindly:** `--discard-untracked` sends `UntrackedConsent::Any`, and `--abort-operation` sends `OperationConsent::Any`. The person or agent typing the flag accepts whatever is there.
- **Dialogs approve exactly what they displayed:** `Shown` with the listed untracked items (each path with its file identity: inode, size and nanosecond mtime on Unix; created, modified and size on Windows; so a different file recreated at an approved path is not covered), and `Shown` with the listed (repository, checkout, operation kind) entries. Operation consent is keyed by checkout and kind (cut 9, Decision Log 2026-11-19): consenting to abort a checkout's in-progress rebase covers whichever rebase is in progress there at the capture cut, which is what the user approved in order to suspend. A second operation of the same kind starting inside the dialog window is a rare race, and an operation identity would bring back mechanism the simplification removed. For a managed checkout's `.mj/keep/`, the dialog lists the files it holds, and consent covers exactly those files; the CLI's blanket `Any` covers the whole directory.

Milestone 1's capture checks untracked paths and operations against that consent inside the barrier, and refuses with the uncovered items before changing anything. The controller checks a managed checkout's `.mj/keep/` (not Git state, so not part of capture) in the same step, immediately before capture under the barrier: after the session's processes are stopped, if `.mj/keep/` holds any file that neither `Any` nor the `Shown` list covers, which includes a scratch file created after the dialog opened, covers it, suspend refuses.

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
- Otherwise, the Git delta size comes from Milestone 1's `estimate_delta`. Its contract is that it changes no ref, index, working file or stash list. It may add unreferenced objects (downloaded tips, `git stash create` results), which garbage collection removes. It sizes the sanitized stash replacements, as capture packs them, so a `git stash -u` full of scratch does not inflate the estimate. It reuses capture's exact exclusion selection: advertised SHAs when origin is reachable, remote-tracking refs otherwise. It writes the pack with capture's own file-backed `pack-objects --revs <basename>` helper into a temporary directory, measures it and deletes it, so the estimate is the size of the real transfer pack and the two cannot disagree. Local storage would be the wrong measure: a blob may be stored as a tiny delta against an object the snapshot excludes, and `rev-list --disk-usage` needs Git 2.31 anyway. The working-state commit is made by running `git stash create`. That writes unreachable objects but changes no ref, index or file, so the estimate includes the staged and unstaged content capture would save; a pushed branch with large tracked edits does not look empty.
- The session-state size is the sum of the native and canonical files that capture would read.
- The dry run changes nothing on hosts or in AWS: it never installs a temporary key or security-group rule to probe. It reports the route it can determine read-only (same host, or a direct path already open), and otherwise reports the route as conditional: "direct if the temporary grant can be installed; otherwise through the daemon".
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

**Bulk paths:** workspace deletion aggregates the preflights of its sessions into its confirmation. It holds daemon handoff admission only while it records its durable deletion intent and dispatches the suspends. It waits for them without holding admission, and a replacement daemon resumes the wait from that intent. A session stuck in uninterruptible sleep therefore cannot block a daemon upgrade through the bulk path either. `mj daemon stop --suspend-sessions` gains `--discard-untracked`. `mj acp --on-exit suspend` sends no consent: `discard_untracked` and `abort_operations` are absent, the same representation every surface uses for "no consent"; on refusal it leaves the session running for a person to handle, as it already does on other refusals.

**Without checkpoint.** Add `without_checkpoint: bool` to the suspend request. In the controller, replace `prepare_discard`/`force_stop` (`lifecycle.rs`, around lines 1171 and 1211) and `suspend_session_without_checkpoint` (around line 945) with one path:

1. Verify the existing checkpoint first, as `prepare_discard` and `force_stop_with` do today: the archive exists and its SHA-256 matches the row. If it verifies, the confirmation names its age and says Resume will continue from it. If there is no checkpoint, or it fails verification, the confirmation says nothing can be resumed, and the user must confirm that explicitly (CLI: `--without-checkpoint` reports it and requires `--yes` in that case). A missing or corrupt archive is never presented as a Resume source.
2. Stop the worker, its children and the environment, using `DestroyCleanup::BestEffort` semantics, so it completes with warnings when the target is unreachable (see `destroy_session_with_cleanup`).
3. Keep the session record, its history and the existing checkpoint row, if it verified; otherwise mark the session as having no resumable checkpoint.
4. Settle the session as `Stopped`.

Resume uses the existing, verified checkpoint. When there is none, the session is shown as "Suspended without a checkpoint: nothing was saved, so it cannot be resumed". Resume is not offered; Destroy remains. A managed clone's branches went with the removed clone, so there is nothing to resume from. The confirmation states the age of the checkpoint Resume would use, or warns that there is none.

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

Verification found two such records, and both get their own retention.
- **Move's accepted command IDs and outcomes:** a crash midway through queue admission must not resubmit an already-accepted command. The worker keeps an operation pin set for each Move, updated by a relay pin-update request. The daemon registers the operation's command IDs before submitting the queue, and releases them only after it has durably recorded `queue_admission_finished`. Pinned receipts are exempt from acknowledgement pruning and from the retry tail. This is mandatory on workers that advertise the pin protocol. Older workers keep their old min(ACK, floor) retention, which never prunes those receipts before a checkpoint, and are never sent pin requests.
- **Native-use evidence:** an explicit, versioned known or unknown record in the relay snapshot, migrated once from the journal while it is still intact.
- **Startup command receipts:** kept for the session's lifetime, until destroy, as owner references in the worker's state. `seed_initial_commands` reads them on every reopen, including after `/clear`, so the retained receipt is the single durable fact it consults. An older worker may already have evicted that receipt under the old terminal-command retention. Native-use evidence cannot stand in for it, because `/clear` resets that evidence for the new conversation. Once, at the upgrade to the new worker state, a worker without a retained startup brief reads `latest_prompt_accepted_ordinal`. It is session-lifetime state, set only when a prompt is accepted, and `/clear` never resets it. Startup is seeded before the control socket is published, so the first prompt ever accepted is the startup brief. Retained startup receipt IDs are pinned as they are. The whole startup sequence is pinned as seeded, including commands whose receipts were pruned (pins with no receipt bodies), when either the watermark shows an accepted prompt or the native-use evidence says the session was used. A worker last upgraded before the watermark existed reads it as `None`, which means unknown, not "no prompt", so native use covers it. Both are absent only in two cases. One is a worker that crashed before its brief, which is still owed. The other is a pre-watermark worker that ran `/clear`, had its brief's receipt evicted, and accepted no prompt since. Today's code resubmits in that second case too, so the migration adds no new loss. Otherwise the existing crashed-prefix completion (`initial_fifo_completes_a_crashed_prefix`) finishes the sequence, which covers a worker that crashed after its configuration commands but before its brief. Resubmitting a configuration command whose receipt was evicted is harmless, because those commands set values idempotently. It then never resubmits its startup commands, and from then on the pin set is the only fact `seed_initial_commands` reads. A checkpoint-only open is the journal's exclusive writer while the worker is stopped, so it may prune too, and these pins protect it.

**Adoption after acknowledgement pruning.** `mj recover adopt` is a manual recovery for a worker whose daemon record and projection were lost while it kept running. Ordinary restart, Move and resume never use it. The journal below the acknowledgement is gone, and the daemon, not the worker, authors canonical history (`checkpoint/latched.rs` builds it from the daemon's projection). When the worker's retained journal still begins at genesis, adoption is ordinary reattach from cursor 0. That is the ordinary path, it keeps full history, and it works with older workers. Its session-record write uses the same install guard as the live cut, described below: the per-session recovery permit is held through the existence check and the write, and the short "adoption install" admission is held only for the write and the reply. Replay from cursor 0 follows outside both. Otherwise adoption cannot rebuild history, and it does not try:
1. It takes one coherent cut of the worker's current operational state at its frontier F, with digest D, only while the worker is quiescent: no executing turn and no running user shell, as the relay's existing turn owner decides. A user shell counts as running only once it has started (`started_at_ms` is set). An accepted shell still waiting for a free slot is queued work: the hold keeps it queued, the cut carries it as a queued row, and it does not block the cut. A live harness terminal, which a harness can leave running after its turn ends and which the relay tracks in its own terminal inventory, also blocks the cut until it exits. A terminal counts from the moment its create request reaches the relay owner, registered before the process is spawned, not from the later `TerminalStarted` event. Only the inventory entry is reserved synchronously. `TerminalStarted`, which writes the fallback card, and any spawn failure travel on the same runtime FIFO, so assistant output queued earlier stays in order. If creation fails, for example because the executable or working directory is missing, the same failure path removes the registration and wakes the waiter. A terminal that never existed cannot block the cut. The terminal and its fallback tool row share that one lifecycle under the relay owner: both are registered before the spawn and settled together on close or failure. A `TerminalStarted` that arrives after the terminal already closed changes neither, so a short-lived terminal cannot leave a stale in-progress row that blocks the cut. Otherwise a background agent could start one in the gap and the cut would miss it. The worker's existing activity count includes queued shells, so the cut does not reuse that count. A turn counts as executing when it is the active prompt, a harness turn, or a `HeldTurn`: an autonomous turn the harness announced before the foreground prompt settled. A foreground tool call in flight (`tools_in_flight`), which a harness without explicit turn boundaries can run with neither an active prompt nor a harness turn, also blocks the cut, so the tool's start and result are never split. So does a context reset (`/clear`) still in progress (`clear_context_in_progress`), any native goal resume still in flight, whichever route started it (a `GoalControl::Resume` dispatch, `goal::recover` when the harness reopens, or `goal::resolve` after a submitted recovery answer), because native work can start before the relay applies the goal-running update that opens a harness turn; the relay owns that in-flight state for every route. A "resume sent" marker is enqueued on the relay's runtime FIFO before the native send, with no apply acknowledgement. The reservation is released only by matching running evidence (which moves protection to the harness turn), explicit goal deactivation, clear or supersession, a typed rejection code that only the harness can produce, or the bridge exiting. An idle notification alone never releases it, because it may belong to an older turn; and any configuration dispatch still awaiting the harness: a promoted `SetConfig` (`promoted_config_in_progress()`), `SetSessionMode` or `RestoreExecutionMode`. Its queue entry is already gone while its completion still depends on it. Where practical, the relay answers all of these through one work-in-flight query, so the cut asks a single owner one question. It runs without an active prompt or harness turn while the worker changes its native session, and a cut taken then would describe the old conversation. A cut can therefore never split a turn that native work has already started. A user shell can outlive the turn that started it, and the worker already holds later prompts behind running shells. Pending questions are allowed. The worker owns the timing. If it is quiescent, it answers the cut request at once. Otherwise it takes the cut at the first point where both have finished, before it starts the next queued or autonomous turn or shell. That needs no polling and starves no one. Wait notices reach the CLI while the request is still open. The worker reports its current blockers over the cut call as they change. Existing lifecycle notices are keyed by a session record (`set_lifecycle_notice` on the active lifecycle), and an orphan has none, so the daemon forwards blockers on the request's own connection instead. Before the final reply, `AdoptRecovery` may send zero or more `AdoptionWaiting { blockers, destroy_command }` replies on the same request, and the client reads them until the final one. A notice is sent only when the blocker set changes, and only the latest set is held, so a slow reader sees newer sets replace older ones. The reply type is gated on the daemon protocol version, like other request-shape changes. The CLI names what it is waiting for: the running turn, the running turn blocked on a question or tool approval, a native goal resume that has not yet opened its turn, a foreground tool call, a configuration change, a context reset, each running user shell with its command, or each live harness terminal. Adoption never interrupts work (user decision, 2026-11-18). An orphan has no tracked session, so a question cannot be answered through Mjolnir, and a shell or terminal like `tail -f` may never end. If the worker never goes quiescent, the wait message says so and names the one way forward. For an orphan, that is `mj recover destroy`, built for that worker as below, which gives up the session. For a tracked session, in the backup-recovery follow-up, it is the ordinary `mj destroy` on that session, because `mj recover destroy` accepts only resources that no record names. Native sub-agents and AIR background tasks do not block the cut, which carries their identities and replay phase. Sending the cut request needs no lifecycle ownership, because the request changes nothing on the worker except holding its queued work, which the request's own connection releases. While any cut request is pending, the relay owner keeps its own queued work queued. The relay owns the set of outstanding cut requests, so when two clients adopt the same busy orphan, one client's Ctrl-C does not release work the other still waits on. Queued work resumes only when the last request has been served or dropped. Its promotion path (`record_command_completed`, which promotes the next prompt under the relay lock), its shell-claim path for starting shells (an accepted `CancelUserShell` still runs, so it can end the shell the cut is waiting on), and its automatic steering (`submit_automatic_steer`, which can feed a queued follow-up into the running prompt) do not start or consume queued prompts or shells. Otherwise a follow-up or a queued shell could start in the same locked step that ends the current work, and the boundary would never be reached. When the running work has finished, the relay takes the relay-state clone at F in that same locked step and then resumes promotion. It serializes and pages the clone outside the lock. If the request is dropped, promotion resumes the same way. Turns the harness starts on its own are not held. A turn the harness starts on its own after that is an ordinary event after F and reaches the daemon through ordinary reattach. No native output is held back.

   The cut request is a long-lived, cancellable relay call, exempt from the ordinary RPC deadline: a deadline would abandon a wait that legitimately lasts a whole turn. The daemon classifies `AdoptRecovery` as non-admitted work, like a sub-agent wait, so neither request-level admission nor any other admission is held while it waits. The wait ends when the cut arrives, when the client disconnects, or when draining begins. In the last two cases the daemon drops the call. The daemon notices the client disconnecting because its handler for `AdoptRecovery` reads the CLI connection concurrently with the wait, the existence check and the local checks. The cancellation boundary is the database writer's acceptance of the install write. Submission decides cancellation and acceptance together. The install waits for queue space in a way that also watches for disconnect, and acceptance is the moment the job is enqueued. A disconnect that wins the race leaves nothing queued, and the job can never be enqueued afterwards by a task still blocked on a full queue. A disconnect before acceptance cancels and discards the cut. Once the writer has accepted the job, it runs to completion whether or not the caller waits. So the install then keeps both the recovery permit and handoff admission through the commit, publication and the attempt to reply. A same-instance `recover destroy` can never see an orphan whose registration is still pending. The ordinary request path awaits its handler without watching the socket, so it would miss a Ctrl-C. The worker detects the closed connection while the cut is still pending, because its relay server reads the connection concurrently with the wait instead of reading it only between requests. It then releases its hold on queued work at once, not when the running turn or shell ends. Ctrl-C, or a daemon handoff, cancels the wait without affecting the worker, which keeps running without a daemon.

   The wait holds no handoff admission; it is cancellable work like a sub-agent wait. When the worker delivers the cut, the install proceeds in one order: first the per-session recovery permit, then the cancellable existence check, then handoff admission as "adoption install", and only then any write. The daemon takes the session's per-session recovery permit, which excludes `recover destroy` and other adoptions of the same worker, before checking that the worker still exists, and holds it through the install. Adoption holds that exclusion without any handoff admission. `RecoveryGate::enter_foreground`'s holder carries only the per-session gate and identity, not upgrade work, so adoption reuses it rather than adding a new gate form, and takes its short handoff admission separately. Destroy keeps its existing entry. The existence check may contact a remote target, so handoff admission is not held during it. The check is also cancelled when draining begins or the client disconnects. Adoption then releases the permit and discards the cut, and reports that a handoff interrupted it if the client is still there to hear it. A disconnect cancels only before writer acceptance, as specified below. A same-instance `recover destroy` that holds request admission while waiting for the permit can therefore never stall handoff behind an unreachable target. Admission is taken only after the check, for the local checks (draining has not begun, no other record now names the worker), the transaction and the reply. A concurrent `recover destroy` therefore either completes before adoption takes ownership, so the existence check finds no worker, or waits for the whole install and then sees a tracked session. If any check fails, it discards the cut, which is only a read and leaves the worker unchanged, and reports why, so the user can rerun the command if that still makes sense. Otherwise the atomic install of the projection and cursor runs under that ownership and admission, which are held through delivery of the reply to the CLI, as for any daemon response, so a handoff cannot commit the adoption and lose the reply. Both are released on every exit path. Attaching and catching up on events after F are not part of the install. They happen through ordinary recovery from the committed cursor, which a handoff may interrupt and the next daemon resumes, so a large post-cut replay or a slow remote worker never holds handoff. A concurrent `recover destroy` therefore either wins before the install, which then finds no worker, or waits for it and then sees a tracked session rather than an orphan. The cut contains typed queue bodies and IDs, in the same representation normal replay uses. That includes each accepted user shell that has not started (it waits for a free slot), as the queued row normal projection created on `CommandAccepted`, so its later start, output and completion update that row; the worker's pending steering state, including a steer whose delivery became uncertain, which outlives its turn and keeps the queued prompt blocked until the user resolves it, so the adopted session shows the same resolution control; question bodies; effective configuration; published and staged native identities with their replay phase; and native-use evidence.
2. It installs that cut atomically as a minimal live projection, with a visible "History before recovery unavailable" notice, and sets the projection cursor to F/D. A later suspend, resume or Move launches a fresh relay, and that launch decides whether to deliver the startup brief by looking for the transcript item `user:<command id>`. So the projection also gets that item, placed before the notice, for every startup prompt the worker's durable state shows as started. The cut is idle, so that means its handled receipt has `outcome = Prompt`, or records any failure other than "queued command was removed". Terminal-command compaction removes the dispatch record, so a durable receipt cannot tell a rejection apart from an interrupted turn. Every failure other than removal therefore counts as delivered, which matches normal projection for an interrupted turn. Receipts record no separate start outcome. Absence from the queue is not proof, because a queued brief can also be removed without running. A brief still queued at F, or whose receipt carries the "queued command was removed" failure, gets no transcript item and remains owed. A pin with no receipt body exists only from the one-time legacy migration. That migration creates it only when the session shows an accepted prompt or native use, so the pin counts as started; such a brief predates the upgrade and has almost certainly run. Two consequences are intended. A brief that started and was then interrupted counts as delivered, as it does in normal projection, which creates the `user:<command id>` item when the command starts. And a legacy brief that was accepted, then removed from the queue before it ran, and later adopted would be treated as delivered. That case is accepted rather than tracked. The item's content comes from the worker's original launch command (`initial.command`), the only source of what the brief said; pins and receipts decide only whether it was delivered. The item is added whenever the brief was delivered, even after a later `/clear`. It is the launcher's delivery record, so omitting it would let a later resume send the assignment again. Placed before the history notice, it reads as earlier history. The content is empty only when the worker has no launch command. The item the launcher reads is then the single fact that the brief was delivered, and the assignment never runs twice.
3. It attaches through ordinary reattach, which replays only events after F. Projection repair never installs a projection behind the durable frontier: when catch-up is impossible, it keeps the current projection and fails visibly, naming the file, offset and frontier. A pending question counts as live at the cut only if its responder bridge is alive, as the `PendingElicitations` owner decides. The install records the session as Disconnected, and it becomes Running only once that catch-up succeeds, so the record never claims a connection whose journal has not yet proven readable. If catch-up fails, the session stays visibly Disconnected or in error, and the CLI reply says it was adopted and that reattaching failed, with the error. The install holders cover only the install, its publication and an "adopted; reattaching" notice on the adoption connection. Catch-up runs under no install holder, so a handoff can interrupt it and the next daemon resumes it. The switch to Running takes a short same-session holder and a compare-and-swap. That holder also covers the runtime refresh and delivery of the final reply, adopted or adopted-but-reattaching-failed, as the daemon's response contract does for other replies. Catch-up itself stays outside every holder. Genesis adoption installs and attaches the same way. A worker too old to serve the cut cannot be restarted onto the current binary, because no restart command reaches a worker without a session record. Its refusal says that the worker's history no longer reaches its start and that it predates live-cut recovery, so it cannot be adopted. It adds that the agent's work continues on the worker and its harness session files remain on the target, and gives the exact `mj recover destroy` command for when the user is done with it. That command is built for the worker just inspected: `--session`, `--target` and `--confirm`, plus the same `--instance` the adoption ran under when it is not the default, and `--all-instances` when the worker carries another instance's stamp or none. `recover destroy` checks the same scope independently, so a command without them would be refused. For that command to work, the worker must still be an orphan after the refusal: `mj recover destroy` only accepts resources that no session record names. So adoption decides whether a live cut is needed and possible before it saves a session record. If the existing flow has already saved a provisional record for the handshake, the refusal removes it in the same failure path. This needs an orphaned worker from before the upgrade whose history was already pruned under the old recovery floor. Milestone 4 checked whether master can adopt such a worker by another route and found none: an adopted record has no checkpoint, and repair only retries from genesis.

A tracked session whose store was restored from a backup can sit behind the worker's acknowledgement, so its cursor is desynchronized. This is not new in kind: a backup older than the recovery-copy floor desynchronizes the same way. Ordinary reconnect reports it visibly and never cuts history silently. A follow-up slice after Milestone 4 lets an explicit `recover adopt` handle a disconnected, desynchronized tracked session with an expected-frontier compare-and-swap. It keeps the backup's transcript up to its cursor, adds a notice that history between the backup and the recovery is unavailable, and replaces operational state with the cut. It also adds the startup `user:<command id>` items the backup lacks. Every row from before the gap is closed for good. Stream chunks after F never extend a pre-gap row; a reply still streaming starts a new row after the notice, so text from both sides of the gap is never joined.

History up to F is not salvaged, including still-readable unacknowledged events. Because the cut is taken while the worker is quiescent, no turn or shell is running at F. A late event can still refer to an item from before F, for example a terminal tool status the harness reports after its turn has settled. Such an event is dropped with the rest of the unavailable history, as the projector already ignores updates for absent rows. The output of a turn that was running when adoption started is also part of the unavailable history. The agent still has all of it in its own context. There is no separate replay or overlay path and no adoption progress state. A retry after an interrupted install finds either no projection, and installs again, or a tracked one, and reattaches normally.

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
- End-to-end adoption tests in an isolated instance, with a worker whose journal is pruned:
  - With a turn running and a follow-up prompt queued, `mj recover adopt` waits while the turn runs, takes the cut before the follow-up starts, and the follow-up's events append after reattach.
  - Ctrl-C during the wait leaves the worker running and the orphan untracked, still listed by `mj recover scan`. A follow-up queued before adoption started is released: when the running turn then finishes, the follow-up starts with no further adoption or daemon attachment.
  - With the journal still reaching genesis, adoption is ordinary reattach with full history.
- Remove tests of removed behavior.

### Milestone 5: Move on the shared snapshot with direct transport

**Scope.** A cross-target Move becomes the following sequence:

1. Quiesce the source with the suspend barrier and stop all of its session processes exactly as suspend does (Milestone 2), before capture. Nothing then writes to the checkout or `.mj/keep/` during capture or the combined transfer. If the Move fails before destination readiness, the source checkout is intact and its agent restarts in place.
2. Capture with the same code as suspend: Milestone 1 for the repository plus the existing native capture. The result is one archive.
3. Transfer the archive straight to the destination.
4. Restore with the same code as resume, but with the queue left empty. Move's separate admission step decides what happens to the captured queue: it honors the user's `Discard`, and controls when `Start` runs. So the shared restore takes a queue policy, `Import` for resume and `Suppress` for Move, and a discarded queue never reaches the destination relay.
5. Wait for destination readiness, then retire the source.

**Ownership of a new cross-target Move** is a durable phase in the `MoveOperation`. It is decided by one owner, the daemon's store transaction, using the same classification today's recovery applies to old records, which is extended to cover the new phases.
- **Phases:** `Capturing` → `Transferring` → `Restoring` → `DestinationStarting` → `DestinationOwns` → `SourceRetired` → `QueueAdmitted`.
- **The commit point** is the single transaction that moves the operation to `DestinationOwns`. It also sets the session's placement to the destination, after the destination's worker reports ready and before the destination agent is allowed to take queued work.
- **Before that point the source owns the session.** Startup recovery and failure handling stop and dispose of any destination environment first, and only then restart the source agent in place. Two agents therefore never run at once: the source agent was stopped before capture, and is restarted only after the destination is torn down.
- **From that point on the destination owns it.** Recovery finishes forward: retire the source if it is not already gone, then complete the exactly-once queue admission with the pinned command IDs from Milestone 4 (the worker-owned pins that survive journal pruning), then settle. As the current executor does, the operation persists the destination relay's `store_id` before submitting any queued command. A retry is allowed only while the destination's relay still reports that same `store_id`. If the store was replaced, it holds neither the receipts nor the pins, so the queue is not replayed; the Move settles with an error that names the unadmitted commands for the user. A lost acknowledgement therefore never authorizes replay.

Add upgrade-free crash regressions for a daemon exit just before and just after the commit point.

The archive is not installed on the daemon as the session's checkpoint; the daemon copy remains whatever the last suspend produced. Delete `mj-worker/src/move_workspace.rs`, the untracked inventory, picker and exclusions in `mj-core/src/move_workspace.rs` and their UI, the daemon-side workspace staging under `data_dir()/moves`, the rsync legs in `transfer.rs`, and Move's separate handoff archive. A Move whose destination is the same target as its source is always in place. It never takes the current in-place path, which seals the relay, requires a handoff archive and runs `WorkerRootReset::InPlace`, stopping the worker and its whole process tree. Instead the worker stays up, keeping its relay, journal and identity, and restarts only the harness under the new launch configuration. That is the mechanism worker upgrades (`mj-controller/src/controller/worker_restart.rs`) and `SessionRestart::ReloadLogins` already use; the Milestone 4 investigation found no harness that needs an archive for a same-home restart. Processes the worker owns keep running. A process the harness itself takes down when it exits goes, as with any agent restart. A switch to a different harness or model service still needs the canonical conversation handed to the new harness, and that reads the durable canonical state directly, without an archive. A same-target Move is durable too, and its dispatch hold always has an owner. First, the daemon persists the Move's intent: its operation ID, the queue choice, the continuation's operation-derived ID, and the relay's current `store_id`. Then a single relay command, carrying that operation ID *and the expected `store_id`*, holds dispatch and records in the relay's durable state the IDs of the pending commands at that instant. The relay rejects the command if its own `store_id` differs, as part of that same decision. Every later queue mutation of the Move (discard, continuation submission, release) carries the expected `store_id` and is checked the same way inside the relay, so no gap between a check and a delivery can let an old Move act on a replacement store. No command can start between choosing what to discard and holding dispatch, and the hold belongs to the operation. The command is idempotent by operation ID: repeating it returns the IDs recorded the first time. After a daemon restart, a hold whose operation has a persisted intent is resumed by reissuing the command, which yields the recorded IDs, but only while the relay reports the intent's `store_id`. If the relay store was replaced, the original decision cannot be applied safely. Nothing is discarded or submitted, and the Move settles with a notice naming what it could not apply. A replacement store never carries this Move's hold: holds are recorded in the store that took them, under the operation ID, and the relay rejects any Move command whose expected `store_id` differs from its own. So there is nothing to release there, and dispatch in the replacement store was never held. A hold whose operation has no intent, or a settled one, is released. So a hold can never be left without an owner. The dispatch hold is part of the worker's durable relay state, so it survives a daemon disconnect and never silently releases. A daemon restart midway reapplies the recorded decision idempotently. `Discard` removes the recorded command IDs if they are still present, and the continuation is submitted under its ID only while the relay's `store_id` matches the recorded one. Then, in one transaction, the Move records the durable transition `DispatchReleased`, and only then tells the worker to release the hold. Recovery tells the cases apart. An intent before `DispatchReleased` redoes the idempotent steps and keeps the hold. An intent at `DispatchReleased` only makes sure the hold is gone (release is idempotent) and settles the Move, never reapplying the queue decision or resubmitting work. The user's queue choice still applies: before dispatch resumes on the restarted harness, `Discard` clears the pending commands in the live relay, and `Start` controls whether dispatch resumes immediately, exactly as today's in-place Move honors them. `Start` also submits the Move's saved continuation, the `ResumeGoal` or continuation prompt that automatic quota Moves rely on to resume interrupted work.
- **It goes first.** While dispatch is still held, the worker places the continuation at the head of the queue through a relay `SubmitFront` command, which is valid only while dispatch is held and gated on worker protocol support, so the interrupted work resumes before any pending prompts or configuration changes, as today's Move orders it.
- **It is submitted once.** It keeps the existing operation-derived command ID. The Move persists the relay's `store_id` before submitting, and retries only while the relay reports that same store, so a lost acknowledgement never resubmits it into a replaced store.
- **Workers from before this release** have neither `SubmitFront` nor a way to restart only the harness under a new launch configuration: `ReloadLogins` restarts the OpenCode bridge under its existing configuration, and `worker_restart.rs` replaces the whole worker, stopping its process tree. A process snapshot cannot make replacement safe, because an existing process can fork after any check, and the user asked that same-target Moves leave processes alone. So the Move itself never replaces a pre-release worker:
  - **A manual Move** is refused with a clear reason and a next step: "this session's worker predates in-place restarts. Restart the agent first; that restarts its worker and stops every process it is running. Then move." The existing "Restart agent" action (terminal UI and web, `POST /api/v1/sessions/{id}/restart`) is offered right there. On a pre-release worker, that restart replaces the worker through the existing atomic idle replacement, and its confirmation says the worker's processes will stop. The CLI has no restart command today, so this milestone adds `mj restart --session <id>`, mirroring the API route, and the refusal names it. Once restarted, the worker supports in-place moves, and the Move is repeated.
  - **An automatic quota Move** does nothing to a pre-release worker. The session keeps its quota notice and waits for the reset, as it does today when no move is possible, and the notice says why.

  After one replacement the worker supports both, and later same-target Moves keep processes running. Such a Move keeps the environment and checkout, swaps only the harness launch configuration, and never captures, transfers or stops writers. Some changes cannot be applied to a running environment, for example changed mounts, which need the container recreated. Those are not applied by the Move. The environment and its processes keep running, and the change is recorded on the session to take effect the next time its environment is created: at the next resume, or through the existing explicit `mj upgrade-container`. The Move's result says so: "<change> takes effect when this session's environment is next recreated". A same-target Move therefore never stops processes. Quota and login switches are the common case.

**Transport.** Implement `mj-controller/src/controller/move_session/transport.rs` with one function that copies an archive file from a source target to a destination target and verifies its SHA-256 there. Choose the route in this order:

1. **Same host.** When both targets resolve to the same machine (the same local host, or equal SSH host identity), run one host-local command that pipes the source's export into the destination's import through the engines' `exec -i` wrappers. No byte leaves the host.
2. **Direct between hosts.**
   1. Generate an ed25519 keypair on the daemon for this transfer. Copy the private key over the daemon's existing SSH connection into the operation's stage directory on the initiating host (the source for a push, the destination for a pull), with mode 0600. Record its path in the move's durable state before writing it, and delete it on every exit path and in startup reconciliation, like the other grants. That host's `ssh` client uses it with `-i` and `IdentitiesOnly=yes`. The key is useful only for one restricted command from one address.
   2. Give the initiating host a verified host key for its peer. The daemon already trusts both hosts' keys from its own connections, so it writes the peer's key, under a `HostKeyAlias` naming the peer, into a temporary known-hosts file in the same stage directory, recorded and cleaned up like the private key. The `ssh` client runs with `-o UserKnownHostsFile=<file> -o StrictHostKeyChecking=yes -o HostKeyAlias=<alias>`, so it never prompts and never trusts an unknown key.
   3. Append its public key to the destination's `~/.ssh/authorized_keys` with `restrict,from="<source address>",command="<worker receive command for this operation>"`. A Move sends ONE stream through that one command: the archive (length-prefixed), followed by the `.mj/keep/` tar. The receiving side verifies the archive's SHA-256, reports the keep tar's byte count, and succeeds only if both parts arrive complete. The key, the known-hosts file and the security-group rule live until that single stream has finished and both parts are verified, so the scratch never needs a second grant or a different route.
   4. When the host being connected to (the destination for a push, the source for a pull) is a persistent EC2 Podman host whose security group does not admit the connecting host, add an ingress rule for the connecting host's address as a /32. This follows the shared-grant rules below (use the EC2 helpers in `mj-cli/src/ec2.rs`, around lines 878–918, moved into a shared crate if needed).
   5. From the source host, run `ssh` to the destination with that key and stream the archive.
   6. If the source cannot reach the destination, try the reverse direction. Install the key on the source host, restricted to the worker *export* command for this operation's archive and to the destination's address, give the destination the source's host key as in step 2, and open the source's security group for the destination as in step 4. Then run `ssh` from the destination to the source and pull, piping into the destination's import. A pushed transfer's key is restricted to import, and a pulled transfer's key to export.
   7. Remove the key, the known-hosts file and the security-group rule on every exit path.

   Before installing either resource, persist its ownership in the move's durable state: the host and the key's comment (`mj-move-<operation-id>`), and the EC2 security group, rule and address.
   - **Keys** are per move (each has its own line and comment), so each move removes its own. Every edit to an `authorized_keys` file goes through one worker helper, for installation, removal and startup reconciliation alike. It takes an exclusive `flock` on `authorized_keys.mj-lock` beside the file, rewrites the file to a temporary name, and renames it into place. Concurrent Moves sharing an account then can never restore a revoked grant or erase a new one.
   - **A security-group rule can be shared:** AWS rejects a duplicate rule, so two concurrent moves between the same hosts need the same one. A SQLite transaction cannot roll back an AWS call, so the grant has durable states, and every AWS call happens between two committed state changes.
     - **Install:** in one transaction, the move records itself as a user of the rule. If the rule is `removing`, the move does not register: it retries after a short backoff until the removal has finished and the record is gone, then installs afresh. This serializes acquisition against an in-flight revoke. If the rule has no other users, the move marks it `installing` and becomes its installer. If another move is installing it, this move registers as a user but uses the rule only once the record says `installed`, waiting with backoff. If the installer's AWS call reports failure, the record is not dropped on that evidence alone, because the rule may have been created with the response lost. The installer queries AWS for a rule with Mjolnir's description. If one exists, the state becomes `installed`. If AWS reports none yet, the outcome is still ambiguous, because AWS may create the rule late. So the record moves to `removing` rather than being dropped: an idempotent revoke is attempted, and a waiting move retries as installer under a new record. Whenever reconciliation, whether live or at startup, resolves an `installing` rule to existing, it checks the user count in the same transaction. With no users left, because the move took the daemon stream and released its registration, it marks the rule `removing` and revokes it, so a rule that finished installing after its last user left is never left open. If the query itself fails, the record stays `installing`. A daemon background task retries the query with backoff until AWS answers, as does startup reconciliation. A move waiting on an unresolved grant does not wait indefinitely: it takes the daemon-stream route, reported as such, and releases its registration. The record's state is the only thing any move consults, and every change to it happens in one transaction, so acquisition, installation and removal are serialized. Mjolnir then authorizes the rule with a description naming Mjolnir and this daemon's instance (`mj-move-grant:<daemon instance ID>`, the stable instance identifier recorded in the store), and in a second transaction marks it `installed`.
     - **Pre-existing rules:** if AWS reports a duplicate, the rule already existed. If its description is not Mjolnir's, it belongs to the user: record it as `external`, use it, and never revoke it.
     - **Release:** in one transaction, the move removes itself as a user. If it was the last user of a Mjolnir-created rule, it marks the rule `removing`. It then revokes the rule and, in a second transaction, deletes the record.
     - **Reconciliation does not depend on records alone.** At startup, and at every grant operation on a security group, the daemon lists that group's rules carrying this daemon instance's description (`mj-move-grant:<daemon instance ID>`; rules of other Mjolnir instances sharing the group are never touched). It handles any with no live user record as the release path does: in one transaction it checks for users and claims the rule as `removing`, and only then revokes. A Move trying to acquire that rule meets the same gate and waits for the removal, so reconciliation never revokes a rule out from under a Move that has just adopted it. A rule that AWS created after its record was released is still removed.
     - **Startup reconciliation** queries AWS for each recorded rule. An `installing` rule becomes `installed` if a rule with this instance's description exists. Otherwise it is treated like the live path's ambiguous case: it moves to `removing`, is never dropped on a single absent check, and stays under the background task. That task retries with exponential backoff until either a revoke succeeds, or AWS reports the rule absent on two checks separated by at least one backoff interval. A `removing` rule is revoked again if present and dropped only on that same evidence. Users whose moves are finished, or that resumed through another route, are released as above.
   - **At startup,** the daemon releases the records of moves that are finished, or that resumed through a different route, by the same rule, deleting key lines by comment.

   This follows the `AGENTS.md` rule that daemon background work must be resumable from durable state.
3. **Daemon stream.** When neither direction connects, run `ssh source export | ssh destination import` from the daemon without writing to its disk. Report the route in the move's progress and in its completion notice.

Record which route each move used in the move's durable state, so a daemon restart resumes the same transfer or restarts it cleanly.

**Keep directory on Move.** The `.mj/keep/` tar follows the archive in the same stream, under the same grant and route (see transport step 3). Export and extraction apply the same containment checks: refuse symlinked ancestors, and extract only into the verified `.mj/keep/<session-id>/` with no path escaping it. leaving out the paths named by `--skip`. Skipped paths are deleted when a managed source is retired; in a raw checkout they stay on disk. `--dry-run`, the CLI confirmation and the Move dialogs say this, listing each skipped path with its size, and passing `--skip` is the consent. Unpack it into the destination checkout's `.mj/keep/` before the agent starts. The keep transfer must succeed at both ends: the source `tar` and the destination importer must both exit 0, and the importer must report the byte count the source sent. Only then may destination readiness allow the managed source to be retired. Otherwise the move fails and the source, with its scratch, stays intact. A `tar` that skips an unreadable file still produces a valid archive, so the exit status is what catches it. The keep directory never enters the archive, so it is never stored on the daemon. When the route is the daemon stream it does pass through the daemon's network, and the move's progress shows its size so the user can cancel and rerun with `--skip`.

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
- It skips any session whose archive is also referenced by durable recovery state, such as an unresolved legacy Move or restart intent (the same references `mj-controller/src/controller/checkpoint/archives.rs` preserves at startup), and reports it as skipped with the reason, so recovery never points at a deleted file.
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

Capture changes nothing in a raw checkout except the objects that an objects-only fetch of missing advertised tips writes and the unreachable objects that `git stash create` and the stash replacements write; Git's garbage collection removes those. It writes no refs, so a crash leaves nothing to clean up. In a managed checkout, the operation abort is the one destructive step; the warning names it, and it happens only after the user consents.

Suspend is resumable from durable intent as today. A retried suspend recaptures, and the new archive replaces the old one only after verified installation. Restore runs in the provisioner's private staging checkout and publishes it only on success, so a retry before publication starts with a new staging checkout. After publication, a resume recognizes its own checkout by the publication receipt bound to its environment ID and finishes repair there; any other occupant is foreign and refused, never adopted or deleted. Move transfers use per-operation names and verify the hash at the destination before anything on the source is retired. The temporary key and security-group rule are recorded in the move's durable state before they are installed, and removed on every exit path. After a daemon crash, startup reconciliation removes any that are still recorded. Until then, a leftover key is restricted to one import command and the source's address.


## Artifacts and Notes

The Git commands at the heart of capture and restore, shown for one managed checkout with HEAD on `feature`:

    git ls-remote origin                          # objects-only fetch of missing advertised tips follows
    git for-each-ref --format='%(refname) %(objectname)'   # pin the cut
    git stash create                              # -> <work> or empty
    git stash list --format=%H%x00%gs             # stash entries to keep
    printf '%s\n' <feature> <v1> <work> <stash...> --not <origin refs and tags> \
        | git pack-objects --revs snap        # writes snap-<hash>.pack and .idx

    # restore runs in the provisioner's staging clone (create_managed_clone), origin already set
    cp ../snap-<hash>.pack .git/objects/pack/ && git index-pack .git/objects/pack/snap-<hash>.pack
    git cat-file --batch-check < prerequisites    # missing ones: one objects-only fetch from origin by SHA
    git update-ref refs/heads/feature <head-commit>
    git stash store -m keepme <stash-entry-commit>
    git symbolic-ref HEAD refs/heads/feature && git reset --hard <head-commit>
    git stash apply --index <work>
    git rev-list --objects --missing=print <every saved tip> | grep '^?' && exit 1   # full graph present
    git submodule update --init -- <recorded paths>   # failure is a notice
    git worktree repair <nested paths>             # after the staging rename


## Interfaces and Dependencies

No new external dependencies. Git 2.30 or newer (Debian 11's version) is the minimum on targets. Every command in this plan must work there: no `rev-list --disk-usage` (2.31), and no reliance on `rev-list --missing=print` reporting missing starting tips (2.45). The existing harness images ship newer Git.

`mj-checkpoint/src/snapshot.rs` exposes the following (Milestone 1 as built; Milestone 1's Plan of Work describes the behavior):

    pub struct WorkspaceSnapshot { pub metadata: WorkspaceSnapshotMetadata, pub pack: Option<PathBuf> }
        // metadata: object format, origin URL (redacted) and upstreams naming origin, saved refs,
        // HEADs, stash list, working-state commits, index gitlinks, initialized submodules,
        // nested worktrees and locks, pack prerequisites, state flags and notices
    pub enum CheckoutOwnership { Managed, Raw { launch: Option<LaunchRecord> } }
    pub struct GitOperation { pub checkout: CheckoutKey, pub kind: GitOperationKind }   // = ShownOperation
    pub enum UntrackedConsent { None, Any, Shown(Vec<UntrackedItem>) }
    pub enum OperationConsent { None, Any, Shown(Vec<ShownOperation>) }
    pub struct CaptureConsent { pub untracked: UntrackedConsent, pub operations: OperationConsent }
    pub enum ExclusionSource { Origin, Commits(Vec<String>) }   // Commits = a history cutoff's base
    pub struct RepositoryInput { pub id, pub checkout, pub ownership, pub origin_override, pub pack_path, pub exclusions, pub keep_dir, .. }

    pub fn capture_session_snapshot(runner, repositories: &[RepositoryInput], consent: &SessionCaptureConsent) -> Result<SessionSnapshot>;
        // precondition: the caller has stopped every writer to managed checkouts (Milestone 2)
    pub fn capture_workspace_snapshot(runner, checkout, ownership, pack_path, consent: &CaptureConsent, origin_override) -> Result<WorkspaceSnapshot>;
    pub fn session_inventory(runner, repositories) -> Result<BTreeMap<String, WorkspaceInventory>>;
    pub fn workspace_inventory(runner, checkout, ownership) -> Result<WorkspaceInventory>;   // typed SnapshotRefused blockers with remedies
    pub fn require_managed_snapshot_without_consent(runner, repository) -> Result<()>;      // read-only preflight
    pub fn restore_workspace_snapshot(runner, snapshot, staging_clone: &Path) -> Result<Vec<String>>;   // notices the caller must surface
    pub fn repair_workspace_snapshot(runner, snapshot, published_clone: &Path) -> Result<()>;   // worktree repair after the staging rename
    pub fn record_launch(runner, checkout, session_id) -> Result<()>;

There is no `RestoreBase`: restore always applies to the clone the provisioner (`create_managed_clone`) made during this resume attempt.

In `mj-checkpoint/src/checkpoint.rs`, add `CheckpointRepositoryCapture::OriginDelta { ownership: CheckoutOwnership }` and raise `CHECKPOINT_EXPORT_PROTOCOL_VERSION` to 3 for specs that use it.

The suspend request gains `discard_untracked` (`"any"` or `{"shown": [items]}`, where each item is `{"repository": ..., "path": ..., "identity": ...}` (untracked files, and `.mj/keep/` files with `"keep": true`) or `{"repository": ..., "stash": <commit>}`; absent for none. The dialog echoes back exactly the items, with their identities, that the preflight returned and it displayed.), `abort_operations` (`"any"` or `{"shown": [{"repository", "path", "kind"}]}`) and `without_checkpoint: bool` and loses `acknowledge_unpublished_work` and `ignore_nested_repositories`. This applies to the HTTP body (`mj-server/src/server/api/turns.rs`), `SessionLifecycleRequest::Suspend` (`mj-client/src/daemon.rs`), and the CLI flags `--discard-untracked`, `--abort-operation` and `--without-checkpoint`. The destroy request loses `delete_branch`. Both suspend and Move accept `dry_run: bool` (CLI `--dry-run`), and Move accepts `skip: Vec<PathBuf>` (CLI `--skip <path>`, repeatable, relative to `.mj/keep/`). The new route `GET /api/v1/sessions/{session_id}/suspend-preflight` returns:

    {
      "untracked": [{"repository": ..., "path": ..., "bytes": ..., "identity": ...}],   // identity = opaque file identity token the request echoes back
      "untracked_stashes": [{"repository": ..., "commit": ..., "message": ...}],   // `git stash -u`/`-a` payloads that will not be kept
      "operations": [{"repository": ..., "path": ..., "kind": ...}],
      "raw": bool,
      "keep": [{"path": ..., "bytes": ..., "identity": ...}],   // files in a managed checkout's .mj/keep/<session-id>/ that suspend will not keep (empty for raw checkouts and for Move)
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

Revision note (2026-11-18, review 15): a recorded group with live but unmarked members blocks capture and teardown with a refusal that names the processes; submodule untracked files go through normal consent.

Revision note (2026-11-18, review 16): handoff admission is held only for bounded control transitions around the worker-owned freeze; tips are checked before the graph walk (Git before 2.45); the estimate is the real pack size and works on Git 2.30; inventory uses capture's stash selection; stash preflight entries carry the repository.

Revision note (2026-11-18, review 17):
- The freeze is per process: recorded group members plus every marked process, including `setsid`-detached ones. On Linux it uses pidfds so verification and signal refer to the same process.
- A Move sends the archive and keep tar as one stream under one grant.
- The submodule check uses the remote's advertised refs.
- Internal stash commands use the existing internal Git identity.
- Purge skips archives that recovery still references.
- Legacy worktree sessions whose base exists only in the original source repository resume only on their original target.

Revision note (2026-11-18, review 18):
- The freeze set includes the worker's subreaped descendants, and any untracked process with files open in a managed checkout blocks capture.
- Rollback leaves already-paused jobs paused.
- `--without-checkpoint` verifies the prior archive before offering it.
- A grant that resolves to installed with no users is revoked.
- Fetch is used only when the Git directory is the checkout's own.
- The dry-run route preview is read-only.

Revision note (2026-11-18, user decision after review 18): the user chose to stop the session's processes before the final capture, for suspend and off-target Moves only, instead of pausing with SIGSTOP and resuming on refusal. That removes freeze records, resume decisions, paused-job tracking and group-ownership recovery. A refused or failed capture now leaves the checkout intact and restarts the agent in place, without its background processes. Same-target Moves are always in place and stop nothing; "if something writes into the checkout it's no big deal" there, since nothing is removed. The user prefers the simpler design.

Revision note (2026-11-18, review 19):
- Fetch requires managed ownership as well as a Git directory inside the checkout.
- Restore clears clone-created branches before installing saved refs.
- The worker sets `PR_SET_CHILD_SUBREAPER` at startup.
- An older worker is replaced only after the read-only preflight passes.
- A same-target Move that must recreate the container says it will stop background processes.
- Overrides recorded: unrelated external writers, macOS PID reuse, and env-cleared processes that escape the tree.

Revision note (2026-11-18, review 20): same-target Moves defer changes that need container recreation, such as mounts, to the next environment creation instead of recreating the container, so they never stop processes (the user's decision).

Revision note (2026-11-18, review 21):
- Termination repeats until no attributable process remains.
- The worker reaps adopted orphans.
- A failed worker swap recovers a worker in place, or surfaces that none is running.
- `.mj/keep/` consent covers only the files shown.
- Submodule checks use advertised refs.
- Clone tags are removed only when a saved tag replaces them.
- Same-target Moves defer environment changes.

Revision note (2026-11-18, review 22):
- Same-target Moves restart only the harness in the same worker, with no relay seal, archive or root reset.
- Linux quiescence is proven by the subreaper having no children.
- Cancelled stops defer the agent restart until the old harness exits.
- Consent binds untracked paths to file identity.
- ACP sends absent consent fields.

Revision note (2026-11-18, review 23): the preflight and request carry file identities and individual `.mj/keep/` entries, so dialogs can send exactly the consent capture checks. Advertised exclusions are limited to heads and tags.

Revision note (2026-11-18, review 24):
- Orphan reaping is serialized with child launch by the guest reaper's lock.
- The origin requirement is enforced at managed-session creation, with an in-surface alternative.
- The preflight and request carry file identities and keep entries.
- Override recorded: a harness change on a same-target Move takes the old harness's process group with it, as any agent restart does.

Revision note (2026-11-18, review 25):
- New cross-target Moves get durable phases with a single ownership commit point and defined recovery on each side of it.
- Capture refuses unregistered nested repositories without consent, and tracked keep files.
- Submodule availability uses heads and tags only.
- Raw sessions capture all nested worktrees.
- Legacy linked worktrees keep only their own refs.
- Managed restores keep deleted tags deleted.

Revision note (2026-11-18, review 26):
- The managed fetch can prune only remote-tracking refs.
- The capture steps carry the decisions on legacy-worktree ref scope, raw nested worktrees and tracked keep files.
- Journal retention specifies Move receipt pins and their lifetime.

Revision note (2026-11-18, review 27):
- Unregistered nested repositories are in the operative consent steps.
- `authorized_keys` edits are serialized per file by one locked helper.
- Ambiguous ingress installs move to `removing`, and reconciliation revokes any orphaned Mjolnir-described rule.
- Raw checkouts behind origin, and submodules whose remote advanced, fetch the advertised objects without touching refs.

Revision note (2026-11-18, review 28):
- Move queue retries are bound to the destination relay's `store_id`.
- Orphan ingress reconciliation claims `removing` in the user-check transaction.
- Uninitialized submodules are skipped when unchanged, and checked read-only when their pointer changed.

Revision note (2026-11-18, review 29):
- Sub-agents are stopped before the parent's quiescence check.
- Workspace deletion releases admission while waiting.
- Legacy archives keep the working files of unmerged paths as unstaged edits.
- Ingress descriptions and reconciliation are scoped per daemon instance.
- Startup keeps ambiguous installs under backoff reconciliation.
- Move restores with a suppressed queue.
- Raw launch comparisons and deleted refs use the local-ref allowlist.

Revision note (2026-11-18, review 30):
- The submodule guard reruns after aborts.
- Relative local origin, push and submodule URLs are made absolute.
- Suspends accepted by the previous release finish along its own finish-forward path.
- Same-target Moves apply Start and Discard before dispatch resumes.
- Declined: the residual pre-subreaper detached-process race.

Revision note (2026-11-18, review 31):
- Legacy suspend intents finish through `recover_interrupted_close_managed` in every relay state.
- Relative submodule URLs resolve against the superproject's remote.
- Same-target Moves submit the saved quota continuation under its operation-derived ID.

Revision note (2026-11-18, review 32):
- Legacy intents with an unsealed relay continue on the new path without consent.
- Sealed legacy intents keep the old completion path, together with the writers it needs, marked for later removal.
- The quota continuation is submitted at the head of the queue, and only into the same relay store.
- Configured relative submodule URLs are made absolute.

Revision note (2026-11-18, review 33): older workers are not replaced for a same-target Move; their continuation goes to the tail rather than having background processes killed.

Revision note (2026-11-18, review 34):
- Sealed legacy intents finish from their already installed checkpoint, so no legacy export or capture code is kept.
- Origin and push URLs are Git's effective, rewritten destinations.
- Older workers keep their processes on same-target Moves.
- Native-use evidence joins the Milestone 2 archive bump.

Revision note (2026-11-18, review 35):
- Same-target Moves on pre-release workers replace the worker once, with a warning.
- Legacy sub-agent close records keep their archive-free route.
- Old archives keep the transcript predicate as their legacy reader.
- Configured URLs are stored and queried so that rewrites apply exactly once.
- Declined: reconciling late writes for sealed legacy suspends, which finish with the previous release's semantics.

Revision note (2026-11-18, review 36):
- Pre-release workers are replaced for a same-target Move only when no background process is running; otherwise a manual Move asks, and an automatic one waits.
- Same-target Moves persist their queue decision and dispatch hold.
- Matched URL rewrite rules travel with the snapshot.
- Rewrite-matched relative URLs are stored unchanged.

Revision note (2026-11-18, review 37):
- The pre-release worker process check runs inside the atomic idle reservation.
- URL rewrites are pinned with a whole-URL rule from the configured URL to the effective URL, so destination rules cannot override them.

Revision note (2026-11-18, review 38): pre-release workers are never replaced on the strength of a process snapshot. A manual same-target Move asks for blanket consent, and an automatic quota Move waits.

Revision note (2026-11-18, review 39): restore initializes and fetches instead of cloning, because `git clone` rejects rewritten relative URLs. This also removes the clone-created-branch cleanup.

Revision note (2026-11-18, review 40):
- The post-abort recheck reruns the whole inventory.
- A relative effective rewrite endpoint is made absolute.
- Same-target Moves never replace pre-release workers: a manual Move is refused and offers the existing Restart agent action, and an automatic one waits.
- Holding dispatch and choosing the discard set happen in one relay step.

Revision note (2026-11-18, review 41):
- The in-place dispatch hold is taken under a persisted operation ID and is recoverable.
- Restore initializes with the source's object format.
- Push URLs keep the explicit-or-fallback distinction.
- Submodules are also initialized by init and fetch.
- Declined: a destination rule for exactly the whole configured URL.

Revision note (2026-11-18, review 42):
- Restore always pins fetch and push URLs, including identity pins.
- A same-target Move records `DispatchReleased` before releasing its hold.
- `object_format` is a snapshot field.

Revision note (2026-11-18, review 43): the snapshot interface stores configured and effective URLs for origin and push, distinguishing explicit push URLs from the fallback, so restore can build its pins.

Revision note (2026-11-18, review 44): the same-target discard set is bound to the relay `store_id` persisted with the intent. Declined again after the baseline reset: destination rules matching the whole configured URL.

Revision note (2026-11-18, review 45): restore writes `pushurl` only for explicit push URLs, and same-target Move commands carry the expected `store_id`, which the relay checks inside each decision.

Revision note (2026-11-18, review 46): push destinations are restored as the source's effective push list, written as identity-pinned explicit push URLs. A replacement relay store never holds an old Move's dispatch.

Revision note (2026-11-18, review 47): push destinations are recorded as configured/effective pairs, so push pins never conflict with the fetch pin.

Revision note (2026-11-18, review 48): push destinations are restored as unique synthetic aliases pinned to their effective URLs, which removes any fetch/push pin collision.

Revision note (2026-11-18, review 49): a default push setup restores with no `pushurl`, so pushes keep following origin.

Revision note (2026-11-18, review 50):
- `push_is_default` is recorded explicitly, and its fallback is pinned with a whole-URL `pushInsteadOf`.
- Synthetic push aliases avoid every existing exact rewrite prefix.

Revision note (2026-11-18, review 51): when a destination rule's prefix is exactly the configured URL, restore uses synthetic fetch and push aliases instead of A, which closes the equal-length tie without an override.

Revision note (2026-11-18, review 52):
- The publication assessor compares resolved push destinations.
- Exact-prefix destination rules switch restore to synthetic aliases.
- Declined: recreating `pushInsteadOf` fallback recomputation after `set-url` for non-default push setups.

Revision note (2026-11-18, review 53):
- Remote URLs are restored as synthetic aliases for fetch and push, pinned to effective destinations, so no pin keyed on a real URL can capture a later URL or collide.
- Snapshots store effective destinations only.
- The publication assessor compares in each archive's own representation and queries by remote name.

Revision note (2026-11-18, review 54):
- The fetch alias also gets a `pushInsteadOf` pin.
- Publication is verified at each resolved push destination through a throwaway alias.

Revision note (2026-11-18, review 55):
- The fetch alias keeps B's path and pins at B's root, so relative submodule URLs resolve correctly after restore.
- Live push destinations are normalized before comparison.

Revision note (2026-11-18, review 56):
- Alias numbers avoid any existing rule extending into the alias namespace.
- Publication query aliases follow the same rule.
- `push_is_default` is defined by behavior: no explicit pushurl, and push equals fetch.

Revision note (2026-11-18, review 57): supersedes the URL pinning and alias scheme. Remotes are restored as configured, with the source checkout's repository-local rewrite rules, and otherwise behave like any clone on the destination host. See the Decision Log.

Revision note (2026-11-18, review 58): rewrite-rule keys are redacted, and initialized submodules carry their own local rules.

Revision note (2026-11-18, review 59):
- Rule capture includes repository-local includes, redacts host-less prefixes, and absolutizes relative bases.
- The stale pin step is removed from the restore sequence.
- The publication assessor queries by remote name.

Revision note (2026-11-18, review 60): publication queries every resolved push destination through a per-process throwaway alias.

Revision note (2026-11-18, review 61):
- Rewrite rules carrying credentials are omitted and reported rather than redacted, which could broaden them.
- Submodules are brought up by init and fetch with their rules in place, never by `git submodule update --init`.
- Relative push URLs are made absolute.
- Publication checks every resolved push destination.

Revision note (2026-11-18, review 62): submodule bootstrap writes the superproject's rules too, and registers the submodule in its parent.

Revision note (2026-11-18, review 63):
- SSH username-only rules travel.
- Relative submodule URLs follow the root's rule.
- Submodule fetches include tags.
- Registration happens at every depth.
- Publication query rules go through a temporary config file, so `=` in a path works.

Revision note (2026-11-18, review 64): superproject rules are applied only to submodule bootstrap fetches, never written into the submodule's config.

Revision note (2026-11-18, review 65):
- Submodule bootstrap uses only the submodule's own rules.
- Relative URLs are kept only when an `insteadOf` rule matches.
- Worktree-scoped rules are captured.
- The publication check fetches advertised tips through the same alias.

Revision note (2026-11-18, review 66):
- All relative local paths, in URLs and in rule prefixes and bases, are made absolute consistently.
- Submodules get a detached HEAD before registration, with no `git submodule update`.
- Worktree rules keep their scope in the snapshot.
- Legacy publication URLs are compared in their recorded form.
- Publication tip fetches leave FETCH_HEAD untouched.

Revision note (2026-11-18, review 67):
- Submodule checks and bootstrap both use the submodule's own origin.
- Submodules get `origin/HEAD`.
- Publication fetches can never prune local tags.
- A retired stopped session is assessed against its snapshot's recorded push configuration.
- Declined: rewrite rules keyed on bare relative fragments, mixed relative and absolute literal matches, and branch-conditional includes.

Revision note (2026-11-18, review 68): the snapshot records the resolved push destinations, which the publication assessor uses for retired sessions.

Revision note (2026-11-18, review 69):
- Submodule bootstrap restores push URLs and honors the destination's transport policy.
- Publication fetches never recurse into submodules.
- Retired sessions are assessed against the resolved push destinations recorded at capture.

Revision note (2026-11-18, review 70):
- The submodule availability check queries by remote name under restore's transport policy.
- Retired-session assessment runs in a temporary bare repository.
- Legacy checkpoints keep the source-based refresh.
- `set-head` is best-effort.

Revision note (2026-11-18, review 71):
- Resolved push destinations are redacted, and absent when there is no origin or they are unsafe to store; the assessor treats absence as unverified.
- A submodule policy refusal is distinguished from unreachability.

Revision note (2026-11-18, review 72):
- The publication scratch repository uses the snapshot's object format.
- Repository-local transport policy travels with each repository's rewrite rules.

Revision note (2026-11-18, review 73):
- Retired-session publication is judged independently per destination by ancestry.
- The submodule check uses the named origin throughout, including stale refs.

Revision note (2026-11-18, review 74): the plan states which host's transport policy capture checks, and how Move ordering and resume failure cover a stricter destination.

Revision note (2026-11-18, review 75):
- Offline submodule checks follow origin's fetch refspecs.
- Retired publication probes run on the session's target and count only heads and tags.
- The plan states which host's transport policy capture checks, and how Move ordering and resume on the original target cover a stricter destination.

Revision note (2026-11-18, review 76): push evidence is a required enum, so a legacy archive (recognized by format) is never confused with unavailable evidence.

Revision note (2026-11-18, review 77):
- Saved tags and notes need exact matches at each destination.
- Publication probes run outside handoff admission.
- Container-local push destinations are `Unavailable`.
- Destination transport denials win over copied allowances.
- Ambiguous offline tracking refs are not evidence.
- `push_evidence` is a required enum.

Revision note (2026-11-18, review 78):
- Per-checkout `has_uncommitted_state` keeps such checkpoints out of automatic archival.
- Push evidence is recorded per checkout.
- The publication probe is a cancellable supervised subprocess.

Revision note (2026-11-18, review 79):
- A stash keeps a checkpoint unpublished.
- Branches are checked against the destinations of the checkout that has them checked out.
- Capture's transport check enforces the policy restore will have.
- Declined: copying credential helpers into retired-session probes, which fails safe.

Revision note (2026-11-18, review 80):
- An unstaged submodule revision counts as uncommitted state.
- Publication probes have a target-side owner that stops them before cleanup.
- Declined: tying offline submodule refs to the URL that populated them, the same accepted risk as the parent's offline path.

Revision note (2026-11-18, review 81): capture's transport check mirrors the restored configuration key for key and lets Git apply its precedence, because a host transport-specific `protocol.<name>.allow` outranks a local general `protocol.allow`. A skipped key is forced to `never`, since a host may deny a transport through its general `protocol.allow` and have no same-named key to copy.

Revision note (2026-11-18, review 82): capture's origin queries run in a scratch repository configured by restore's own code, so Git resolves rules and transport precedence from one configuration at both ends. This replaces the per-key `-c` emulation, which could not see settings restore drops, such as worktree-scoped transport keys. Each query uses its own repository's restore environment: `GIT_PROTOCOL_FROM_USER=0` for submodules only.

Revision note (2026-11-18, review 83):
- Each scratch query uses its own repository's restore environment: only submodules set `GIT_PROTOCOL_FROM_USER=0`.
- Capture's downloads, ancestry checks and pack also run in the scratch repository, which borrows the source's objects through alternates, so discovery and download share one endpoint. Capture no longer fetches into any checkout, including independent managed clones.
- Accepted: host conditional includes keyed on the repository path may resolve differently for the scratch repository.

Revision note (2026-11-18, review 84): the scratch repository copies a shallow source's `shallow` file, because alternates expose objects but not shallow boundaries.

Revision note (2026-11-18, review 85):
- Capture step 1's submodule availability check now defers to the scratch-repository procedure, leaving one procedure.
- The scratch repository mirrors a partial clone's promisor settings, so a needed blob the source never fetched is fetched on demand from the same origin.

Revision note (2026-11-18): adoption takes a current operational cut plus a visible history-unavailable notice, replacing a tail-replay bootstrap. That bootstrap drew new review findings each round (overlay conflicts, quadratic replay memory, a proposed progress table) only to salvage the unacknowledged tail of a manual recovery command. Startup command receipts are pinned for the session's lifetime.

Revision note (2026-11-18, user ruling): removed the transport-policy copying, the restricted submodule fetch environment and capture's scratch repository. Capture queries origin from the source repository, restore runs ordinary fetches, and resume failing on a host that cannot reach origin is accepted. Revision notes for reviews 72–85 describe the superseded mechanism.

Revision note (2026-11-18): uninitialized submodules are checked by exact tips against `.gitmodules` only. This removes the leftover admin-directory check added in 0fedb7f11, because that directory's origin can differ from the URL restore will initialize from.

Revision note (2026-11-18, user decision): adoption cuts only between turns. This removes the mid-turn handling (active-turn identity, the execution and start-clock mapping, steering attribution, shell rows, and interrupted-turn settling) that drew review findings round after round.

Revision note (2026-11-18, user decision): `mj recover adopt --interrupt` is dropped. Adoption only waits for quiescence, and a worker that never goes quiescent can only be given up with `mj recover destroy`. Live harness terminals join the quiescence check. Branch ownership among forced same-branch worktrees covers siblings, not only the primary.

Revision note (2026-11-19): leftover submodule admin directories and unrepresented worktrees now block a managed teardown by their presence, without content inspection, replacing per-kind checks of HEADs, refs, stashes, indexes and operation state. Shallow managed checkouts refuse outright, replacing the origin-coverage test. Both choices refuse in strictly more cases than the checks they replace. The user was away when asked, so the plan author took the conservative option.
