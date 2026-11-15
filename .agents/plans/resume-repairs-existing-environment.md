# Resume repairs a failed session in its existing environment

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

On 2026-11-14 session 94d7c3846306d798c13d5c32fe7b9c2c (Claude, Podman over SSH on target morannon-podman, workspace in a Podman volume) went to the `error` state because a background worker upgrade failed during harness preparation. Its container kept running, and two of its Mjolnir sub-agents (sessions started by the parent's `spawn` tool, which run inside the parent's container through a borrowed locator) were still working in it. `mj resume --session 94d7…` then stopped and removed the container, deleted the workspace volume, provisioned a new container and restored the parent from a recovery archive taken 14 minutes before the failure. The two children lost their environment permanently and the parent lost its newest work, although the old container and volume were intact.

The root cause is a decision with no evidence behind it: in `mj-controller/src/controller/resume.rs` (~1727) a non-Move Resume of an `Error` session that retains its target calls `environment::remove_runtime_preserving_workspace`, which for a volume-backed workspace reclaims everything (`controller/environment.rs` ~513-538, `Reclamation::acquire` ~69-192 stops borrowers, `targets/cleanup.rs` ~458-524 removes container and volume). Being in `Error` does not prove the environment is unusable; a failed harness preparation says nothing about the container.

After this change, Resume of a failed session whose environment still exists brings the session back in that environment: same container, same volume, current files and native harness state, and its live children keep running. Restoring from the archive into a new environment happens only when the old environment is proven absent. A user can see it work by repeating the incident in an isolated instance: the parent resumes with its newest file present and the child finishes its turn.


## Progress

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

- Validation fixture corrections: container creation needs explicit CPU/RAM allocation and the container harness's lifecycle wait, rather than generic five-second HTTP / twenty-second snapshot waits. Child completion must track its original command ID because a handback reminder can immediately start another turn. Shell `kill -- -PID` was replaced with Python `killpg`. An ACP initialization error occurs after coordinator startup; an invalid private Codex config produces the required terminal preparation state before startup.

- Implementation: Restart already had an engine availability probe. It now delegates to the environment module, preserving Restart's existing reachable/missing/unreachable behavior; Resume adds worker liveness classification and treats unreachable as unknown.
- Implementation: API and terminal entry checks previously refused repair without a checkpoint. They now allow retained failed sessions through; archive preflight remains required after proven absence.
- Validation: local Podman works outside the sandbox. The real-container regression reuses `container_upgrades.py`, with its deterministic ACP fixture. It injects the durable Error result while the isolated daemon is stopped rather than reproducing the nondeterministic background upgrade failure. The live and dead cases passed with a child staying busy; an additional passing case induces a real private-profile preparation failure, restores that profile, and retries the live failed worker.

- (mapping, 2026-11-14) `ResumePlan::InPlace` today means converting a checkout, not repairing a container; do not reuse the name for the new path without renaming one of them. `in_place.rs` restore from checkpoint can roll back newer state, so the repair path must not restore the archive over a present workspace.
- Sub-agent placement: `controller/subagents.rs` ~349 and ~700-760 create and propagate borrowed locators (`borrowed_from`); `database/environments.rs` ~182-268 binds children to the parent environment; `environments.rs` ~348 marks an environment Retiring and snapshots its workers, ~535 admits only while Active.


## Decision Log

- Implementation decision: a present placement keeps its current profile and target; changing them follows repair through Move. Requested resource or mount changes produce a notice that the existing container settings were kept. Date: 2026-11-14.

- Implementation decision: arbitrary attach failures leave a live worker alone. A typed `HarnessPreparationFailure` is different: `mj-worker/src/worker_runtime/unix.rs` keeps the relay available after this terminal failure but has no coordinator and cannot dispatch ACP work. Resume can replace this failed boot in the same root, retaining its journal. Any old boot intent is settled only when it names the current placement. This completes the planned “cannot serve” case without killing a worker on a lost handshake. Date: 2026-11-14.

- Decision: the `Error` state alone never authorizes environment teardown in Resume. Resume classifies the environment through the target engine: reachable live worker, container present without a usable worker, container proven absent (engine says it does not exist), or unknown (engine unreachable or ambiguous). Date: 2026-11-14.
- Decision: present environment → repair in place. First try to reattach the existing worker and replay its journal. If the worker is gone or cannot serve, start a fresh worker in the same container with the existing workspace and native harness home, using the same worker preparation the background upgrade uses. Do not restore the checkpoint archive over the present workspace or native state. Children are not stopped. Date: 2026-11-14.
- Decision: proven absent → the existing archive restore into a new environment, as today. Unknown → do not tear anything down; fail the Resume visibly, keep the environment and its placements, and tell the user what was unreachable and that Resume can be retried. Per AGENTS.md "never leave the user stuck", the error must say the next step, and the TUI must keep offering Resume. Date: 2026-11-14.
- Decision: a failed repair keeps the environment and the session in `Error` with the repair failure as its error, so retrying Resume repairs again. `rollback_failed_resume` must only remove resources this Resume created. Date: 2026-11-14.
- Decision: Destroy, Close, Suspend, Move and container upgrade keep their current child semantics in this change (deliberate operations that stop or move children). Only Resume's evidence-free teardown is in scope. Note in Outcomes whether upgrade-container's child admission looked sound, without changing it. Date: 2026-11-14.
- Decision: no schema or protocol change is expected. If a new durable intent turns out to be required, add a new migration classified compatible or breaking per AGENTS.md; never rewrite migration 122. Date: 2026-11-14.


## Outcomes & Retrospective

Resume now derives teardown permission from proven absence, rather than `Error`. Existing environments reattach their worker or restart only the parent worker in its retained root. A terminal preparation failure is repaired in place after its cause is corrected; unknown availability and failed repairs preserve the environment. Archive restoration remains the absent-environment path. Rollback distinguishes retained runtime ownership from placements created by the attempt. No schema or protocol change was needed.

The real local Podman regression passed live reattachment, dead-worker repair with no checkpoint metadata, and repair following an actual terminal profile-preparation failure, while preserving the container, volume mounts, newer sentinel, native session ID and the child's original command. The child completed after release. This uses the deterministic ACP fixture and local Podman, not a live provider or Podman over SSH. Initial `Error` states are injected while the isolated daemon is stopped; the final preparation failure is produced by temporarily invalidating and then restoring the parent's private Codex configuration.

Final controller validation had one unrelated failure: `daemon::tests::move_task_panic_reports_failure_and_releases_mutation_hold` timed out at its existing two-second wait (`daemon/tests.rs:2611`). Its isolated rerun passed in 0.81 seconds. All other controller tests passed, as did the full TUI suite, doc tests and workspace clippy. Evidence is retained in the sub-agent report directory; the final handback records the commit and review verdict.

The existing explicit container-upgrade admission appears sound for this issue: `daemon/restart.rs::try_commit_container_upgrade` serializes cancellation through the lifecycle owner and invokes `database/session_restart.rs::try_commit_container_upgrade`, whose transaction refuses commit while children have live workers or targets, or accepted delegation requests remain. Child registration uses that database writer too. This behavior is unchanged.


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
