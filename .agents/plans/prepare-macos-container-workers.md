# Prepare Linux workers before macOS development launches

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Running `scripts/run.sh` on a Mac with Docker or Podman must prepare the Linux worker needed by containers before starting the controller daemon. Installing or rebuilding a worker must also refresh an already-running development daemon, whose worker source snapshot otherwise remains stale.

## Progress

- [x] Identify the missing development Linux worker; the npm installation already includes both Linux architectures.
- [x] Add a container-based source build to the macOS launcher and fix empty optional arguments under macOS Bash 3.
- [x] Build and execute the ARM64 musl worker in the standard agent image.
- [x] Add worker modification checks to development daemon refresh and regression tests.
- [x] Pass debug/release launcher tests and the failed-build launch barrier test.
- [x] Full Rust tests, Clippy with warnings denied, formatting, and shell checks pass.
- [x] Full launcher builds both workers and restarts the daemon as PID 54756.
- [x] A real Docker session using the mjolnir bundle and Codex profile starts successfully without a prompt and is removed through the daemon afterward.
- [x] An unchanged repeated container build preserves the exported artifact's timestamp.
- [x] Complete the validated changes for the current-branch commit accompanying this plan.

## Surprises & Discoveries

The previous daemon restart selected the development executable without its Linux workers. macOS `scripts/run.sh` built only a native macOS worker, while the installed npm bundle included the missing portable executables. The standard cached agent image has Rust 1.96.0, musl-gcc, and the ARM64 musl target, so the exact checkout can be built without another toolchain installation. Bash 3 with `set -u` rejects expansion of an empty array, including the old optional release arguments.

The full doctor smoke reports the worker ready but finds a separate Docker Desktop OverlayFS attachment failure: the daemon VM cannot resolve host paths used by the local overlay volume driver. Ordinary session startup without attached directories must be verified separately; do not claim that all doctor checks pass.

## Decision Log

Build from current source in the standard agent image instead of silently mixing the development controller with packaged workers. Keep native worker builds for raw local sessions. Use a functioning local Docker or Podman engine when available; without one, report that only native local-session assets were built. Keep Linux build caches in a volume specific to this checkout and architecture, with only the completed executable exported to `target/worker/<triple>/<profile>/mj-worker`. Publish atomically, execute the worker before publication, and preserve the existing artifact and timestamp when unchanged.

Compare portable worker modification times with daemon startup in the existing opt-in development refresh path. This fixes newly installed workers as well as worker-only code changes without changing ordinary installed-app daemon behavior. File and process inspection remains in a supervised blocking task.

## Outcomes & Retrospective

Implementation and validation are complete. The ARM64 Linux worker reports `mj-worker 2.6.0`, and doctor identifies the expected isolated development musl worker. `scripts/run.sh -- daemon restart` prepared both workers and started daemon PID 54756. The actual daemon registered verification session `735250c02513185300b113876cf00ec5` for the mjolnir bundle, Codex profile, Docker target, and no additional mounts or initial prompt; `wait_create_session` returned Done, then `force_destroy_session` returned Done. Ordinary session startup now works through the full provisioning and worker path. The separate Docker Desktop OverlayFS attachment smoke failure remains outside this missing-worker fix.

Full `cargo test --quiet` exited 0, including the worker freshness regression; `cargo clippy --all-targets -- -D warnings` exited 0 after removing unnecessary borrows in that test. Three launcher behavior tests pass. Formatting, shell syntax, and diff checks pass. A repeated actual worker build completed in 0.19 seconds of Cargo work, executed the worker successfully, and preserved its modification timestamp.

## Context and Orientation

`scripts/run.sh` builds development assets and launches Cargo. `scripts/build-linux-worker.sh` builds the portable worker using the selected container engine. `mj-cli/src/daemon.rs` replaces stale development daemons before connection; `development_workers_changed_since` now checks available portable artifacts. `scripts/run.test.mjs` exercises the launcher with temporary fake tools, requiring the Linux artifact to exist before the client starts.

## Plan of Work

Build the native worker and then the portable container worker on macOS. Reuse the standard image's pinned toolchain and existing controller discovery path, with separate native and Linux build storage. Extend daemon freshness checks and test initially missing, unchanged, and rebuilt worker files. Validate the launcher and an ordinary Docker session, then restart and commit.

## Concrete Steps

From `/Users/ryansvihla/code/mjolnir`, run `node --test scripts/run.test.mjs`, `bash -n scripts/run.sh scripts/build-linux-worker.sh`, elevated `cargo test --quiet`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Run elevated `scripts/run.sh -- doctor --json --smoke` and record specific results, then `scripts/run.sh -- daemon restart`. Use the existing daemon creation protocol for a disposable session without an initial prompt, and remove only that verification session through the daemon afterward.

## Validation and Acceptance

Both debug and release launchers provide their portable worker before client execution. A failed worker build prevents launch. An unchanged worker does not trigger refresh; a newly available or rebuilt worker does. The real Linux worker executes in the configured image, is discoverable before daemon startup, and an ordinary Docker session gets beyond worker installation. Report independent failures explicitly.

## Idempotence and Recovery

Container builds use `--rm` and leave persistent compilation caches. A failed build never replaces the completed worker. Normal daemon restart preserves detached workers. Remove disposable sessions only through their owning daemon so process termination precedes filesystem cleanup. Stage only task files and do not push without authorization.

## Artifacts and Notes

Doctor evidence is saved under `target/dev-worker-validation/doctor.json`. It reports `worker.docker` ready and the separate OverlayFS smoke failure. Build output confirms the executable is a statically linked aarch64 Linux ELF.

## Interfaces and Dependencies

Reuse `WorkerBinaryAvailability`, `worker_binary_prerequisite_for_arch`, standard filesystem timestamps, the existing daemon management protocol, and the standard agent image. No crate or dependency additions are needed.

Revision 2026-09-10: record the missing-worker regression, implementation, and validation work prompted by the user's next launch failure.

Revision 2026-09-10: record successful live ordinary session creation and teardown, completed checks, and the separate attachment limitation.
