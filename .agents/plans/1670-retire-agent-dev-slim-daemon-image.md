# Retire `agent-dev` in favor of a slim daemon container image (#1670)

This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md`. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

## Purpose / Big Picture

Session containers no longer use `ghcr.io/brokkai/mjolnir/agent-dev`: repositories with a `mise` recipe run on per-bundle images (`mj-dev`, `bifrost-dev`), and all other container targets default to `docker.io/library/buildpack-deps:noble`. Keeping `containers/Containerfile.agent-dev` alive only for daemon-container mode (`[daemon] runtime = "docker" | "podman"`) forces every push to `master` to spend ~30 minutes rebuilding a multi-gigabyte image containing Rust, Go, Node, OpenJDK, Playwright/Chromium, profiling tools, and all eight harnesses, and forces every harness pin bump to update the Containerfile and satisfy an obsolete pin-lockstep test.

After this change, daemon-container mode runs on a dedicated slim image `ghcr.io/brokkai/mjolnir/daemon` (built from `containers/Containerfile.daemon` on `debian:trixie-slim`), containing only `/opt/mjolnir/{mj,mj-worker}` and the minimal OS and container-client tools needed by the daemon. Local harness invocations inside the daemon container use the same `HarnessRuntimePolicy::Managed` on-demand installer into the persistent `/var/lib/mjolnir` volume that bare hosts already use. The `agent-dev` Containerfile, build script, publish workflow, and pin-lockstep test are retired, while legacy v14 config migration from `agent-dev:latest` to `buildpack-deps:noble` remains intact.

## Progress

- [x] (2026-11-23 18:42Z) Mapped all `agent-dev` and daemon-container dependencies across Rust, Go, workflows, scripts, tests, K8s manifests, and docs, and verified `mj` dynamic libraries (`libgcc_s`, `libm`, `libc` only).
- [x] (2026-11-23 18:44Z) Opened and claimed tracking issue BrokkAi/mjolnir#1670 (`agent-in-progress`).
- [x] (2026-11-23 18:46Z) Authored this ExecPlan in `.agents/plans/1670-retire-agent-dev-slim-daemon-image.md`.
- [x] (2026-11-23 19:03Z) Milestone 1: Updated Rust and Go config/placement/runtime/tests/settings-schema/skills/docs to use `ghcr.io/brokkai/mjolnir/daemon`, switched daemon-container local harnesses to `HarnessRuntimePolicy::Managed`, fixed `worker_runs_in_image()`, preserved legacy `agent-dev` session-container upgrade compatibility across refreshes, and removed `bridge_fallback_pins_match_the_agent_dev_containerfile`.
- [x] (2026-11-23 18:52Z) Milestone 2: Replaced `Containerfile.agent-dev`, build/test scripts, publish/release/CI workflows, `scripts/build-linux-worker.sh`, K8s manifest, and docs/`AGENTS.md` with the slim `daemon` image.
- [ ] Milestone 3: Commit, review via `request_review`, push to `origin/master`, verify `publish-daemon-image.yml` succeeds, and close #1670.

## Surprises & Discoveries

- Observation: `mj` (`brokk-mjolnir`) dynamically links only `libgcc_s.so.1`, `libm.so.6`, and `libc.so.6`; it does not link OpenSSL, SQLite, Python, or ALSA at runtime.
  Evidence: `ldd /home/jonathan/.cargo/bin/mj` lists only `linux-vdso.so.1`, `libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, and the dynamic loader.
- Observation: `mj-core/src/placement.rs` is the sole decision point for daemon-container harness runtime (`local_harness_runtime()`), while `mj-worker-staging/src/worker_binary/launch.rs` also derived `worker_runs_in_image()` from `harness_runtime_policy(backend) == HarnessRuntimePolicy::Image`.
  Evidence: `mj-worker-staging/src/worker_binary/launch.rs:606-609` now checks `matches!(backend, TargetLocator::LocalBare { .. }) && mj_core::placement::daemon_container().is_some()` directly so `seed_image_environment` stays accurate when `local_harness_runtime()` is `Managed`.
- Observation: Daemon-container mode invokes `docker` (`docker-cli`) and `podman` (`podman-remote` symlinked to `/usr/local/bin/podman`) against the host container socket, plus `git`, `openssh-client`, `aws` (for EC2 management), and archive tools for checkpoints and managed installs.
  Evidence: `containers/Containerfile.agent-dev` and `rg` across `mj-controller/src/` and `mj-targets/src/`.
- Observation: `scripts/build-linux-worker.sh` (used by macOS `run.sh`/`install.sh`) relied on `ghcr.io/brokkai/mjolnir/agent-dev:latest` to compile the static Linux musl worker.
  Evidence: Updated `scripts/build-linux-worker.sh` to read the pinned toolchain from `rust-toolchain.toml` and build inside `docker.io/library/rust:<toolchain>-trixie` with `musl-tools` and `rustup target add "$triple"`.
- Observation: Post-upgrade recording in `mj-controller/src/controller/container_upgrade.rs` previously overwrote `settings.default_image` using `ContainerSettings::default_marker()`, which would have dropped the legacy `agent-dev` default marker after the first explicit refresh once `default_marker` stopped treating `agent-dev` as a default.
  Evidence: Added `ContainerSettings::record_upgrade_image()` to retain an existing legacy marker when upgrading within `LEGACY_DEFAULT_CONTAINER_IMAGE`, while still treating newly created sessions on `agent-dev:latest` as explicitly configured.
- Observation: Pushing a brand-new GHCR container package (`ghcr.io/brokkai/mjolnir/daemon`) under the `BrokkAi` organization may default to private/internal visibility on GitHub Packages until flipped to public in the GitHub web UI.
  Evidence: Prior `mj-dev` and `bifrost-dev` GHCR package creation precedent, and `gh api` returns 403 without `read:packages` scope.

## Decision Log

- Decision: Use a dedicated slim `ghcr.io/brokkai/mjolnir/daemon` image (`containers/Containerfile.daemon`) rather than reusing `mj-dev`.
  Rationale: `mj-dev` is a project-specific `mise` toolchain image for developing Mjolnir itself, does not bake `/opt/mjolnir/{mj,mj-worker}`, and is not release-versioned. External users running daemon-container mode should pull a small versioned daemon image, not Mjolnir's Rust/Node/Playwright dev toolchain.
  Date/Author: 2026-11-23 / Antigravity
- Decision: Switch `mj_core::placement::local_harness_runtime()` in daemon-container mode from `HarnessRuntimePolicy::Image` to `HarnessRuntimePolicy::Managed`, while keeping `HarnessRuntimePolicy::Image` deserializable for backward compatibility.
  Rationale: The daemon container mounts a persistent named volume at `/var/lib/mjolnir` (`HOME=/var/lib/mjolnir/home`), so managed installs cache harnesses and managed Node across daemon restarts and image upgrades via the same single code path used by bare hosts and `buildpack-deps:noble` session containers. Dropping baked harnesses removes all 9 global npm packages, Node, Muse, Antigravity, `install-muse.py`, `install-antigravity.py`, and the pin-lockstep test from the image build.
  Date/Author: 2026-11-23 / Antigravity
- Decision: Keep `LEGACY_DEFAULT_CONTAINER_IMAGE = "ghcr.io/brokkai/mjolnir/agent-dev:latest"` and `migrate_legacy_default_image` unchanged, and keep `mj-worker-staging/src/worker_binary/image_worker.rs` intact.
  Rationale: Existing pre-v15 configs that recorded `agent-dev:latest` must continue migrating cleanly to `DEFAULT_SESSION_IMAGE` (`docker.io/library/buildpack-deps:noble`), and `image_worker.rs` remains useful for probing `/opt/mjolnir/mj-worker` on the daemon image and any user/legacy image that bakes a worker.
  Date/Author: 2026-11-23 / Antigravity
- Decision: Rename compile-time override env var `MJ_AGENT_DEV_IMAGE` to `MJ_DAEMON_IMAGE` (and `MJ_AGENT_DEV_IMAGE_RELEASE` to `MJ_DAEMON_IMAGE_RELEASE`) in Rust and Go build scripts, and rename release pointer asset `agent-dev-image.txt` to `daemon-image.txt`.
  Rationale: Avoid carrying legacy `agent-dev` naming into new builds and releases now that the image is strictly the daemon image.
  Date/Author: 2026-11-23 / Antigravity
- Decision: Keep legacy `agent-dev` session container upgrades on `LEGACY_DEFAULT_CONTAINER_IMAGE` (`ghcr.io/brokkai/mjolnir/agent-dev:latest`) instead of `RELEASE_CONTAINER_IMAGE` (`daemon`), and mark only `DEFAULT_SESSION_IMAGE` in `ContainerSettings::default_marker`.
  Rationale: `mj-core/src/state/target_runtime.rs` (and its Go mirror) previously mapped recorded `agent-dev` session defaults to `RELEASE_CONTAINER_IMAGE` so explicit container upgrades preserved `hel` user/home/tools. Mapping them onto the new slim `daemon` image would strip all dev toolchains from existing `agent-dev` session containers.
  Date/Author: 2026-11-23 / Antigravity

## Outcomes & Retrospective

(To be completed upon finishing Milestone 3.)

## Context and Orientation

Mjolnir's containerized daemon launcher lives in `mj-cli/src/daemon/container.rs` and resolves its default image from `mj_core::config::RELEASE_CONTAINER_IMAGE` (`mj-core/src/config.rs`, populated by `mj-core/build.rs`). The Go mirror lives in `go/internal/core/config.go`, `go/internal/core/config_daemon.go`, `go/internal/core/worker_launch.go`, and `go/scripts/build.sh`. Session containers already default to `DEFAULT_SESSION_IMAGE = "docker.io/library/buildpack-deps:noble"` (`mj-core/src/config.rs:1223`).

The image definition is `containers/Containerfile.agent-dev`, built locally by `scripts/build-agent-dev-image.sh`, checked in CI by `scripts/agent-dev-image.test.mjs` and `mj-worker-staging/src/worker_binary/tests.rs` (`bridge_fallback_pins_match_the_agent_dev_containerfile`), and published by `.github/workflows/publish-agent-dev-image.yml` (also called from `.github/workflows/release.yml`, with release workflow tests in `scripts/release-workflow.test.mjs`).

## Plan of Work

### Milestone 1: Rust and Go runtime, config, and test updates

Update `mj-core/build.rs` and `mj-core/src/config.rs` (plus `go/internal/core/config.go`, `go/internal/core/config_daemon.go`, `go/scripts/build.sh`) so `CONTAINER_IMAGE_REPOSITORY` is `"ghcr.io/brokkai/mjolnir/daemon"` and the compile-time env override is `MJ_DAEMON_IMAGE` / `MJ_DAEMON_IMAGE_RELEASE`. Preserve `LEGACY_DEFAULT_CONTAINER_IMAGE = "ghcr.io/brokkai/mjolnir/agent-dev:latest"` and `migrate_legacy_default_image`.

Update `mj-core/src/placement.rs` so `local_harness_runtime()` returns `HarnessRuntimePolicy::Managed` unconditionally (including inside a daemon container). Keep `HarnessRuntimePolicy::Image` in `mj-core/src/worker_launch.rs` and `mj-harness-install/src/harness_install.rs` for backward-compatible deserialization of any persisted specs written by earlier daemon containers, updating its docstrings and error text to refer to the daemon image. Update `mj-worker-staging/src/worker_binary/launch.rs` so `worker_runs_in_image()` checks `backend.container_engine().is_some() || (matches!(backend, targets::TargetLocator::LocalBare { .. }) && mj_core::placement::daemon_container().is_some())`.

Remove `bridge_fallback_pins_match_the_agent_dev_containerfile` from `mj-worker-staging/src/worker_binary/tests.rs` and remove the stale `Containerfile.agent-dev` comment from `mj-worker-staging/src/worker_binary/harness.rs`. Update daemon-container user-facing hints and diagnostics in `mj-cli/src/daemon/container.rs`, `mj-core/src/config/daemon.rs`, `mj-setup/src/doctor.rs`, `mj-worker-staging/src/worker_binary/image_worker.rs`, and Go mirrors (`go/internal/core/worker_launch.go`, `go/internal/worker/acp_session.go`, `go/internal/core/assets/skills/mj/references/configuration.md` + Rust skill reference). Update active Rust/Go unit tests that assert the compiled daemon image name (`mj-core/src/config/tests.rs`, `mj-cli/src/daemon/container.rs`, `mj-setup/src/doctor/tests.rs` if affected, `go/internal/core/*test.go`). Update ignored/E2E session tests that still hardcoded `agent-dev:latest` where appropriate to `docker.io/library/buildpack-deps:noble` or `ghcr.io/brokkai/mjolnir/daemon:latest`.

### Milestone 2: Slim `Containerfile.daemon`, workflows, scripts, K8s, and docs

Create `containers/Containerfile.daemon` with two stages:
1. `FROM docker.io/library/rust:1.99.1-trixie AS worker`: identical static musl `mj-worker` + release `mj` build and `MJ_BUILD_REVISION`/`MJ_WORKER_INPUTS_ID`/`MJ_WORKER_BUILD_ID` validation into `/out/opt/mjolnir/{mj,mj-worker,mj-worker-<musl>}`.
2. `FROM docker.io/library/debian:trixie-slim`: installs only minimal daemon runtime packages (`awscli`, `bash`, `ca-certificates`, `curl`, `docker-cli`, `gh`, `git`, `gzip`, `libatomic1`, `libgcc-s1`, `libstdc++6`, `openssh-client`, `podman-remote` symlinked to `/usr/local/bin/podman`, `procps`, `rsync`, `tar`, `tmux`, `unzip`, `xz-utils`, `zstd`), creates user `hel` and `/workspace`, `/var/lib/hel`, `/var/lib/mjolnir/{home,data,config}`, copies `/out/opt/mjolnir/` to `/opt/mjolnir/`, runs `/opt/mjolnir/mj --version` and `/opt/mjolnir/mj-worker --version` as a smoke check, and attaches the `org.brokk.mjolnir.worker-build` and `org.brokk.mjolnir.revision` labels plus `STOPSIGNAL SIGKILL` and `CMD ["sleep", "infinity"]`.

Remove `containers/Containerfile.agent-dev`, `scripts/build-agent-dev-image.sh`, `scripts/agent-dev-image.test.mjs`, `scripts/install-muse.py`, and `scripts/install-antigravity.py`. Add `scripts/build-daemon-image.sh` and `scripts/daemon-image.test.mjs` (retaining the BuildKit global-ARG redeclaration invariant check against `containers/Containerfile.daemon`). Update `.dockerignore` to drop the removed installer scripts.

Replace `.github/workflows/publish-agent-dev-image.yml` with `.github/workflows/publish-daemon-image.yml` publishing `ghcr.io/<owner>/mjolnir/daemon` from `containers/Containerfile.daemon` (including all worker build inputs plus daemon/CLI/controller crates in the `paths` trigger). Update `.github/workflows/release.yml` (`publish-daemon-image` job and `daemon-image.txt` asset), `.github/workflows/publish-npm.yml` (exclude `daemon-image.txt`), `.github/workflows/ci.yml` (`scripts/daemon-image.test.mjs`), and `scripts/release-workflow.test.mjs`.

Update `containers/k8s/daemon.yaml` to `ghcr.io/brokkai/mjolnir/daemon:latest`. Update `AGENTS.md` (Harness pins section), `CONTRIBUTING.md`, `docs/src/content/docs/{containers,custom-images,daemon-container,profiles}.md`, `mj-targets/docs/{PODMAN,DOCKER}.md`, `go/internal/targets/data/docs/PODMAN.md`, and `scripts/test-*-import-e2e.sh`.

### Milestone 3: Validation, review, push, and workflow check

Run targeted Rust tests (dev profile via `mbx`), clippy on edited crates, `rustfmt --check`, Go tests on edited Go packages, and `node --test scripts/daemon-image.test.mjs scripts/release-workflow.test.mjs`. Commit cleanly, run `request_review`, fix any findings, push to `origin/master`, verify `publish-daemon-image.yml` builds and publishes `ghcr.io/brokkai/mjolnir/daemon:latest` cleanly, update this ExecPlan's `Progress` and `Outcomes & Retrospective`, and close #1670 (noting the one-time GHCR public visibility flip for `ghcr.io/brokkai/mjolnir/daemon`).

## Concrete Steps

All commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/56fc094db6272e59007c115d5bb50835`:

    node --test scripts/daemon-image.test.mjs scripts/release-workflow.test.mjs
    TZ=UTC mbx test -p brokk-mj-core -p brokk-mj-harness-install -p brokk-mj-worker-staging -p brokk-mj-setup -p brokk-mjolnir --lib
    TZ=UTC mbx clippy -p brokk-mj-core -p brokk-mj-harness-install -p brokk-mj-worker-staging -p brokk-mj-setup -p brokk-mjolnir --all-targets -- -D warnings
    (cd go && go test ./internal/core/... ./internal/targets/... ./internal/worker/...)

## Validation and Acceptance

- `node --test scripts/daemon-image.test.mjs scripts/release-workflow.test.mjs` passes with 0 failures, proving `Containerfile.daemon` global-ARG scoping, `.dockerignore` completeness, worker build-input path triggers, release tagging, and `daemon-image.txt` handling.
- Targeted Rust unit tests pass (including `mj-core` config/channel/migration tests, `mj-cli` daemon container tests, `mj-worker-staging` tests without the deleted `agent-dev` pin test, and `mj-setup` doctor tests) and `mbx clippy --all-targets -- -D warnings` is clean on all edited crates.
- Go tests pass for `./internal/core/...`, `./internal/targets/...`, and `./internal/worker/...`.
- After pushing to `origin/master`, `.github/workflows/publish-daemon-image.yml` succeeds on both `amd64` and `arm64` and publishes `ghcr.io/brokkai/mjolnir/daemon:latest`.

## Idempotence and Recovery

All changes are additive/replacive in Git; if `publish-daemon-image.yml` fails after push, fix `Containerfile.daemon` or the workflow in a follow-up commit. Existing `/var/lib/mjolnir` volumes keep the same `hel` user and directory layout so daemon container upgrades remain idempotent. Legacy v14 configs with `agent-dev:latest` continue migrating to `buildpack-deps:noble`.

## Artifacts and Notes

Reference mapping from read-only investigation: `.mj/agents/5a4d1d416f9b9db8773c59f592b0a429/agent-dev-retirement-map.md`.

## Interfaces and Dependencies

- `mj_core::config::CONTAINER_IMAGE_REPOSITORY`: `"ghcr.io/brokkai/mjolnir/daemon"`
- `mj_core::config::RELEASE_CONTAINER_IMAGE`: `env!("MJ_DAEMON_IMAGE")`
- `mj_core::config::LEGACY_DEFAULT_CONTAINER_IMAGE`: `"ghcr.io/brokkai/mjolnir/agent-dev:latest"` (retained for v14 migration)
- `mj_core::placement::local_harness_runtime()`: returns `HarnessRuntimePolicy::Managed`
- `mj_worker_staging::worker_binary::launch::worker_runs_in_image(backend)`: checks container engine OR `LocalBare` inside `daemon_container()`
- `go/internal/core`: `ContainerImageRepository = "ghcr.io/brokkai/mjolnir/daemon"`, `go/scripts/build.sh` reads `MJ_DAEMON_IMAGE`
