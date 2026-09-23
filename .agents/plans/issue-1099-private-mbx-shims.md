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
- [x] (2026-09-23) Verify both published MBX 1.16.0 Linux musl archives against SHA256SUMS and upgrade the local mbx executable to 1.16.0.
- [x] (2026-09-23) Pin the released MBX and add a doctor warning for older native versions. Published-binary two-container acceptance passes; baseline stock 1.15.0 reproduces exit 127.
- [x] (2026-09-23) Pass the full dev-profile Cargo test suite, clippy with warnings denied, formatting, and diff checks.
- [x] (2026-09-23) Verify the named-instance doctor CLI output against a temporary 1.15.0 host binary.
- [x] (2026-09-23) Commit on hel4 as `4b134226`, integrate the advanced origin/master, and push the validated result as `cabe833f` to origin/master.

## Surprises & Discoveries

MBX 1.15.0 still installs persistent C/C++ shims under the shared cache. Mjolnir is pinned to 1.12.0, and the locally installed binary is also 1.12.0. The sibling checkout is already at release 1.15.0. Local Podman has the agent-dev image; Docker is not installed. The sibling repository has an unrelated untracked `scripts/mbx-incremental-histogram.py`, which must be preserved.

## Decision Log

2026-09-20, user and agent: expose `shims_dir` / `MBX_SHIMS_DIR`, defaulting to the existing cache directory layout. Mjolnir selects `<worker_root>/mbx-shims`. This avoids mounts, executable copies, automatic namespaces, cache migrations, and changes to default MBX behavior.

2026-09-20, user and agent: upgrade local MBX and the Mjolnir pin to 1.15.0 independently. Validate the new integration using `MJ_MBX_BINARY`; ordinary distribution still needs a released MBX containing the setting. No remote publication is authorized.

2026-09-23, user and agent: pin the published 1.16.0 release, upgrade local mbx without a backup, and push the validated Mjolnir commit to origin/master. Doctor should warn when an older host mbx disables the shared cache. Because caching is optional, the warning leaves doctor's exit status successful. These instructions supersede the 2026-09-20 no-push and backup decisions.

## Outcomes & Retrospective

The 2026-09-20 implementation and validation completed with the release dependency outstanding. Both dev-profile suites and clippy passed. Live Podman acceptance passed. Upstream full CI had three existing shell-test failures reproduced with the official 1.15.0 binary. MBX 1.16.0 now publishes the setting. Its release archives are verified, the local installation is 1.16.0, the published binary passes the two-container acceptance, and the required Mjolnir checks pass. The named-instance doctor check also passes. Mjolnir is committed and pushed to origin/master.

## Context and Orientation

The upstream checkout is `/home/jonathan/Projects/mr-boxington`, on its existing `main` branch. Its `crates/mbx/src/config.rs` resolves global configuration and environment variables. `session.rs`, `session/cmake.rs`, and `cli/exec.rs` install compiler wrappers (the same MBX executable invoked under compiler names). `storage.rs` verifies storage placement. Default wrappers are symlinks into the running installation. Keep that mechanism unchanged.

Mjolnir's `mj-controller/src/controller/mbx.rs` selects and checksum-verifies the pinned release. `mj-controller/src/controller/worker_binary/launch.rs::worker_launch_config` supplies the shared target environment used by harnesses, terminals, and reviewers. `mj_core::targets::worker_root` already determines each worker's private persistent root. The integration was committed on `hel3`; this delivery is on the current `hel4` branch and must be pushed to `origin/master`.

## Plan of Work

### Milestone 1: independent upgrade

Verify the 1.15.0 musl archives, update the pin and both checksums, and atomically replace the local executable after saving its old bytes in the task artifacts. Preserve configuration and caches. Keep the existing native-version compatibility rule. Verify `mbx --version` and validate Mjolnir before committing the pin change.

### Milestone 2: explicit MBX shim placement

Add `RawConfig.shims_dir: Option<PathBuf>` and resolved `Config.shims_dir: PathBuf`. Absolute overrides stay absolute; relative overrides resolve under `cache_dir`, as target roots already do. Defaults retain `<cache_dir>/shims`. Route persistent Rust, C/C++, targeted compiler, CMake, and exec wrappers through this value. Temporary wrappers retain their lifetime. Storage checks inspect the selected shim directory using the existing NFS policy. Generate documentation with `mise run render:docs`, and explain container usage and recovery of build configurations retaining old compiler paths.

Prove configuration precedence and unchanged defaults, then exercise actual wrappers with independent executable installations and shared cache data. Do not add fallback behavior that conceals a failed primary design.

### Milestone 3: Mjolnir integration and acceptance

For build-cache-enabled sessions, add `MBX_SHIMS_DIR` to the existing target environment, using `Path` joins under the resolved worker root. Keep each child's directory independent and keep the same worker's directory stable across relaunches. Existing provisioning and cleanup own this directory; MBX creates it lazily. Verify environment delivery to harnesses, terminals, and reviewers using existing launch behavior tests.

Use two disposable Podman containers with one cache on local storage and different worker installation paths. Reproduce a broken shared `HOST_CC`, then prove private wrappers support concurrent Rust/native builds, cache reuse, repeated runs, restart, replacement between builds, and removal of the other container. Test a CMake build directory configured before changing the wrapper directory; record an explicit reconfiguration procedure if needed. Preserve user caches and sessions.

### Milestone 4: deliver the published release and doctor diagnostic

Pin MBX 1.16.0 with verified Linux musl checksums. Its release contains the private shim setting. Add one doctor check per configured container host with the build cache enabled. Reuse the native-version probe and compatibility comparison: an older installed mbx produces a warning naming both versions, affected targets, and an upgrade; an absent native mbx is compatible; a failed probe is unknown rather than absent. The warning leaves doctor successful because caching is optional. Update the local mbx executable to the verified release without a backup, as the user requested. Validate with the published binary and required Mjolnir checks, commit on `hel4`, and push to `origin/master`.

## Concrete Steps

The original MBX source validation used focused `cargo test` commands, `mise run render:docs`, and `mise run ci`, and Mjolnir used `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. For this released-binary delivery, run the three Mjolnir checks from `/home/jonathan/Projects/mjolnir4`, with every Cargo test outside the restricted sandbox. The released-binary Podman fixture and verified archives are under `/mnt/optane/issue-1099-v1.16.0`. Do not redirect Cargo build output.

## Validation and Acceptance

New behavior tests must fail without the setting implementation and pass with it. Two independent installations must keep their own compiler paths while cached artifacts remain reusable. CMake paths must remain executable on subsequent runs; any legacy reconfiguration limitation must be reported. Verify Docker and Podman launch configuration; live acceptance is Podman unless Docker becomes available. Run the repositories' required checks and record failures honestly, distinguishing infrastructure limitations from regressions.

## Idempotence and Recovery

Stop and reassess if acceptance requires cache-format changes, executable retention, container detection, or successive compatibility fallbacks. Never delete live build outputs to conceal a failure. Stop disposable containers before removing their files. Stage only task files, commit on `hel4`, and push `HEAD:master` to origin. The user expressly declined a local mbx backup.

## Artifacts and Notes

Release 1.15.0 musl archive SHA-256: x86_64 `bf9fcc7ed39e4923588f6c25a781aa59eb466fec347102a3bb1a0c2acd12ebf8`; aarch64 `d0bb6c0eb4b4ba9abed39516ab769ca62429e3e47b548848ac40becee6870c2a`.

Release 1.16.0 musl archive SHA-256, verified against the published SHA256SUMS: x86_64 `be74eb96c62e774d90e8e036ed3d5541bde682802b11dfb1bf340d57276f5ab1`; aarch64 `01cce632e7bacacd78935226e778c5199f6942a55789645eb3e56c662f448792`.

## Interfaces and Dependencies

The only new public configuration is MBX `shims_dir` / `MBX_SHIMS_DIR`, now published in 1.16.0. Use the existing configuration loader, path types, and subprocess/test helpers. No new Mjolnir settings or database fields are required. Doctor adds a check in its existing human and JSON output; no wire protocol or schema change is needed.

2026-09-20: Created after user approval, including the explicit stopping rule and release dependency.

2026-09-20: Local upgrade and independent pin validation passed. Podman baseline reproduced exit 127 in container A after container B replaced the shared compiler symlink. Focused upstream private-shim tests pass. Artifact logs are under `/mnt/optane/hel3-1099`.

2026-09-20 validation evidence: `container-baseline.log` records container A exit 127 after B replaces the shared `cc` symlink. `container-acceptance.log` records 3 cache hits in the second checkout, successful concurrent C/C++ and Rust builds, 2 hits after executable replacement, and 2 hits after removal of the peer container. All fixture executables returned the expected native result of 15. A legacy CMake configuration failed with error 127 naming the old shared `cc`; running `mbx exec cmake --fresh -S <source> -B <build>` with the new setting restored it. Reapply original configure options when using `--fresh`. No user cache was modified by this acceptance fixture.

2026-09-20 validation environment: the MBX linker test initially failed because inherited `LIBRARY_PATH=/usr/lib/wsl/lib:` adds a relative current-directory search path. That same test passed after clearing host `LIBRARY_PATH` and `LD_LIBRARY_PATH`; full workspace validation then passed without source changes. MBX commands use `env -u NO_COLOR -u LIBRARY_PATH -u LD_LIBRARY_PATH MBX_DISABLE=1`. Mjolnir uses `env -u NO_COLOR MBX_DISABLE=1`. The upstream CI task runner was absent from PATH, so an isolated mise 2026.9.2 was downloaded under the task artifact directory. The documented wasm target was installed for upstream behavioral tests.

2026-09-20 review: Mjolnir's pin update is committed as `01a52a13`. Upstream generated configuration documentation and the site/link checks pass. The complete upstream CI gate is still running. The patched portable test binary is `/mnt/optane/hel3-1099/mbx-patched`; it can be selected for development through the existing controller `MJ_MBX_BINARY` override. The local default executable remains the verified stock 1.15.0 requested by the user. No release, push, or PR has been performed.

2026-09-20 final validation: `mise run ci` completed with exactly three failures in `test/cc_standalone_exec.bats`: “a make build's C objects restore into a second checkout”, “gdb and full debug objects restore across checkouts and invalidate on source changes”, and “objects retaining absolute source paths cache without leaking another checkout's FILE string”. All three were also reproduced using the checksum-verified official 1.15.0 binary (`mbx-baseline-bats.log`); no unrelated fixes were made. The first two fail expected cache-hit assertions and the third cannot find the expected recorded build. The final CI run used caching enabled, an isolated cache, and the isolated mise executable on PATH (`mbx-ci-clean.log`). An earlier run's build-only `MBX_DISABLE=1` setting and missing mise on PATH caused additional test-harness failures; these were removed for the final result.

The generated-documentation check, documentation build and 4,488-link scan, terminal/PTY tests, full Rust workspace suite, and release diagnostic test passed. Both dev and release clippy and formatting passed; the lint/release tasks were also completed independently so failure of the shell suite could not cancel them (`mbx-ci-remaining.log`). Final source diffs remain limited to the approved setting, its integration, documentation, and regression tests. The upstream unrelated untracked script is preserved.

For a repeatable container experiment, the task artifacts contain `container-check.py` (run with `setup`) and `accept-private.py`. The latter uses the portable patched build, exercises both installed copies against the shared isolated cache, tests the documented CMake recovery, and stops/removes the disposable containers on success. Baseline and acceptance logs are preserved. No Docker daemon was available; Docker launch configuration was verified by the controller behavior tests. The public release dependency remains intentionally unresolved rather than inventing a release version or publishing without authorization.

2026-09-23 delivery continuation: the released binary is 1.16.0 and contains `MBX_SHIMS_DIR`. Doctor had no build-cache check, so this delivery adds a per-host native-version warning for configured Podman and Docker targets. A missing native mbx is allowed, and probe errors are not reported as absence. Released-binary acceptance artifacts are under `/mnt/optane/issue-1099-v1.16.0`.

2026-09-23 validation evidence: stock 1.15.0 again reproduced container A's missing shared compiler with exit 127 after container B installed its shim. The published 1.16.0 binary produced three hits in the second checkout, passed concurrent C/C++ and Rust builds, and retained working private shims through restart, in-place replacement, and peer removal. The old CMake tree needed `cmake --fresh`; the reconfigured tree passed. All disposable containers were removed. The full Mjolnir dev-profile Cargo suite, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check` passed.

2026-09-23 CLI evidence: a dev-profile `mj --instance issue-1099-release doctor --json` against an isolated config and a temporary stock 1.15.0 binary reported `build-cache.local` as a warning: host 1.15.0 is older than pinned 1.16.0, affected target `podman` runs without the cache, and remediation says to upgrade mbx to 1.16.0 or newer. The deliberately minimal config caused unrelated fixable doctor checks and an exit status of 1; the build-cache warning itself does not change doctor's exit status. The JSON output is stored at `/mnt/optane/issue-1099-v1.16.0/doctor-old.json`.

2026-09-23 delivery: Mjolnir change `4b134226` was committed on hel4. Origin/master advanced twice during validation; both updates were merged without conflicts. The first changed controller code, so 1,582 controller tests, dev clippy, and formatting were rerun and passed. The second changed only AGENTS.md. Push `df50feb8..cabe833f HEAD -> master` succeeded. The new coordination rule was applied by self-assigning #1099 and adding `agent-in-progress`.

2026-09-23 change note: The final update records the validated merge and successful push, closing the release dependency and this ExecPlan.

2026-09-23 change note: This continuation replaces the old release dependency and delivery instructions because upstream published the feature and the user authorized a direct push to origin/master without a local mbx backup.
