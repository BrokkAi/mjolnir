# Use the host mbx in container sessions

This ExecPlan is a living document. Maintain `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Container sessions share the host's mbx store without binding a version-managed executable path into the container. Mjolnir atomically refreshes a copy at `<cache>/.mjolnir/bin/mbx` inside the cache directory already mounted read-write. New containers, resumed sessions, and periodic reconciliation all use this copy. The worker's marked Cargo and `mbx` launchers execute it after an in-container version check. Missing or too-old native mbx leaves old cache copies intact and new sessions uncached.

The observable result is that a compatible new container reports matching `mbx --version` and `cargo --version`, while a host without a usable mbx starts sessions without the shared cache and explains how to install or upgrade it.

## Progress

- [x] (2026-10-04) Read `AGENTS.md`, `.agents/PLANS.md`, and the mbx delivery map; confirmed the worktree is clean and on `mbx-host-session`.
- [x] (2026-10-04) Confirmed mbx's supported Cargo-shim variables and PATH exclusion behavior in the local mbx source.
- [x] (2026-10-04) Implement resolved host probing, shared eligibility classification, mount persistence, and version verification before installing the Cargo launcher.
- [x] (2026-10-04) Remove per-session executable delivery, use the recorded host executable for cleanup, and preserve the legacy-container skip path.
- [x] (2026-10-04) Update mount, compatibility, serialization, doctor, setup-help, and documentation coverage.
- [x] (2026-10-04) Finish full controller and TUI test suites and format/diff checks; Podman smoke test passed.
- [x] (2026-10-04) Finish workspace clippy and review the diff.
- [x] (2026-10-04) Commit the validated changes on the current branch.
- [x] (2026-10-05) Replace the removable single-file bind mount with an atomic cache-directory copy; sync during provisioning, resume, and service reconciliation.
- [x] (2026-10-05) Merge the install-button branch, remove its duplicate probe, and keep one `NativeMbx` contract.
- [x] (2026-10-05) Remove only marker-owned shims on verification failure; preserve markerless legacy files.
- [x] (2026-10-05) Run fmt, workspace clippy, core/controller/TUI/CLI suites, and the restart-after-atomic-replacement Podman smoke test.
- [x] (2026-10-05) Commit the merged branch and follow-up as `3df456d3`.

## Surprises & Discoveries

- Observation: mbx's own POSIX Cargo shim is a short launcher that exports `MBX_CARGO_SHIM_MODE=1` and `MBX_CARGO_SHIM_PATH` before execing mbx. Its shim dispatch uses the reported script path to remove that launcher from PATH before resolving real Cargo.
  Evidence: `~/Projects/mr-boxington/crates/mbx/src/cli/setup.rs` and `crates/mbx/src/cli/shim.rs`.
- Observation: Podman refuses to start an existing container when a single-file bind source was removed, as can happen when mise prunes an old versioned install directory.
  Evidence: user-confirmed Podman reproduction; the shared cache mount remains available at the same absolute path.
- Observation: the cache copy can be refreshed without restarting running mbx processes by copying to a temporary file in the same directory, checking version and size, then renaming atomically.
  Evidence: `SYNC_MBX_BINARY_SCRIPT` and the inode-preservation test in `mj-controller/src/controller/mbx.rs`.
- Observation: installation and cache delivery now share `NativeMbx` and `probe_native_version` in the parent mbx module; install no longer carries a second probe protocol.
  Evidence: `mj-controller/src/controller/mbx/install.rs` imports the parent probe and type.

## Decision Log

- Decision: Initially use `/opt/mjolnir/mbx/mbx` as the fixed in-container binary path, then replace the single-file mount after a Podman restart reproduced a missing-source failure.
  Rationale: A version-manager upgrade can remove the source path and prevent an existing container from starting. The durable design copies the native executable into `<cache>/.mjolnir/bin/mbx`, under the cache directory already mounted read-write.
  Date/Author: 2026-10-04 / Codex; superseded by the user decision below.
- Decision: Persist the resolved host executable path as an optional field on `SessionBuildCache`; its JSON column needs no SQL migration because it stores the serde representation.
  Rationale: A resumed legacy record with no path must be distinguishable from a newly provisioned container that recorded a mount.
  Date/Author: 2026-10-04 / Codex.
- Decision: Initially preserve all existing launchers after failed verification; the follow-up instead removes only generated launchers carrying Mjolnir's exact marker.
  Rationale: Resumed new-scheme sessions must fall through to the image's Cargo if the shared binary stops working. Markerless legacy executables are never removed.
  Date/Author: 2026-10-04 / Codex; superseded by the user decision below.
- Decision: Supersede the single-file bind mount with `<cache>/.mjolnir/bin/mbx` in the shared read-write cache mount. Refresh by host-side `cp` to a same-directory temporary file, `chmod 755`, then atomic rename; compare reported version and size to skip unchanged copies.
  Rationale: Container mount sources must remain present for the lifetime of existing containers; package-manager upgrades can remove versioned source paths.
  Date/Author: 2026-10-05 / User and Codex.
- Decision: On failed verification, remove `bin/cargo` and `bin/mbx` only when each is a regular file containing the exact `# mjolnir-mbx-shim` marker. Leave markerless files, including legacy copied binaries, in place.
  Rationale: The user authorized cleanup of Mjolnir-generated files and explicitly prohibited touching anything without the marker.
  Date/Author: 2026-10-05 / User.
- Decision: The install-button branch's probe is removed in favor of the existing host probe and shared `NativeMbx { program: PathBuf, version }` type. The successful install action attempts an immediate cache sync; periodic reconciliation remains the recovery path if that sync fails.
  Date/Author: 2026-10-05 / User and Codex.

## Outcomes & Retrospective

The previous single-file-mount implementation and its checks were committed as `fb5aec43`; this follow-up supersedes that delivery detail after the user reproduced its container-start failure. The install-button branch (`dcab2736`) is merged. Host-side cache copies now refresh atomically during provisioning, resume, reconciliation, and immediately after install when possible. Failed verification removes only marked launchers. Formatting, workspace Clippy, the controller/core/TUI/CLI suites, and Podman restart smoke all passed. The smoke used `rust:1.96.0-trixie`: both launchers reported `cargo 1.96.0` and `mbx 1.22.0` before and after the stopped container's in-cache executable was atomically replaced.

## Context and Orientation

`mj-controller/src/controller/mbx.rs` owns the shared native probe, compatibility classification, cache path, and host-side atomic refresh. `mj-controller/src/controller/provisioning.rs` creates containers and later calls `prepare_worker_files`; that path is also used when resuming or starting sub-agents. `mj-controller/src/controller/worker_binary/launch.rs` verifies the in-cache copy inside the container and installs marked Cargo and `mbx` scripts. `mj-controller/src/controller/mbx/service.rs` refreshes copies for configured machines and active new-scheme sessions. `mj-controller/src/controller/mbx/install.rs` contains the machine-settings installer and reuses the shared probe. `mj-controller/src/controller/mbx/release.rs` uses the in-cache copy for cleanup while preserving bare-native and legacy cleanup. `mj-core/src/state.rs::SessionBuildCache` is serialized into the existing `build_cache_json` column, so adding an optional serde-defaulted field does not change SQL schema.

`CacheHost` represents the machine where Podman or Docker creates the container. For SSH targets, host commands run remotely; the path returned by the probe must therefore be an absolute path on that remote machine. Only supported Linux container hosts are eligible. `MBX_VERSION` remains the minimum version compatible with the shared store; newer versions are accepted.

## Plan of Work

Keep `probe_native_version` as the one shared PATH/`$HOME/.local/bin/mbx`/`$HOME/.cargo/bin/mbx` probe, returning its canonical `PathBuf` and version for doctor, cache selection, and install/upgrade. Centralize absent, too-old, compatible, and probe-error classification so preview, doctor, and new-session eligibility agree. Absent and too-old hosts must have no shared cache; there is no Mjolnir-binary fallback.

Store the in-cache path on `SessionBuildCache` as `mbx_binary: Option<String>` with a serde default; the optional field remains the marker for the new delivery scheme. Sync the resolved native executable into `<cache>/.mjolnir/bin/mbx` before using a newly created or recreated cached container. Reconciliation refreshes the configured cache and active new-scheme session cache directories. Resume/sub-agent/checkpoint preparation refreshes its recorded cache before verifying that exact copy in the container. After installation, try an immediate sync and leave retry to reconciliation if that attempt fails.

After container creation and before installing the session launchers, execute the synchronized cache copy's `--version` inside the container and compare it with the version returned by that sync. If it cannot run or differs, report the cache issue, remove only marked Mjolnir launchers, and continue with image Cargo. Otherwise write marked POSIX `bin/cargo` and `bin/mbx` scripts that execute `<cache>/.mjolnir/bin/mbx`; the Cargo script sets mbx's two shim environment variables. Keep `MBX_CONFIG_SCRIPT` and the existing cache environment.

Keep the install-button module's pinned download and digest support, but remove binary downloads and uploads from per-session delivery. Cleanup should use the cache copy for new-scheme records and keep bare-native and legacy cleanup working. On resume and sub-agent preparation, records without `mbx_binary` must leave old `bin/mbx` and `bin/cargo` untouched.

Update focused tests that encode the old fallback, mounts, copies, hard-link, sync decisions, marker cleanup, and cleanup assumptions. Update container and configuration guidance. Format, run workspace clippy with warnings denied, run core/controller/TUI/CLI suites, and smoke-test a read-write temporary cache mount in Podman: exercise the shim, atomically replace its binary while the container is stopped, and confirm it restarts. Commit the merge and follow-up changes on the current branch.

## Concrete Steps

Run commands from the repository root, `/home/jonathan/Projects/mjolnir/.mj/clones/da87dcd462b28e417d1b68b8df1d6346/.claude/worktrees/mbx-host-session`.

1. Merge `mbx-install-button`, resolve the shared probe/type conflict, and remove temporary dead-code allowances.
2. Implement an atomic host copy in the already-mounted cache, with synchronization from provisioning, resume, service reconciliation, and successful installs.
3. Verify inside the container against the version just synchronized; remove only marked launchers on failure and preserve markerless legacy files.
4. Run fmt, workspace clippy, and core/controller/TUI/CLI test suites in the dev profile.
5. Use Podman with a throwaway directory mounted read-write at the same path. Verify both commands, stop the container, atomically replace the cache copy, restart, and verify both commands again.
6. Review the diff, stage only task files, and commit on the current branch. Do not push.

Success requires formatting and clippy to pass; all touched crate suites to pass or report named failures with logs; one shared probe/type; absent or too-old native mbx to leave old copies intact and new sessions uncached; cache copies to refresh atomically; verification failures to remove only marked launchers; and the restarted Podman smoke container to run both commands from the replacement.

## Validation and Acceptance

Tests must establish the host candidate order and canonical path, status-1 absence behavior, cache-off classification for absent and too-old hosts, atomic cache-copy refresh and no-op detection, serde compatibility with an old `SessionBuildCache` JSON record, version mismatch/failure preventing shim installation without failing session start, marker-only cleanup, and preservation of legacy copied executables on resume. Existing cleanup coverage must continue to prove bare-native cleanup.

The container smoke test passes when the mounted executable runs and both `mbx --version` and `cargo --version` succeed through the real launcher inside the test container. It is unavailable when neither Docker nor Podman can run; record that plainly.

## Idempotence and Recovery

Native probing is read-only. Sync never deletes the last valid cache copy: unavailable or too-old hosts leave it intact, and same-directory rename lets already-running processes continue through their original inode. Containers keep one read-write cache-directory mount. The optional serde field marks the new scheme; records without `mbx_binary` preserve their old copied files on resume. Failed in-container verification removes only marked new launchers and does not prevent session startup. No database migration is needed.

## Artifacts and Notes

The investigation map is `/home/jonathan/Projects/mjolnir/.mj/clones/da87dcd462b28e417d1b68b8df1d6346/.mj/agents/7ec48162c8c745193da85b443c24450e/mbx-delivery-map.md`. The local mbx source confirms the shim contract in `crates/mbx/src/cli/setup.rs` and `crates/mbx/src/cli/shim.rs`.

## Interfaces and Dependencies

`NativeMbx` has one definition with `program: PathBuf` and `version: String`; the install button and cache sync use the same probe. `SessionBuildCache::mbx_binary` is a serde-defaulted optional marker and, for new records, stores `<cache>/.mjolnir/bin/mbx`. `AdditionalMount` remains directory-oriented. The Cargo wrapper sets `MBX_CARGO_SHIM_MODE=1` and `MBX_CARGO_SHIM_PATH` to its own absolute path before executing the cache copy.

Plan update note (2026-10-04): created after reading the repository instructions and investigation map; the assigned behavior and in-container paths are recorded before implementation.
Plan update note (2026-10-05): recorded the follow-up's cache-mounted atomic copy, marker-owned cleanup, merged installer probe, four-crate tests, workspace Clippy, and Podman restart smoke test.
