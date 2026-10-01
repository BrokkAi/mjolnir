# Put container temporary files on native storage

This ExecPlan follows `.agents/PLANS.md` and must be kept current during implementation.

## Purpose / Big Picture

Issue #1212 reports test timeouts caused by temporary files on a container's slow writable overlay. Podman and Docker sessions, local and over SSH, will mount a private disk-backed volume at `/tmp`. Programs keep their ordinary temporary-file paths. Restart session already checkpoints, suspends, creates a new container, and restores the session, so it also applies this change to existing sessions.

## Progress

- [x] (2026-10-01) Inspected provisioning, cleanup, checkpoint capture, staged instructions, and the session restart path; claimed #1212.
- [x] (2026-10-01) Implemented scratch volume creation, permissions, failure cleanup, ordinary cleanup, and setup smoke cleanup.
- [x] (2026-10-01) Updated staged instructions and published container documentation; Astro check passed with no diagnostics.
- [x] (2026-10-01) Added lifecycle regressions and passed the compiled real Podman regression for root/nonroot users and both workspace policies. Verified Restart session uses the existing suspend/checkpoint/resume provisioning path.
- [x] (2026-10-01) Integrated nonconflicting upstream changes by fast-forwarding the current branch; Clippy and formatting passed on the combined tree.
- [x] (2026-10-01) Passed the full dev-profile workspace test suite, all-targets Clippy, formatting, documentation checks, and actual Podman storage regression.
- [x] (2026-10-01) Passed final controller integration checks after the second nonconflicting upstream fast-forward: 2,117 passed, zero failed, ten ignored. All-targets Clippy, formatting, and diff review passed; prepared the change for the required current-branch commit and upstream push.

## Surprises & Discoveries

The host's Cargo command is already an mbx shim and its target directory links into `/mnt/optane/mbx-targets`; preserve that layout. Podman and the agent-dev image are available locally. Docker is not installed locally. The dashboard's RestartSession action suspends and resumes the session; it is distinct from replacing a worker process inside an existing container. Checkpoint capture selects repositories and harness artifacts, not arbitrary container directories.

The setup smoke plan previously used raw launch/removal commands. It must share session launch and exact cleanup so named scratch volumes are not leaked. Borrowed sub-agents can start in older containers, so instruction staging inspects the actual `/tmp` mount before claiming native temporary storage.

Workspace runs exposed six existing tests that assumed a raw engine launch, a fixed cleanup command count, or interpreted rollback text in a shell script as a separately issued cleanup command. These assertions were updated to check executed command boundaries and actual mount arguments. Workspace tests use eight threads to reduce contention on this busy shared build host. Cargo's mbx configuration and target directory remain unchanged.

## Decision Log

Decision: Use a separate named native volume, `<container-name>-tmp`, for both Podman and Docker, independent of Podman's workspace policy. Rationale: the user selected disk storage and both runtimes; this avoids memory limits and does not require changing the optional workspace helper protocol. Date/Author: 2026-10-01, Codex.

Decision: Initialize `/tmp` as container root with mode 1777 before provisioning proceeds. Remove storage only after its owning container is confirmed removed. Rationale: nonroot images must work and cleanup must never delete files beneath a surviving writer. Date/Author: 2026-10-01, Codex.

Decision: Render native temporary-storage guidance from the engine's actual mount metadata. Rationale: adopted legacy containers and children borrowing them must not be described as having a volume they lack. Date/Author: 2026-10-01, Codex.

Decision: Validate restart rollout through the existing isolated checkpoint/resume tests and direct inspection of RestartSession, alongside real Podman provisioning and cleanup. Rationale: this change adds storage to the shared provisioning path without changing restart or checkpoint formats. A full daemon/container conversation restart was not run; the actual-container test covers the changed storage boundary, and existing isolated tests cover state restoration. Docker is unavailable on this host, so its lifecycle is exercised with the fake engine. Date/Author: 2026-10-01, Codex.

## Outcomes & Retrospective

Podman and Docker launches now use private native temporary storage, and owned storage survives failed container removal so cleanup can be retried safely. Actual Podman tests passed for root and nonroot image users with both workspace policies; failed-launch, foreign-ownership, exact-generation, and repeated-cleanup regressions passed. The full workspace suite and all-targets Clippy passed. Final controller integration checks and Clippy also passed after incorporating independently validated upstream commits published while this work was running. The implementation is complete and ready for publication. Docker coverage uses a fake engine because Docker is unavailable here; a full daemon/container conversation restart was not run. Existing sessions acquire the mount through Restart session after the updated controller is installed. No live host session or default daemon/store was changed.

## Context and Orientation

`mj-controller/src/targets/container.rs` builds launch commands and shell scripts that create Podman workspace volumes and Docker attachment volumes. `mj-controller/src/targets/cleanup.rs` removes exact owned resources and confirms absence. The scripts run through shared subprocess helpers and existing supervised lifecycle operations. `mj-controller/src/controller/worker_binary/install.rs` appends environment guidance to staged harness instruction files; the common text lives in its sibling `harness.rs`. `mj-cli/src/dashboard/actions.rs` implements RestartSession using suspend and resume. `mj-checkpoint/src/checkpoint/capture.rs` captures only selected repositories and native harness artifacts. Tests are colocated under `mj-controller/src/targets/tests.rs` and worker-binary tests.

## Plan of Work

Share scratch-volume ownership, creation, initialization, and removal behavior across launch scripts. Add the mount to Podman and Docker arguments and initialize it before the launch script succeeds. Include scratch cleanup in traps, regular close plans, and absence checks; retain storage and report errors when container removal fails. Derive names from the exact container generation, so retiring a move source cannot remove destination scratch storage. Preserve Apple and bare target behavior and existing explicit temporary-directory attachments.

Update container instructions to explain temporary storage and the actual session workspace path. Do not claim volume backing for runtimes or workspace modes without it. Document that scratch storage is not checkpointed and Restart session provisions it. Add behavior tests with fake container engines covering successful and failed launch, permissions initialization, foreign ownership, failed container removal, repeatable cleanup, and multiple generations.

## Milestones

First deliver launch and cleanup behavior with focused lifecycle regressions. Then update guidance and validate isolated real Podman containers, including root and nonroot image users and a scratch-file workload. Finally run required workspace checks and push the validated commit.

## Concrete Steps

Run from `/home/jonathan/Projects/mjolnir3`: `cargo test -p brokk-mj-controller targets::tests`, then `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Every Cargo test runs outside the restricted sandbox. Preserve the current mbx target layout. Any built `mj` invocation must use `--instance tmp1212`; unit tests keep their existing isolated data directories. Use exact disposable Podman resource names and identity labels for native-storage smoke checks. Never operate on the default instance or another session's resources.

## Validation and Acceptance

Successful launch leaves a native `/tmp` mount writable by the image user with mode 1777. Failed launch removes scratch only after container removal succeeds. Foreign resources remain untouched, errors remain visible, and repeating cleanup succeeds. Existing isolated checkpoint/resume regressions verify state preservation, while inspection of RestartSession confirms it provisions a new target through the changed launch path. Checkpoint capture selects repositories and native harness artifacts; arbitrary `/tmp` content is outside that selection. Compare file creation/deletion and synced writes at `/tmp` with workspace-volume storage. Required tests and Clippy pass on the dev profile.

## Idempotence and Recovery

Creation checks resource identity before reuse. Cleanup handles absent volumes from old sessions and interrupted launches. A failed removal leaves owned resources for a later retry instead of reporting successful cleanup. No schema, protocol, or configuration migration is required. Existing containers are not changed in place.

## Artifacts and Notes

Record validation output and limitations here as the implementation proceeds. The available upstream is `origin/master`; the current branch is `hel3`. The user explicitly requested a push after completion.

Real Podman smoke validation using the implementation's launch script passed for uid 0 and uid 1000. Both had a separate root-owned `/tmp` mount with mode 1777 and accepted a 256 KiB write. Across these measurements, create/delete took 0.075–0.079 ms per operation at `/tmp`, 0.105–0.117 ms in workspace storage, and 0.279–0.431 ms in the parent `/workspace`. Synced-write timings varied with host load. Exact labeled containers and volumes were removed afterward. `npm --prefix docs run check` reported zero errors, warnings, and hints.

`cargo test -- --test-threads=8` passed for the fix on upstream base `61030348`, including integration and documentation tests. After fast-forwarding to upstream `e65eb04b`, `cargo test -p brokk-mj-controller --lib -- --test-threads=8` passed with 2,117 successes and ten ignored tests. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all --check` passed on that final source. The actual Podman regression was run outside the sandbox with `MJ_INSTANCE=tmp1212` and passed all four user/workspace combinations in 41.52 seconds.

## Interfaces and Dependencies

Use existing CommandSpec, CommandExecutor, ownership labels, container generation names, and lifecycle stages. Keep public configuration and stored target locators unchanged. Add only private helpers for scratch storage and any private instruction arguments required to render the recorded workspace path. No new dependencies or workspace crates.

Revision note: Added the setup-smoke cleanup and observed-mount guidance decisions, corrected the focused Cargo package name, and recorded native smoke and documentation validation.
