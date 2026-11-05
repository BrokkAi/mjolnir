# Give the harness source one owner, and remove managed installs from the daemon container

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. It builds on `.agents/plans/container-native-daemon.md`, which moved the whole daemon into one container when `[daemon] runtime` is `docker` or `podman`.

## Purpose / Big Picture

A harness is the agent program a session runs (Codex, Claude Code, Muse, Kimi Code, Grok Build, OpenCode or DSH), together with its ACP bridge, the adapter that speaks the Agent Client Protocol to Mjolnir's worker. Mjolnir can get a harness from three places. A managed installation is a pinned copy Mjolnir downloads itself into a cache directory (`~/.cache/mjolnir/harnesses/...`, plus a pinned Node.js under `~/.cache/mjolnir/node/...`). An ambient installation is whatever the target machine already carries, for example a container image, with a managed installation as the fallback when it is missing. An image installation is the agent-dev image's own pinned harnesses, which is what a daemon running in its container must use.

Before this plan, nothing owned the question "where does this harness come from here". The session launch policy, the worker's fallback, the model-discovery probe, `mj login`, the three quota checks and the Node preflight each answered it with their own rule, and they computed the managed cache directory from different `HOME` values. In container placement that meant a daemon whose image already carries every pinned harness still downloaded, looked for, or tried to start managed copies.

After this plan, one function decides the source for work a machine runs itself (`mj_core::placement::local_harness_runtime`), one resolver turns a source into a launch (`mj_core::harness_install::locate`), and every consumer asks them. In container placement the only source is the image: a missing or incompatible harness is an error that names the image, never a download, and no code path computes a managed cache directory. On a host nothing changes for the person.

To see it, in an isolated instance on a machine with Docker:

    mj --instance t config set daemon.runtime docker
    mj --instance t login --profile codex
    # prints "Using the image's Codex at /usr/local/bin/codex-acp."
    docker exec mj-daemon-<digest> sh -c 'ls /home/hel/.cache/mjolnir /var/lib/mjolnir/home/.cache/mjolnir 2>&1'
    # both report "No such file or directory" after sessions, discovery, login and quota checks ran

## Progress

- [x] (2026-11-05) Inventoried every consumer that chooses a harness source (listed in Context and Orientation).
- [x] (2026-11-05) Milestone 1: `mj_core::placement` owns the daemon-container fact (`daemon_container`) and the local harness source (`local_harness_runtime`); `HarnessRuntimePolicy::Image` exists with `may_install`. The controller's `daemon::placement()`, the launch policy, `seed_image_environment`, the Node preflight and the CLI's `exec_in_container` guard ask it; `MJ_IN_CONTAINER` is gone.
- [x] (2026-11-05) Milestone 2: `mj_core::harness_install::locate` owns selection, including the `Ambient` rules moved from the worker. The worker's `prepare_harness_launch` and `discover_profile_config` call it; `ProfileProbeSpec::harness_runtime` carries the source. `resolve` and `unmanaged_launch` are gone and `resolve_with_environment` is private.
- [x] (2026-11-05) Milestone 3: `mj login` and `pinned_cli` call `locate`; `QuotaRuntime::resolve` returns `Host` in the daemon container; `CodexLauncher::Image` runs the image's `codex app-server`. Doctor's session-environment check and the TUI's managed-agent wording no longer promise an installation.
- [ ] Milestone 4: full suites and workspace Clippy pass; live check on a machine with Docker or Podman (not available in this environment).

## Surprises & Discoveries

- Observation: a local worker's `HOME` comes from the account database, not from its parent. Evidence: `mj-core/src/login_environment.rs` builds the login environment from `getpwuid` (`pw_dir`), which is `/home/hel` in the image, while the container daemon runs with `HOME=/var/lib/mjolnir/home`. The daemon's Codex quota check used `dirs::home_dir()`, which reads `$HOME`. The two never named the same managed cache.
- Observation: inside the daemon container the quota runtime picked a nested container. Evidence: `Config::with_local_targets` (`mj-core/src/config.rs`) always adds the standard `podman` and `docker` targets, and the image ships `docker-cli` and `podman-remote` linked as `podman`, so `QuotaRuntime::resolve` (`mj-controller/src/quota_runtime.rs`) chose `Container(Podman)` with no engine socket to reach.

- Observation: `find_command` canonicalizes, so a test fake made by `test_hooks::install_fake_command` (a symlink to a shared dispatcher) resolves to the dispatcher and loses its script. Evidence: the first run of `image_harnesses_never_fall_back_to_a_managed_install` saw an incompatible Codex bridge. The test writes real scripts instead.
- Observation: doctor's `session.environment` check and the TUI's managed-agent chooser both promised that Mjolnir installs the harness or Node.js, which is untrue under `Image`. Evidence: `mj-controller/src/doctor.rs`, `mj-tui/src/setup/render.rs`, `mj-tui/src/setup/draft.rs`.

## Decision Log

- Decision: add `HarnessRuntimePolicy::Image` rather than a second flag. Rationale: one enum value per source keeps the question single-owner; a bool beside the policy would be two predicates for one fact. Only a container daemon emits it, and only to workers in its own container, which run the image's own build (a container whose image changes is replaced, and one still holding workers blocks a second one), so an older worker never reads it. Date/Author: 2026-11-05.
- Decision: under `Image`, a missing or incompatible harness is an error, not a managed fallback. Rationale: the launcher only runs an image whose revision label matches the build, so a mismatch means a broken or custom image; hiding it behind a download is the narrow fallback AGENTS.md forbids. Date/Author: 2026-11-05.
- Decision: `Ambient` (container targets) keeps its managed fallback. Rationale: a container target may run any image, including one without harnesses; that behavior is documented and outside this plan's problem. Date/Author: 2026-11-05.
- Decision: the daemon-container fact is read from `MJ_DAEMON_CONTAINER` by one function in `mj-core`, and `mj setup`/`mj login`/`mj doctor`/`mj import` run inside the container with that variable instead of a separate `MJ_IN_CONTAINER`. Rationale: the nested command is in the daemon container; one variable and one reader replace two. Date/Author: 2026-11-05.
- Decision: the discovery probe follows the local source instead of hardcoding `Managed`. Rationale: it starts the same harness a local session would. Probe specs written by older daemons omit the field and keep meaning `Managed`. Date/Author: 2026-11-05.

- Decision: in the daemon container the quota runtime is always `Host`, even for a named `[quota] runtime` engine. Rationale: the daemon's image is the image a quota container would run, and the container has no engine socket; refusing would leave the person with a quota error they cannot fix. Date/Author: 2026-11-05.
- Decision: `locate` takes an already resolved environment and never starts the login shell. Rationale: every caller (worker session, discovery, `mj login`) already merges the login environment, and `mj login` must keep working when the login shell cannot start non-interactively. Date/Author: 2026-11-05.

## Outcomes & Retrospective

Every consumer listed in Context and Orientation now asks `mj_core::placement` for the source and `mj_core::harness_install::locate` for the launch. Under `Image` no code path computes a managed cache directory: `locate` never calls the installer, workers skip managed Node.js, the controller skips the Node preflight, the quota runtime never starts a nested container and the Codex check runs the image's `codex`. A live run in container placement remains to be done on a machine with an engine.

## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-controller`) owns sessions and starts one worker (`mj-worker`) per session. In container placement the launcher in `mj-cli/src/daemon/container.rs` runs the daemon in a container built from the agent-dev image (`containers/Containerfile.agent-dev`), which installs every pinned harness with `npm install --global` and declares `CODEX_PATH`, `CLAUDE_CODE_EXECUTABLE` and `MUSE_CLI`. The launcher sets `MJ_DAEMON_CONTAINER=<container name>` for the daemon; local workers start beside it in the same container.

`HarnessRuntimePolicy` in `mj-core/src/worker_launch.rs` is the source carried in each worker's `launch.json`. The managed installer is `mj-core/src/harness_install.rs` (`resolve`, `resolve_with_environment`, `ensure_node_runtime`, `spawn_gc_on`); pins live in `mj-core/src/harness_runtime.rs`.

The consumers that chose a source before this plan were: the controller's `harness_runtime_policy` and `seed_image_environment` in `mj-controller/src/controller/worker_binary/launch.rs`; the controller's Node preflight `preflight_harness` in `mj-controller/src/controller/worker_binary/harness.rs`; the worker's selection and managed fallback in `mj-worker/src/worker_runtime/harness_launch.rs` (`prepare_harness_launch`); the discovery probe in `mj-worker/src/worker_runtime/discovery.rs`, whose spec the controller writes in `mj-controller/src/controller/profile_config.rs`; `mj login` in `mj-cli/src/acp_login.rs`; the Codex quota launcher in `mj-controller/src/codex_usage.rs` and `mj-controller/src/quota.rs`; and the quota runtime in `mj-controller/src/quota_runtime.rs`. Grok and Claude quota checks run the program on `PATH` when they run locally, which in the image is the image's harness.

## Plan of Work

Milestone 1 adds `mj-core/src/placement.rs` with `DAEMON_CONTAINER_ENV`, `daemon_container() -> Option<String>` (the trimmed, non-empty variable) and `local_harness_runtime() -> HarnessRuntimePolicy` (`Image` in the daemon container, `Managed` otherwise). `mj-client` re-exports the constant. The controller's `daemon::placement()` builds `DaemonPlacement` from `daemon_container()`. The launch policy maps `LocalBare` to `local_harness_runtime()`, SSH and EC2 bare targets to `Managed`, and everything else to `Ambient`; `seed_image_environment` becomes "a container target, or policy `Image`". `preflight_harness` checks for Node or a downloader only when the policy is `Managed`. `exec_in_container` sets `MJ_DAEMON_CONTAINER` for the nested command and uses `daemon_container()` as its recursion guard.

Milestone 2 moves selection into `mj_core::harness_install::locate(policy, harness, execution_policy, command, args, environment) -> Result<Located>`. `Located` carries the launch command and arguments, environment additions, the native entrypoint and the CLI entrypoint (used by login and quota), and the managed installation when there is one (which holds its lease). `Managed` resolves the managed installation. `Ambient` keeps today's rule: use the target's bridge when present and compatible, else the managed installation. `Image` finds the image's programs on `PATH` (the same names the Containerfile installs: `codex-acp`/`codex`, `claude-agent-acp`/`claude`, `muse-acp`/`muse`, `kimi`, `grok`, `opencode`, `dsh-acp`), checks the Codex bridge version as `Ambient` does, and otherwise fails with a message naming the image. Managed Node.js is prepared only for `Managed` and `Ambient`. The worker's `prepare_harness_launch` and `discover_profile_config` call `locate`; `ProfileProbeSpec` gains `harness_runtime` (default `Managed`), set by the controller from `local_harness_runtime()`.

Milestone 3 makes `mj login` call `locate` with `local_harness_runtime()` and the built-in launch, so in the container it signs in through the image's bridge and CLI. `QuotaRuntime::resolve` returns `Host` in the daemon container, so checks run beside the daemon; the Codex launcher gains an `Image` variant that runs the image's `codex app-server`, chosen when `local_harness_runtime()` is `Image`.

## Concrete Steps

From the repository root:

    cargo fmt --all
    cargo clippy --all-targets -- -D warnings
    cargo test -p brokk-mj-core harness_install
    cargo test -p brokk-mj-worker harness_launch
    cargo test -p brokk-mj-controller quota_runtime
    cargo test -p brokk-mj-controller worker_binary

Then the full suites of the touched crates once at the end.

## Validation and Acceptance

A test in `mj-core/src/harness_install.rs` locates every harness under `Image` with an empty `PATH` and a temporary `HOME`; each must fail with the image message and leave no `.cache/mjolnir` under that `HOME`. Removing the `Image` arm (falling through to `Ambient`) makes it fail because a managed install starts. The existing quota-runtime test gains the daemon-container case: with Podman on `PATH` and an enabled Podman runtime, the decision is `Host`. Host behavior is covered by the existing suites, which must still pass. The live check in Purpose needs a Docker or Podman engine.

## Idempotence and Recovery

The change is code only; no store migration. Existing `launch.json` files keep their policy. A container daemon's existing managed caches become unused; deleting `/home/hel/.cache/mjolnir` inside the container or recreating the container removes them.

## Artifacts and Notes

(Added as work proceeds.)

## Interfaces and Dependencies

`mj_core::placement::{DAEMON_CONTAINER_ENV, daemon_container, local_harness_runtime}`; `mj_core::worker_launch::HarnessRuntimePolicy::{Ambient, Managed, Image}`; `mj_core::harness_install::{locate, Located}`; `mj_core::worker_launch::ProfileProbeSpec::harness_runtime`; `mj_controller::codex_usage::CodexLauncher::Image`.
