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

- [x] (2026-10-08 16:02Z) Write and check in this ExecPlan (issue #1139 claimed with the
      `agent-in-progress` label and the `foundev` assignee).
- [x] (2026-10-08 16:11Z) Milestone 1: release builds publish `agent-dev:<version>` from the tagged commit, and the
      release is blocked if that build or push fails.
- [x] (2026-10-08 16:11Z) Milestone 2: `mj`'s default container image is version-derived for release builds and
      `:latest` for development builds, including a config migration for files that spell out the
      old literal default.
- [x] (2026-10-08 17:00Z) Integrate with master and land the work: merged `origin/master`
      (release 2.37.0 and per-architecture macOS packaging) into the two commits, re-ran the checks,
      and pushed to `master`.
- [x] (2026-10-21 12:40Z) Resumed from a fresh session. Re-mapped every path that installs, replaces or
      starts a container worker, the Restart/Resume recreation machinery, and `mj doctor`, and
      revised Milestones 3-5 below to match what the code actually does (see Surprises).
- [x] (2026-10-21 13:20Z) Milestone 3: the image bakes the worker in at `/opt/mjolnir/mj-worker`,
      built from the same commit and labelled with its build; a new container links its `hel` to
      that worker when it is this `mj`'s build and otherwise installs the host worker as before.
      Validated by the workflow alignment tests, a Podman build of the worker stage, the Docker
      integration test, and a real session created, suspended and resumed in an isolated instance on
      a derived image (see Outcomes).
- [x] (2026-10-21 14:10Z) Milestone 4, first part: a container's applied settings record the default
      image it was created from (`ContainerSettings::default_image`); `recreation_required` follows
      the default to this build's, so Restart offers recreation once an upgrade moved it, and
      `default_image_moved` names that case. Breaking migration 87 (older writers would drop the
      field). Restart confirmation wording updated in the TUI, the web viewer and both goldens.
- [x] (2026-10-21 15:05Z) Milestone 4, second part: a quiet session whose worker is stale because
      its default image moved is recreated automatically through Restart's container recreation.
      Its checkpoint uses the new `LatchExclusivity::HoldThroughIdleClose`, which defers instead of
      interrupting work and never restarts the worker; a durable `automatic` column on Restart
      intents (breaking migration 88) keeps those rules for a recovered intent; the worker-upgrade
      coordinator returns `WorkerUpgradeOutcome::RecreateContainer` instead of copying a worker in
      and owns the retry policy through `ContainerRecreationReport`. Validated by two new tests and
      an isolated-instance upgrade from a fake release A to a fake release B (see Outcomes).
- [x] (2026-10-21 15:30Z) Milestone 5: every path that installs a worker in place (recovery,
      checkpoint restart, the export-protocol fallback, a parked sub-agent's wake, the idle upgrade
      and its harness preparation) asks `WorkerInstall::for_target` and re-links the image's worker
      when it is this build, so nothing is copied into such a container; a moved default image
      triggers recreation even when a recovery already replaced the worker; the Move helper runs the
      image's worker directly; preflight and launch admission let a release's own image supply the
      worker without a host copy; `mj doctor` says which worker a container target runs and why, and
      what worker each present image carries; CONTRIBUTING and the container docs describe the dev
      loop and the upgrade behavior.

- [x] (2026-10-21 16:10Z) Independent review of the four commits found eight defects in the
      automatic recreation and the release shortcut; all are fixed in a fifth commit (see
      Surprises and Decision Log), with a daemon regression for the abandoned recreation and an
      isolated-instance run of release-channel builds A, C (unpullable image) and B.

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
- Observation: the in-place config editor (`mj-core/src/config/document.rs`) only writes
  differences between two serializations of the *same* config, the one read from the file
  (`before`) and the one being saved (`after`). It cannot delete a key that is absent from both,
  so a `skip_serializing_if` that hides the default image also hides the migration: the literal
  stayed in the file. The save now drops the legacy literal at the document level for the one
  save that reads a pre-version-15 file.
- Observation: `TryFrom<StoredConfig>` only upgrades versions in an explicit range
  (`matches!(version, 1..=13)`), so adding a config version without extending the range makes
  every existing file fail `validate` with "unsupported Mjolnir config version". The range is now
  `1..=14`. Any later config-version bump must extend it again.
- Observation: master moved from 2.36.0 to 2.37.0 while this work was local, and `7988e3db`
  replaced the single `package-macos` job with `package-x86_64-apple-darwin` and
  `package-aarch64-apple-darwin`. The merge's only conflict was the `release` job's `needs` list;
  the resolution keeps upstream's per-architecture macOS packaging and adds
  `publish-agent-dev-image` to it.

- Observation: a stopped container session has no container. Stopping checkpoints the session and
  destroys its container (`destroy_after_verified_checkpoint_with` in
  `mj-controller/src/controller/lifecycle.rs` sets `target = None`; Podman only defers the removal
  to `cleanup_stopped_target`), and Resume always provisions a new container from the template's
  current image. So "recreate stopped sessions from the new image" already happens on every resume;
  the work in Milestone 4 is about sessions that are still running.
- Observation: `<worker_root>/hel` is the worker's name everywhere. The start command, the relay
  reconnect (`hel worker proxy`), checkpoint transfer, staged Claude and Kimi MCP commands, and the
  daemon's process identity (`hel_match="hel worker run --root $hel_root"` in
  `mj-controller/src/targets/worker_daemon.rs`) all use it, and the worker re-execs itself keeping
  `argv[0]`. An image worker therefore stays behind a `hel` symbolic link to
  `/opt/mjolnir/mj-worker` rather than being started by its own path.
- Observation: the digest-gated recovery refresh hashes `<root>/hel` with `sha256sum`, which follows
  the link; the image's worker and a host binary of the same build never have the same bytes, so
  that refresh would copy a host worker over the link. Milestone 5 must keep it away from a
  container whose worker is the image's.
- Observation: Restart already has a checkpoint-bound container recreation phase
  (`SessionRestartPhase::RecreatingContainer`, `restart_recreate_container` in
  `mj-controller/src/daemon/restart.rs`), driven by `ContainerSettings::recreation_required`, which
  compares the settings applied when the container was created (`target_runtime.container_settings`)
  with the requested ones. The image is not part of that comparison yet.
- Observation: a checkpoint barrier can either interrupt running work (close) or defer while the
  session works (`BarrierBusyPolicy` in `mj-controller/src/controller/checkpoint/barrier.rs`). An
  automatic recreation must use the deferring policy so it can never cancel accepted work.
- Observation: Apple's `container` CLI has no `--format` and no stopped-target recovery plan, so
  reading the image label from a container's configuration is engine-specific. Asking the image's
  worker itself (`exec ... /opt/mjolnir/mj-worker worker build-id`) works the same way on every
  engine and over SSH through `targets::locator_command`.

- Observation: an agent working inside a Mjolnir session inherits `MJ_CONFIG_DIR` and `MJ_DATA_DIR`
  from its worker, and those override `--instance`, so `mj --instance NAME ...` silently talks to the
  default instance. Unset both (and `MJ_SESSION_ID`) for every isolated-instance command:

      env -u MJ_CONFIG_DIR -u MJ_DATA_DIR -u MJ_SESSION_ID ./target/debug/mj --instance issue-1139-image daemon status

- Observation: a container session needs its repository to have a network remote; a local-only
  fixture is refused with "needs valid default network Git remotes". A clone of a small public
  repository works as the fixture.

- Observation (review): a crates.io build counts as a release for its image name, but its worker
  identity falls back to the commit (`.cargo_vcs_info.json`), while the image's worker carries the
  Git-tree identity of its inputs. The two can never match, so the release shortcut (skip the host
  preflight, "no host worker is needed" in doctor) was wrong for it. Evidence: `worker_inputs_id`
  in `mj-core/build.rs` returns `fallback_revision` when `.cargo_vcs_info.json` exists.
- Observation (review): a Restart that fails keeps its intent, and `remove_completed_lifecycle`
  keeps a failed lifecycle whose intent is still set. A failed automatic recreation therefore left
  the session owned by a failed Restart: every later attempt stood down on the lifecycle entry and
  automatic continuation refused the session, until a person acted.
- Observation (review): a worker's identity covers committed inputs only, so on a development
  checkout with uncommitted worker edits the image's worker and the developer's report the same
  build; preferring the image would silently run master's worker.
- Observation (review): Restart retires the old container before provisioning the new one, so an
  automatic recreation on a host that cannot pull the new image would have stopped idle sessions
  nobody asked to touch.

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
- Decision: drop the legacy literal from the saved document, rather than trusting
  `skip_serializing_if` alone. Rationale: the in-place editor compares two serializations of the
  same config, so a key both omit is preserved from the file; the explicit drop is what makes the
  migration visible on disk. Date/Author: 2026-10-08, Codex.
- Decision: keep `DEFAULT_CONTAINER_IMAGE` as a `const &str` built from `env!("MJ_AGENT_DEV_IMAGE")`
  instead of replacing it with a function. Rationale: every reader, re-export and the TUI's
  editable-defaults JSON keep working unchanged, and the value is still resolved once at compile
  time by `build.rs`. Date/Author: 2026-10-08, Codex.
- Decision: bump `CONFIG_VERSION` to 15 and extend the upgrade range to `1..=14`. Rationale: this
  is the repository's established way to change the file's meaning, and the extended range is what
  keeps an installed 14 file loadable. The cost is that an older binary refuses a version-15 file,
  which the version gate already reports as "Update Mjolnir". Date/Author: 2026-10-08, Codex.

- Decision: decide a container's worker after the container exists, by asking the image's worker
  for its build (`worker build-id`), not from the image label. The image label
  `org.brokk.mjolnir.worker-build` is still written, and the image build fails unless the compiled
  worker reports exactly that label, so the two cannot disagree in a published image. Rationale:
  the binary is the one owner of its own build; the probe is the same on Podman, Docker, Apple
  `container` and SSH engines; and it distinguishes "no worker" (`absent`) from "a worker that
  cannot run" (a failed command) instead of reading an empty label as either. Date/Author:
  2026-10-21, Claude.
- Decision: link `<root>/hel` to `/opt/mjolnir/mj-worker` with `ln -sfn` instead of starting the
  image path directly or copying the file inside the container. Rationale: every caller and the
  process-identity probe keep working unchanged, nothing is duplicated, and the link always names
  the container's own immutable image. Date/Author: 2026-10-21, Claude.
- Decision: when the container's image does not carry this `mj`'s worker build, install the host
  worker exactly as before, still guarded by the #1138 build stamp. Rationale: this is the designed
  path for custom images and for development builds whose worker differs from master's `:latest`,
  not a fallback that hides a failure: the invariant that a session's worker is this `mj`'s build
  holds either way, and when no host worker exists the error names both the image's build and the
  missing host worker. The release guarantee (the image carries the release's worker) is enforced
  where it is made, by the image build itself. Date/Author: 2026-10-21, Claude.
- Decision: the developer loop is the existing host copy-in (`scripts/run.sh` already builds the
  musl worker beside `mj`), not a locally derived image. Rationale: it needs no image rebuild per
  edit, and the stamp check already guarantees a development session never runs a stale worker.
  `scripts/build-agent-dev-image.sh` exists to build the full image from a checkout, with the build
  identities the worker stage needs. Date/Author: 2026-10-21, Claude.
- Decision: compile the worker in a builder stage that copies every workspace member, rather than
  copying a worker the workflow built separately. Rationale: the issue asks for a self-contained
  image built from the same commit, and Cargo needs every member's sources to resolve the
  workspace. `.dockerignore` admits exactly the members, and a test keeps both lists equal to
  `Cargo.toml`. Date/Author: 2026-10-21, Claude.

- Decision: record only the default image a container was created from
  (`ContainerSettings::default_image`), and derive the requested value as "this build's default"
  when one was recorded. Rationale: the default is the only image that moves with an upgrade, and
  the comparison then needs no configuration: `ContainerSettings::recreation_required` is called
  from the database admission check and the dashboard projections, which have the session record
  but not the configuration. A configured image changes only when the person edits it, which
  Restart does not track today and this issue does not ask for. Date/Author: 2026-10-21, Claude.
- Decision: classify migration 87 as breaking and raise the compatibility floor to 87, exactly as
  migrations 79 and 83 did for earlier `target_runtime_json` fields. Rationale: an older daemon
  rewrites that JSON without the new field, after which the container would never be recognized as
  due for recreation. Date/Author: 2026-10-21, Claude.

- Decision: recreate a running session's container automatically, once it is quiet, through the
  existing Restart recreation phase rather than a new path. Rationale: the issue asks for "the next
  time they stop or go idle", and Restart already owns a checkpoint-bound, durable,
  handoff-recoverable recreation (`SessionRestartPhase::RecreatingContainer`). Stopped sessions need
  nothing: they have no container, and Resume provisions from the new image. Date/Author:
  2026-10-21, Claude.
- Decision: express "never interrupt work" as a third latch mode,
  `LatchExclusivity::HoldThroughIdleClose`, instead of a separate busy-policy argument. Rationale:
  every place that already branches on the latch mode had to decide the automatic case anyway
  (interrupt or defer at the barrier, restart an unresponsive worker or not, cancel a capacity
  retry or not); one enum keeps those decisions in one vocabulary. It asks
  `RelayOperationalState::safe_to_replace`, the predicate the upgrade observer used, both before
  opening the barrier and again once it is ready, because background commands run on under a
  barrier. Date/Author: 2026-10-21, Claude.
- Decision: store `automatic` on the Restart intent (migration 88, breaking) rather than keeping it
  in memory. Rationale: a handoff or crash mid-recreation is recovered from the intent by the next
  daemon; without the marker it would be recovered as a Restart a person asked for and could
  interrupt a turn the worker started meanwhile. Date/Author: 2026-10-21, Claude.
- Decision: the coordinator keeps the retry policy; the daemon runs the recreation and reports back
  through `WorkerUpgradeObserver::report_recreation`. Rationale: the recreation must run outside the
  coordinator's attempt (Restart reserves the same recovery gate the attempt holds), but one owner
  must decide when the next attempt is due. A stand-down waits like a deferral, a failure takes the
  widening backoff, and `recreation_pending` blocks attempts until the report arrives. Date/Author:
  2026-10-21, Claude.
- Decision: decide "recreated or stood down" from the durable record
  (`ContainerSettings::default_image_moved` after the lifecycle finished), not from a new lifecycle
  result variant. Rationale: the record is the owner of the fact the coordinator acts on, and the
  answer is then the same whichever path (including a joined Restart) ended the operation.
  Date/Author: 2026-10-21, Claude.
- Decision: an automatic recreation keeps sub-agents. It stands down while any child of the session
  still has a target, checked in the before-close hook under the checkpoint barrier. Rationale: a
  person's Restart stops children, but children's workers (parked ones included) live in the
  parent's container, and an automatic operation must not end work nobody chose to end. Under the
  barrier the parent cannot start another child, so the check cannot race. Date/Author: 2026-10-21,
  Claude.
- Decision: admit an automatic recreation with `activity_unless_draining("upgrade container
  recreation")` (the new `LifecycleStart::deferrable_admission`). Rationale: AGENTS.md requires
  deferrable work to be refused while a handoff drains. Once admitted it holds admission until it
  finishes, as a person's Restart does, because after the old container is retired the operation
  must complete; see Outcomes for this limitation. Date/Author: 2026-10-21, Claude.

- Decision: keep the host copy-in for the exceptional in-place paths (recovery of a dead worker,
  the worker restart a wedged checkpoint needs, and the export-protocol fallback) only when the
  container's image cannot supply this build, i.e. a container an upgrade left on an older image.
  Rationale: those paths run when a session would otherwise be unrecoverable or impossible to
  checkpoint, and a recreation itself needs a checkpoint from a live worker; refusing would strand
  the session. The routine path no longer patches (Milestone 4), and the coordinator now asks
  `default_image_moved` before `AlreadyCurrent`, so a container whose worker a recovery replaced
  is still recreated from the new image as soon as it is quiet. Date/Author: 2026-10-21, Claude.
- Decision: re-link rather than compare digests when the image carries this build. Rationale: a
  host copy and the image's worker of the same build never share bytes, so the existing digest gate
  would have copied a host worker over a current image worker on every recovery; `ln -sfn` to a
  staged name and a rename are cheap, atomic and idempotent. Date/Author: 2026-10-21, Claude.
- Decision: skip the host-worker preflight only for a release build whose template names its own
  default image (`release_image_supplies_worker`, from `RELEASE_IMAGE_CARRIES_WORKER`).
  Rationale: preflight runs before the image is even pulled, so it cannot ask the image; the
  release workflow is what guarantees that image carries the release's worker. Installation still
  asks the image's worker and fails, naming both, if neither it nor a host worker is this build.
  Date/Author: 2026-10-21, Claude.
- Decision: `mj doctor` reads the image label (from the `image inspect` JSON it already fetches,
  which for Podman replaces `image exists`) instead of starting a container. Rationale: doctor
  predicts, and must not create containers outside `--smoke`; the label is verified against the
  binary when the image is built. Date/Author: 2026-10-21, Claude.

- Decision (review fixes): `MJ_RELEASE_IMAGE_CARRIES_WORKER` / `RELEASE_IMAGE_CARRIES_WORKER` is
  set only for a release whose worker identity came from its inputs, and it alone enables the
  release shortcut and the image-first choice. Every other build prefers the worker it was built
  with and runs the image's worker only when it has none. Date/Author: 2026-10-21, Claude.
- Decision (review fixes): before an automatic recreation is admitted,
  `Controller::prepare_container_recreation` pulls the new image under the shared pull lock and
  confirms a worker (the image's label, or the host preflight); a failure takes the coordinator's
  backoff and touches nothing. An automatic recreation that fails before sealing releases its
  intent and reports through the coordinator (`AutomaticRecreationAbandoned`), not as a failed
  close on the session. Its checkpoint releases handoff admission (`begin_resumable_work`) and
  re-takes it before teardown, because until then it restarts from its durable intent. It joins a
  person's Restart (same workspace key) and stands down when a person's intent already owns the
  session. A parent whose sub-agents occupy its container is upgraded in place meanwhile and is
  recreated once they are gone; the coordinator no longer trusts its "known current" cache for a
  container whose default image moved. The occupancy predicate is `State::subagents_occupy_container`.
  Date/Author: 2026-10-21, Claude.

## Outcomes & Retrospective

Milestones 1 and 2 are complete. A tagged release now builds and publishes
`ghcr.io/brokkai/mjolnir/agent-dev:<version>` from the tag through the same reusable workflow that
publishes `:latest` from master, the release is blocked if that build fails, and the release notes
carry an `agent-dev-image.txt` asset naming the published digest. The default container image is
now resolved at build time: `MJ_BUILD_CHANNEL=release` (set for the whole release workflow) or a
crates.io source package bakes `agent-dev:<version>`; anything else bakes `agent-dev:latest`.
Verified by building the CLI both ways and reading the string out of the binary:

    MJ_BUILD_CHANNEL=release cargo build -p brokk-mjolnir
    strings target/debug/mj | grep -o 'ghcr.io/brokkai/mjolnir/agent-dev:2[^ ]*'
    → ghcr.io/brokkai/mjolnir/agent-dev:2.36.0

    cargo build -p brokk-mjolnir
    strings target/debug/mj | grep -c 'agent-dev:2.36.0' → 0

What remains: milestones 3 through 5, which bake the worker into the image and recreate stopped
containers on upgrade. Until those land, the container path still copies the worker in and relies
on the #1138 build-stamp check. Documentation examples that show an explicit
`image = "...:latest"` were removed from the default-target examples and the configuration
reference; `docker pull ...:latest` commands in the runtime guides were left, since pulling the
master image to prove the registry is reachable is still meaningful.

The lesson worth keeping: `skip_serializing_if` and the in-place editor do not compose. Any future
field whose default is meant to be *absent* from an existing file needs a document-level step in
the save that migrates it, not just a serialization predicate.

Milestone 3 is complete. `containers/Containerfile.agent-dev` has a `worker` stage that compiles
`mj-worker` for the image's musl target from the build context (every workspace member, admitted by
`.dockerignore`), refuses to finish unless the compiled worker reports exactly the
`MJ_WORKER_BUILD_ID` it was given, and copies it to `/opt/mjolnir/mj-worker` as the last filesystem
layer, labelled `org.brokk.mjolnir.worker-build`. The publish workflow computes the identities with
`mj-core/worker_build_inputs.py` and rebuilds `:latest` when any worker input changes;
`scripts/build-agent-dev-image.sh` does the same for a local build. In the controller,
`WorkerInstall::for_target` (`mj-controller/src/controller/worker_binary/image_worker.rs`) asks a
container's image worker for its build with the new hidden `worker build-id` command and either
links `<root>/hel` to it or installs the host worker exactly as before.

Evidence. The worker stage built under Podman and printed the identity this checkout's `mj` uses:

    scripts/build-agent-dev-image.sh podman localhost/mjolnir/agent-dev-worker-stage:issue-1139 --target worker
    ...
    Finished `release` profile [optimized] target(s) in 1m 21s
    2.51.0+2254268f81bbdb9e171fb6ec90e567a59a3337c0

The ignored Docker integration test `a_container_runs_its_images_worker_through_the_hel_link`
(run with `MJ_WORKER_BINARY` set to that worker and `-- --ignored`) passed: no image worker selects
the host worker, an unusable one is an error, a matching one is linked, the worker started through
the link reports this build, and the daemon's liveness probe still recognizes it. A real session in
the isolated instance `issue-1139-image`, whose Docker target names a derived image
(`FROM agent-dev:latest` plus that worker and label), ran with

    /var/lib/hel/workers/9311ccf6.../hel -> /opt/mjolnir/mj-worker
    187 /var/lib/hel/workers/9311ccf6.../hel worker run --root /var/lib/hel/workers/9311ccf6... --config ...

and after `mj suspend` (the container was removed) and `mj resume` it ran in a new container with
the same link.

Milestone 4 is complete. Evidence from the isolated instance `issue-1139-upgrade`, using two fake
releases built from this tree: release A (`MJ_AGENT_DEV_IMAGE=localhost/mjolnir/agent-dev:issue-1139-a`,
`MJ_WORKER_INPUTS_ID=aaaa...`) and release B (image `...-b`, inputs `bbbb...`), each image derived
from `agent-dev:latest` plus its own worker and label. A session created under A ran from image A
with `hel -> /opt/mjolnir/mj-worker` and recorded `default_image: localhost/mjolnir/agent-dev:issue-1139-a`.
After `mj daemon restart` from build B:

    15:00:38 state=running    image=localhost/mjolnir/agent-dev:issue-1139-a
    15:00:44 state=suspending image=localhost/mjolnir/agent-dev:issue-1139-a
    15:00:57 state=suspending image=
    15:01:03 state=running    image=localhost/mjolnir/agent-dev:issue-1139-b

The new container's `hel` links to image B's worker, which reports `2.51.0+bbbb...`; the record
names image B; no Restart intent and no error remain; and the daemon log shows
"Restart is recreating the container" followed by "recreated the session container from this
Mjolnir's image", with no worker upload. The busy case is covered by
`an_automatic_restart_stands_down_instead_of_cancelling_a_running_turn`, which drives a real relay
actor against a stand-in worker holding a running turn, and the retry policy by
`a_recreation_holds_further_attempts_until_the_daemon_reports_it`.

Limitation: once admitted, an automatic recreation holds daemon handoff admission until it
finishes, like a person's Restart, and exporting a large workspace can take minutes. It is only
started after a handoff and is refused while one drains, so this matters only for two upgrades in
quick succession.

Milestone 5 is complete. With release B's daemon running and its host worker replaced by a
debug build of the same worker identity (same `worker build-id`, different bytes, so the old digest
gate would have copied it), killing the session's worker made recovery restart it with

    lrwxrwxrwx. 1 hel hel 22 Oct 21 13:18 hel -> /opt/mjolnir/mj-worker

and no host copy. `golden_doctor_reports_external_aws_ssh_docker_and_podman_observations` now
covers the sentence an image check adds about the image's worker.

Overall outcome: the issue's acceptance holds. A release `mj` names `agent-dev:<version>`
(Milestone 2), whose worker is baked in and verified at image build time (Milestone 3), and new
container sessions link to it. After an upgrade a stopped session resumes on a new container from
the new image, and a running one is recreated from it once idle, never interrupting work
(Milestone 4); nothing is copied into a container whose image carries the current worker
(Milestone 5). A development build runs `:latest` and installs its own musl worker whenever that
image's worker is not exactly its build; a custom image keeps the stamp-guarded copy-in. A failed
image build already blocks the release (Milestone 1).

What remains outside this plan: bare local, SSH-bare and EC2 targets have no image and keep the
#1138-guarded copy-in by design. Versioned images accumulate on hosts (`image prune -f` removes
only dangling ones); a cleanup of old `agent-dev:<version>` tags could follow. The Containerfile
and workflow changes take effect on the next push to master and the next release; the first
release built from them is the first whose image carries its worker.

Lesson: the worker's name `<root>/hel` is load-bearing far beyond installation (process identity,
reconnect, staged MCP commands), so a change of where the worker comes from has to keep that name.
A link did, and made the rest of the change local.

Review fixes, evidence from the isolated instance `issue-1139-release` with release-channel builds
(`MJ_BUILD_CHANNEL=release`): under A, a session started with no host worker configured and ran
`hel -> /opt/mjolnir/mj-worker` reporting `2.51.0+aaaa...` with no worker upload. Handing off to C,
whose image `localhost/mjolnir/agent-dev:issue-1139-missing` cannot be pulled, left the session
running on its A container with no error and no intent, and logged "container recreation for the
upgrade failed ... pull container image ...". Starting B then moved it within seconds:

    16:08:18 state=running    image=localhost/mjolnir/agent-dev:issue-1139-a
    16:08:22 state=suspending image=localhost/mjolnir/agent-dev:issue-1139-b
    16:08:26 state=running    image=localhost/mjolnir/agent-dev:issue-1139-b

`a_failed_automatic_recreation_gives_back_its_intent_and_records_no_failure` fails without the
release-on-abandon fix and passes with it.

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
it as `cargo:rustc-env=MJ_AGENT_DEV_IMAGE`, plus `MJ_AGENT_DEV_IMAGE_RELEASE` (1 or 0). The rule:
an explicit `MJ_AGENT_DEV_IMAGE` wins; otherwise `MJ_BUILD_CHANNEL=release` or the presence of
`.cargo_vcs_info.json` yields `ghcr.io/brokkai/mjolnir/agent-dev:<CARGO_PKG_VERSION>`; otherwise
`ghcr.io/brokkai/mjolnir/agent-dev:latest`. Set `MJ_BUILD_CHANNEL: release` for the whole
`release.yml` workflow so shipped archives embed the version tag. In `mj-core/src/config.rs`,
keep `DEFAULT_CONTAINER_IMAGE` as a `const &str` whose value is `env!("MJ_AGENT_DEV_IMAGE")`
(every reader and re-export then works unchanged), add `CONTAINER_IMAGE_REPOSITORY`,
`LEGACY_DEFAULT_CONTAINER_IMAGE`, the pure rule `container_image_for(version, release)` and
`migrate_legacy_default_image(target, default_image)`. Add a `skip_serializing_if` predicate so a
template whose image equals the resolved default writes no `image` line. In `TryFrom<StoredConfig>`,
for a file at or below `LEGACY_DEFAULT_IMAGE_LAST_VERSION`, replace any container image equal to
the legacy literal with the resolved default. Extend the upgrade range from `1..=13` to `1..=14`
and bump `CONFIG_VERSION` to 15. Because the in-place editor only diffs two serializations of the
same config, also drop the legacy literal from the document in `document.rs` during that first
save; otherwise the file keeps the old value. No re-export sites change, but the configuration
docs and the embedded skill reference that name the default and the schema version are updated.

Milestone 3 — bake the worker. `containers/Containerfile.agent-dev` gains a `worker` stage from the
same Rust image that copies `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and every workspace
member (Cargo resolves the whole workspace to build one package), requires the build arguments
`MJ_BUILD_REVISION`, `MJ_WORKER_INPUTS_ID` and `MJ_WORKER_BUILD_ID` (Git is not in the context),
builds `brokk-mj-worker` for the musl triple of the build architecture, checks the binary is static
and that `mj-worker worker build-id` prints exactly `MJ_WORKER_BUILD_ID`, and leaves it at
`/out/mj-worker`. The final stage copies it to `/opt/mjolnir/mj-worker` as its last filesystem
layer and labels the image `org.brokk.mjolnir.worker-build`. `.dockerignore` admits the members;
`scripts/build-agent-dev-image.sh` and the publish workflow compute the three identities with
`mj-core/worker_build_inputs.py`, and the workflow's push filter lists every entry of
`mj-core/worker-build-inputs.txt`. Two tests in `scripts/release-workflow.test.mjs` keep those lists
aligned. In the worker, a hidden `worker build-id` prints `WORKER_BUILD_ID`. In the controller,
`mj-controller/src/controller/worker_binary/image_worker.rs` defines `WorkerInstall` (`Image` or
`Host(path)`) and `WorkerInstall::for_target`, which asks the container's `/opt/mjolnir/mj-worker`
for its build and chooses `Image` only when it equals this controller's; `install_worker_files`
then runs `ln -sfn /opt/mjolnir/mj-worker <root>/hel` instead of copying, and skips the chown and
chmod a copy needs.

Milestone 4 — replace containers on upgrade. A stopped session already gets a new container on
resume, so the work is for running sessions. `ContainerSettings` (in
`mj-core/src/state/target_runtime.rs`) records `default_image` when provisioning created the
container from this build's default image, `requested` follows the default to this build's, and
`default_image_moved` names a container left on an older one; migration 87 is breaking.
`Controller::upgrade_session_worker` returns `WorkerUpgradeOutcome::RecreateContainer` for such a
session before resolving any binary; the daemon (`spawn_container_recreation` in
`mj-controller/src/daemon/support.rs`) calls `RuntimeState::recreate_container_for_upgrade`
(`mj-controller/src/daemon/restart.rs`), which admits an automatic Restart only while no handoff
drains and reports the outcome back to the coordinator. The automatic intent is recorded with
`automatic = 1` (migration 88, breaking), its checkpoint uses `LatchExclusivity::HoldThroughIdleClose`
(defer instead of interrupting, never restart the worker), its before-close hook keeps sub-agents
(standing down while any child has a target), and a stand-down releases the intent and leaves the
session running with no error.

Milestone 5 — no patching where the image can supply the worker. `replace_installed_worker_binary`,
`stage_worker_binary_for_upgrade`, `prepare_managed_harness_for_upgrade`, the recovery refresh
(`prepare_recovery_worker_binary`, `refresh_target_worker_binary_if_stale`) and the export fallback
take a `WorkerInstall`; for `Image` they stage a link and rename it into place. The Move helper runs
`/opt/mjolnir/mj-worker` directly in that case. `preflight_worker_binary` and
`worker_source_problem` accept a release's own default image without a host worker
(`release_image_supplies_worker`). `mj doctor`'s worker check names the image and why it is used,
and its image checks add what worker the image carries, read from the label in the `image inspect`
JSON. CONTRIBUTING and `docs/src/content/docs/containers.md` describe the development loop and the
upgrade behavior.

## Concrete Steps

Run all commands from the repository root unless a command says otherwise. `cargo` commands normally build for the host; a portable container
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

For Milestone 3, build the image's worker stage and a derived test image, then run the Docker
integration test against that worker:

    scripts/build-agent-dev-image.sh podman localhost/mjolnir/agent-dev-worker-stage:issue-1139 --target worker
    mkdir -p target/issue-1139
    podman create --name extract localhost/mjolnir/agent-dev-worker-stage:issue-1139 true
    podman cp extract:/out/mj-worker target/issue-1139/mj-worker && podman rm extract
    printf 'FROM ghcr.io/brokkai/mjolnir/agent-dev:latest\nCOPY mj-worker /opt/mjolnir/mj-worker\n' > target/issue-1139/Containerfile
    docker build -f target/issue-1139/Containerfile -t localhost/mjolnir/agent-dev:issue-1139 target/issue-1139
    MJ_WORKER_BINARY=$PWD/target/issue-1139/mj-worker cargo test -p brokk-mj-controller a_container_runs_its_images_worker_through_the_hel_link -- --ignored

For a real session, write `~/.config/mjolnir/instances/issue-1139-image/config.toml` with a Codex
profile, a bundle whose repository has a network remote, and a `docker` target whose `image` is
`localhost/mjolnir/agent-dev:issue-1139` with `pull_policy = "never"`, then (with the variables above
unset) create a workspace and a session and check the link:

    mj --instance issue-1139-image workspaces create image-test
    mj --instance issue-1139-image --workspace image-test new --profile codex --target docker --bundle fixture
    docker exec <container> readlink /var/lib/hel/workers/<session>/hel   # /opt/mjolnir/mj-worker

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

Milestones 3 to 5 are accepted by these observable behaviors, each recorded in Outcomes: the
image's worker stage builds and its worker reports the identity it is labelled with; a container
session on an image carrying this build has `hel -> /opt/mjolnir/mj-worker` and nothing copied, and
keeps it across suspend/resume and worker recovery; after a daemon handoff to a build with another
default image, an idle session moves to a new container from that image within seconds; and a
session with a running turn is never interrupted
(`an_automatic_restart_stands_down_instead_of_cancelling_a_running_turn`).

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

In `mj-core/src/config.rs`, keep the public constant that returns the resolved default and add the
historical literal and the rule as separate names:

    pub const DEFAULT_CONTAINER_IMAGE: &str = env!("MJ_AGENT_DEV_IMAGE");
    pub const CONTAINER_IMAGE_REPOSITORY: &str = "ghcr.io/brokkai/mjolnir/agent-dev";
    pub const LEGACY_DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/brokkai/mjolnir/agent-dev:latest";
    pub fn container_image_for(version: &str, release: bool) -> String;

`ContainerTemplate::image` must keep the serde default of the resolved reference and gain a
`skip_serializing_if` that omits the key when the value equals it. `CONFIG_VERSION` becomes 15, and
`TryFrom<StoredConfig>` treats every earlier version as needing the image migration.

The reusable workflow contract is:

    inputs:
      extra_tags: string, default ""
      publish_latest: string, default "true"   # a string so the push/schedule triggers keep :latest
    outputs:
      digest: string

Milestones 3 to 5 add, in `mj-controller/src/controller/worker_binary/image_worker.rs`:

    pub(in crate::controller) const IMAGE_WORKER: &str = "/opt/mjolnir/mj-worker";
    pub(in crate::controller) enum WorkerInstall { Image, Host(PathBuf) }
    impl WorkerInstall {
        pub(in crate::controller) fn for_target(locator, executor) -> Result<Self>;
        pub(in crate::controller) fn host_binary(&self) -> Option<&Path>;
    }
    pub(in crate::controller) fn image_worker_build(locator, executor) -> Result<Option<String>>;

in `mj-core/src/state/target_runtime.rs`, `ContainerSettings::default_image: Option<String>`,
`ContainerSettings::applied(session, image)` and `ContainerSettings::default_image_moved(session)`;
in `mj-core/src/config.rs`, `RELEASE_IMAGE_CARRIES_WORKER: bool`; in
`mj-core/src/worker_build.rs`, `is_worker_build(&str) -> bool`; in the worker CLI, the hidden
`mj-worker worker build-id`; `LatchExclusivity::HoldThroughIdleClose`;
`WorkerUpgradeOutcome::RecreateContainer`, `ContainerRecreationReport` and
`WorkerUpgradeObserver::report_recreation`; `SessionRestartIntent::automatic` and
`begin_automatic_session_restart`; `RuntimeState::recreate_container_for_upgrade`; and
`LifecycleStart::deferrable_admission`.

Revision history. 2026-10-08, Codex: recorded milestones 1 and 2 as complete and corrected the
plan's Milestone 2 and Interfaces sections to match what was built. The original plan proposed a
resolver function and `publish_latest` as a boolean; the implementation keeps
`DEFAULT_CONTAINER_IMAGE` as a compile-time constant built from `env!` (so no reader or re-export
changes), needs `publish_latest` as a string because GitHub's `inputs` context is empty for the
push and schedule triggers, and must drop the legacy image from the saved document because the
in-place editor cannot delete a key that both serializations omit.

Revision 2026-10-21, Claude: resumed the plan in a fresh session, mapped the code again, and
replaced the original sketches of Milestones 3 to 5 with what was built and why: the image worker
is linked behind `<root>/hel` because that name is the worker's identity; stopped sessions needed
no work because they have no container; running sessions are recreated through Restart's existing
recreation phase with a new non-interrupting latch mode and a durable `automatic` marker
(migrations 87 and 88); and in-place installs share one `WorkerInstall` decision.
