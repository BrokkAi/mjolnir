# Run harness quota checks inside the agent-dev image

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir shows each harness profile's subscription quota (how much of the five-hour and weekly allowance is left). The daemon reads it on a schedule. Three of those checks start a harness program on the host. Codex starts the Mjolnir-managed `codex app-server --stdio` and reads rate limits over JSON-RPC. Grok starts `grok agent stdio` from `PATH` and asks an ACP billing extension. Claude starts the `claude` CLI to refresh an expired login, and on macOS to print `/usage`. A host that runs every session in Podman or Docker often has none of those programs installed, so its quota rows read "not installed" even though the agent-dev image (`ghcr.io/brokkai/mjolnir/agent-dev`, built from `containers/Containerfile.agent-dev`) carries every harness at its pinned version.

After this change, when Podman or Docker is installed, those three checks run the harness inside the agent-dev image instead, with the profile's home directory mounted at the same path so logins and refreshed tokens land where the host expects them. A new configuration option chooses where quota checks run: `[quota] runtime = "auto"` (the default: Podman, else Docker, else the host), `"podman"`, `"docker"`, or `"host"`. To see it working: on a host with Podman and no `codex` or `grok` installed, configure a Codex ChatGPT or Grok profile, start the daemon in an isolated instance, and the profile's quota row shows its windows; `podman ps` during the check shows a container labelled `dev.mj.quota=true`.

Kimi, Muse and Z.ai quota checks are plain HTTPS requests from the daemon and are unchanged.

## Progress

- [x] (2026-11-02) Researched the quota probes, local container launch code, image pre-pull, and config plumbing; wrote this plan.
- [x] (2026-11-02) Milestone 1: `[quota] runtime` in `mj-core/src/config.rs` (`QuotaConfig`, `QuotaRuntimeChoice`), the Settings schema entries in `mj-client/src/settings/schema.rs`, and the row under Sessions in `mj-client/src/settings/page.rs`.
- [x] (2026-11-02) Milestone 2: `mj-controller/src/quota_runtime.rs` with `QuotaRuntime::resolve`, `ContainerRuntime::{ensure_image, launch}`, `ContainerCheck`, `QuotaContainer` (removal on shutdown and on drop), `sweep_quota_containers`, and `add_quota_image`.
- [x] (2026-11-02) Milestone 3: `QuotaRefreshRequest.runtime`; Codex (`CodexLauncher`), Grok (`GrokUsageSession::spawn_in_container`) and Claude (`run_claude_refresh`) use the container when resolved; the daemon adds the image to its background downloads (`daemon/process.rs`) and sweeps leftovers when its quota poller starts (`pollers/quota.rs`).
- [x] (2026-11-02) Milestone 4: tests `a_container_check_passes_secrets_by_name_and_removes_its_container_on_shutdown` and `a_container_check_without_its_image_reports_the_download_and_runs_nothing` (`codex_usage.rs`); `auto_runs_quota_checks_in_an_enabled_local_runtime_and_otherwise_on_the_host` (`quota_runtime.rs`, loads a real config file so the loader's standard runtimes take part); settings goldens regenerated; touched suites, clippy and fmt pass (see Outcomes).

## Surprises & Discoveries

- Observation: session containers never bind-mount a profile home; profiles are staged (copied) into the container by `controller/worker_binary/harness.rs`. Quota checks read and refresh the person's real login (`HarnessProfile.home`, for example `~/.codex`), so they must bind-mount it read-write instead.
  Evidence: `QuotaRefreshRequest::for_profile` in `mj-controller/src/quota.rs` sets `source_home: profile.home.clone()`.
- Observation: the engine executable is resolved to a path once, when the check is decided, and every later engine command (image lookup, run, removal) uses that path. This keeps the decision and the action on the same engine and lets tests use a stand-in engine without changing the process's `PATH`.
  Evidence: `ContainerRuntime.program` in `mj-controller/src/quota_runtime.rs`.
- Observation: rootless Podman sessions run as container uid 0 (`--user 0:0`), which the default user namespace maps to the host account, so files written to a bind mount stay owned by the host user. Docker has no such mapping, so a Docker check runs as the host's own uid and gid.
  Evidence: `container_run_args` in `mj-controller/src/targets/container.rs`.

## Decision Log

- Decision: the option is a new top-level `[quota]` section with `runtime = "auto" | "podman" | "docker" | "host"`, default `auto`.
  Rationale: the repository owner asked for a configuration option that defaults to the container whenever an engine exists. `auto` prefers Podman, the engine Mjolnir supports most completely, then Docker. Naming an engine explicitly lets a host with both choose, and `host` keeps the old behaviour.
  Date/Author: 2026-11-02, agent.
- Decision: the engine is chosen at probe time by finding `podman` or `docker` on `PATH` (`mj_core::program_path::find_program_on_path`). The image is `mj_core::config::DEFAULT_CONTAINER_IMAGE`, the image this release's sessions default to.
  Rationale: the decision is cheap and current; a probe on a host where the engine was just installed uses it. One image per release keeps every check on the release's harness pins.
- Decision: a check never pulls the image (`--pull=never`). When the image is absent the check reports `agent-dev image <ref> is not pulled yet` as its error, and the daemon's hourly image refresher pulls it because the image is added to its plan whenever quota checks use a container.
  Rationale: a multi-gigabyte pull inside a bounded quota request would time out and repeat. Failing visibly with the reason, while the background refresher fixes it, matches AGENTS.md ("fail visibly", "never leave the user stuck").
- Decision: when a container check is selected and something is wrong (engine missing for an explicit `podman`/`docker`, image absent, run failure), the check reports the error; it does not fall back to the host.
  Rationale: AGENTS.md forbids narrow fallbacks that hide a primary design failure. `auto` with no engine installed is not a failure; it is the documented host choice.
- Decision: Claude on macOS stays on the host.
  Rationale: Claude Code keeps its macOS login in the Keychain, which a Linux container cannot read, so the macOS `/usage` path only works on the host.
- Decision: each container is named `mj-quota-<random>` and labelled `dev.mj.managed=true`, `dev.mj.instance=<instance>` and `dev.mj.quota=true`. The owner of the check removes it with `<engine> rm --force` on every exit path (normal shutdown, error, drop), and the daemon removes any labelled quota container of its instance when its quota poller starts.
  Rationale: killing the `podman run` client does not stop the container, so the client process is not the owner; the named container is. The startup sweep covers a daemon that died mid-check (AGENTS.md: terminate the owning process first, release on every exit path).
- Decision: environment variables pass by name (`--env NAME`) with their values set on the engine process, never as `NAME=value` arguments.
  Rationale: profile environments carry API keys and tokens; argument lists are visible to every local user through `ps`.

- Decision (revision, 2026-11-03): `auto` uses a container only for an enabled local Podman or Docker runtime (`[targets.<id>]`, standard candidates included, `enabled = false` excluded) whose engine is installed, and runs that runtime's image; a runtime the person configured wins over a standard candidate. With no such runtime the check runs on the host. `podman` and `docker` use that engine's runtime image, or `DEFAULT_CONTAINER_IMAGE` when none is configured. `QuotaRefreshRequest.runtime` is now `QuotaRuntimeConfig` (in `mj-controller/src/quota.rs`), built by `QuotaRuntimeConfig::of(&Config)`.
  Rationale: the repository owner asked that checks run locally when no container runtime is set up. `Config::load` always adds the standard `podman` and `docker` candidates, so "a runtime exists" alone would always be true; the installed engine and the enabled flag decide it. Using the runtime's own image means the daemon's existing background download already covers it.
  Date/Author: 2026-11-03, agent.

## Outcomes & Retrospective

Delivered as planned. The suites of mj-core, mj-client, mj-tui, mj-controller and the `mj` CLI pass except for failures that also occur without this change on the development host: the move-session transfer tests need `rsync`, which the host lacks, and `golden_transcript_navigation` in mj-chat depends on the host's time zone. Clippy with `-D warnings` and rustfmt are clean for every crate the host can build (mj-desktop and voice-worker need GTK and ALSA headers it lacks). No end-to-end run against a real Podman was possible on the development host, which has no container engine; the engine is exercised through a stand-in script that records its arguments.

Remaining limitation: rootless Docker maps the host user's id to a subordinate id inside the container, so a Docker check run with `--user <uid>:<gid>` cannot read a profile home there. Rootful Docker and rootless Podman work as designed. A daemon handoff can sweep the outgoing daemon's in-flight check container, which costs that one probe; the next poll repeats it.

## Context and Orientation

The quota code lives in `mj-controller`. `mj-controller/src/quota.rs` defines `QuotaRefreshRequest` (built by `QuotaRefreshRequest::for_profile` from a configured `HarnessProfile`) and `refresh_profile_with_cache_root`, which dispatches by harness. The Codex probe is `mj-controller/src/codex_usage.rs`: `CodexUsageClient::spawn` calls `spawn_codex`, which runs the managed install's launcher with `app-server --stdio` through `tokio::process::Command`; the client is cached across polls and stopped by `CodexUsageClient::stop`. The Grok probe is `mj-controller/src/grok_usage.rs`: `GrokUsageSession::spawn` runs `grok agent stdio`. The Claude probe is `mj-controller/src/claude_usage.rs`: on Linux it calls the HTTPS usage endpoint itself and only runs the CLI (`run_claude_refresh`) to refresh a login; on macOS `run_claude_usage` runs `claude -p /usage`. Both use the blocking `CommandSpec`/`CancellableProcessExecutor` from `mj-core/src/targets.rs`.

The daemon's quota poller is in `mj-controller/src/pollers/quota.rs`; the same file holds the image refresher (`spawn_image_refresher`), whose plan comes from `image_refresh_plan` in `mj-controller/src/controller/backend.rs`. Configuration is `mj_core::config::Config` in `mj-core/src/config.rs`, stored through `StoredConfig`; the Settings screen's schema is `mj-client/src/settings/schema.rs`, with golden files `mj-tui/tests/golden/settings-root.txt` and `mj-controller/tests/golden/viewer-settings-exchange.txt`.

## Plan of Work

Milestone 1 adds `QuotaConfig { runtime: QuotaRuntimeChoice }` to `mj-core/src/config.rs` (with `Config.quota`, `StoredConfig.quota` skipped when default) and the settings schema entries (defaults, label "Quota checks", choices, help), then regenerates the two settings goldens.

Milestone 2 adds `mj-controller/src/quota_runtime.rs`. `QuotaRuntime::resolve(choice)` returns `Host` or `Container { engine, image }`, or an error for an explicit engine that is missing. `QuotaContainer` builds the engine argument list for one check (`run --rm -i --init --pull=never --name … labels … user rule … --volume <home>:<home> --workdir /tmp --env HOME=/tmp --env NAME… <image> <program> <args>`), checks the image is present with `<engine> image inspect`, and removes the container on every exit path. `sweep_quota_containers` removes leftovers at poller start.

Milestone 3 threads the choice through `QuotaRefreshRequest`, makes `spawn_codex`, `GrokUsageSession::spawn` and the Claude CLI commands use the container when resolved, adds the image to the image refresh plan when the runtime is a container, and calls the sweep when the quota poller starts.

Milestone 4 adds the tests described under Validation, runs the checks in AGENTS.md, and commits.

## Concrete Steps

Work from the repository root. Build and test with `cargo test -p brokk-mj-controller <filter>` and `cargo test -p brokk-mj-core <filter>` outside the sandbox; finish with `cargo clippy --all-targets -- -D warnings` and the full suites of the touched crates.

## Validation and Acceptance

Tests protect the tricky conditions, per AGENTS.md: a fake `podman` script on `PATH` records its arguments and proves that the Codex probe's container is removed with `rm --force` when the client shuts down and when the check fails, that secrets reach the engine by name and never as argument values, and that an absent image yields the "not pulled yet" error without running the harness. The settings goldens prove the option is visible. Acceptance by hand: with Podman installed, `[quota] runtime = "auto"`, and the image present, a Codex ChatGPT profile's quota row shows windows in an isolated `--instance`.

## Idempotence and Recovery

Every step is additive. The startup sweep makes an interrupted check harmless: its container is removed when the daemon next starts its quota poller.

## Interfaces and Dependencies

In `mj-core/src/config.rs`:

    pub struct QuotaConfig { pub runtime: QuotaRuntimeChoice }
    pub enum QuotaRuntimeChoice { Auto, Podman, Docker, Host }

In `mj-controller/src/quota_runtime.rs`:

    pub enum QuotaRuntime { Host, Container(ContainerRuntime) }
    impl QuotaRuntime { pub fn resolve(choice: QuotaRuntimeChoice, path: Option<&OsStr>) -> Result<Self> }
