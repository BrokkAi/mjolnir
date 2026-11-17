# Session environments from mise: project tools after the clone, `mj image build`, and a generic default image

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md` (repository root).

Tracking issue: BrokkAi/mjolnir#1493.

## Purpose / Big Picture

Today every container session runs in one image chosen per target, by default Mjolnir's own `ghcr.io/brokkai/mjolnir/agent-dev`, which bakes in Mjolnir's worker, all harness bridges, and Mjolnir's own toolchain (Rust, Go, Node, profilers). A repository that needs other tools has no way to say so; its owner must hand-build an image and edit a target.

After this change a repository says what it needs with mise, a tool-version manager. mise reads `mise.toml` (for example `[tools] node = "24"` and `[bootstrap.packages] "apt:libssl-dev" = "latest"`) and also the version files repositories already carry, such as `rust-toolchain.toml`, `.nvmrc` and `.python-version`. Three things become possible:

1. With no image work at all, a session clones the repository and then runs mise inside its container, so the agent's first turn already has the repository's tools. This works on any image, including a user's own.
2. `mj image build` builds an image from the repository's mise recipe on a target's host, pushes it to an OCI registry (the standard container image registry protocol: ghcr.io, ECR, Docker Hub), and records it for that project, so later sessions start with everything preinstalled. An agent inside a session can run it.
3. The default session image becomes Docker's official `buildpack-deps:noble` (Ubuntu 24.04 with compilers, common development headers, git and curl). Mjolnir no longer needs its own image for sessions; Mjolnir's own toolchain moves into the mjolnir repository's `mise.toml` and is published with `mj image build` as `mj-dev`.

To see it working, in an isolated instance: create a session for a repository that contains only `rust-toolchain.toml` and `.nvmrc`, on a Podman target with the default image. The launch shows a "Preparing ...: Install project tools" step, and in the session `!cargo --version` and `!node --version` print the pinned versions. Then run `mj image build` for that project, create another session, and observe that the step finishes in well under a second because the image already holds the tools.

## Progress

- [x] (2026-11-16) Reconnaissance: create pipeline and image selection, custom-image worker and bridge installation, CLI/API/config/secrets patterns, kitchen-sink inventory, mise spike. Findings are folded into Context and Surprises below; raw reports live in this session's `.mj/agents/*/findings.md` (not committed).
- [ ] Milestone 1: pinned mise artifact (`mj-core/assets/mise/runtime.json`) and the worker's post-clone project-tools step.
- [ ] Milestone 2: per-project session images in configuration and one effective-image resolution used by every image consumer.
- [ ] Milestone 3: `mj image build` and `mj image status` (Containerfile generator, daemon build operation, push, assignment).
- [ ] Milestone 4: mjolnir's own `mise.toml` (the `mj-dev` recipe), built and pushed with `mj image build`.
- [ ] Milestone 5: default session image becomes `buildpack-deps:noble`; the release image stays for the daemon-container mode and quota containers; docs and the `mj` skill updated.
- [ ] Full validation, review, commit, push.

## Surprises & Discoveries

- Observation: the session workspace is cloned inside the session container after the container is created, not on the daemon host. Reading a repository's recipe before creating the container would need a separate host-side git read that is not available for every repository source (GitHub-only projects have no daemon-side clone).
  Evidence: clone commands are wrapped in container exec in `mj-controller/src/targets/provision.rs` (around lines 114-230) and `mj-controller/src/targets/bootstrap.rs` (around 105-169), and run after target creation from `execute_repository_setup` in `mj-controller/src/controller/provisioning.rs`.
- Observation: on an image without baked bridges, the harness bridge cache lives under the container's `$HOME/.cache/mjolnir`, so each new container installs its harness bridge again.
  Evidence: `mj-core/src/harness_runtime.rs` cache root resolution (around lines 49-65), used by `mj-core/src/harness_install.rs` (around 580-620).
- Observation: the Nitro microVM runtime pulls images only from a registry with `crane`; it cannot import an image from a local Podman or Docker store.
  Evidence: `mj-worker/src/microvm/image.rs` `pull_reserved` (around 137-190); `mj-worker/src/microvm/cli.rs` has no import command.
- Observation (mise spike, mise v2026.11.9): `mise bootstrap packages apply --yes` installs `[bootstrap.packages]` apt entries non-interactively as root on Debian and Ubuntu without experimental flags; `mise install` never installs them. Shims (small launcher programs in `$MISE_DATA_DIR/shims`) work without a login shell and without network, for root and for a non-root user, and `[env]` values from `mise.toml` reach programs started through a shim. Rust needs explicit `RUSTUP_HOME` and `CARGO_HOME`. A no-op `mise install` takes about 30 ms. A cold four-language image build (Rust, Node, Go, Python, from a lockfile) took 158 s and produced 3.08 GB; changing only Node rebuilt in 72 s. Concurrent `mise install` runs take per-tool locks.
  Evidence: spike report in this session's `.mj/agents/1afce28c97ceadfbbab55a1f5a23798f/`.
- Observation (Milestone 4): mise reads the repo's Rust channel and components when `rust` is enabled for idiomatic files, but `mise.lock` records Rust version and options rather than rustup component artifact checksums; it does not lock apt packages. The repo toolchain file currently omits musl targets, so the recipe adds both worker targets explicitly. Playwright's browser download requires a separate `npx playwright install --only-shell chromium` step; the image builder currently runs only `mise bootstrap packages apply --yes && mise install`, so the browser payload is not part of this recipe. Podman validation is blocked in this worker environment because its runtime directory is read-only (`chmod /run/user/1000/libpod` fails).
  Evidence: mise spike `DETAILS.md` and the Podman command output for this milestone.
- Observation: the release image is also the image for the opt-in daemon-container mode and for quota-check containers, which run harness CLIs and need them baked in.
  Evidence: `docs/src/content/docs/daemon-container.md`; `mj-controller/src/quota_runtime.rs` (around 113-146).

## Decision Log

- Decision: mise, not devcontainer, is the recipe format.
  Rationale: mise reads files repositories already have, covers language toolchains and (through `[bootstrap.packages]`) system packages, needs no container engine to apply, and runs the same inside any container or VM. devcontainer needs a feature builder and carries Docker- and IDE-specific keys with no meaning here.
  Date/Author: 2026-11-16, jbellis with Claude.
- Decision: by default, mise runs inside the session container after the clone, every session; there is no shared tool cache between containers.
  Rationale: the recipe is only available after the clone (see Surprises). Building an image solves slow starts; the no-image path stays simple. A Nitro microVM gets a fresh filesystem per VM anyway.
  Date/Author: 2026-11-16, jbellis.
- Decision: the worker runs the project-tools step as part of harness preparation, before the harness starts, on container targets only (Podman, Docker, Apple container, their SSH forms, and SSH Nitro). Bare targets never run it.
  Rationale: the worker already owns harness preparation, reports its steps through its relay state (`RelayOperationalState::harness_preparation`, a free-text step), owns the harness environment that must include the mise shims, and survives daemon restarts. On a bare host, `[bootstrap.packages]` would install system packages on the user's machine with sudo.
  Date/Author: 2026-11-16, Claude.
- Decision: a failed project-tools step never fails the session. The worker records the failure, keeps the log, starts the harness, and the user sees a notice naming the failing command and the log path.
  Rationale: AGENTS.md "Never leave the user stuck": the agent can fix a broken recipe from inside the session.
  Date/Author: 2026-11-16, Claude.
- Decision: `mj image build` always pushes to an OCI registry; there is no local-only build.
  Rationale: user requirement ("oci repo only"). A pushed image is usable on every host, including Nitro.
  Date/Author: 2026-11-16, jbellis.
- Decision: v1 relies on the host container engine's own registry login for push and for pulls of private images. A push or pull refused for authentication reports the engine's message and the exact login command for that host.
  Rationale: the user asked not to solve every corner case; credential plumbing can follow if needed.
  Date/Author: 2026-11-16, Claude.
- Decision: the default session image is `docker.io/library/buildpack-deps:noble`.
  Rationale: Docker maintains it, it is multi-architecture (amd64, arm64), and it has a C toolchain, development headers, git and curl, so a repository's mise recipe can build native code. Approved by the user ("docker buildpack-deps wfm").
  Date/Author: 2026-11-16, jbellis.
- Decision: a project's built images are recorded per platform in the project bundle's configuration, as `[bundles.<id>.images]` with keys such as `"linux/amd64"`. A session uses the bundle's image for its target's platform when one exists, otherwise the target's image.
  Rationale: one owner (configuration) for the fact "which image does this project use", visible and editable, with no database migration. Per-platform keys avoid multi-architecture manifest machinery while letting every host of the same architecture reuse one push.
  Date/Author: 2026-11-16, Claude.
- Decision: the published dogfood image built from mjolnir's recipe is named `mj-dev`. The release-published Containerfile image keeps its current name and role for the daemon-container mode and quota containers; it stops being the session default.
  Rationale: user ("rename it to mj dev when we publish it next"). The daemon-container mode needs `mj`, the worker and every harness baked in, which a mise recipe cannot provide. Slimming or renaming that image is a follow-up.
  Date/Author: 2026-11-16, Claude.
- Decision: anything mise cannot express is out of scope; users can always bake their own image and set a target's `image`, and mj still supplies its worker and bridges there.
  Rationale: user ("we don't need to solve every corner case").
  Date/Author: 2026-11-16, jbellis.

## Outcomes & Retrospective

(None yet.)

## Context and Orientation

Mjolnir (`mj`) runs coding agents in sessions. The daemon (`mj daemon-run`, crate `mj-controller`) is the control plane: it owns configuration, the database, session lifecycle and the HTTP API. Each session has a worker (crate `mj-worker`, binary `hel`), the data plane, which runs inside the session's environment, starts the agent's harness (Codex, Claude Code and others, through an ACP "bridge" program), and keeps running across daemon restarts. A target (configuration `[targets.<id>]` in `~/.config/mjolnir/config.toml`, type `TargetTemplate` in `mj-core/src/config/targets.rs`) says where sessions run: on a host directly ("bare") or in a container run by Podman, Docker or Apple's `container`, locally or over SSH, or in a Nitro microVM on an EC2 host. A project bundle (`[bundles.<id>]`, type `ProjectBundle` in `mj-core/src/config/targets.rs`) lists the repositories a session clones.

Container images today. A container target's `image` key (`ContainerTemplate.image`, `mj-core/src/config/targets.rs` around line 359) defaults to `mj_core::config::DEFAULT_CONTAINER_IMAGE` (`mj-core/src/config.rs` around line 988), which `mj-core/build.rs` sets to `ghcr.io/brokkai/mjolnir/agent-dev` with a release version tag or `:latest`. The configuration file omits the key when it equals the default. `mj-controller/src/controller/backend.rs` `backend_container` (around line 1036-1061) copies the configured image into the runtime `ContainerTemplate` (`mj-core/src/targets.rs` around 2431). `mj-controller/src/controller/provisioning.rs` obtains that runtime target around line 851, before building the creation plan, which makes it the place where a different image can be substituted. Other consumers read the configured image directly: worker preflight (`mj-controller/src/controller/worker_binary/binary_select.rs` `release_image_supplies_worker` around 72 and preflight around 91 and 148), the applied-settings record (`provisioning.rs` around 1059-1081 and `mj-core/src/state/target_runtime.rs` around 33-91), explicit container upgrade (`mj-controller/src/controller/container_upgrade.rs` around 373) and missing-container recovery (same file around 204-214), the doctor (`mj-controller/src/doctor.rs` around 3495-3551), the background image download plan (`backend.rs` `image_refresh_plan` around 823), and quota containers (`mj-controller/src/quota_runtime.rs` around 113-146). Downloads before a launch go through the image pull gate (`mj-controller/src/image_pull_gate.rs` `with_image_ready`), which shows the "Pull image" stage.

Custom images today. Any image works if it has a POSIX shell, `sleep`, `bash`, `chown` and `stat` (`docs/src/content/docs/custom-images.md`). For an image other than this release's default, mj uploads its worker into `/var/lib/hel/workers/<session>` and the worker installs the pinned harness bridge into a cache under the container's home directory, reusing the image's Node 22+ or downloading the pinned Node described by `mj-core/assets/node/runtime.json` (`mj-core/src/harness_install.rs`). That pinned-artifact pattern (a JSON manifest with a version and per-platform sha256, included with `include_str!`) is the model for the mise artifact.

Session creation. `Controller::provision_top_level` in `mj-controller/src/controller/provisioning.rs` creates the target, then runs repository setup (the clone inside the container) concurrently with installing the worker payload, then prepares the harness and starts the worker. Since the work in `.agents/plans/worker-serves-before-harness-preparation.md`, the worker opens its control socket first and prepares the harness in the background, publishing `RelayOperationalState::harness_preparation` (`mj-core/src/relay/snapshot.rs`) as `Preparing { step, since_ms }`, `Started` or `Failed { step, error, at_ms }`; the daemon shows `ProvisionStage::PreparingHarness` with the step text ("Preparing Codex: <step>", `mj-controller/src/controller/readiness.rs` around 188 and 869). The step is free text, so a new step needs no wire change. Creation is restartable as a whole after a daemon restart (`.agents/plans/create-survives-daemon-restart.md`) and does not hold daemon-upgrade admission during this phase.

mise. A single static binary. Configuration files: `mise.toml`, `.mise.toml`, `mise/config.toml`, `.config/mise.toml`, `.tool-versions`, a lockfile `mise.lock`, and "idiomatic" version files of other tools (`rust-toolchain.toml`, `.nvmrc`, `.node-version`, `.python-version`, `.ruby-version`, `.go-version`), which current mise reads only for the tools named in its setting `idiomatic_version_file_enable_tools` (environment variable `MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS`). `mise install` installs `[tools]` into `$MISE_DATA_DIR/installs` and creates shims in `$MISE_DATA_DIR/shims`; putting that directory first on `PATH` makes the tools visible to any process. `mise bootstrap packages apply --yes` installs `[bootstrap.packages]` through the system package manager and needs root (it uses `sudo` when not root). mise refuses configuration files in directories it does not trust; `MISE_TRUSTED_CONFIG_PATHS` lists trusted roots.

## Plan of Work

Milestone 1 adds the pinned mise artifact and the worker's project-tools step. Create `mj-core/assets/mise/runtime.json` with the mise version and per-platform sha256 for the static Linux binaries (x86_64 and aarch64; take the asset names and checksums from the mise GitHub release's published checksum file and record where they came from in a comment in `mj-core/src/harness_runtime.rs` next to a `MISE_VERSION` constant kept in lockstep, as `NODE_VERSION` is). Install it the way the worker installs other direct artifacts: downloaded with the worker's own HTTPS client, verified, into the worker's managed cache; an image that already has `mise` on `PATH` uses its own. Then, in the worker's harness preparation, before the harness starts and only when the session's target is a container target, run for each repository checkout in the session's workspace: `mise bootstrap packages apply --yes`, then `mise install`, with the working directory at the checkout root. The environment for those commands and for the harness comes from one function (single owner of "the mise environment for this session"): `MISE_DATA_DIR` from the image's environment if set, otherwise a directory under `/var/lib/hel`; `RUSTUP_HOME` and `CARGO_HOME` under the same data directory unless the image sets them; `MISE_TRUSTED_CONFIG_PATHS` covering the workspace root; `MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS` naming rust, node, python, ruby and go; and `PATH` with the shims directory first. Publish the step as `Preparing { step: "Install project tools", .. }`. Write the combined output to a log file in the session's worker directory. When a repository has no mise configuration and no enabled idiomatic file, skip it without invoking package installation. If a command fails, record the failure (command, exit status, log path), continue to the harness, and surface a notice the user sees in the session; find the existing worker-to-user notice path (worker relay notices or transcript system notices) and use it rather than adding a new channel. Do not put a deadline on this step (AGENTS.md forbids speculative timeouts, and a cold toolchain download can take minutes); exempt it from the worker's per-step preparation deadlines and keep it cancellable by session close.

Milestone 2 adds per-project images to configuration and one effective-image resolution. Add `images: BTreeMap<String, String>` to `ProjectBundle` (`mj-core/src/config/targets.rs`), serialized as `[bundles.<id>.images]` with platform keys (`"linux/amd64"`, `"linux/arm64"`), omitted when empty, validated (keys are `linux/<arch>`, values non-empty image references). Add one function, for example `mj_core::config::Config::session_image(&self, bundle_id: Option<&str>, target_id: &str, platform: &str) -> Option<&str>` (None for non-container targets), and make every consumer listed in Context use it with the session's bundle and its target's platform: runtime target construction for create and fresh resume, worker preflight (`release_image_supplies_worker` must compare the effective image), the applied-settings record, container upgrade's target image, missing-container recovery, the doctor's per-target image check, and the background download plan (add bundle images for container targets whose platform is known from the target's `platform` key; skip targets whose platform is only known by probing). Find where the target's platform is known before provisioning (the worker preflight probes it in `binary_select.rs` around 198-220) and resolve the image after that point. Quota containers keep using the release image and must not follow bundle images. Keep the change backward compatible for configurations without `images`. Note that an older installed daemon rejects an unknown `images` key (`ProjectBundle` denies unknown fields), so never write `images` into the user's live configuration before the new build is installed there.

Milestone 3 adds `mj image build` and `mj image status`. The CLI (`mj-cli/src/main.rs` `Command`, implementations in `mj-cli/src/api_commands.rs`, client in `mj-cli/src/api_client.rs`) gains `mj image build [--path DIR] [--bundle ID] [--target ID] [--name NAME] [--print]` and `mj image status [--path DIR] [--bundle ID]`. The CLI collects the recipe files (the mise configuration files and idiomatic version files listed in Context, plus `mise.lock`) from the repository root containing `--path` (default: the current directory), and sends their contents with the request; this works the same on the host and inside a session, where `/workspace` holds the checkout. The bundle defaults to the calling session's bundle (`MJ_SESSION_ID`) or the bundle whose repository has that local path; the target defaults to the bundle's or user's default container target; the name defaults to the bundle id. A new configuration section `[images]` with `repository` (for example `ghcr.io/brokkai/mjolnir`) is required for `build` (except `--print`); when it is missing, the error names the key and shows an example.

The daemon builds on the target's host with the engine commands it already runs there (`mj-core/src/targets.rs` `ImageHost` and its command wrappers, which handle local and SSH hosts). It first makes sure the target's base image is present (through the image pull gate) and reads its digest for the target's platform. It then generates a Containerfile, deterministic text that is the whole input of the build: `FROM <base>@<digest>`; `ADD --checksum=sha256:<pinned> <mise release URL for the platform> /usr/local/bin/mise` plus `chmod`; `ENV` for `MISE_DATA_DIR=/opt/mise`, `RUSTUP_HOME`, `CARGO_HOME`, `MISE_TRUSTED_CONFIG_PATHS`, `MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS`, and `PATH` with `/opt/mise/shims` first; a `RUN` that writes each recipe file under `/opt/mj-image/recipe/` from base64 text embedded in the Containerfile (so no build context is needed and the layer cache keys on content); `RUN mise bootstrap packages apply --yes && mise install` in that directory as root; labels `dev.mj.image.recipe=<hash>` and `dev.mj.image.mise=<version>`; and the base image's original `USER` and `WORKDIR` restored. The tag is `<repository>/<name>:<hash>` where the hash is the first 16 hex digits of the sha256 of the Containerfile text, so the same recipe on the same base and platform always yields the same tag. If that tag already exists in the registry, skip the build. Otherwise build with the Containerfile on standard input and an empty context, push, then write the reference into `[bundles.<bundle>.images]` for the platform. `--print` prints the Containerfile and does nothing else. `mj image status` reports, per platform, the recorded image and whether its tag matches the hash the current recipe would produce on that target.

The build is a daemon operation that a client starts and then follows. There is no existing template that combines a user-started host job, a streamed log and cancellation (see Surprises in the CLI reconnaissance); implement the smallest one: accepting the request takes `upgrade::activity_unless_draining("image build start")` (`mj-controller/src/upgrade.rs` around 470-489) only for the acceptance itself; the build runs as a supervised daemon task registered so that daemon shutdown and handoff cancel it (terminating the engine process group first, per AGENTS.md); the client long-polls for log text by offset and the final result; operation state lives in daemon memory only because a rerun reproduces it (same tag, engine layer cache). When the client finds the daemon replaced and the operation unknown, it reports that and submits the same request once to the new daemon (the change of daemon identity ends the loop). Ctrl-C cancels the operation. Authentication failures on pull or push report the engine's message and the login command for that host (`podman login <registry>` locally or over `ssh <host>`). Nitro targets are supported because the result is pushed; Apple `container` builds with `container build`.

Milestone 4 writes mjolnir's own recipe. Add `mise.toml` (and `mise.lock`) at the repository root that reproduces what Mjolnir development needs from `containers/Containerfile.agent-dev`, excluding mj-owned pieces (worker, `mj`, harness CLIs and bridges, SlopCop): the Rust toolchain from `rust-toolchain.toml` with its components and musl targets, Node 24, Go at the Containerfile's pinned version, Python 3 and uv, Java 25, cargo-nextest, cargo-llvm-cov, flamegraph and samply at the Containerfile's pins, and as `[bootstrap.packages]` the apt packages the Containerfile installs for builds, tests and profiling (for example libasound2-dev, mold, musl-tools, lcov, heaptrack, linux-perf, eatmydata, time, clang, cmake, jq, ripgrep, fd-find, tmux, zip, the Playwright Chromium system dependencies). Install the Playwright headless shell into a fixed `PLAYWRIGHT_BROWSERS_PATH` through a mise task or hook only if mise can express it; record anything it cannot express in Surprises and leave it out. Build and push it with `mj image build --name mj-dev` for linux/amd64 (and linux/arm64 if an arm64 host is available) to `ghcr.io/brokkai/mjolnir/mj-dev`, from an isolated test instance.

Milestone 5 switches the default. Split the constant: the release image (`MJ_AGENT_DEV_IMAGE`, worker baked in) remains the image for the daemon-container mode and quota containers; a new default session image constant is `docker.io/library/buildpack-deps:noble`, used by `default_container_image()` and `is_default_image` in `mj-core/src/config/targets.rs`. Make `release_image_supplies_worker` and the doctor's exemption refer to the release image explicitly (they will no longer match the session default). Check the legacy-default migration in `mj-core/src/config.rs` (around 1000-1027 and 1383) and the applied-settings default marker so that an upgraded configuration follows the new session default and old sessions' records stay readable. Update `docs/src/content/docs/targets.md`, `containers.md`, `custom-images.md` (the post-clone mise step and `mj image`), add a section or page on project environments, and document `mj image build` and `mj image status` in `mj-core/assets/skills/mj/SKILL.md` under Commands. Verify in an isolated instance that every harness kind starts on `buildpack-deps:noble` (the worker installs Node and bridges); record failures in Surprises and fix those that are Mjolnir's.

## Concrete Steps

All commands run from the repository root. Every daemon, CLI and TUI invocation of a new build uses a named test instance, for example `--instance mise-img`, and the instance's daemon is stopped afterwards with `mj --instance mise-img daemon stop`. Never point a test build at the default instance. Run `cargo test` outside the restricted sandbox.

Focused tests while iterating, for example:

    cargo test -p mj-core mise
    cargo test -p brokk-mj-worker project_tools
    cargo test -p mj-cli image
    cargo test -p mj-controller session_image

A local registry for push tests, removed afterwards:

    podman run -d --name mise-img-registry -p 5000:5000 docker.io/library/registry:2
    # [images] repository = "localhost:5000/mj"  (and the engine's registries.conf or --tls-verify=false for this test registry)
    podman rm -f mise-img-registry

Full validation once, at the end:

    cargo clippy --all-targets -- -D warnings
    cargo test -p mj-core -p brokk-mj-worker -p mj-controller -p mj-cli -p mj-client

## Validation and Acceptance

Milestone 1 is accepted when, in an isolated instance with a Podman target on `buildpack-deps:noble`, a session for a test repository containing `rust-toolchain.toml` (channel 1.99.1) and `.nvmrc` (24) shows the "Install project tools" preparation step, and inside the session `!cargo --version` prints 1.99.1 and `!node --version` prints v24.x. A repository whose `mise.toml` names a nonexistent tool still yields a running session with a visible notice naming the log. A repository with no recipe shows no tools step beyond a sub-second check.

Milestone 2 is accepted when a bundle with `[bundles.x.images] "linux/amd64" = "<ref>"` makes a new session on an amd64 Podman target run `<ref>` (visible in `podman inspect` of the session container), while a session of another bundle on the same target runs the target's image, and an explicit container upgrade of the first session keeps `<ref>`.

Milestone 3 is accepted when `mj --instance mise-img image build --print` prints a Containerfile matching a checked-in golden file for a fixed recipe and base digest (golden test in `mj-cli`), and `mj --instance mise-img image build` against the local registry builds, pushes, records the reference in the instance's configuration, and a second run with the same recipe prints that the image already exists and does not build. `mj image status` output is covered by a golden test. A build started and then interrupted by `mj --instance mise-img daemon restart` is resubmitted by the client and completes.

Milestone 4 is accepted when a mjolnir session on the `mj-dev` image runs `cargo nextest run -p mj-core` and the Playwright web tests without installing anything, and the project-tools step finishes in under a second.

Milestone 5 is accepted when a fresh isolated instance with no `image` keys starts sessions on `buildpack-deps:noble` for each harness kind, the daemon-container mode and quota containers still use the release image, and docs and the skill describe the new behavior.

Tests to add follow AGENTS.md "Which tests to add": golden tests for `mj image build --print` and `mj image status`; an end-to-end test (isolated instance, real Podman) for the post-clone step and for build-and-push to a local registry; and tests for the tricky conditions: a failing recipe never fails the session, a cancelled build leaves no tag and no running engine process, and a daemon restart mid-build is followed by one resubmission.

## Idempotence and Recovery

The project-tools step is idempotent: mise skips installed tools and packages, so rerunning it on worker restart or resume costs well under a second. Image builds are content-addressed; rerunning a build reuses the engine's layer cache and an existing pushed tag. Writing `[bundles.<id>.images]` replaces one platform's entry. To undo a project image, delete the entry. Test registries and instances are disposable; remove them after testing.

## Artifacts and Notes

(To be filled with the golden Containerfile excerpt, timings from the end-to-end tests, and the `mj-dev` digest.)

## Interfaces and Dependencies

No new crates or Cargo dependencies are expected. mise is a pinned external binary described by `mj-core/assets/mise/runtime.json` with `MISE_VERSION` in `mj-core/src/harness_runtime.rs`. Container builds use the target host's engine CLI (`podman build`, `docker build`, `container build`), which must support `ADD --checksum` and reading the Containerfile from standard input; an engine that does not reports its version and the requirement.

In `mj-core/src/config/targets.rs`, `ProjectBundle` gains `pub images: BTreeMap<String, String>`. In `mj-core/src/config.rs`, `Config` gains `pub images: ImagesConfig` with `pub repository: Option<String>`, and the effective image function `Config::session_image`. The daemon API gains an image-build start request, a follow request (operation id, log offset) returning log text and an optional final result, a cancel request, and an image-status request; `mj-client` and `mj-cli` gain matching methods and commands.

Revision note (2026-11-16): first version, written after reconnaissance and the user's design decisions in conversation.
