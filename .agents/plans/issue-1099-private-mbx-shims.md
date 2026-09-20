# Give MBX workers private persistent compiler shims

This ExecPlan follows `.agents/PLANS.md` and must remain current throughout implementation.

## Purpose / Big Picture

Two containers sharing an MBX build cache currently overwrite compiler symlinks with paths into their private worker installations. One build then fails because `HOST_CC` points to an executable that exists only in the other container. Add an explicit persistent shim directory to MBX and point each Mjolnir worker at its own directory. Build artifacts remain shared. The user explicitly rejected an expanding automatic compatibility design.

## Progress

- [x] (2026-09-20) Inspect issue, both repositories, shim installation, configuration, and container availability; agree on explicit isolation.
- [x] (2026-09-20) Upgrade local MBX and Mjolnir's pin to checksum-verified 1.15.0; full Cargo tests, clippy, formatting, and diff checks pass.
- [x] (2026-09-20) Implement and commit MBX configuration, generated documentation, and behavior tests as `3da3fd8`; 1,237 workspace tests and dev clippy pass. Remaining CI checks are complete; three stock-release shell-test failures are documented below.
- [x] (2026-09-20) Reproduce the collision and pass the two-container acceptance experiment, including CMake recovery, restart, replacement, and peer removal. Disposable containers removed.
- [x] (2026-09-20) Integrate Mjolnir worker environment; 4,147 tests, dev clippy, formatting, and diff checks pass. The integration and final evidence are recorded in the commit accompanying this plan.

## Surprises & Discoveries

MBX 1.15.0 still installs persistent C/C++ shims under the shared cache. Mjolnir is pinned to 1.12.0, and the locally installed binary is also 1.12.0. The sibling checkout is already at release 1.15.0. Local Podman has the agent-dev image; Docker is not installed. The sibling repository has an unrelated untracked `scripts/mbx-incremental-histogram.py`, which must be preserved.

## Decision Log

2026-09-20, user and agent: expose `shims_dir` / `MBX_SHIMS_DIR`, defaulting to the existing cache directory layout. Mjolnir selects `<worker_root>/mbx-shims`. This avoids mounts, executable copies, automatic namespaces, cache migrations, and changes to default MBX behavior.

2026-09-20, user and agent: upgrade local MBX and the Mjolnir pin to 1.15.0 independently. Validate the new integration using `MJ_MBX_BINARY`; ordinary distribution still needs a released MBX containing the setting. No remote publication is authorized.

## Outcomes & Retrospective

The explicit setting and Mjolnir integration are implemented and validated. Both dev-profile suites and clippy pass. Live Podman acceptance passes without expanding the design. Upstream full CI has three existing shell-test failures reproduced with the official 1.15.0 binary; all other completed gates pass. Local MBX is stock 1.15.0; production delivery still needs a released upstream setting. No remote publication was performed.

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

2026-09-20 validation evidence: `container-baseline.log` records container A exit 127 after B replaces the shared `cc` symlink. `container-acceptance.log` records 3 cache hits in the second checkout, successful concurrent C/C++ and Rust builds, 2 hits after executable replacement, and 2 hits after removal of the peer container. All fixture executables returned the expected native result of 15. A legacy CMake configuration failed with error 127 naming the old shared `cc`; running `mbx exec cmake --fresh -S <source> -B <build>` with the new setting restored it. Reapply original configure options when using `--fresh`. No user cache was modified by this acceptance fixture.

2026-09-20 validation environment: the MBX linker test initially failed because inherited `LIBRARY_PATH=/usr/lib/wsl/lib:` adds a relative current-directory search path. That same test passed after clearing host `LIBRARY_PATH` and `LD_LIBRARY_PATH`; full workspace validation then passed without source changes. MBX commands use `env -u NO_COLOR -u LIBRARY_PATH -u LD_LIBRARY_PATH MBX_DISABLE=1`. Mjolnir uses `env -u NO_COLOR MBX_DISABLE=1`. The upstream CI task runner was absent from PATH, so an isolated mise 2026.9.2 was downloaded under the task artifact directory. The documented wasm target was installed for upstream behavioral tests.

2026-09-20 review: Mjolnir's pin update is committed as `01a52a13`. Upstream generated configuration documentation and the site/link checks pass. The complete upstream CI gate is still running. The patched portable test binary is `/mnt/optane/hel3-1099/mbx-patched`; it can be selected for development through the existing controller `MJ_MBX_BINARY` override. The local default executable remains the verified stock 1.15.0 requested by the user. No release, push, or PR has been performed.

2026-09-20 final validation: `mise run ci` completed with exactly three failures in `test/cc_standalone_exec.bats`: “a make build's C objects restore into a second checkout”, “gdb and full debug objects restore across checkouts and invalidate on source changes”, and “objects retaining absolute source paths cache without leaking another checkout's FILE string”. All three were also reproduced using the checksum-verified official 1.15.0 binary (`mbx-baseline-bats.log`); no unrelated fixes were made. The first two fail expected cache-hit assertions and the third cannot find the expected recorded build. The final CI run used caching enabled, an isolated cache, and the isolated mise executable on PATH (`mbx-ci-clean.log`). An earlier run's build-only `MBX_DISABLE=1` setting and missing mise on PATH caused additional test-harness failures; these were removed for the final result.

The generated-documentation check, documentation build and 4,488-link scan, terminal/PTY tests, full Rust workspace suite, and release diagnostic test passed. Both dev and release clippy and formatting passed; the lint/release tasks were also completed independently so failure of the shell suite could not cancel them (`mbx-ci-remaining.log`). Final source diffs remain limited to the approved setting, its integration, documentation, and regression tests. The upstream unrelated untracked script is preserved.

For a repeatable container experiment, the task artifacts contain `container-check.py` (run with `setup`) and `accept-private.py`. The latter uses the portable patched build, exercises both installed copies against the shared isolated cache, tests the documented CMake recovery, and stops/removes the disposable containers on success. Baseline and acceptance logs are preserved. No Docker daemon was available; Docker launch configuration was verified by the controller behavior tests. The public release dependency remains intentionally unresolved rather than inventing a release version or publishing without authorization.
