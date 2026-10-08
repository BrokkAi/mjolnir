# Version agent-dev images with releases and bake the worker in

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md` at the repository root. Read
that file before editing this plan. This plan implements GitHub issue #1139, "Version the
agent-dev image per release, bake the worker into it, and replace containers on upgrade."

## Purpose / Big Picture

Today every container session gets two things that are not tied to the running `mj` binary: the
session image (always `ghcr.io/brokkai/mjolnir/agent-dev:latest`) and the session worker (a host
file that the daemon copies into the container). Because nothing ties either artifact to the
running `mj`, an upgrade can faithfully reinstall a stale worker or start a session on a cached
`:latest` image, which is what #1138 was about. #1138 added a build-stamp check so a mismatched
worker is refused; this plan removes the mismatch by construction.

After this work a person who installs Mjolnir 2.X.Y gets container sessions that run exactly the
`agent-dev` image published for 2.X.Y, and (in later milestones) that image already contains
2.X.Y's worker. Upgrading `mj` means new containers from the new image, with the workspace carried
over; nothing is patched inside a running container. A developer building from a checkout keeps
using the master `:latest` image.

The user-visible behavior to build first, in this milestone: `mj` 2.X.Y starts new container
sessions from `agent-dev:2.X.Y`, while a checkout build still starts them from `agent-dev:latest`.
The pull policy needs no new setting: a version tag is immutable, so the existing `Auto` policy
already resolves to "pull only if missing" for it (see `ImagePullPolicy::resolve`).

## Progress

- [ ] (2026-10-08 16:02Z) Write and check in this ExecPlan (issue #1139 claimed with the
      `agent-in-progress` label and the `foundev` assignee).
- [ ] Milestone 1: release builds publish `agent-dev:<version>` from the tagged commit, and the
      release is blocked if that build or push fails.
- [ ] Milestone 2: `mj`'s default container image is version-derived for release builds and
      `:latest` for development builds, including a config migration for files that spell out the
      old literal default.
- [ ] Milestone 3: the image bakes the worker in at `/opt/mjolnir/mj-worker` with a build label,
      built from the same commit as the image.
- [ ] Milestone 4: upgrading `mj` recreates stopped container sessions from the new image
      (checkpoint, provision, restore); running sessions are untouched until they stop.
- [ ] Milestone 5: remove in-place worker copy-in and patching for the default container path;
      bare, SSH-bare, EC2 and custom-image targets keep it. `mj doctor` reports the resolved image
      and worker build.

## Surprises & Discoveries

- Observation: the repository owner is mixed case (`BrokkAi`) but GHCR requires lowercase, so the
  published name is `ghcr.io/brokkai/mjolnir/agent-dev`. The Rust constant already hardcodes the
  lowercase owner; the workflows derive it with `tr '[:upper:]' '[:lower:]'`. Any Rust-side change
  must keep the lowercase owner.
- Observation: `ImagePullPolicy::resolve` already treats a non-digest, non-`:latest` remote image as
  `Missing`, and `at_launch` resolves `Auto` to `Missing`. Switching the default to
  `agent-dev:<version>` therefore changes the effective pull policy with no code change and no new
  schema field.
- Observation: `Config::update` deliberately strips implicit local targets before saving, so a user
  who never edited `[targets.docker]` has no image line on disk and follows whatever the running
  build resolves. Only a user who edited a container target (or ran setup) has a literal image in
  the file.
- Observation: `mj-core/build.rs` already treats the presence of `.cargo_vcs_info.json` as "this is
  a published crate", which is the only reliable in-checkout difference between a `cargo install`
  of a release and a developer's local build.

## Decision Log

- Decision: pin release builds to the immutable tag `agent-dev:<CARGO_PKG_VERSION>` rather than to
  a digest for this milestone, and record the published digest in the release for anyone who wants
  digest pinning. Rationale: repository policy already forbids moving a published tag, and a
  digest would force the release workflow to publish the image before it can compile any binary,
  adding a serial registry dependency to every release. Digest pinning can be layered on later
  without changing the user-visible behavior. Date/Author: 2026-10-08, Codex.
- Decision: choose the channel in `mj-core/build.rs` from `MJ_BUILD_CHANNEL=release` (set by the
  release workflow) or the presence of `.cargo_vcs_info.json` (a crates.io install), defaulting to
  development (`:latest`) otherwise. Rationale: the version string alone cannot distinguish master
  (which already reports the last released version) from that release. Date/Author: 2026-10-08,
  Codex.
- Decision: make the publish workflow reusable with `workflow_call` instead of copying its build
  steps into the release workflow. Rationale: the two callers must build the same multi-arch image
  the same way; one implementation cannot drift. Date/Author: 2026-10-08, Codex.
- Decision: on load of a config older than the new version, replace a container image equal to the
  historical literal default with the build's resolved default, and stop serializing an image that
  equals the resolved default. Rationale: a migrated file must not pin today's version forever;
  the value only becomes concrete again when the user customizes it. Date/Author: 2026-10-08,
  Codex.

## Outcomes & Retrospective

Not yet started. This section will summarize the result against the purpose at each milestone and
at completion.

## Context and Orientation

Mjolnir builds one image, `containers/Containerfile.agent-dev`, published to
`ghcr.io/brokkai/mjolnir/agent-dev`. A "target" is a place a session runs; the container targets
are `LocalPodman`, `LocalDocker`, `AppleContainer`, `SshPodman` and `SshDocker`, each carrying a
`ContainerTemplate`. A "worker" is the `mj-worker` process that runs a session inside the target;
the controller stages a Linux worker binary into the target before starting it.

The pieces this plan touches:

- `containers/Containerfile.agent-dev` — the image definition, `USER hel`, tooling layers, and a
  final metadata-only `LABEL` block.
- `.github/workflows/publish-agent-dev-image.yml` — builds the two architectures natively, joins
  them into one manifest, and publishes `:latest` and `:sha-<7>` on master pushes, weekly, and on
  manual dispatch. It uses a registry build cache so unchanged layers are shared.
- `.github/workflows/release.yml` — runs on `v*.*.*` tags. `verify-version` requires the tag to
  equal every crate version, `ci` runs the full suite, several `build-*` jobs produce platform
  binaries from the tag, `package-*` jobs assemble archives, and `release` publishes the GitHub
  Release and dispatches crates.io publication. It currently builds no image.
- `mj-core/src/config.rs` — `CONFIG_VERSION: u32 = 14`, `DEFAULT_CONTAINER_IMAGE` (the literal
  `ghcr.io/brokkai/mjolnir/agent-dev:latest`), `Config::with_local_targets` (which constructs the
  implicit `podman`, `docker` and `apple-container` targets), and `TryFrom<StoredConfig>` (which
  interprets old files by their `version`).
- `mj-core/src/config/targets.rs` — `ContainerTemplate` and its `default_container_image` serde
  default; `ImagePullPolicy::resolve`.
- `mj-core/build.rs` — computes `MJ_BUILD_ID` (version plus Git revision) and
  `MJ_BUILD_COMMIT_TIME`, and already reads `.cargo_vcs_info.json` for registry installs.
- `mj-core/src/worker_build.rs` — the `MJ-WORKER-BUILD:<version>+<commit>` stamp and
  `verify_worker_build`, added by #1138.
- `mj-controller/src/controller/worker_binary/` — selects, caches and stages worker binaries, and
  `worker_restart.rs` patches workers in place for upgrades.
- `mj-client/src/target.rs`, `mj-controller/src/setup.rs`, `mj-tui/src/setup/schema.rs` — re-export
  and display the default image.
- `scripts/release-workflow.test.mjs` — a Node test that extracts shell steps from `release.yml` and
  exercises them against fixtures. It is the model for testing workflow changes without a registry.

Key terms in plain language. A "manifest list" (also "multi-arch manifest") is one image name that
points at a per-architecture image, so the host pulls the right one. "Digest" is the immutable
content hash of an image or manifest list; "tag" is the movable name. "Musl" is the C library a
statically linked Linux worker is built against so it runs in any container. "Checkpoint" is the
worker's own snapshot used to move a session between targets.

## Plan of Work

Milestone 1 — release images. Turn `publish-agent-dev-image.yml` into a reusable workflow. Add
`on.workflow_call` with inputs `extra_tags` (a space-separated list, default empty) and
`publish_latest` (boolean, default true), and a workflow output `digest`. The merge job builds the
tag list from these inputs: `:sha-<7>` always; `:latest` only when `publish_latest`; each name in
`extra_tags` otherwise. The existing `push`, `schedule` and `workflow_dispatch` triggers keep
their current behavior because they use the defaults. Then add a `publish-agent-dev-image` job to
`release.yml` that calls the reusable workflow with `extra_tags: <version without the leading v>`
and `publish_latest: false`, and add that job to the `needs` of the final `release` job so a failed
image build blocks publication. Add `packages: write` to `release.yml`'s permissions. Record the
returned digest in the release body or as a small asset so digest pinning is available.

Milestone 2 — version-derived default. In `mj-core/build.rs`, compute an image reference and emit
it as `cargo:rustc-env=MJ_AGENT_DEV_IMAGE`. The rule: an explicit `MJ_AGENT_DEV_IMAGE` wins;
otherwise `MJ_BUILD_CHANNEL=release` or the presence of `.cargo_vcs_info.json` yields
`ghcr.io/brokkai/mjolnir/agent-dev:<CARGO_PKG_VERSION>`; otherwise
`ghcr.io/brokkai/mjolnir/agent-dev:latest`. Set `MJ_BUILD_CHANNEL: release` for the whole
`release.yml` workflow so shipped archives embed the version tag. In `mj-core/src/config.rs`
replace the `DEFAULT_CONTAINER_IMAGE` constant with a resolver plus a private
`LEGACY_DEFAULT_CONTAINER_IMAGE` literal, keep a public
`default_container_image()` (or equivalent) that returns the resolved reference, and add a
`skip_serializing_if` predicate so a template whose image equals the resolved default writes no
`image` line. In `TryFrom<StoredConfig>`, when the file's version is at most
`CONFIG_VERSION - 1`, replace any container image equal to the legacy literal with the resolved
default. Bump `CONFIG_VERSION` to 15. Update every re-export and display site
(`mj-client/src/target.rs`, `mj-controller/src/setup.rs`, `mj-tui/src/setup/schema.rs`) and the
configuration docs that name the default.

Milestone 3 — bake the worker. Add a builder stage to `containers/Containerfile.agent-dev` that
builds `mj-worker` for the image architecture from the same commit (mirroring the existing Bifrost
stage), copy the binary to `/opt/mjolnir/mj-worker`, and add a label such as
`org.brokk.mjolnir.worker-build=<version>+<sha>`. Make the publish workflow's path filter include
`mj-worker/**`, `mj-core/**` and the other crates the worker compiles from so `:latest` never
carries a stale worker. Add a build-alignment test in the style of
`scripts/build-alignment.test.mjs`.

Milestone 4 — replace containers on upgrade. Reuse the existing recover-on-a-fresh-target path
(checkpoint, provision, restore) to recreate stopped container sessions from the new image when
the daemon hands off. Store the image version/digest a session runs so "needs recreation" is a
comparison, not a probe. Never touch a running session: it is recreated after it stops or goes
idle, which is what the AGENTS.md worker-replacement rule requires.

Milestone 5 — remove the patch-in-place path for the default image. Once the default container path
serves a worker from the image, delete `stage_worker_binary_for_upgrade`, the staged-binary install,
the digest-gated recovery refresh, and the `docker cp` of the worker for that path. Keep them for
bare local, SSH-bare, EC2 and custom images, guarded by the #1138 stamp check. Make `mj doctor`
report which image and worker build a container session would use and why (release pin, dev latest,
or custom). Document the developer loop (a local `FROM ...:latest` image that `COPY`s the freshly
built worker) so a checkout never silently falls back to a stale worker.

## Concrete Steps

Run all commands from the repository root, `/home/ryansvihla/.codex/worktrees/e7f0/mjolnir`,
unless a command says otherwise. `cargo` commands normally build for the host; a portable container
worker needs an explicit `--target <arch>-unknown-linux-musl`.

Before Milestone 1, read the two workflows end to end so the reusable-workflow edit preserves the
existing triggers:

    sed -n '1,80p' .github/workflows/publish-agent-dev-image.yml
    sed -n '1,40p' .github/workflows/release.yml

After Milestone 1, exercise the workflow text with the existing Node test harness:

    node --test scripts/release-workflow.test.mjs

After Milestone 2, run the focused config and worker-build tests, then the full check:

    cargo test -p brokk-mj-core config
    cargo clippy --all-targets -- -D warnings

Every `cargo test` invocation must run outside the restricted sandbox with elevated permissions:
the suite uses loopback TCP and Unix sockets and can fail with `EPERM` or hang otherwise. Use an
isolated instance name for any command that starts a daemon or a session, for example
`--instance issue-1139`.

## Validation and Acceptance

Milestone 1 is accepted when the workflow tests pass and a reviewer can read `release.yml` and see
that the ticket's tag (`v2.X.Y`) causes a call that builds `agent-dev:2.X.Y` for `linux/amd64` and
`linux/arm64` and that the `release` job cannot publish if that call fails. Because a real registry
push is not reproducible in the sandbox, the acceptance evidence is the workflow text plus the
Node test. The `digest` output is visible in the image job's log.

Milestone 2 is accepted with two observable behaviors. First, a unit test on the pure resolver
shows `("2.37.0", Release)` yields `ghcr.io/brokkai/mjolnir/agent-dev:2.37.0` and
`("2.37.0", Development)` yields `ghcr.io/brokkai/mjolnir/agent-dev:latest`. Second, a config
round-trip test loads a `version = 14` file whose `[targets.podman]` names
`ghcr.io/brokkai/mjolnir/agent-dev:latest`, resolves it to the running build's default, and
confirms a save in a release-channel build writes the version tag while a save in a
development-channel build writes nothing (the default is implicit). The existing test at
`mj-core/src/config/tests.rs` that asserts a container without an image uses `DEFAULT_CONTAINER_IMAGE`
must be updated to the resolved default and must keep passing.

Beyond compilation, prove the default is wired end to end by resolving a `Config` and printing the
implicit `docker` target's image:

    cargo test -p brokk-mj-core a_container_target_without_an_image_uses_the_default_and_names_unknown_keys -- --nocapture

## Idempotence and Recovery

Every step is additive or a re-runnable text edit. Re-running the workflows is safe: the merge job
overwrites the same tags, and a tag that already resolves to the same image is a no-op. The weekly
schedule rebuild is the only step that intentionally ignores the cache, and it does not change
tags' meaning. If the reusable-workflow refactor goes wrong, `publish-agent-dev-image.yml` is
self-contained and can be restored from Git without touching the release workflow. If the config
migration is wrong, the in-memory migration only changes how the next save is written; a user can
revert by setting `image` explicitly. Never rewrite an applied database migration; this plan adds
no database migration. If a release's image fails to build, the release is not published and can
be retried by rerunning the failed workflow jobs for the same tag.

## Artifacts and Notes

The issue text is the source of the design; this plan restates it so a reader needs nothing else.
The short-term guard this plan supersedes was issue #1138, "always install the worker built with
the running mj", which added the `MJ-WORKER-BUILD` stamp and its verification in
`mj-core/src/worker_build.rs` and `mj-controller/src/controller/worker_binary.rs`.

## Interfaces and Dependencies

In `mj-core/build.rs`, emit a stable build-time value consumed by `env!("MJ_AGENT_DEV_IMAGE")` in
`mj-core`. The resolver must be a pure function that can be unit tested without the environment:

    pub enum ImageChannel { Release, Development }
    pub fn container_image_for(version: &str, channel: ImageChannel) -> String;

In `mj-core/src/config.rs`, keep a public accessor that returns the resolved default and a private
literal for the historical name:

    pub fn default_container_image() -> &'static str; // env!("MJ_AGENT_DEV_IMAGE")
    const LEGACY_DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/brokkai/mjolnir/agent-dev:latest";

`ContainerTemplate::image` must keep the serde default of the resolved reference and gain a
`skip_serializing_if` that omits the key when the value equals it. `CONFIG_VERSION` becomes 15, and
`TryFrom<StoredConfig>` treats every earlier version as needing the image migration.

The reusable workflow contract is:

    inputs:
      extra_tags: string, default ""
      publish_latest: boolean, default true
    outputs:
      digest: string
