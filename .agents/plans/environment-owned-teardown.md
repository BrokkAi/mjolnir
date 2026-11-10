# Give shared environments durable teardown ownership

This ExecPlan follows `.agents/PLANS.md`. Maintain its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as work proceeds. It replaces the implementation strategy in `.agents/plans/destroy-with-cleanup-warning.md`.

## Purpose / Big Picture

A user must be able to destroy a session whose host is unavailable. Destroy removes the logical session and reports incomplete cleanup. Removing that session must not erase the information required to protect files that its worker might still use.

An environment is the physical resource owner: its owned workspace or storage, its current runtime placement, its worker processes, and the private files those workers use. Its identity survives replacement of a worker or container when the workspace is retained. A move to a different owned workspace creates a different environment. A session can borrow an environment; deleting that session does not delete its environment or its worker ownership record.

The observable result is that deleting an unreachable child, restarting the daemon, and later destroying or suspending its parent cannot delete the child's shared files without stopping that worker. Destroy can still remove the requested sessions with warnings. Suspend and Move retain their non-destructive failure semantics.

## Progress

- [x] Identified the original unreachable-host failure and the user's requested warning semantics.
- [x] Verified that the unpublished patches do not fix ownership across independent requests.
- [x] Traced worker placement, process stopping, sub-agent registration, target persistence, and destructive call sites.
- [x] Specified the replacement ownership boundary and acceptance cases.
- [ ] Confirm the expanded storage/lifecycle refactor scope before implementation.
- [ ] Introduce durable environments and worker ownership with an isolated migration regression.
- [ ] Register worker leases and serialize launch admission against environment retirement.
- [ ] Route all physical teardown through the environment owner and remove the cascade-specific patch mechanisms.
- [ ] Run the isolated regressions, required validation, review, commit, and push.
- [ ] Install and apply the fix to the affected live session only after installation is authorized.

## Surprises & Discoveries

`session_targets` has a foreign key to `sessions` with cascading deletion. `subagent_sessions` also depends on logical session rows. These are suitable for the current-session view, but cannot be the durable inventory of workers that may survive session deletion.

Bare sub-agents have sibling worker roots. Their process groups are independent even though they borrow their parent's checkout. The parent's worker being dead does not establish that the other workers are dead.

`WorkerPermit` in `mj-controller/src/worker_lifecycle.rs` serializes mutation of one worker. It does not own the shared environment or establish the membership of all its writers.

`stop_subagents_for_suspend` in `mj-controller/src/daemon/close.rs` logs a failed stop and removes the child's records anyway. This is an existing instance of the same ownership defect. Several Resume, provisioning rollback, and recovery paths invoke `targets::close_plan` directly instead of sharing one environment destructor.

The local commits `3c523cbdd`, `5b9868d0d`, and `ae628e069` remain unpublished and uninstalled. Their warning presentation and regression cases are useful reference material. The in-memory cascade outcome and deferred session deletion strategy are not a replacement for durable environment ownership.

## Decision Log

The user explicitly chose ordinary Destroy with a cleanup warning on 2026-11-10. Physical cleanup failure therefore cannot require keeping a visible session indefinitely.

The ownership evidence must survive deletion of either a child or its parent and survive daemon replacement. The environment and its worker leases belong in the controller database, without foreign keys that erase them when logical sessions are deleted. The daemon remains the control plane; workers retain their current data-plane role. This does not require an additional supervisor daemon or a new workspace crate.

Environment identity follows shared workspace ownership. Restarting a worker or explicitly replacing a container with retained storage preserves that identity. A new workspace placement receives a new identity. This preserves the project's retained-container semantics and prevents old cleanup from gaining authority over a replacement workspace.

All physical deletion requires a capability issued by the environment owner after it has closed admission and stopped the relevant writers. A caller cannot infer that capability from a missing session, an empty child list, a successful parent-worker stop, or a warning count.

The migration is breaking. Older readers and writers do not understand independent environment ownership and could delete files after erasing child records. Advance the store revision and minimum compatible revision together. Do not apply this migration to the live store while developing or testing the change.

## Outcomes & Retrospective

Investigation and replacement design are complete. No replacement storage or runtime code has been implemented. No live state has been changed and no code has been pushed or installed. The remaining scope decision is whether to perform the shared storage/lifecycle ownership refactor now; the current Destroy patches are not safe to ship on their own.

## Context and Orientation

`mj-controller/src/database/state_io.rs::replace_targets` currently rewrites the logical session's target row. `insert_session` and `update_lifecycle_fields` are the transaction boundaries for that projection. `mj-controller/src/database/sessions.rs::register_subagent_with_startup` commits a child and its relation before startup. `mj-controller/src/database/session_restart.rs` already serializes container replacement against child registration and wake-up; preserve that guarantee when integrating environment admission.

`mj-controller/src/controller/worker_binary/process.rs::start_worker` is the process-launch boundary. It already receives a worker permit. Every managed worker must have a committed environment lease before this function can execute its start command, including workers prepared for Move and recovery.

`mj-controller/src/targets/worker_daemon.rs` already provides a stop-only script. `targets::borrowed_worker_cleanup_plan` combines stopping and file removal. The stop-only operation is needed while gathering proof that all environment writers have stopped; private worker files cannot be removed during that first phase if another member might depend on them.

`mj-controller/src/controller/lifecycle.rs` contains `execute_target_cleanup` and the logical Destroy implementation. Direct physical deletion also occurs in `controller/resume.rs`, `controller/provisioning.rs`, and `controller/recovery_scan.rs`. `controller/worktree.rs::cleanup_managed_worktree` removes an owned checkout and must require the environment owner's deletion authority when workers may have used that checkout.

The environment inventory is private control-plane state. It must not be exposed through browser snapshots as credential or runtime configuration data. Safe warnings can continue to use the existing public notice projection.

## Plan of Work

### Milestone 1: Persist the physical owner independently of sessions

Add a controller-owned environment module and database access module within `mj-controller`; do not create a crate. Introduce an opaque environment ID, an environment record containing the owned workspace/storage and runtime placement, and worker lease records identifying the exact worker root and its environment. Preserve the runtime connection information required for later cleanup after a logical session has disappeared. Store only physical cleanup metadata in these records, not a copy of the complete agent conversation or session state.

Logical sessions refer to their current environment. Environment and worker records do not cascade away with a logical session. The environment is the sole authority for destructive workspace metadata; logical `SessionRecord` fields needed by clients become projections of that owner rather than a second independently writable ownership record.

Backfill existing recorded targets and relationships in one migration. Reuse the existing normalized target decoder rather than independently interpreting paths or target JSON. Backfill must preserve target evidence even for sessions whose harness is no longer supported. Represent incomplete historical ownership explicitly and fail visibly when it prevents safe reclamation; absence of a supported session must not be interpreted as absence of a worker.

Allocate the migration revision against current upstream at integration time. Classify it as breaking beside the migration and update the compatibility floor atomically. Retain all shipped migrations and the isolated upgrade fixtures. Existing active placements keep their paths and identity; migration does not launch, stop, or replace workers.

### Milestone 2: Give the owner launch and retirement admission

Commit worker membership before process launch. Sub-agents borrow the environment selected by their parent's committed placement. Prepared Move destinations have their own environment ownership before any destination worker starts. Worker and container replacement reuse or replace the environment according to whether owned workspace/storage is retained.

Serialize the decision to retire an environment and the decision to admit a new worker through the database writer. Closing admission and taking the member snapshot are one decision. A worker already admitted before retirement must settle under its existing worker permit before the owner can stop it; later admissions are explicitly refused. Keep downloads, staging that does not mutate the environment, and other long preparation outside the critical launch/swap reservation.

Use the existing per-worker permits to drain actual worker mutations. The environment owner holds the relevant permits through stopping and file removal. Preserve concurrency between independent workers and independent environments. Do not add concurrency limits, retry budgets, or guessed timeouts.

Persist retirement progress so a daemon restart cannot reopen an environment whose files may be partly reclaimed. Follow the existing short-or-resumable control-operation rule and destroy's bounded handoff exception. Reuse the existing durable startup, Move, and replacement formats where they remain the authority for accepted work; move physical cleanup obligations into environment ownership before deleting a logical operation that held them.

### Milestone 3: Use one destructor for all physical reclamation

Implement a single environment retirement operation. It closes admission, acquires the member permits, and attempts stop-only teardown for every recorded worker, including workers whose sessions have been deleted. Only after all relevant writers are confirmed stopped may it issue the capability authorizing owned-file deletion.

A failed stop leaves the environment and its worker evidence intact. Explicit Destroy removes logical sessions and reports incomplete cleanup. A later operation reads the same environment owner and sees the outstanding workers. The request that initiated the operation receives all relevant warnings, including failures of descendants.

Normal Suspend, Move, automatic archival, container replacement, provisioning rollback, Resume cleanup, and orphan recovery use the same authority when removing shared resources. Remove the existing Suspend path that erases failed child-stop evidence and proceeds as if stopping succeeded. Temporary resources in which no worker was ever admitted use an explicit unstarted-resource authority; do not add a fallback that assumes no worker because no session row exists.

Make physical removal helpers require the owner's deletion capability. This turns the process-before-files rule into an API boundary instead of a convention repeated at each caller. Keep target plan builders pure: database and network work belongs in supervised controller tasks, not TUI or web render loops.

Resume and replacement must preserve the environment/resource distinction. A failed cleanup cannot silently hand its old workspace to a fresh environment or allow an old teardown command to delete replacement files. Retained-storage container upgrades retain the workspace owner and use the existing atomic idle admission. If a fresh environment is needed, expose that as an actionable recovery path using the verified checkpoint, not a request to edit configuration manually.

### Milestone 4: Replace the unpublished patch mechanisms and validate

Remove `DestroyWorkers`, retained session-ID propagation, and root-versus-descendant deletion policies once the environment owner supplies their intended guarantees. Keep useful warning presentation and the hard-won regression scenarios, rewriting assertions around durable physical ownership rather than internal session tombstones. Do not ship both cleanup mechanisms.

Validate focused cases while implementing. After the last implementation round, run the required touched-crate suites and workspace clippy once. Review the resulting implementation before pushing. Merge upstream normally if a push is rejected; do not create a branch or rebase.

## Concrete Steps

Work in the current checkout on `master`. Preserve the three unpublished commits as local checkpoints. Replace their unsafe lifecycle changes with ordinary commits on the same branch; do not reset shared history or push an intermediate unsafe revision.

Use `cargo test -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui -- <affected test filters>` during development. Run every Cargo test outside the restricted sandbox. Keep the existing isolated configuration/data directories and use a separate named instance for every real CLI, daemon, worker, or TUI regression. Keep test roots outside `target/` and do not redirect Cargo build storage.

At the final implementation checkpoint run `TZ=UTC cargo test -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui` and `cargo clippy --all-targets -- -D warnings`. Run the affected browser notice tests with `node --test tests/e2e/web/viewer.unit.test.mjs`. Re-run only failing or subsequently changed cases when correcting validation failures.

## Validation and Acceptance

The principal isolated regression starts a parent and child sharing an owned checkout, makes the child's stop unconfirmable, destroys that child directly, restarts the daemon, and then destroys the parent. Both logical sessions are removed with warnings; the shared files remain and the owner still knows the outstanding physical worker. Once that worker is confirmed stopped, environment reclamation can remove the files and its physical records.

A controlled concurrent regression holds a worker launch at admission while the owner begins retirement. Exactly one serialized decision determines whether launch was accepted. An accepted worker is included in teardown; a refused launch starts no process. Use barriers or existing test hooks rather than sleeps to create the ordering.

Test partial root deletion or reclamation failure with real isolated database transactions. Logical deletion must not delete physical leases, and retries after daemon replacement must use those same leases. Test direct child deletion, parent cascades, and independently recovered destruction through the same owner API.

Run the equivalent failure through Suspend and Move. They retain resources and report failure when the owner cannot stop a writer. Successful retirement, retained-container replacement, checkpoint recovery, and upgrade handoff continue to pass their existing isolated regressions.

Migration integration tests cover fresh stores, every shipped revision's existing upgrade path, preservation of recorded physical targets and relationships, rollback of the new migration, and rejection of older incompatible readers/writers. No test uses the live store or real profile homes.

## Idempotence and Recovery

Physical leases are released only after the corresponding resource is confirmed reclaimed or transferred through an explicit ownership transition. Logical deletion is idempotent and cannot erase those leases. A lost client acknowledgement does not authorize replay of arbitrary lifecycle mutations; retain existing command IDs and handoff behavior.

All validation remains isolated. Installing this change upgrades the live store and requires the user's installation instruction after implementation and validation are complete. Do not apply the breaking migration to the live store during development.

## Interfaces and Dependencies

Provide an environment-owner API with explicit registration/admission, retirement, stop-only member teardown, and reclamation. Return safe cleanup outcomes for logical session operations. The owner-issued reclamation capability is the only way shared target and checkout deletion helpers may be invoked after workers have used them.

Use `Path` and `PathBuf` for filesystem data, existing target/runtime parsers for placement interpretation, the serialized database writer for durable decisions, existing subprocess helpers for process control, and existing worker permits for per-worker mutation. No new host supervisor, workspace crate, polling reaper, or speculative resource cap is part of this proposal.

Design reset recorded after the user's request to address the underlying fragility. This plan intentionally changes the ownership model across lifecycle operations; it does not describe another local exception in Destroy.
