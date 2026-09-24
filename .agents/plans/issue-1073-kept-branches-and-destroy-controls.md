# Give managed sessions independent Git clones and publication-aware recovery

This ExecPlan is maintained under `.agents/PLANS.md`. It implements issue #1073 with the revised workspace model agreed on 2026-09-23. The current `mj/<session-id>` branch cleanup proposal is superseded. A session owns its execution environment and recovery copy; Git branches follow the user's development workflow. This plan is self-contained and must be updated as work proceeds.

## Purpose / Big Picture


A new isolated raw session should open on the selected branch in its own repository, with ordinary `git commit` and `git push` behavior. Two agents can independently start on `master`, and neither creates a session-named branch in the user's source repository. Suspension verifies and saves work before deleting that checkout. The Open/Resume dialog marks work without a verified published copy, suspension asks for confirmation when such work exists, and automatic aging retains any recovery copy that still holds unpublished work.

The daemon is mj's controller process; it owns session records, lifecycle changes, the HTTP API, and the database. Workers own running agent turns. Git and network checks are daemon-owned bounded background operations, never TUI or web render-loop work. A checkpoint is mj's verified recovery archive. "Published" means all saved committed work has a confirmed copy on a configured remote, independent of whether a PR has merged. Uncommitted changes and stashes prevent automatic archival even when their base commits are published.

## Progress


- [x] (2026-09-23) #1073 self-assigned with `agent-in-progress`; old retained-branch plan superseded after design discussion.
- [x] (2026-09-23) Inspected raw worktree and clone provisioning, checkpoint history, resume, aging, lost-target cleanup, and TUI/web suspend paths.
- [x] (2026-09-23) Proved in a disposable repository that local hardlinked clones share original objects and advance `master` independently.
- [x] (2026-09-23) New raw local/SSH sessions use independent clones; old worktree records retain their restore path. Network-backed clones start on their default or selected branch.
- [x] (2026-09-23) Checkpoints save clone refs, detached HEAD, the full stash stack, and file changes; restoration rebuilds them before resuming.
- [x] (2026-09-23) Migration 47 stores checkpoint-bound publication evidence; age cleanup requires clean published evidence and lost cleanup retains owned clone storage.
- [x] (2026-09-23) Open/Resume marks unpublished or unknown work; TUI/web confirm suspension, while CLI/API require explicit acknowledgment.
- [ ] Validate with isolated named instances and automated tests, commit coherent checkpoints, push to origin/master, and close #1073.

## Surprises & Discoveries


The current `ManagedWorktree` shape in `mj-core/src/state.rs` validates a branch named `mj/<session-id>` and a path under `.mj/worktrees/<session-id>`. `mj-controller/src/controller/worktree.rs` creates it from HEAD, stores the original commit, and keeps that branch when suspension removes the worktree. `mj-controller/src/controller/resume.rs` restores the retained branch and overlays the checkpoint. `mj-checkpoint/src/archive/git.rs` bundles only commits reachable from HEAD beyond `origin` refs; secondary unpublished branches and the stash stack would be lost if an entire clone were deleted under that rule.

`mj-controller/src/daemon/resume.rs` has an optional hourly `archive_after_days` path that removes the session record and checkpoint after SessionWiki has indexed the conversation. Its old Git safety argument is the surviving source-repository branch. A missing execution target without a checkpoint is separately treated as Lost and discarded; it may still have an owned checkout worth retaining. `mj-controller/src/database/baseline.sql` stores checkpoint path/hash/time/frontier, but no publication verdict. `mj-tui/src/resume.rs::ResumeRow` has no publication fact. Idle TUI suspension currently dispatches without confirmation; the web surface asks a generic confirmation; the CLI directly submits the command.

The local proof used `git clone --local` twice on one source. Three object files had link count 3, and committing in clone A advanced only clone A's master while source and clone B stayed on the original commit. The proof directory was removed afterward.

## Decision Log


- Decision: Use independent clones for new isolated raw sessions; leave existing worktree sessions in the legacy path. Rationale: clones allow independent local `master` branches and ordinary pushes without session-named refs. Date: 2026-09-23, user and Codex.
- Decision: Start on an explicitly selected branch, otherwise the remote default; if no remote exists, use the source checkout's named branch. Preserve an explicit launch-base commit separately. Rationale: branch identity should come from the development workflow. Date: 2026-09-23, user and Codex.
- Decision: Suspension verifies a checkpoint and then removes the checkout; recovery uses the recorded source plus saved work. Rationale: this preserves the existing target-release behavior while avoiding a full-history archive per checkpoint. Date: 2026-09-23, user and Codex.
- Decision: A pushed branch qualifies for automatic archiving even without merge. Any uncommitted work, stash, unpublished commit, or failed publication check blocks it. Rationale: a published copy is sufficient, and uncertainty must not erase the only copy. Date: 2026-09-23, user and Codex.
- Decision: Open/Resume shows an unpublished-work icon; suspension with unpublished or unknown publication state requires confirmation across control surfaces. Rationale: the user wants visible work state and explicit acknowledgment before releasing its checkout. Date: 2026-09-23, user and Codex.

## Outcomes & Retrospective


The independent-clone test starts two sessions on the same branch, confirms independent commits, and observes a normal non-fast-forward push rejection from the second clone. The archive test restores a secondary branch, annotated tag, two named stashes, dirty tracked work, and a payload above the pipe buffer size. The publication test distinguishes an unpushed commit, a pushed unmerged commit, uncommitted files, a stash, and a separate push URL. Migration 47 preserves older rows and makes new ownership state inaccessible to older writers. The full `cargo test` and `cargo clippy --all-targets -- -D warnings` runs passed on the dev profile. A named isolated CLI smoke check and delivery are pending.

## Context and Orientation


`mj-core/src/state.rs` defines durable session records and the old managed-worktree shape. `mj-controller/src/controller/worktree.rs` provisions and removes raw managed checkouts; `mj-controller/src/controller/network_git.rs` initializes container/network clones. `mj-controller/src/controller/lifecycle.rs` stops and destroys targets, while `mj-controller/src/controller/resume.rs` restores them. `mj-checkpoint/src/archive/git.rs` selects committed objects, stores file patches, and restores branch/HEAD. `mj-controller/src/daemon/resume.rs` runs age and lost-session cleanup. The HTTP API and client carry start/suspend choices; `mj-tui/src/resume.rs` and the web viewer render Open/Resume and suspend actions.

Existing worktree JSON must remain readable. New clone ownership and saved publication status require a new database revision; classify it breaking because an older writer cannot safely preserve the new ownership and retention contract. Never modify an already-applied migration. Existing archives must remain readable; expanded archives need a version older code rejects explicitly. Test all migration and runtime behavior with isolated `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, and a named `--instance`; never use the live store.

## Plan of Work


### Milestone 1: Independent raw clone provisioning


Add a managed-clone record and mutually exclusive ownership validation beside `ManagedWorktree`. Keep old worktree records and resume code. Provision new local/SSH managed checkouts under `<source>/.mj/clones/<session-id>` with a local hardlinked clone into a temporary owned path, verify it, then publish it. Do not use `--shared` or alternates. Preserve the source's intended fetch and push destinations, branch tracking, identity/signing/hook/exclude behavior without blindly copying its structural `.git/config`. Local seed paths must never become an accidental push destination. Make selected branch and launch base independent inputs. Surface the resolved branch, commit, and push destination in both launch reviews; rename the control "Create isolated checkout" and accept the old API field as a deprecated alias. Adjust container clone initialization to use the same branch choice rather than an `mj/<id>` branch. Supervise all Git work in background tasks. A completed test must run two sessions on master and show independent commits and normal push conflict behavior without changing source refs.

### Milestone 2: Clone recovery and source-dependent checkpoints


Expand Git snapshots to carry every saved local branch, tag, note, detached HEAD commit, and stash entry along with the existing file patches and untracked files. Record exact source prerequisites and bundle all required objects absent from that source; confirm prerequisites and archive integrity before deleting a checkout. Keep ignored files and build artifacts excluded. On resume, recreate the clone and refs, reapply file state, and only then start the worker. Preserve checkpoint and session on every failed restoration. For raw/container moves, rebuild the delta against the destination seed before retiring the old target. Refuse suspension when Git state cannot be represented safely, including in-progress operations. Tests must use more than 64KB when exercising stream/pipe behavior and include multiple unpublished branches and stashes.

### Milestone 3: Publication evidence and retention


Store a checkpoint-hash-bound publication summary in the database: dirty/stash presence, saved commit identities, publication result, destination evidence, and check time. Distinguish Published, Unpublished, and Unknown; Unknown means verification was unavailable or metadata inadequate. Verify all saved committed work against configured remote destinations, not merely the current branch's ahead count. Age cleanup refreshes evidence through bounded background work and removes only Published, clean, stash-free sessions whose conversation is indexed. Lost cleanup checks owned storage before deleting a record. Explicit Destroy can discard the owned clone and archive after confirmation, never source or remote branches. A pushed but unmerged branch must qualify for aging; a clean clone holding an unpushed commit must not.

### Milestone 4: Open/Resume and suspension controls


Add publication state to operational session rows and display a warning-colored `↑` (ASCII fallback) and a concrete explanation for unpublished work in `mj-tui/src/resume.rs`. Show `?` with "Publication status unknown" if verification is incomplete and no unpublished work is established. Do not claim a native/history-only row is published. On dialog open, use cached persisted status immediately and supervise refresh work off the event loop; label stale assessments. Add a shared suspend preflight for TUI/web/CLI/API. Confirm when unpublished or unknown work is present, combining existing interruption/subagent warnings. Add `acknowledge_unpublished_work` in the API and `--acknowledge-unpublished-work` in the CLI. Without acknowledgment, leave the session running and explain the requirement. Recheck at the checkpoint boundary so work appearing after preview cannot be torn down without acknowledgment. The verified checkpoint remains mandatory even after acknowledgment.

### Milestone 5: Validation and delivery


From `/home/jonathan/Projects/mjolnir3`, run focused behavior tests, a named isolated CLI/API/TUI/browser smoke test, `cargo test` outside the restricted sandbox, `cargo clippy --all-targets -- -D warnings`, rustfmt verification, `git diff --check`, and applicable documentation checks. Keep normal mbx storage. Verify older worktree sessions, old archives, migration from the preceding revision, missing source, failed checkpoint, interrupted provision, and failed restore. Update human docs. Commit only changed files in coherent validated checkpoints on the current branch; after completion push to origin/master as authorized, comment on #1073 with evidence, close it, and remove `agent-in-progress`. Then plan #1083 separately.

## Validation and Acceptance


A fresh isolated raw session appears on the selected/default branch and `git push` targets the intended remote. Two clones can start on the same branch and diverge independently. A suspended session has no checkout but its verified archive restores every saved unpublished commit, stash, and supported file change. A worktree session from an older version still resumes. Open/Resume flags unpublished or unknown work with readable explanations. Suspension with that work asks for confirmation in TUI and web and requires CLI/API acknowledgment. Automatic archiving keeps any session with unpublished, uncommitted, stashed, or uncertain work and can archive clean work confirmed pushed, whether or not merged. A failed operation retains enough state to retry. All tests use disposable repositories and isolated named instances.

## Idempotence and Recovery


A clone's owned directory is created only if absent and tied to the session id. Staging paths are cleaned only after the owning process group is stopped and only when they belong to the same session. Repeated provisioning verifies an existing path instead of resetting it. A failed checkpoint or missing source prerequisite leaves the old checkout intact. A failed resume retains the checkpoint and session, reports why, and supports retry. The migration preserves old rows and advances revision and compatibility floor atomically. Git refs outside an owned clone are never removed by automatic cleanup.

## Artifacts and Notes


Local proof: two `git clone --local` checkouts had hardlinked seed objects; after a commit in clone A, only A's master moved. The temporary proof directory was deleted. Record focused test output, migration revision, and final commit hash here as work proceeds.

## Interfaces and Dependencies


Use existing shared `CommandExecutor`/subprocess helpers for local and SSH Git calls. Extend the existing session and checkpoint formats rather than creating a new crate. API start gains an optional branch selector while retaining launch-base and the old isolation alias. Suspend gains a preflight and `acknowledge_unpublished_work`; the CLI exposes `--acknowledge-unpublished-work`. Database publication evidence is scoped to an exact checkpoint hash. UI controllers fetch evidence asynchronously and render cached state only.

Revision (2026-09-23): replaces the branch-inventory plan after the user chose independent clones, publication-based automatic retention, and a suspend confirmation.
