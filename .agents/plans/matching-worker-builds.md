# Install only workers built with the running controller

This ExecPlan follows `.agents/PLANS.md` and is a living document.

## Purpose / Big Picture


Issue #1138 describes new containers receiving an old worker executable and then rejecting the controller's launch configuration. After this change, the controller will select only workers with its package version and source revision. Missing or stale workers will produce an actionable error before provisioning. Existing stopped sessions will receive the matching worker before their launch configuration is refreshed and their worker restarts.

## Progress


- [x] (2026-09-24) Read the issue, claimed it with `agent-in-progress`, and traced acquisition, pinning, provisioning, recovery, and idle upgrades.
- [x] (2026-09-24) Added shared package/revision identity, retained worker marker, and unknown-field diagnostics.
- [x] (2026-09-24) Enforced matching candidates, snapshots, cache publication, downloads, uploads, prepared recovery, and provisioning preflight.
- [x] (2026-09-24) Added isolated regressions for stale candidates, overrides, pins, downloads, digest equality, provisioning, and recovery retry. The real stopped-Docker regression passes with a portable Linux worker and a non-root container user.
- [x] (2026-09-24) Full dev-profile suite, Clippy, formatting, build-script tests, Linux worker build, stripping, build metadata verification, and final diff review passed. Prepared the completed change for the required local commit.

## Surprises & Discoveries


Recovery already refreshes the binary before the launch configuration, and idle upgrades already reserve an idle worker before promotion. Their digest comparisons become valid once the source itself is verified. New-session provisioning currently checks the harness but does not preflight the worker source. The worktree begins at detached HEAD; repository instructions prohibit creating or changing branches, so the completed work will be committed at the current HEAD.

The Linux build container cannot follow a Git worktree's absolute host-side metadata link. `scripts/build-linux-worker.sh` now passes `MJ_BUILD_REVISION`. Cargo also rebuilds perpetually when asked to watch a nonexistent file; the build script watches only metadata that exists and watches the refs directory for a packed ref becoming loose. The existing API suspend test reads controller state outside its fake backend; supplying explicit private configuration/data directories lets it finish without depending on the host's installed state.

## Decision Log


Use the shared `mj-core` crate to derive package version plus source revision once, and retain a marker in the worker executable. Source revision comes from Cargo's packaged VCS metadata or the checkout's Git metadata, with an explicit build environment override for source archives. Do not run a Linux worker to inspect it on macOS. Preserve existing idle admission and staged replacement rather than adding a second upgrade mechanism. These decisions were made on 2026-09-24 to cover release, registry, and development builds without interrupting active turns.

Keep the full stamp referenced only by the worker entry point (and test fixtures); the controller scans a prefix and compares the shared build identity. Use `memchr` for efficient byte scanning of large development executables. Generate the stamped fake worker during compilation, preserving the existing rule against writing executables while parallel tests fork. These decisions were made on 2026-09-24 while implementing and validating the marker.

## Outcomes & Retrospective


Implementation and validation are complete. Real Docker recovery verified both the installed executable digest and the running worker's reported digest, then read its checkpoint-only status to prove it consumed the refreshed configuration. The Linux marker survives stripping. The full dev-profile workspace suite and Clippy passed; this includes 1,631 controller tests, 466 core tests, and 573 worker library tests, plus other workspace suites, integration tests, and doctests. Existing opt-in tests remained ignored except the explicitly executed Docker regression. No database or protocol revision was needed: verifying the source repairs every existing install/upgrade path's digest comparison.

## Context and Orientation


The daemon is the controller process that manages sessions. A worker is a separate executable copied into a local or remote session and responsible for running its harness. `mj-controller/src/controller/worker_binary/binary_source.rs` chooses candidate paths and snapshots them into a content-addressed cache (directories named for the file's SHA-256 digest). `binary_select.rs` resolves target architecture and downloads optional worker sources. `install.rs` uploads initial files; `upgrade.rs` replaces executable files through a temporary name and atomic rename. `worker_restart.rs` handles idle admission. `session_manager/recovery.rs` starts stopped targets and refreshes their worker and configuration in sequence. `controller/provisioning.rs` must reject unavailable workers before creating containers. `mj-core/src/worker_launch.rs` parses launch configuration.

## Plan of Work


First add build metadata generation in `mj-core/build.rs` and a shared `worker_build` module. Embed the delimited marker through a live reference from `mj-worker/src/main.rs`. Validate identity by reading executable bytes, including missing, conflicting, and stale identities. Improve unknown-field diagnostics in `WorkerLaunchConfig::read`.

Then make every local candidate eligible only after validation, continuing past stale candidates and collecting their paths and identities for errors. Validate copied snapshots and existing cached files, and remove the re-resolution path's uncached fallback. Validate remote downloads before cache publication and all binary upload entry points before commands execute. Run worker preflight before provisioning. Keep recovery's digest gate: equality to an already validated source proves that the installed binary matches this controller.

Finally add isolated source-selection and recovery regressions, exercise the retained marker in a built worker, and run required workspace validation. Preserve the existing isolated test helpers and use `--instance issue-1138` for any new-build CLI invocation.

## Concrete Steps


Run from the repository root. Use `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. Focused tests may use `cargo test -p brokk-mj-controller worker_binary` and `cargo test -p brokk-mj-core worker_build`. Do not redirect build output or build storage to temporary directories. If container tooling is available, use a dedicated test instance and containers; otherwise exercise stopped-container recovery with a command executor that models container state and records installed bytes.

The executed full-suite command is:

    MJ_INSTANCE=issue-1138-suite MJ_CONFIG_DIR="$PWD/target/issue-1138-suite/config" MJ_DATA_DIR="$PWD/target/issue-1138-suite/data" cargo test -- --quiet

Build the Linux worker with `bash scripts/build-linux-worker.sh docker --profile dev`. On this arm64 host, the real-container regression is:

    MJ_INSTANCE=issue-1138-docker MJ_WORKER_BINARY="$PWD/target/worker/aarch64-unknown-linux-musl/debug/mj-worker" cargo test -p brokk-mj-controller stopped_docker_session_recovers_with_the_current_worker_build -- --ignored --nocapture

This test refuses any other instance identity, labels its own disposable container, seeds an existing relay journal, stops the container before recovery, and removes the container afterward even on failure. The explicit config/data overrides used for the suite may also be supplied here.

## Validation and Acceptance


A stale earlier candidate must be skipped in favor of a matching later candidate. An all-stale set must name the rejected paths and expected build and execute no container creation. Missing stamps must be rejected. Pinned or downloaded cache entries with stale identities must never be returned for upload. Recovery must leave the old configuration untouched when no valid source exists, then install a valid binary and configuration and restart on retry. Active workers must remain untouched until existing idle admission succeeds. Full tests and Clippy must pass, or environmental limitations must be recorded precisely.

## Idempotence and Recovery


Cache publication remains atomic and content-addressed. Worker replacement continues to stage through `hel.next`, restore ownership for non-root container users, and rename. Configuration updates follow successful binary installation; failures propagate and can be retried by existing recovery. Tests use private directories and named instances and must not alter live sessions.

## Artifacts and Notes


The original lookup returned the first `is_file` candidate. The pinned snapshot's usability test also checked only file existence. These are the key behaviors the regressions must distinguish.

Verified evidence:

    stopped_docker_session_recovers_with_the_current_worker_build ... ok
    Full isolated cargo test: exit 0
    cargo clippy --all-targets -- -D warnings: Finished dev profile
    node --test scripts/build-alignment.test.mjs: 4 passed
    Original and stripped Linux worker: 2.20.0+f91da06f960c9a719680b17b62c57b45f53e864c

The compiled build script was also exercised against synthetic Cargo VCS metadata, worktree refs, packed refs, and an explicit revision override; each emitted the expected build identity.

## Interfaces and Dependencies


Add `mj_core::worker_build::BUILD_ID`, a worker marker constant, and `verify_worker_build(path: &Path) -> anyhow::Result<()>`. Reuse existing SHA-256, filesystem, staging, subprocess, target recovery, and isolated test helpers. No new workspace crate or database migration is needed.

Revision note: initial plan records the failure mechanism and implementation sequence before editing runtime code.

Revision note (2026-09-24): recorded implemented checks, packaging/worktree discoveries, private test commands, and successful real-container and build-metadata validation.

Revision note (2026-09-24): recorded the completed workspace test run and final review before the local commit.
