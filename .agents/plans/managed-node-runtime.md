# Install a managed Node runtime when the target has none

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

A person can use Mjolnir's npm-based ACP harnesses on a fresh bare or custom-container target without first installing Node.js. Mjolnir first reuses a compatible Node.js already visible in the session environment. If `node`, `npm`, or `npx` is missing or Node.js is older than version 22, Mjolnir downloads the pinned Node.js distribution, verifies its SHA-256 digest, publishes it atomically in the managed cache, and prepends its `bin` directory to the harness environment. The user-visible result is that Codex, Claude, Muse, MiMo, and DSH sessions can install and launch on a machine where `node --version` previously failed.

This change does not add a managed `uv` runtime. No ACP harness or bridge in this repository invokes `uv`; the existing `uv` in the reference development image remains a general development tool.

## Progress

- [x] (2026-10-24 21:07Z) Traced managed harness installation, worker launch preparation, profile discovery, container fallbacks, controller preflight, tests, and documentation.
- [x] (2026-10-24 21:07Z) Confirmed that no ACP harness uses `uv` and that Zed's Node runtime chooses a compatible system Node before installing a managed Node.
- [x] (2026-10-24 21:07Z) Add pinned Node.js metadata and managed-cache helpers in `mj-core`.
- [x] (2026-10-24 21:14Z) Implement system-runtime detection, verified download, locked installation, and PATH preparation.
- [x] (2026-10-24 21:18Z) Wire prepared Node into managed harness resolution, worker launch preparation, and profile discovery.
- [x] (2026-10-24 21:20Z) Restrict controller preflight to the remaining downloader prerequisite and update diagnostics.
- [x] (2026-10-24 21:23Z) Add tests for system-runtime reuse, missing/incompatible runtime installation, verification, concurrent installation, and native DSH launch.
- [x] (2026-10-24 21:25Z) Update human documentation and setup guidance.
- [x] (2026-10-24 21:35Z) Run focused tests, affected worker and controller suites, and workspace clippy.
- [x] (2026-10-24 21:37Z) Verify the official Linux x64 artifact checksum, rerun focused Node tests, and classify the pre-existing core subprocess failure.
- [x] (2026-10-24 21:39Z) Commit the validated change on the current branch as `ff83bea41`.

## Surprises & Discoveries

- Observation: The npm paths fail at three distinct layers. `mj-core/src/harness_install.rs` requires Node 22 and npm before `npm ci`; `mj-core/src/harness_runtime.rs` embeds npx checks in generated bridge scripts; and `mj-controller/src/controller/worker_binary/harness.rs` rejects launches before provisioning.
  Evidence: the existing errors say `Node.js 22 or newer is required in the target environment` and `managed harness installation needs...`, while the managed harness installer itself already downloads and verifies several native runtimes.
- Observation: Zed's relevant behavior is in `crates/node_runtime/src/node_runtime.rs`. It probes the system runtime, falls back to a downloaded Node distribution, and prepends that installation's `bin` directory. Zed currently pins `v24.22.1`.
  Evidence: the upstream source uses `SystemNodeRuntime::detect`, `ManagedNodeRuntime::install_if_needed`, and `path_with_node_binary_prepended`.
- Observation: `uv` is not referenced by any ACP bridge or harness adapter in this repository. It is installed as a general development tool in `containers/Containerfile.agent-dev`.
  Evidence: searching harness configuration, adapter assets, launch scripts, and managed-install code found no `uv` dependency.
- Observation: DSH can launch a native `dsh-acp` without Node. Requiring Node before inspecting that command would have made a no-curl target fail even though its configured harness was already runnable.
  Evidence: the worker launch helper now asks whether DSH's native executable is present before selecting the npm fallback, and controller preflight accepts it independently.
- Observation: The full `brokk-mj-core` suite leaves one unrelated existing test failing in this container: `subprocess::tests::spawn_detached_leaves_no_zombie_under_a_spawner_that_keeps_running` observes a zombie for ten seconds because PID 1 does not reap it here.
  Evidence: the test also fails in isolation with `process 23797 never left the process table (last state: Some('Z'))`; this change does not touch `mj-core/src/subprocess.rs`. All 17 runnable `harness_install` tests pass.

## Decision Log

- Decision: Use Node.js 24.22.1 as the managed runtime pin.
  Rationale: It is the version selected by current Zed, satisfies every existing Node 22+ requirement, and is a published Node.js distribution with official platform checksums.
  Date/Author: 2026-10-24, Codex
- Decision: Prefer a compatible target runtime, but require `node`, `npm`, and `npx` together; otherwise use the complete managed distribution.
  Rationale: A partially installed system runtime cannot satisfy both npm installation and npx bridge fallbacks. Switching to a self-contained distribution gives one owner for all three commands.
  Date/Author: 2026-10-24, Codex
- Decision: Store managed Node under `$XDG_CACHE_HOME/mjolnir/node`, or `$HOME/.cache/mjolnir/node` when `XDG_CACHE_HOME` is unset.
  Rationale: This is beside `mjolnir/harnesses`, follows the existing account cache and XDG contract, and is shared across sessions for the same target user.
  Date/Author: 2026-10-24, Codex
- Decision: Download through `curl` and extract with the Rust `tar` and existing `flate2` crates.
  Rationale: The repository already requires `curl` for several managed runtimes. Rust extraction avoids adding a new target prerequisite for `tar`, and the official archive checksum prevents publication of corrupted content.
  Date/Author: 2026-10-24, Codex
- Decision: Apply the managed Node fallback only to harnesses that need Node: Codex, Claude, Muse, MiMo, and DSH.
  Rationale: Native Kimi, Grok, and OpenCode installations do not need Node. Installing it only when a launch needs it avoids an unrelated startup cost while solving every affected bridge path.
  Date/Author: 2026-10-24, Codex
- Decision: Do not require Node for DSH when a native `dsh-acp` executable is present.
  Rationale: Only DSH's npx fallback needs Node. The native executable remains a complete launch path and must not acquire a new curl dependency.
  Date/Author: 2026-10-24, Codex

## Outcomes & Retrospective

Mjolnir now has one owner for selecting a Node.js runtime. Compatible system
tools remain untouched; missing or too-old tools are replaced by a
checksum-verified, exact-version managed installation shared by sessions. The
PATH and lease travel with the launch so npm shebangs, npx fallbacks, harness
installation, and child processes use the same runtime. DSH keeps its native
fast path. Documentation and doctor no longer instruct people to install Node
on every target.

The worker suite passed 695 unit tests and every integration suite; the
controller suite passed 2,086 tests. The core change passed all 17 runnable
harness-install tests and workspace clippy. The main validation limitation is
environmental rather than feature-related:
`subprocess::tests::spawn_detached_leaves_no_zombie_under_a_spawner_that_keeps_running`
assumes an init process reaps a reparented zombie, which this container's PID 1
does not do. The failure reproduces without these changes. The feature was
committed as `ff83bea41`.

## Context and Orientation

Mjolnir is a Rust workspace. The controller starts a worker on each session target. On local-bare, SSH-bare, and EC2 targets the worker installs an exact harness version; on container targets it normally uses the image's harness bridge. The worker normally runs as an unprivileged account and obtains its environment by starting the account's interactive login shell.

`mj-core/src/harness_install.rs` owns managed harness installation and returns a `ManagedHarness` containing the command, arguments, extra environment, entrypoint, cache root, and a lease file. `mj-core/src/harness_runtime.rs` owns pin metadata and cache-path helpers. `mj-worker/src/worker_runtime/harness_launch.rs` prepares the final launch spec. `mj-worker/src/worker_runtime/discovery.rs` performs the short-lived ACP probe that discovers models and settings. `mj-controller/src/controller/worker_binary/harness.rs` creates fallback bridge commands and currently runs a controller-side Node preflight.

A "managed runtime" means bytes owned by Mjolnir, addressed by an exact version, installed below the user's cache, and selected by absolute paths or by a PATH explicitly prepared for that process. A "compatible system runtime" means executables named `node`, `npm`, and `npx` found on the session PATH where `node --version` reports major version 22 or newer and `npm --version` and `npx --version` both succeed.

## Plan of Work

First, extend `mj-core/src/harness_runtime.rs` with the pinned Node version, platform artifact/checksum data loaded from a new `mj-core/assets/node/runtime.json`, a `managed_node_cache_root` shared-cache helper, and a helper identifying harnesses that require Node. Include the new asset in `mj-core/Cargo.toml`.

Next, extend `mj-core/src/harness_install.rs` with a public asynchronous `ensure_node_runtime` operation. It will check the supplied environment, preserve a compatible system installation, and otherwise lock the Node cache, remove abandoned staging, verify or atomically install the pinned artifact, prepend its `bin` directory to PATH, and return the selected installation path. Installation will download through `curl` using the existing bounded subprocess helpers, verify the official SHA-256 digest, extract with the Rust `tar` crate and `flate2`, and validate `node`, `npm`, and `npx` before publication. Existing managed harness resolution will call this operation for Node-using harnesses and include any PATH override in `ManagedHarness.environment`.

Then, call the same operation from `prepare_harness_launch` and `discover_profile_config` before DSH's unmanaged launch path. This makes the prepared PATH part of the supervisor spec, so generated npx scripts and npm package shebangs resolve the managed runtime. Replace controller preflight's Node requirement with a check that accepts any compatible Node or, when no compatible Node exists, verifies `curl` so the worker can perform the download.

Tests will use fake executables and a small locally generated Node archive. They will prove that a compatible system runtime causes no download, a missing or incompatible runtime installs the pinned archive, a bad checksum never publishes that archive, concurrent first use publishes one usable installation, and the prepared PATH points at the installed commands. No test will install from the public network.

Finally, update target, quickstart, profile, troubleshooting, custom-image, doctor, and setup text so Node is described as automatic for Node-based harnesses, with `curl` needed only when Mjolnir must download Node. Run the affected crate tests and workspace clippy, then commit only the files changed for this feature.

## Concrete Steps

All commands run from `/workspace/ae911c12fa00925acc1ee40ae1dce296/mjolnir`.

To inspect the pinned metadata and implementation:

    cargo test -p brokk-mj-core managed_node

To test worker integration:

    cargo test -p brokk-mj-worker harness_launch
    cargo test -p brokk-mj-worker discovery

To test controller preflight and diagnostics:

    cargo test -p brokk-mj-controller preflight
    cargo test -p brokk-mj-controller session_environment

After the final code round:

    cargo test -p brokk-mj-core
    cargo test -p brokk-mj-worker
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

Expected output is successful exit status with zero failed tests. Any real end-to-end daemon/session test must use an explicit isolated `--instance managed-node-test`; no command may target the default instance.

Manual acceptance uses an isolated target account or temporary HOME whose PATH contains `curl` but no `node`, `npm`, or `npx`. Start a Codex or DSH session in an explicit test instance. The provisioning stage should complete, `$XDG_CACHE_HOME/mjolnir/node/node-v24.22.1-<platform>` should contain `bin/node`, `bin/npm`, and `bin/npx`, and the ACP bridge should start with that directory first on PATH. Starting another session must reuse the same installation.

## Validation and Acceptance

Acceptance is behavioral: with a compatible system Node present, Mjolnir leaves PATH unchanged and downloads nothing. With all three commands absent, or with Node below version 22, Mjolnir verifies the pinned archive checksum, atomically publishes a cache installation, and prepends only its `bin` directory. An archive with the wrong digest is rejected and no final installation appears. Two simultaneous first launches still produce one complete installation. Generated npm bridge scripts and npx processes see the selected runtime.

The focused tests named above must pass before the broader affected-crate tests. `cargo clippy --all-targets -- -D warnings` must pass on the final tree. Tests are run outside the restricted sandbox as required by `AGENTS.md`. Cargo commands are not run for documentation-only follow-up rounds.

Observed results on 2026-10-24:

    cargo test -p brokk-mj-core node
    6 passed

    cargo test -p brokk-mj-worker
    695 unit tests passed; every integration suite passed

    cargo test -p brokk-mj-controller
    2086 passed; 17 ignored

    cargo clippy --all-targets -- -D warnings
    passed

The official `node-v24.22.1-linux-x64.tar.gz` download hashes to the pinned
`eb6728e77c576a35162c122c45ccadf224d89ff00abc8dae46cc2897c6002d7c`.

## Idempotence and Recovery

Node installation is idempotent. A valid cache hit performs executable checks and returns without downloading. Staging directories use a hidden prefix and are safe to remove after an interrupted install. Downloads and extraction stay inside staging; the final version directory appears only through one rename after checksum and executable validation. The cache is exact-versioned, so a future pin selects a different directory instead of mutating an installation currently used by another session. Retrying after a network failure starts from a fresh temporary staging directory and leaves the previous valid installation untouched.

No system Node installation is modified. Removing the Mjolnir cache while no session is running is safe and causes the next Node-based launch to download the pinned runtime again.

## Artifacts and Notes

The pinned official checksums for Node.js 24.22.1 are:

    89c682593d76cd44233fa53b1563264e7de9cbb189cad169135c5b4fadfbc0a7  node-v24.22.1-darwin-arm64.tar.gz
    d132557e7ee2b85af08b0c8c34338e65c6430f2238d01f727728ef42dc83b035  node-v24.22.1-darwin-x64.tar.gz
    33c37be234ba8bee041961047b19160f4d7d0f164e8e246f78bdd10feff148b3  node-v24.22.1-linux-arm64.tar.gz
    eb6728e77c576a35162c122c45ccadf224d89ff00abc8dae46cc2897c6002d7c  node-v24.22.1-linux-x64.tar.gz

## Interfaces and Dependencies

In `mj-core/src/harness_runtime.rs`, expose:

    pub const NODE_VERSION: &str;
    pub fn managed_node_cache_root(
        xdg_cache_home: Option<&OsStr>,
        home: Option<&OsStr>,
    ) -> Result<PathBuf>;
    pub const fn harness_uses_node_runtime(kind: HarnessKind) -> bool;

In `mj-core/src/harness_install.rs`, expose:

    pub async fn ensure_node_runtime(
        environment: &mut BTreeMap<String, String>,
    ) -> Result<Option<ManagedNodeRuntime>>;

The returned value is `Some(ManagedNodeRuntime)` when Mjolnir selected its managed runtime and `None` when the compatible system runtime was retained. `ManagedNodeRuntime::path()` names the installation, and its held lease keeps a corrupt-cache repair from replacing files below a live ACP process. In both cases `environment["PATH"]` describes the runtime that the worker must pass to harness installation and ACP startup.

Use `flate2` (already a dependency), add the workspace `tar` dependency, and use existing `crate::subprocess::run_bounded*`, checksum, atomic-directory publication, PATH, tracing, and test-hook conventions. Use `curl` through the target/session environment because the controller does not necessarily have direct network access to the worker target.

Revision note, 2026-10-24 21:39Z: recorded the validated commit and completed
the plan.
