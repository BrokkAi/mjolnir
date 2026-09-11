# Support Docker attachments across the macOS VM boundary

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Local Docker on macOS must pass doctor smoke checks and launch sessions with isolated writable attachments. Colima runs the Docker daemon, the service creating mounts and containers, inside Linux. Host temporary and cache paths cannot be used directly as Linux OverlayFS backing directories. Keep shared source directories as the live read-only lower layer and store only upper/work directories in Docker-managed volumes.

## Progress

- [x] (2026-09-11) Confirmed the active Colima VM cannot access the macOS temporary directory.
- [x] (2026-09-11) Proved shared Colima source plus Docker-owned upper/work works without copying.
- [x] (2026-09-11) Replaced the unvalidated snapshot approach with daemon-owned writable backing storage and ordered cleanup.
- [x] (2026-09-11) Live Colima test passed: shared source, late-added file, 256KB fixture, symlink, isolated writes, cleanup, and production doctor smoke.
- [x] (2026-09-11) Full `cargo test`, `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks passed.
- [x] (2026-09-11) Built `target/debug/mj`; implementation and validation are complete and included in the current-branch fix commit.

## Surprises & Discoveries

Colima shares the home directory with virtiofs but not the reported `/var/folders` path. Sharing temp alone does not provide a Linux-compatible upper filesystem. The agent-dev image runs as UID 1000 while macOS source files belong to UID 501. Preserve ordinary Docker ownership semantics rather than changing session identity; copy only source-root ownership/mode to the upper directory. Use writable disposable fixtures to test mounts independently of UID matching.

## Decision Log

Keep source-sharing semantics. A live probe on Colima successfully mounted a home-directory source with upper/work inside a Docker volume, wrote through the overlay, and preserved the original. The earlier proposed copying approach was rejected because it unnecessarily changed attachment behavior. Use a short-lived labeled helper only to create upper/work directories in a backing volume, with no source copying or privileged mode. Create macOS doctor fixtures under the shared home directory instead of the unshared system temp directory. Retain legacy cache cleanup.

## Outcomes & Retrospective

The no-copy implementation passes the live Colima attachment and doctor smoke test. The full default-member Cargo test suite, clippy with warnings denied, formatting, and diff checks passed. The live test covers shared data and permission-neutral fixtures; ordinary host/image UID permissions remain unchanged and are documented.

## Context and Orientation

`src/hel_targets.rs` builds supervised subprocess plans. Its original `DOCKER_OVERLAY_RUN_SCRIPT` allocated backing paths on the CLI host, but Docker interprets them on its daemon host. The updated script uses a labeled backing volume initialized by a non-privileged helper. `docker_container_run` serves local and SSH targets. `run_docker_overlay_smoke_test` reproduces the bug with a macOS tempfile. `close_plan` removes session containers and volumes. Tests are in `src/hel_targets/tests.rs` and doctor tests in `mj-controller/src/hel_doctor.rs`. `mj-controller/src/hel_controller/provisioning.rs` handles attachment filesystem compatibility. Runtime documentation is `docs/DOCKER.md`.

## Plan of Work

Milestone one replaces host upper/work paths with a Docker backing volume per attachment. A labeled helper creates its directories. Inspect the daemon mountpoint and create the existing overlay using the original source and daemon-owned upper/work. Verify ownership before reuse or removal. Remove helpers before overlays, and overlays before backing volumes. Report cleanup failures and retain backing storage if teardown fails.

Milestone two adds fake-backed initialization/rollback tests and an opt-in Colima test using a shared home fixture, more than 64KB of data, symlinks, source files added after launch, write isolation, and cleanup. The late-added file proves the source remains shared rather than snapshotted. Document shared source requirements and macOS smoke directory selection. Keep the existing source filesystem compatibility policy.

## Concrete Steps

Work in `/Users/ryansvihla/code/mjolnir`. Run elevated focused `cargo test -p brokk-mj-core docker`, then the opt-in test on Colima using the cached agent-dev image. Run elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Commit only changed files on the current branch.

## Validation and Acceptance

Live testing must show shared source files remain visible, including files added after launch, container writes leave the originals unchanged, and close removes all test containers and volumes. The production doctor smoke path must succeed on Colima. Required Cargo tests and clippy must pass.

## Idempotence and Recovery

Use unique test session names and deterministic owned resource names. Validate labels before reusing or deleting resources. Stop containers before deleting volumes and retain backing volumes whenever an overlay remains. Preserve cleanup of legacy host cache directories.

## Artifacts and Notes

The active daemon reports Ubuntu 24.04.4 LTS and `/var/lib/docker`; Colima uses virtiofs. The failing `/var/folders` source is absent inside the VM.

## Interfaces and Dependencies

Reuse `CommandSpec`, `ProcessExecutor`, `docker_container_run`, and `close_plan`. Add no crate or helper image dependency. All process work remains within existing supervised background execution.

Plan revised after the user challenged snapshot semantics and a live Colima probe proved copying unnecessary.

Live validation passed with the cached agent-dev image; no file snapshot or session user change remains in the implementation.

Final validation: full Cargo tests passed, clippy passed with warnings denied, formatting and diff checks passed, and `cargo build -p brokk-mjolnir --bin mj` succeeded. The original Colima mount failure is resolved without copying attachments.
