# Resume repairs a failed session in its existing environment

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

On 2026-11-14 session 94d7c3846306d798c13d5c32fe7b9c2c (Claude, Podman over SSH on target morannon-podman, workspace in a Podman volume) went to the `error` state because a background worker upgrade failed during harness preparation. Its container kept running, and two of its Mjolnir sub-agents (sessions started by the parent's `spawn` tool, which run inside the parent's container through a borrowed locator) were still working in it. `mj resume --session 94d7…` then stopped and removed the container, deleted the workspace volume, provisioned a new container and restored the parent from a recovery archive taken 14 minutes before the failure. The two children lost their environment permanently and the parent lost its newest work, although the old container and volume were intact.

The root cause is a decision with no evidence behind it: in `mj-controller/src/controller/resume.rs` (~1727) a non-Move Resume of an `Error` session that retains its target calls `environment::remove_runtime_preserving_workspace`, which for a volume-backed workspace reclaims everything (`controller/environment.rs` ~513-538, `Reclamation::acquire` ~69-192 stops borrowers, `targets/cleanup.rs` ~458-524 removes container and volume). Being in `Error` does not prove the environment is unusable; a failed harness preparation says nothing about the container.

After this change, Resume of a failed session whose environment still exists brings the session back in that environment: same container, same volume, current files and native harness state, and its live children keep running. Restoring from the archive into a new environment happens only when the old environment is proven absent. A user can see it work by repeating the incident in an isolated instance: the parent resumes with its newest file present and the child finishes its turn.


## Progress

- [x] Third review rulings implemented: per-component evidence, missing-only parent recovery, retained-volume recreation, advisory preflight, and ancestor absence probing.
- [x] Partial checkpoint recovery regression passed, including current files/native state, retry after a failed seed write, and a current durable queue/frontier newer than the archive.
- [x] Updated controller regressions and real Git/filesystem/native-goal boundary checks passed. Full checkpoint, controller, TUI and worker suites passed; workspace clippy passed.
- [x] Expanded isolated Podman and localhost E2E passed, including parent runtime loss while a child remains busy, retained named-volume recreation, current durable relay frontier, and timestamped recovery notices.
- [x] Final retained bind-mount E2E passed in the isolated named instance; workspace clippy, formatting, Python compilation and diff checks passed.
- [x] Final complete touched-crate run finished; three unrelated failures/stalls passed targeted reruns with the same package selection.
- [x] Component correction committed as 337bc5e565a3ffe2a2424df7da55e0fb7689d080; fourth independent review completed.
- [x] Fourth-review fixes implemented and expanded isolated E2E passed: atomic missing-file publication, retryable container failures, explicit queue disposition, every missing bundle repository, and idle parent restart after live checkout loss.
- [x] Atomic-write and real Git publication regressions passed; broad Resume/recovery selections passed; existing legacy reservation regression passed after restoring disconnect-on-deferral.
- [x] Added durable-boot/handoff regression passed: swap admission is released after durable launch, before readiness observation. Shared reservation retains the existing 30-second handshake deadline.
- [x] Latest full touched-crate suites completed: checkpoint 174 / 2 ignored, controller 2,652 / 27 ignored, TUI 591 / 2 ignored, worker 846 + four passing targeted reruns / 11 ignored; all 29 worker main/integration cases and doc suites passed.
- [x] Workspace clippy, formatting, Python compilation and diff checks passed on final source.
- [ ] Follow-up commit and required independent review.

- [x] Second review: distinguish a deleted bare worker root from a reachable host. Real local/SSH-shaped shell regression passed; full touched-crate suites and workspace clippy completed. Follow-up commit prepared for required review.

- [x] First required review completed; all three findings accepted and implemented.
- [x] Review regressions for Apple local/SSH start ordering and failed-checkout conversion previews passed.
- [x] Review regression for absent-target admission and extended Podman run passed; the same run also passed live-worker repair in a localhost managed worktree.
- [x] Post-review validation completed; follow-up commit prepared for required review.

- [x] Milestone 1: Resume decides from the environment's actual condition (live worker reachable / container present but worker gone / container proven absent / unknown) instead of from the `Error` state.
- [x] Milestone 2: repair-in-place path (reattach the live worker and journal when reachable; otherwise start a fresh worker and harness in the existing container) and the archive path gated on proven absence; `rollback_failed_resume` (~2160-2182) no longer destroys an environment that was present before Resume began.
- [x] Milestone 3: end-to-end test in an isolated instance; validation; prepared commit.
- [x] Focused controller regression `failed_resume_keeps_present_and_unknown_environments_without_an_archive` passed (one test, including rollback preservation; `/tmp/resume-regression.log`).
- [x] Initial CLI build passed; final rebuild is queued after the portable worker because the initial compile overlapped a source correction.
- [x] Matching portable worker built for `x86_64-unknown-linux-musl` using normal mbx storage.
- [x] Broader Resume selection passed: 68 controller tests, 56 TUI tests (one ignored); `/tmp/resume-focused-suites.log`.
- [x] Local Podman end-to-end live-worker and dead-worker repair passed, including no checkpoint on the dead-worker case and completion of the original child command; `/tmp/resume-e2e-7.log`.
- [x] Extended local Podman end-to-end case passed: a real private-profile preparation failure leaves a live relay; correcting that profile and resuming replaces only the failed parent boot while the child continues (`/tmp/resume-e2e-preparation.log`).
- [x] Final touched-crate suites completed: controller 2,648 passed / 27 ignored / one existing two-second Move panic timeout; that test passed alone in 0.81 seconds. Cargo stopped at that failure, so the already-built TUI executable was run separately: 591 passed / two ignored. Both crates' doc tests passed (zero cases).
- [x] `cargo clippy --all-targets -- -D warnings` passed on the dev profile. Formatting, Python compilation and diff checks passed. Required review is tracked by commit review and the final handback.


## Surprises & Discoveries

- The existing private-review/legacy-goal upgrade regression caught a refactor that released a managed connection on deferral even though BeginCheckpoint had already been accepted. Deferral must disconnect to cancel that barrier. The same handshake and verification now serve both managed upgrades and standalone checkout repair.
- Worker replacement admission must end at durable launch, not at harness readiness. The reserved repair state transfers the owned admission to the existing installed-worker launch owner, which releases it before resumable observation. A dedicated isolated partial-failure regression verifies both the durable phase and the released gate.

- Component recovery must preserve the daemon's newer durable queue/frontier as well as current files. Restoring an older archive queue could replay commands completed after capture. The controller supplies a current relay seed; missing native artifacts still come from the verified recovery archive. Existing relay state/journals always win, and partial recovery never replaces the controller's materialized projection.
- The container replacement helper already preserves the named workspace volume. Resume shares that replacement, then calls missing-only parent recovery rather than the legacy whole restore, which could replace child reports on the retained workspace.
- Missing-checkout recovery reconstructs in private staging beside the destination, then atomically publishes without replacement (Linux renameat2 / macOS renamex_np). A directory created by another writer wins. Retry specifications are stored before mutation; ready-worker notice IDs are stable across retries, and staging/receipts are removed only after the notice is accepted.

- Second review (`4c7ea3e31`) found that the shared target probe only proves bare-host reachability. Resume now also consults the existing directory-listing worker-root probe. That probe had merged absent roots with empty roots; it now reports them separately, while its original recovery consumer still treats both as no durable relay state. The regression executes real shell probes locally and through the SSH command quoting boundary, including an unusable non-directory path that must remain Unknown. Its first fixtures violated existing cleanup identity rules: local worker roots and SSH workspaces must both end in the exact session ID. Those paths were corrected; diagnostic assertions retain the underlying error for future failures.

- The second production incident (`07d292ca`, localhost managed worktree) used the same Error branch after a false readiness failure. The generic classifier and live-worker attach cover bare targets too. The real isolated E2E now checkpoints a managed worktree, writes newer workspace and native-home files, keeps a child busy, injects Error with the daemon stopped, and verifies the same parent and child PIDs, native session ID, files and original child command through completion. `/tmp/resume-e2e-bare-review.log` passed.

- Required review of `c77d71fc3` found three gaps. Apple `list --all` proves presence but not that `exec` can run; Resume now checks the running list and starts only the retained container, on its owning host. Restart's probe behavior is unchanged. Raw-to-workspace conversions still take the normal credential/preview path before confirmation. Proven absence with no checkpoint now publishes `Lost` with no usable session target, while the durable environment/worker inventory retains ownership of physical resources for explicit Destroy.

- Validation fixture corrections: container creation needs explicit CPU/RAM allocation and the container harness's lifecycle wait, rather than generic five-second HTTP / twenty-second snapshot waits. Child completion must track its original command ID because a handback reminder can immediately start another turn. Shell `kill -- -PID` was replaced with Python `killpg`. An ACP initialization error occurs after coordinator startup; an invalid private Codex config produces the required terminal preparation state before startup.

- Implementation: Restart already had an engine availability probe. It now delegates to the environment module, preserving Restart's existing reachable/missing/unreachable behavior; Resume adds worker liveness classification and treats unreachable as unknown.
- Implementation: API and terminal entry checks previously refused repair without a checkpoint. They now allow retained failed sessions through; archive preflight remains required after proven absence.
- Validation: local Podman works outside the sandbox. The real-container regression reuses `container_upgrades.py`, with its deterministic ACP fixture. It injects the durable Error result while the isolated daemon is stopped rather than reproducing the nondeterministic background upgrade failure. The live and dead cases passed with a child staying busy; an additional passing case induces a real private-profile preparation failure, restores that profile, and retries the live failed worker.

- (mapping, 2026-11-14) `ResumePlan::InPlace` today means converting a checkout, not repairing a container; do not reuse the name for the new path without renaming one of them. `in_place.rs` restore from checkpoint can roll back newer state, so the repair path must not restore the archive over a present workspace.
- Sub-agent placement: `controller/subagents.rs` ~349 and ~700-760 create and propagate borrowed locators (`borrowed_from`); `database/environments.rs` ~182-268 binds children to the parent environment; `environments.rs` ~348 marks an environment Retiring and snapshots its workers, ~535 admits only while Active.


## Decision Log

- Fourth review and parent ruling (2026-11-14): probe checkout presence even for live workers. If paths disappeared while the worker lived, rebuild only missing checkout directories without replacement, then restart only the parent through the same atomic idle reservation used for worker upgrades. A surviving process's deleted cwd is not repaired merely by recreating its pathname. Wait visibly for accepted work, with no timeout granting permission to cancel it; children stay untouched. The managed and standalone paths now share the reservation and verification handshake.
- Fourth-review corrections: publish missing native/relay files from private complete, synced temporary files with no replacement; failed writes never become authoritative truncated history. Missing-container Resume keeps Error throughout reconstruction and records Error on every failure. Explicit queue discard is propagated through every retained-container recovery branch. Bundle recovery publishes each missing repository independently and retains current repositories; a private staging receipt prevents reapplying the archive after partial publication.

- Parent clarification (2026-11-14): failed archive Resume may still remove newly created, wholly rebuildable destinations. Pre-existing volumes are never disposable. Once a restored worker can have accepted work, that destination is retained for all later failures. Resume uses the existing durable boot owner (admitted before launch), or published Running state, as that ownership boundary; this also prevents rolling the materialized projection or conversion archive back after a lost startup acknowledgement.

- Internal target commands add explicit `--missing-parent` / `--missing-checkout` modes; old workers reject these flags instead of interpreting a partial recovery as an ordinary restore. The controller installs its matching worker first. Archive/spec formats used by existing operations, database schema, and relay protocol are unchanged. The new checkout retry spec has version 1 and denies unknown fields; the parent retry file uses the existing restore spec. Date: 2026-11-14.

- Parent ruling after the third review (2026-11-14): presence is per component, not one bit inferred from the parent worker root. Preserve any present checkout and live children. Restore only proven-missing parent runtime/native/relay data from a recovery archive; the successful notice must say native history was restored from the copy's timestamp while files are current. Without a suitable archive, preserve everything and give recovery guidance. Read-only advisory preflight asks this owner whether the checkout is absent, preserving repository-source confirmation; the lifecycle permit remains the decision point and reclassifies. Missing ancestor directories must be distinguishable from lookup failures.
- Parent ruling (2026-11-14): a missing container with a surviving named workspace volume is recreated on that volume using the existing retained-storage replacement. Resume never deletes that volume or restores repository content over it. Only proven-absent components use the archive; explicit storage destruction policy is unchanged.

- Superseded by the third-review component ruling: the second review made bare Resume use the worker-root directory-listing probe after host reachability. The probe now distinguishes proven absence from an empty present root; its recovery caller retains the same no-relay semantics for both. Missing roots take archive recovery, empty roots repair, lookup errors remain Unknown. No persisted or wire format changes. Date: 2026-11-14.

- Review decision: `Lost` and a cleared session target express proven absence when no archive exists. This changes client admission without erasing the environment ledger or reclaiming storage. The existing state and fields need no schema/protocol change; API admission/refusal use one projected source predicate. Date: 2026-11-14.

- Implementation decision: a present placement keeps its current profile and target; changing them follows repair through Move. Requested resource or mount changes produce a notice that the existing container settings were kept. Date: 2026-11-14.

- Implementation decision: arbitrary attach failures leave a live worker alone. A typed `HarnessPreparationFailure` is different: `mj-worker/src/worker_runtime/unix.rs` keeps the relay available after this terminal failure but has no coordinator and cannot dispatch ACP work. Resume can replace this failed boot in the same root, retaining its journal. Any old boot intent is settled only when it names the current placement. This completes the planned “cannot serve” case without killing a worker on a lost handshake. Date: 2026-11-14.

- Decision: the `Error` state alone never authorizes environment teardown in Resume. Resume classifies the environment through the target engine: reachable live worker, container present without a usable worker, container proven absent (engine says it does not exist), or unknown (engine unreachable or ambiguous). Date: 2026-11-14.
- Decision: present environment → repair in place. First try to reattach the existing worker and replay its journal. If the worker is gone or cannot serve, start a fresh worker in the same container with the existing workspace and native harness home, using the same worker preparation the background upgrade uses. Do not restore the checkpoint archive over the present workspace or native state. Children are not stopped. Date: 2026-11-14.
- Decision: proven absent → the existing archive restore into a new environment, as today. Unknown → do not tear anything down; fail the Resume visibly, keep the environment and its placements, and tell the user what was unreachable and that Resume can be retried. Per AGENTS.md "never leave the user stuck", the error must say the next step, and the TUI must keep offering Resume. Date: 2026-11-14.
- Decision: a failed repair keeps the environment and the session in `Error` with the repair failure as its error, so retrying Resume repairs again. `rollback_failed_resume` must only remove resources this Resume created. Date: 2026-11-14.
- Decision: Destroy, Close, Suspend, Move and container upgrade keep their current child semantics in this change (deliberate operations that stop or move children). Only Resume's evidence-free teardown is in scope. Note in Outcomes whether upgrade-container's child admission looked sound, without changing it. Date: 2026-11-14.
- Decision: no schema or protocol change is expected. If a new durable intent turns out to be required, add a new migration classified compatible or breaking per AGENTS.md; never rewrite migration 122. Date: 2026-11-14.


## Outcomes & Retrospective

Resume now classifies retained components under the lifecycle owner rather than treating Error or a missing parent worker root as permission to discard the environment. Live workers whose checkouts remain present reattach. Missing repositories are reconstructed without replacement, and a live parent then restarts only after the shared idle reservation admits it; accepted work and children stay alive while it waits. Dead parents restart in their retained placement. Present checkout content, native files, relay journals and child workers win over an older recovery copy. Missing parent state is recovered separately, using the current durable projection and pending queue; a durable notice identifies the recovery copy's timestamp and says files are current. A parent whose native state survives can rebuild missing runtime from the current store without an archive.

Fourth-review validation also proved retryable container creation failure, explicit queue discard during retained-container recreation, two missing repositories with a third newer repository preserved, and a busy parent/child retaining their PIDs and original commands until the parent finishes. The expanded E2E passed in /tmp/resume-review4-e2e.log. The final handoff-only correction transfers swap admission to the existing launch owner and releases it once AwaitingReadiness is durable, preserving daemon replacement during slow startup. The isolated checkout_repair_releases_swap_admission_after_durable_launch_before_readiness test passed. The earlier complete suites had one unrelated rsync fixture ETXTBSY failure; its targeted rerun passed. Final workspace clippy passed. Final suites passed 174 checkpoint, 2,652 controller, 591 TUI and 846 worker library tests, plus all 29 worker main/integration cases and doc suites. Four unchanged worker fixtures failed under the full run and passed targeted reruns: three saved-model ACP cases hit their ten-second progress wait and the work-facts changed-commit probe was unavailable. No source change was needed for these failures. Logs and full names are retained in the assigned report directory and final report.

A missing container is recreated on retained storage using the existing replacement helper, then missing parent state is repaired. Bind mounts containing the checkout also forbid repository restore over retained files. Missing checkouts are reconstructed in private staging and published atomically without replacement; source confirmation remains visible. Retry receipts survive interruption. Before startup, rollback removes only new rebuildable destinations; after durable boot admission, failures preserve the placement and projection. Explicit Destroy, Close, Suspend, Move and upgrade child policies remain unchanged. No database migration, relay protocol change, or existing archive-format change is needed. Internal partial-restore CLI flags fail visibly on old workers; matching workers are installed before use.

The third-review component tests passed: checkpoint 173 / two ignored, controller 2,651 / 27 ignored, TUI 591 / two ignored, worker library 850 / 11 ignored, and every worker integration/main group. Workspace clippy passed. The expanded isolated E2E passed for Podman retained named volumes and localhost managed worktrees, including deletion of only the parent runtime while its busy child retained its PID and original command. The final expanded E2E also passed retained bind-mounted checkout recreation; /tmp/resume-components-e2e-bind-2.log identifies the exact private instance and artifacts. The final post-correction full run passed all but three unchanged tests: the shell-profile result, the two-second Move panic wait, and a scoped Git capture subprocess stall. All three passed targeted reruns (the Git test in 0.52 seconds). The stalled Git fixture was verified and terminated narrowly after recording its process/pipe evidence; the rest of the suite completed. The final report retains all failure and rerun logs. A bind fixture initially mounted the wrong path because workspaces include the session ID; it was corrected to derive the recorded checkout beneath a retained workspace mount.

Earlier rounds found Apple stopped-container ordering, missing source confirmation, and inappropriate absent-target admission. Their regression fixes passed. An initial coarse bare-root absence fix was superseded by the component decision above. Historical full-suite failures were an unchanged two-second Move panic wait and an unchanged startup draft race; both passed isolated reruns, and the later complete suites passed. The final report retains names and logs, including corrected fixture failures. No unresolved validation failure remains from those rounds.

The first three commits are c77d71fc39d248c5dd9536dbaf728e761497ad3a, 4c7ea3e31de221fe474076457cfa3bad6507a263 and 055d8a7722e3f08ab5e2f2bf87c9ad955eb3448f. The component correction is a follow-up commit requiring independent review. No push or installation is authorized. Evidence uses a private named instance, real CLI/daemon/workers/Git/Podman, and a deterministic ACP fixture. Error results are injected while its daemon is stopped; one real terminal preparation failure uses a temporarily invalid private Codex profile. No native Mac, live SSH target or real provider was exercised. Apple local/SSH engine ordering is covered by contract fakes on Linux.

The existing explicit container-upgrade admission appears sound for this issue: daemon/restart.rs::try_commit_container_upgrade serializes through the lifecycle owner and database/session_restart.rs::try_commit_container_upgrade refuses commit while children have live workers or targets or accepted delegation requests remain. Child registration uses that same database writer. This policy is unchanged.


## Context and Orientation

Entry: `mj resume` → `mj-controller/src/daemon/resume.rs` ~122 → `controller/resume.rs` `resume_session_with_origin` ~1563; Error accepted ~1603; checkpoint required ~1613 (keep requiring a checkpoint only for the archive path; the repair path does not need one). Teardown call ~1727; provisioning and archive restore ~2037-2090; rollback ~2160-2182. Environment ownership and teardown: `controller/environment.rs`; environment and worker ledger: `database/environments.rs`. Target cleanup plans: `targets/cleanup.rs`. Restart fallback has similar logic in `daemon/restart.rs` ~480-482, ~850-871, ~1079 (refuses reachable-target fallback) — reuse its reachability classification if it already has one rather than writing a second predicate (AGENTS.md: one owner per fact).


## Plan of Work

Find or create one function, owned by the environment code, that classifies a session's environment for lifecycle decisions, and make Resume branch on it. Implement the repair branch by reusing the worker attach and worker preparation code paths that ordinary daemon reattach and background worker upgrade use. Gate the existing archive path on the absent classification. Make rollback track what this Resume created.


## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir-wt/resume-shared-env`. Iterate with targeted `cargo test -p <package> <filter>` (package names are in each crate's Cargo.toml). Finish once with the full suites of touched crates and `cargo clippy --all-targets -- -D warnings` on the dev profile, outside the sandbox.


## Validation and Acceptance

Add an end-to-end test (AGENTS.md category 2) in an isolated `--instance`, following the existing e2e or controller integration harness that already provisions Podman sessions; use local Podman if Podman over SSH is not available in the harness, and say which. Scenario: parent session with a live child in its container; write a sentinel file newer than the parent's last checkpoint; force the parent into `Error` through a harness-preparation failure (the cheapest real way the harness allows); run Resume. Assert: same container and volume, sentinel present, the parent's native state continues, and the child is still reachable. Mark it `// Hard-won: 94d7 resume: Resume of a failed session deleted a container its live sub-agents used and restored older work`. If no harness can run Podman end to end, write the closest integration test at the Resume decision with a hand-written fake target engine and record why in Surprises. Extend the existing retirement race test in `controller/environment.rs` (~690) only if the change touches retirement.


## Idempotence and Recovery

Work is in a detached worktree and isolated instances only; never point a test build at the default instance.


## Interfaces and Dependencies

No new crates. One environment classification owner, used by Resume (and by restart fallback if it can share it without behavior change).
