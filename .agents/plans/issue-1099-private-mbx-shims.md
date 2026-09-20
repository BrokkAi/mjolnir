# Give MBX workers private persistent compiler shims

This ExecPlan follows `.agents/PLANS.md` and must remain current throughout implementation.

## Purpose / Big Picture

Two containers sharing an MBX build cache currently overwrite compiler symlinks with paths into their private worker installations. One build then fails because `HOST_CC` points to an executable that exists only in the other container. Add an explicit persistent shim directory to MBX and point each Mjolnir worker at its own directory. Build artifacts remain shared. The user explicitly rejected an expanding automatic compatibility design.

## Progress

- [x] (2026-09-20) Inspect issue, both repositories, shim installation, configuration, and container availability; agree on explicit isolation.
- [x] (2026-09-20) Upgrade local MBX and Mjolnir's pin to checksum-verified 1.15.0; full Cargo tests, clippy, formatting, and diff checks pass.
- [ ] Implement MBX configuration and behavior tests in `../mr-boxington` (code and focused tests complete; documentation and full checks pending).
- [ ] Reproduce the collision and validate private shims with real containers.
- [ ] Integrate Mjolnir worker environment, run checks, and commit each repository's changes.

## Surprises & Discoveries

MBX 1.15.0 still installs persistent C/C++ shims under the shared cache. Mjolnir is pinned to 1.12.0, and the locally installed binary is also 1.12.0. The sibling checkout is already at release 1.15.0. Local Podman has the agent-dev image; Docker is not installed. The sibling repository has an unrelated untracked `scripts/mbx-incremental-histogram.py`, which must be preserved.

## Decision Log

2026-09-20, user and agent: expose `shims_dir` / `MBX_SHIMS_DIR`, defaulting to the existing cache directory layout. Mjolnir selects `<worker_root>/mbx-shims`. This avoids mounts, executable copies, automatic namespaces, cache migrations, and changes to default MBX behavior.

2026-09-20, user and agent: upgrade local MBX and the Mjolnir pin to 1.15.0 independently. Validate the new integration using `MJ_MBX_BINARY`; ordinary distribution still needs a released MBX containing the setting. No remote publication is authorized.

## Outcomes & Retrospective

Implementation and validation are pending. Do not describe 1.15.0 as containing the fix.

## Context and Orientation

The upstream checkout is `/home/jonathan/Projects/mr-boxington`, on its existing `main` branch. Its `crates/mbx/src/config.rs` resolves global configuration and environment variables. `session.rs`, `session/cmake.rs`, and `cli/exec.rs` install compiler wrappers (the same MBX executable invoked under compiler names). `storage.rs` verifies storage placement. Default wrappers are symlinks into the running installation. Keep that mechanism unchanged.

Mjolnir's `mj-controller/src/controller/mbx.rs` selects and checksum-verifies the pinned release. `mj-controller/src/controller/worker_binary/launch.rs::worker_launch_config` supplies the shared target environment used by harnesses, terminals, and reviewers. `mj_core::targets::worker_root` already determines each worker's private persistent root. The current branch is `hel3`; commit there.

## Plan of Work

### Milestone 1: independent upgrade

Verify the 1.15.0 musl archives, update the pin and both checksums, and atomically replace the local executable after saving its old bytes in the task artifacts. Preserve configuration and caches. Keep the existing native-version compatibility rule. Verify `mbx --version` and validate Mjolnir before committing the pin change.

### Milestone 2: explicit MBX shim placement

Add `RawConfig.shims_dir: Option<PathBuf>` and resolved `Config.shims_dir: PathBuf`. Absolute overrides stay absolute; relative overrides resolve under `cache_dir`, as target roots already do. Defaults retain `<cache_dir>/shims`. Route persistent Rust, C/C++, targeted compiler, CMake, and exec wrappers through this value. Temporary wrappers retain their lifetime. Storage checks inspect the selected shim directory using the existing NFS policy. Generate documentation with `mise run render:docs`, and explain container usage and recovery of build configurations retaining old compiler paths.

Prove configuration precedence and unchanged defaults, then exercise actual wrappers with independent executable installations and shared cache data. Do not add fallback behavior that conceals a failed primary design.

### Milestone 3: Mjolnir integration and acceptance

For build-cache-enabled sessions, add `MBX_SHIMS_DIR` to the existing target environment, using `Path` joins under the resolved worker root. Keep each child's directory independent and keep the same worker's directory stable across relaunches. Existing provisioning and cleanup own this directory; MBX creates it lazily. Verify environment delivery to harnesses, terminals, and reviewers using existing launch behavior tests.

Use two disposable Podman containers with one cache on local storage and different worker installation paths. Reproduce a broken shared `HOST_CC`, then prove private wrappers support concurrent Rust/native builds, cache reuse, repeated runs, restart, replacement between builds, and removal of the other container. Test a CMake build directory configured before changing the wrapper directory; record an explicit reconfiguration procedure if needed. Preserve user caches and sessions.

## Concrete Steps

In the MBX checkout, run focused `cargo test` commands, `mise run render:docs`, and `mise run ci`. In the Mjolnir checkout, run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Use dev-profile Rust validation and run every Cargo test outside the sandbox. Use `MBX_DISABLE=1` when building the implementation so the existing shim collision cannot contaminate validation. Store build/test artifacts under normal build roots or `/mnt/optane/hel3-1099`, never `/tmp`.

## Validation and Acceptance

New behavior tests must fail without the setting implementation and pass with it. Two independent installations must keep their own compiler paths while cached artifacts remain reusable. CMake paths must remain executable on subsequent runs; any legacy reconfiguration limitation must be reported. Verify Docker and Podman launch configuration; live acceptance is Podman unless Docker becomes available. Run the repositories' required checks and record failures honestly, distinguishing infrastructure limitations from regressions.

## Idempotence and Recovery

Stop and reassess if acceptance requires cache-format changes, executable retention, container detection, or successive compatibility fallbacks. Never delete live build outputs to conceal a failure. Stop disposable containers before removing their files. Keep the previous local MBX binary for rollback. Stage only task files and commit on the current branches, without pushing, branching, or opening a PR.

## Artifacts and Notes

Release 1.15.0 musl archive SHA-256: x86_64 `bf9fcc7ed39e4923588f6c25a781aa59eb466fec347102a3bb1a0c2acd12ebf8`; aarch64 `d0bb6c0eb4b4ba9abed39516ab769ca62429e3e47b548848ac40becee6870c2a`.

## Interfaces and Dependencies

The only new public configuration is MBX `shims_dir` / `MBX_SHIMS_DIR`. Use the existing configuration loader, path types, and subprocess/test helpers. No new Mjolnir settings or database fields are required. Until upstream publishes the setting, test via `MJ_MBX_BINARY`; a future normal-release pin update must use the actual published version and checksums and must not be claimed complete prematurely.

2026-09-20: Created after user approval, including the explicit stopping rule and release dependency.

2026-09-20: Local upgrade and independent pin validation passed. Podman baseline reproduced exit 127 in container A after container B replaced the shared compiler symlink. Focused upstream private-shim tests pass. Artifact logs are under `/mnt/optane/hel3-1099`.
