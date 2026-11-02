# Let controller-side harness work run in a container instead of on the host

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir (`mj`) runs coding agents ("harnesses": Codex, Claude Code, Kimi Code, Grok Build, Muse Code, OpenCode, MiMo Code, DSH). Sessions on a container target already run their harness inside a Podman or Docker container. Four jobs belong to no session and used to run a harness only on the machine running `mj`: signing a profile in (`mj login`, `mj login --setup-token`), discovering a profile's models and efforts before a session starts, reading subscription quota (Codex `app-server`, Grok Build `agent stdio`, Claude Code `/usage` on macOS), and renewing an expired Claude login (`claude auth login`).

After this change the person chooses where those jobs run with `[harnesses] run_on` in `config.toml`. `"host"` (the default) keeps the pinned installation on this machine. `"container"` runs each job in a short-lived container from the agent image of a local Podman or Docker target, so someone who wants nothing installed can use `mj` plus Podman or Docker alone, while someone who prefers local harnesses keeps them. To see it: in an isolated instance with a Docker target, set `run_on = "container"`, run `mj --instance <name> models --profile <id>` and `mj --instance <name> login --profile <id>`, and observe that `~/.cache/mjolnir` and `~/.npm/_npx` stay absent while the profile home receives its credential file; with `run_on = "host"` the same commands run on this machine.

## Progress

- [x] (2026-11-01 16:30Z) Inventory every place the controller or CLI runs a harness on the host.
- [x] (2026-11-01 17:40Z) First version: container-only (commit 158f415). Superseded, see the Decision Log.
- [x] (2026-11-02 08:40Z) `[harnesses] run_on` (`mj-core/src/config.rs`, `HarnessesConfig`, default `host`).
- [x] (2026-11-02 08:50Z) `mj-controller/src/harness_runner.rs`: the `HarnessRunner` trait, `LocalRunner`, `select`, and the shared procedures (`harness_cli`, `HarnessExecutor`, `work_directory`); `HarnessSandbox` implements the trait.
- [x] (2026-11-02 08:55Z) Discovery, Codex/Grok/Claude quota and renewal, and `mj login` rewritten as procedures over the runner; host setup detection restored.
- [x] (2026-11-02 09:05Z) Both modes verified end to end in `--instance sbx` with Docker.
- [x] (2026-11-02 09:15Z) Documentation describes the option.
- [ ] Follow-up: Apple's `container` runtime as a sandbox engine on macOS.
- [ ] Follow-up: real-account check of Codex ChatGPT sign-in through the published callback port on macOS Docker Desktop and Podman machine.

## Surprises & Discoveries

- Observation: tokio's `Command::output()` forces stdout and stderr to pipes even when the caller set them to inherit, which swallowed the in-container sign-in's terminal output.
  Evidence: `mj login` printed only the host's own lines until the child was spawned and its stdout read manually.
- Observation: the worker rebuilds its environment from a login shell, which takes `HOME` from the account database, so the sandbox's `HOME=/mj/home` was replaced by `/root`. Specs and `harness-cli --keep-env` now carry `HOME` and `XDG_CACHE_HOME` as explicit overrides.
  Evidence: the stand-in harness printed `home=/root` before the fix.
- Observation: passing container environment as `--env NAME=VALUE` exposes values (refresh tokens, API keys) in the host's process list. The sandbox now passes `--env NAME` and sets the value in the engine client's own environment.
- Observation: this cloud test container blocks GHCR blob downloads and rate-limits Docker Hub, so validation used a locally built image (Debian base, host Node 22, `@brokkai/dsh-acp` from npm, and a scripted `kimi`).

## Decision Log

- Decision: running harnesses in containers is an option, `[harnesses] run_on = "host" | "container"`, default `host`; the host behavior is kept.
  Rationale: the user asked for a way to need no local installation, not to forbid local installations ("if a user would RATHER NOT DO THAT, they should be able to"). The first version made containers mandatory and was reworked.
  Date/Author: 2026-11-02 / Claude.
- Decision: express the difference as a template method. Each job is one fixed procedure (work directory, spec, `mj-worker` invocation, cleanup) over the `HarnessRunner` trait, whose steps (`runtime`, `work_root`, `work_view`, `profile_view`, `profile_environment`, `base_environment`, `launch_worker`, `may_sign_in_foreign_store`, `reads_host_keychain`) `LocalRunner` and `HarnessSandbox` implement. `harness_runner::select` is the only place that reads `run_on`.
  Rationale: requested by the user; no procedure tests which mode it is in, so a third runner (Apple container, SSH) adds one implementation, not branches in every caller. Both modes run the same worker code (`worker sign-in`, `worker harness-cli`, `worker discover-config`) and differ only in where it runs and which installation it selects.
  Date/Author: 2026-11-02 / Claude.
- Decision: a machine that runs no worker of its own (Windows) always uses the container runner, logged once.
  Rationale: the host procedures now run the native worker, which Windows lacks; containers are the sensible action instead of a dead end. Windows `mj login` previously ran the harness CLI from PATH directly; that path is gone.
  Date/Author: 2026-11-02 / Claude.

- Decision: one owner, `HarnessSandbox::select`, decides the engine and image; every caller asks it. Podman before Docker, disabled targets never, image and pull policy from the target.
  Rationale: the repository's concurrency rule ("one owner decides each fact"); the sandbox runs the same harness versions as that target's sessions.
  Date/Author: 2026-11-01 / Claude.
- Decision: the harness inside the sandbox is always selected by the worker (`prepare_harness_launch` with the `Ambient` policy and the built-in fallback launch), never by the controller naming a binary.
  Rationale: identical to container sessions; an image without a harness (Kimi, Grok, OpenCode, DSH, custom images) gets the pinned install inside the container, cached in the persistent sandbox home.
  Date/Author: 2026-11-01 / Claude.
- Decision (container mode): no host fallback. Without a usable engine, sign-in stops with a message naming the fix, discovery is reported as "no retry can fix" (sessions still report their own choices), and quota readings show as unavailable.
  Rationale: the user asked that nothing run locally; a silent host fallback would hide that the design is not working (AGENTS.md "A narrow fallback usually indicates a design problem").
  Date/Author: 2026-11-01 / Claude.
- Decision: refuse `mj login` for a profile pointed at the person's own OpenCode, Muse, or MiMo login database.
  Rationale: the sandbox's pinned version could migrate a database another installation owns; the person has that installation and can sign in with it, or use a managed profile.
  Date/Author: 2026-11-01 / Claude.
- Decision: keep `query_with` and the Claude refresh code unchanged and add `SandboxExecutor`, a `CommandExecutor` that runs each harness command in the sandbox with one profile home mounted.
  Rationale: the logical command stays the whole description, and existing tests keep driving it with direct fakes.
  Date/Author: 2026-11-01 / Claude.
- Decision: files written into a mounted home must belong to the person: rootless Podman runs as container root (mapped to the person); Docker runs as the person's uid/gid with generated `/etc/passwd` and `/etc/group` mounts unless the daemon is rootless.
  Rationale: Node and harness CLIs look up the user; a root-owned credential file in the person's home would be a defect.
  Date/Author: 2026-11-01 / Claude.

## Outcomes & Retrospective

Delivered: with `run_on = "container"` no code path in `mj-cli` or `mj-controller` installs or executes a harness on the host for sign-in, setup token, model discovery, quota, or login renewal; with the default `host` they run on this machine through the same procedures. Lesson: the first version read "make it possible" as "make it mandatory"; options that change where work runs should be confirmed as options first. Gaps: Apple `container` is not yet a runner, and the browser-callback path on macOS VMs is untested against real accounts.

## Context and Orientation

The control plane is the daemon (`mj-controller`, started by `mj daemon-run`); the CLI is `mj-cli` (binary `mj`); the data plane is the per-session worker `mj-worker`, a static Linux binary when used in containers (`mj-worker-<arch>-unknown-linux-musl`, resolved by `mj-controller/src/controller/worker_binary/binary_source.rs`). The agent image `ghcr.io/brokkai/mjolnir/agent-dev` (`containers/Containerfile.agent-dev`) carries the Codex and Claude ACP bridges, Muse, and MiMo.

Key files after this change:

- `mj-controller/src/harness_runner.rs`: the `HarnessRunner` trait, `LocalRunner` (native worker, `Managed` runtime, identity paths, desktop variables), `select`, `harness_cli` (a run of `mj-worker worker harness-cli --harness <id> --runtime <managed|ambient> --keep-env NAME... --drop-env NAME... -- <args>`), `HarnessExecutor` (a `CommandExecutor` that runs each harness command through a runner), `WorkerLaunch` (`spec`, `process`, `finish`).
- `mj-controller/src/harness_sandbox.rs`: the container runner. `HarnessSandbox::select`, `HarnessSandbox::command` (builds `podman|docker run --rm --init ...`), `HarnessSandbox::harness_cli` (a run of `mj-worker worker harness-cli`), `SandboxExecutor`, `SandboxCommand::remove` (called on every exit path), container paths `/mj/home` (persistent sandbox home under `<data dir>/harness-sandbox/<engine>`), `/mj/profile` (profile home), `/mj/work` (private work directory under `<data dir>/harness-sandbox/work`, swept after a day), `/mj/bin/mj-worker` (the controller's own Linux worker, read-only).
- `mj-controller/src/controller/profile_config.rs`: `probe_profile` stages the profile into a work directory and runs `mj-worker worker discover-config` in the sandbox with `ProfileProbeSpec.runtime = Ambient`.
- `mj-worker/src/worker_runtime/discovery.rs`: `managed_supervisor` (bare targets, unchanged behavior) and `target_supervisor` (sandbox).
- `mj-worker/src/worker_runtime/sign_in.rs` (moved from `mj-cli/src/acp_login.rs`): `run_sign_in` for `worker sign-in --spec`, and `run_harness_cli` for `worker harness-cli --harness <id> --keep-env NAME... -- <args>`.
- `mj-core/src/worker_launch.rs`: `ProfileProbeSpec.runtime` (defaults to `Managed` for older specs), `SignInSpec`, `SignInAction`.
- `mj-cli/src/harness_login.rs`: the sign-in procedure over the runner. The browser relay for containers lives in `harness_sandbox.rs`.
- `mj-controller/src/codex_usage.rs`, `grok_usage.rs`, `claude_usage.rs`: quota and renewal through the sandbox.
- `mj-controller/src/setup.rs`, `setup/availability.rs`: no host CLI probes.

## Plan of Work

Milestone 1 added the sandbox module and moved discovery. Milestone 2 moved the ACP sign-in client into the worker and replaced `mj login`'s host paths with one sandbox run. Milestone 3 added `worker harness-cli` and moved Codex, Grok, and Claude harness calls onto it. Each milestone was verified in an isolated instance (`--instance sbx`) as described below.

## Concrete Steps

From the repository root, with Docker running and an image that has the harness (the published agent image, or a local test image):

    export MJ_INSTANCE=sbx MJ_WORKER_DIR=/path/to/dir/with/mj-worker-x86_64-unknown-linux-musl
    cargo build -p brokk-mjolnir
    cargo build -p brokk-mj-worker --target x86_64-unknown-linux-musl
    # ~/.config/mjolnir/instances/sbx/config.toml: version = 19, [harnesses] run_on = "container" (or "host"),
    # a [targets.docker] kind = "docker" with the image, and a profile
    ./target/debug/mj models --profile dsh
    ./target/debug/mj login --profile kimi

## Validation and Acceptance

Observed in this session with a DSH profile and the stand-in `kimi`:

    $ mj models --profile dsh
    deepseek-flash  deepseek-flash
    deepseek-v4-pro  deepseek-v4-pro
    effort (deepseek-flash): none, low, high, max
    $ ls ~/.cache/mjolnir/harnesses ~/.npm/_npx
    ls: cannot access ...: No such file or directory
    $ mj login --profile kimi
    Signing in in a docker container from localhost/mj-sandbox-test:latest.
    Signing in with Kimi login (login).
    Running `kimi login`.
    Signed in through the ACP method login.
    Open this address in a browser to sign in: https://example.invalid/device?code=TEST
    Credentials updated for profile kimi. ...
    $ docker run ... mj-worker worker harness-cli --harness kimi --keep-env KIMI_CODE_HOME --keep-env HOME -- probe "two words"
    kimi stand-in: probe two words

No sandbox container remained after each command (`docker ps -a --filter label=org.brokk.mjolnir.harness-sandbox`), and the discovery work directory was removed. Automated checks: `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings` and the full suites of `brokk-mj-core`, `brokk-mj-worker`, `brokk-mj-controller`, and `brokk-mjolnir`.

## Idempotence and Recovery

Every sandbox container carries `--rm` and the label `org.brokk.mjolnir.harness-sandbox=<instance identity>`, and is removed with `rm --force` after its command on every exit path. A container whose client was killed also exits when its standard input closes. Work directories left by an interrupted owner are swept after a day. Removing `<data dir>/harness-sandbox` is always safe; it only drops cached in-sandbox harness installs.

## Artifacts and Notes

The sign-in spec the CLI writes for the worker:

    {"harness":"kimi","profile_home":"/mj/profile","environment":{"BROWSER":"/mj/work/open-url","HOME":"/mj/home","XDG_CACHE_HOME":"/mj/home/.cache"},"action":"login","cli":false,"desktop":false}

## Interfaces and Dependencies

In `mj-controller/src/harness_sandbox.rs`:

    pub struct HarnessSandbox { .. }
    impl HarnessSandbox {
        pub fn select(config: &Config, executor: &impl CommandExecutor) -> Result<Self>;
        pub fn command(&self, run: &SandboxRun) -> Result<SandboxCommand>;
        pub fn harness_cli(&self, kind: HarnessKind, home: &Path, environment: BTreeMap<String, String>, arguments: Vec<String>, purpose: &str) -> Result<SandboxRun>;
    }
    pub struct SandboxExecutor<E> { .. }   // implements CommandExecutor
    pub fn is_unavailable(error: &anyhow::Error) -> bool;

In `mj-worker`: `worker sign-in --spec <path>` and `worker harness-cli --harness <id> [--keep-env NAME]... -- <args>`.

Revision 2026-11-02: reworked from container-only to an option chosen with `[harnesses] run_on`, structured as a template method over `HarnessRunner`, after the user clarified that local installation must remain possible and asked for a template method rather than control branches.
