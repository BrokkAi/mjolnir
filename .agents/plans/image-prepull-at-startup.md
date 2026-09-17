# Download configured container images when the daemon starts

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

When a person creates their first session on a container target, the container engine has to download the image first, often several gigabytes, and the Create sits in "Provisioning" for minutes. After this change the daemon starts downloading every configured container image it does not already have a couple of seconds after it starts, so by the time the person opens the New Session wizard the image is usually there. A Create issued while that download is still running waits for it (showing a "Pull image" stage) instead of starting a second download. A failed download is reported once in the dashboard notices. Images whose pull policy is `never` are not touched.

## Progress

- [x] (2026-09-17 23:20Z) Milestone 1: `RefreshWhen` on `ImageRefresh`, `ImageHost::AppleContainer`, `TargetTemplate::image_host`, plan merge in `image_refresh_plan`, present-skip in `refresh_host_image`; tests.
- [x] (2026-09-17 23:35Z) Milestone 2: `IMAGE_REFRESH_DELAY` to 2 s with a rewritten comment; `the_first_refresh_runs_at_startup`.
- [x] (2026-09-17 23:47Z) Milestone 3: `image_pull_gate` module + tests; the refresher holds the gate.
- [x] (2026-09-17 23:55Z) Milestone 4: `ProvisionStage::PullingImage`; provisioning hooks in `podman_image_user` and `provision_target_creation`; test.
- [x] (2026-09-18 00:12Z) Milestone 5: report closure, daemon-global notices, once-per-change failure suppression; tests.
- [x] (2026-09-18 00:30Z) Milestone 6: docs; `cargo test -p brokk-mj-core -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings` both clean; command forms checked against a real Podman 5.7.0 (completed: everything except the Apple `container` CLI, which cannot run on Linux, and the end-to-end dashboard check, which needs a daemon run on a host with a fresh image store).

## Surprises & Discoveries

- Observation: the workspace crates are named `brokk-mj-core` and `brokk-mj-controller`, so `cargo test -p mj-core` fails with "package ID specification `mj-core` did not match any packages". Use `-p brokk-mj-core -p brokk-mj-controller`.
  Evidence: `mj-core/Cargo.toml:2` and `mj-controller/Cargo.toml:2`; the `[lib] name` is the `mj_core` / `mj_controller` the source refers to.
- Observation: `#[tokio::test(start_paused = true)]` needs a `yield_now().await` before the first `advance`, or the refresher task has not yet created its interval and its deadline moves with the clock.
  Evidence: advancing by exactly `IMAGE_REFRESH_DELAY` first left `the_first_refresh_runs_at_startup` asserting 1 call and seeing 0.

- Observation: on Podman the first-session download does not happen in `container run` but earlier, in the image-user probe (`podman run --rm --entrypoint '' <image> sh -c 'id -u; id -g'`).
  Evidence: `mj-controller/src/controller/provisioning.rs:1086-1125` (`podman_image_user`) → `mj-controller/src/targets/preflight.rs:691-710` (`probe_image_user`). Docker has no probe; its pull happens in `docker run --pull=missing` (`targets/container.rs:293-300`).

## Decision Log

- Decision: `refresh_host_image` takes the report callback, and the once-per-change failure suppression lives in `record_refresh_result`, called by `refresh_images` after each host task joins.
  Rationale: `Started` has to fire before the download begins, which only the function that runs the pull can do. `Failed` has to be suppressed across ticks, which only the long-lived refresher task can do, because each download runs on its own blocking thread. Splitting them this way also made both testable with plain function calls: `a_failed_pull_is_reported_once_until_the_error_changes` drives `record_refresh_result` directly instead of simulating hours of wall-clock time.
  Date/Author: 2026-09-17, Opus.
- Decision: extract the notice filter in `mj-controller/src/daemon/snapshot.rs` into `notice_reaches_workspace` and test that, instead of asserting through `RuntimeState::runtime_snapshot`.
  Rationale: `runtime_snapshot` reads the session database (`load_move_operations`, `list_workspaces`, `session_ids_for_workspace`), so a snapshot-level test needs an isolated `MJ_DATA_DIR` subprocess and a seeded workspace row. The rule being added is one line of policy, and the named function states it where the reader meets it.
  Date/Author: 2026-09-17, Opus.
- Decision: keep `ProvisionStage::PullingImage` open only while the launch is actually waiting, not for the work that follows.
  Rationale: the stage answers "why is this Create not moving", which is true only during the wait. The creation and probe that follow already report their own stages.
  Date/Author: 2026-09-17, Opus.
- Decision: implement `image_host` twice, once on `mj_core::config::TargetTemplate` (in `mj-core/src/targets/convert.rs`) and once on `mj_core::targets::TargetTemplate` (in `mj-core/src/targets.rs`).
  Rationale: the plan named one method, but the two callers hold different types. `image_refresh_plan` walks the configured targets (`config::TargetTemplate`, whose SSH side is an `SshConnection`), while `with_image_ready` in Milestone 4 receives the execution-plan target (`targets::TargetTemplate`, whose SSH side is already an `SshTarget`). Each method is three lines of mapping with no logic to share, and putting the config one in `convert.rs` keeps the `SshConnection` to `SshTarget` conversion where every other such conversion lives.
  Date/Author: 2026-09-17, Opus.
- Decision: put the two new mj-core-level refresh tests in `mj-controller/src/targets/tests.rs`.
  Rationale: `mj-core/src/targets.rs` has no `#[cfg(test)]` module, and `mj-controller/src/targets.rs` re-exports all of `mj_core::targets`, so the existing tests for this code (`an_automatic_pull_policy_never_pulls_during_a_launch`, the failing-pull test) already live there.
  Date/Author: 2026-09-17, Opus.
- Decision: widen the existing hourly refresher rather than add a separate startup task.
  Rationale: `spawn_image_refresher` already has cancellation, per-host blocking threads, and plan recomputation on each tick; only its first-tick delay and its eligibility gate are wrong for this purpose.
  Date/Author: 2026-09-17, Fable.
- Decision: `missing` (including `auto` on a versioned tag or digest) images are pulled once when absent and then only checked with `image inspect` each hour; `always`/`newer` keep today's hourly pull; `never` is excluded.
  Rationale: this adds at most one pull per image ever and no extra registry load per hour.
  Date/Author: 2026-09-17, Fable.
- Decision: include Apple container with `container image inspect` and `container image pull` only, no prune.
  Rationale: the prune flag set cannot be verified on Linux; inspect and pull are enough for a pre-pull. The argument forms must be checked on macOS before this milestone merges.
  Date/Author: 2026-09-17, Fable.
- Decision: no configuration switch for the pre-pull; `never` is the opt-out.
  Rationale: the user asked for the download to start on its own; disk use for unused targets is documented.
  Date/Author: 2026-09-17, Fable.
- Decision: coalesce a Create with an in-flight pull through a per-daemon `std::sync::Mutex` keyed by host and image, polled with `try_lock`.
  Rationale: both sides run on blocking threads through the synchronous `CommandExecutor`; polling keeps the wait cancellable through `executor.cancellation_requested()` without a `Condvar`.
  Date/Author: 2026-09-17, Fable.

## Outcomes & Retrospective

All six milestones are implemented. A daemon now downloads every configured
container image its hosts lack a couple of seconds after it starts, across all
five container target kinds including Apple's `container` engine; `never` is
the only opt-out. A Create issued during a download waits on a per-(host,
image) lock and reports the wait as a "Pull image" stage with a notice, rather
than starting a second download. Downloads, completions and failures reach the
dashboard as daemon-owned notices that every workspace sees, and a host that
keeps failing the same way is reported once rather than hourly.

Two things in the plan could not be done here. The Apple `container` argument
forms (`container image inspect <image>`, `container image pull <image>`) are
implemented as the plan specified but cannot be executed on Linux, so they
remain unverified against the real CLI; that check still has to happen on
macOS before this ships. The end-to-end dashboard observation (start the
daemon against a Podman store without the image, watch the notices, create a
session mid-download) needs a real daemon run and was not performed. What was
verified against a real Podman 5.7.0 is that the three command forms the
refresher builds behave as the code assumes: `podman image inspect --format
'{{.Id}}' <absent image>` exits 125 with empty stdout, `podman pull` then
succeeds, the same inspect then prints the id, and `podman image prune -f`
succeeds.

The two structural surprises were that the two `TargetTemplate` families
needed the same `image_host` method twice, and that reporting had to be split
between the download thread (`Started`, `Pulled`) and the refresher task
(`Failed`, with suppression). Both are recorded in the Decision Log.

## Context and Orientation

A "target" is where a session runs. Container targets are `TargetTemplate::{LocalPodman, LocalDocker, AppleContainer, SshPodman, SshDocker}` (`mj-core/src/config/targets.rs:351-390`), each flattening a `ContainerTemplate { image, pull_policy, platform, … }` (`:221-243`). `ImagePullPolicy` (`:278-288`) is `Auto` (default), `Always`, `Newer`, `Missing`, `Never`. `ImagePullPolicy::resolve` (`mj-core/src/targets.rs:1336-1346`) turns `Auto` into `Newer` for remote `:latest`-style tags and `Missing` otherwise; `at_launch` (`:1352-1360`) turns `Auto` into `Missing` so a launch never pulls explicitly. Podman and Docker launches therefore rely on `--pull=missing`.

The background refresher: `image_refresh(host, image, platform, pull_policy) -> Option<ImageRefresh>` (`mj-core/src/targets.rs:1430-1481`) returns `None` unless the resolved policy is `Always | Newer`; `ImageRefresh { host: ImageHost, image, platform, image_id, pull, prune }` carries the three `CommandSpec`s; `ImageHost { LocalPodman, LocalDocker, SshPodman(SshTarget), SshDocker(SshTarget) }` (`:1379-1416`) with `engine()`, `label()`, `command()`. `image_refresh_plan(config) -> Vec<ImageRefresh>` (`mj-controller/src/controller/backend.rs:565-597`) walks the configured targets and deduplicates. `spawn_image_refresher` (`mj-controller/src/pollers/quota.rs:116-137`) ticks at `now + IMAGE_REFRESH_DELAY` (30 s, `pollers.rs:63`) then every `IMAGE_REFRESH_INTERVAL` (1 h, `pollers.rs:60`), `MissedTickBehavior::Skip`; `refresh_images` (`:139-190`) spawns one blocking task per host with a shared cancel flag through `CancellableProcessExecutor`; `refresh_host_image` (`:192-224`) reads `image_id` before and after the pull and logs. It is spawned from `run_daemon_runtime` (`mj-controller/src/daemon/process.rs:116-121`) with a plan closure re-evaluated every tick and is awaited at shutdown (`:323-328`).

Provisioning runs on a blocking thread per Create (`daemon/create.rs:30,129`) through the synchronous `CommandExecutor` trait (`mj-core/src/targets.rs:490-520`). Progress is reported through `ProvisionStage` (`mj-core/src/targets.rs:37-77`, matched exhaustively only in `label()`), `ProvisionStageGuard`, and `executor.notify_notice`. Dashboard notices are `RuntimeNotice { id, session_id, text }` (`mj-client/src/daemon.rs:212-216`), pushed through `RuntimeState::push_notice` (`daemon/views.rs:348`), and filtered per workspace in `daemon/snapshot.rs:226` by `session_id`.

Keyed-lock precedent: `recovery_gate::worker_target_mutex` (`mj-controller/src/recovery_gate.rs:11-27`) uses `OnceLock<Mutex<BTreeMap<String, Weak<Mutex<()>>>>>`.

## Plan of Work

### Milestone 1: widen the plan

In `mj-core/src/targets.rs`:

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum RefreshWhen {
        /// Pull only when the host has no copy (missing, or auto on a fixed tag).
        WhenAbsent,
        /// Pull every tick (always, or newer / auto on a moving tag).
        Always,
    }

Add `pub when: RefreshWhen` to `ImageRefresh`. `image_refresh` maps the resolved policy: `Always | Newer → Always`, `Missing → WhenAbsent`, `Never → return None`. Add `ImageHost::AppleContainer` (engine `"container"`, label `"apple container"`, local command form; `image_id` command `container image inspect <image>` whose success exit status means present, the trimmed stdout being the opaque id; pull `container image pull <image>` with no `--platform`; no prune command: make `prune: Option<CommandSpec>`). Add `TargetTemplate::image_host(&self) -> Option<(ImageHost, &ContainerTemplate)>` next to `container_engine()` (`:1621`) covering the five container variants.

In `image_refresh_plan` (`backend.rs:565-597`) use `TargetTemplate::image_host` for all five variants and merge duplicates by `(host, image, platform)` with `entry.when = entry.when.max(refresh.when)`. Update the doc comment (it currently says versioned tags and `missing` are left out).

In `refresh_host_image` (`quota.rs:192-224`):

    pub(super) enum ImageRefreshOutcome { Present, Unchanged, Pulled { id: String } }

Read `image_id` first; when `refresh.when == WhenAbsent && cached.is_some()` return `Present` without pulling or pruning; otherwise pull, re-read the id, prune when `prune` is `Some`, and return `Pulled`/`Unchanged`.

Tests: extend `the_image_refresh_plan_covers_every_image_a_launch_no_longer_pulls` (`backend/tests.rs:400`; rename to `the_image_refresh_plan_covers_every_configured_container_image_except_never`) so digest-pinned and Apple entries appear as `WhenAbsent`, a `Never` target is absent, and two targets sharing an image with `Newer` and `Missing` merge to one `Always`; `ssh_docker_image_refresh_runs_docker_on_the_configured_host` asserts `when == Always`; new in `pollers/tests.rs` with fake executors in the style of `a_failed_pull_is_reported_and_leaves_the_other_host_alone` (`:1076`): `a_missing_image_is_pulled_once_and_not_again_when_present` (inspect fails until a pull was seen; two calls; exactly one pull; no prune on the WhenAbsent path), `an_always_refresh_pulls_even_when_the_image_is_present`; in `mj-core` targets tests: `a_never_policy_is_not_pre_pulled`, `an_apple_container_image_refresh_uses_the_container_cli`.

### Milestone 2: first tick at startup

`IMAGE_REFRESH_DELAY` (`pollers.rs:63`) becomes `Duration::from_secs(2)` with the comment rewritten: the first refresh is the pre-pull for the person's first session; two seconds keeps it behind the cleanup and recovery work scheduled at startup, which share the ssh admission slots. Nothing in `run_daemon_runtime` before the refresher uses the registry, and provisioning does not need the refresher idle.

Test: `the_first_refresh_runs_at_startup` (`#[tokio::test(start_paused = true)]`): plan closure counts calls and returns an empty plan; advance by `IMAGE_REFRESH_DELAY`, assert one call; advance one hour, assert two.

### Milestone 3: the pull gate

New `mj-controller/src/image_pull_gate.rs`, declared in `lib.rs` next to `recovery_gate`:

    /// One lock per (host, image). Whoever may pull holds it; a launch that
    /// needs the image waits on it instead of starting a second download.
    pub(crate) fn image_pull_mutex(host: &ImageHost, image: &str) -> Arc<std::sync::Mutex<()>>
    // key = format!("{}|{image}", host.label())

    pub(crate) fn hold_image_pull<'a>(
        lock: &'a std::sync::Mutex<()>,
        is_cancelled: impl Fn() -> bool,
        on_wait: impl FnOnce(),
    ) -> Result<std::sync::MutexGuard<'a, ()>>
    // loop: try_lock → Ok returns; WouldBlock → on first block call on_wait();
    // if is_cancelled() bail!("cancelled while waiting for image download");
    // sleep 250 ms. Poisoned → into_inner.

The refresher acquires `image_pull_mutex(&refresh.host, &refresh.image)` through `hold_image_pull(.., || executor.is_cancelled(), || {})` before its first `image_id` read, so a Create mid-pull makes the hourly tick wait and then find the image present.

Tests: `a_create_waits_for_the_in_flight_pull_of_its_image` (thread A holds, thread B calls `hold_image_pull` with a recording `on_wait`; B returns only after A releases; `on_wait` ran once) and `a_waiting_create_stops_when_cancelled`.

### Milestone 4: provisioning waits

Add `ProvisionStage::PullingImage` with label `"Pull image"` (`mj-core/src/targets.rs:37-77`). In `image_pull_gate.rs`:

    pub(crate) fn with_image_ready<T>(
        target: &mj_core::targets::TargetTemplate,
        executor: &impl CommandExecutor,
        work: impl FnOnce() -> Result<T>,
    ) -> Result<T>

For a container target (`target.image_host()`), hold the gate around `work`; on contention only, open `ProvisionStageGuard::new(executor, ProvisionStage::PullingImage)` and call `executor.notify_notice(&format!("Waiting for image {image} to finish downloading"))`. For other targets just run `work`. Hook it at exactly two points: around the `targets::probe_image_user(...)` call inside `podman_image_user` (`provisioning.rs:1086`, the cache lookup stays outside) and around the creation execution in `provision_target_creation` (`provisioning.rs:863-893`, both the `execute_concurrent` branch and the no-split branch). Cancellation reaches the wait through `executor.cancellation_requested()`, which `DaemonStageReportingExecutor` forwards.

Test in `controller/provisioning/tests.rs`: `provisioning_reports_the_pull_stage_only_while_waiting` (fake executor recording `stage_started`; with the gate held by another thread the stage is reported; with it free it is not). `an_automatic_pull_policy_never_pulls_during_a_launch` (`targets/tests.rs:1409`) stays green: the gate adds no command.

### Milestone 5: reporting

    pub enum ImageRefreshReport {
        Started { host: String, image: String },
        Pulled { host: String, image: String },
        Failed { host: String, image: String, error: String },
    }

`spawn_image_refresher` takes `report: impl Fn(ImageRefreshReport) + Send + Sync + 'static`, wrapped in an `Arc<dyn Fn …>` and passed into each blocking task. `Started` fires only when a download actually begins (WhenAbsent with no cached id, or an Always pull), `Pulled` when the id changed or the image was absent before, `Failed` on error. The refresher task owns `last_failures: BTreeMap<String, String>` keyed by host|image and emits `Failed` only when the error text differs from the last one for that key (and clears the entry on success), so an offline SSH host does not produce a notice every hour; `tracing::warn!` stays for every failure.

In `daemon/process.rs:116-121` supply a closure that calls `state.push_notice("", text)` with: "Downloading image {image} for {host}…", "Image {image} is ready on {host}.", "Could not pull image {image} on {host}: {error}". In `daemon/snapshot.rs:226` let notices with an empty `session_id` through to every workspace, with a comment that an empty id marks a daemon-owned notice. `RuntimeNotice` needs no change.

Tests: extend `a_failed_pull_is_reported_and_leaves_the_other_host_alone` to assert the report closure sees `Failed` for the broken host only; `a_failed_pull_is_reported_once_until_the_error_changes` (two ticks with the same error produce one report; a different error produces another; success then failure produces another); a snapshot test that an empty-session notice reaches a workspace snapshot.

### Milestone 6: docs and validation

- `docs/src/content/docs/containers.md:107-121`: the daemon downloads every configured container image the host lacks as soon as it starts, so the first session does not wait on the registry, and refreshes eligible remote `:latest` images once an hour; Apple container joins the startup download; `never` images are never downloaded in the background.
- `docs/src/content/docs/targets.md:303-312`: the `auto` row ("use the existing image; missing images are downloaded when the daemon starts; moving tags are refreshed hourly") and the local-image note (a missing `localhost/` image cannot be pulled; the failure is reported once).
- `docs/src/content/docs/apple-container.md:58-60`: Apple participates in the startup download and hourly refresh.
- Manual check: start the daemon against a Podman store without the image; the dashboard shows "Downloading image …" then "Image … is ready"; a Create started during the download shows the "Pull image" stage and then proceeds without its own pull.

## Concrete Steps

Working directory: `/home/jonathan/Projects/hel4`.

    cargo test -p brokk-mj-core -p brokk-mj-controller -- image   # focused, after each milestone
    cargo test -p brokk-mj-core -p brokk-mj-controller            # both suites, outside the sandbox, dev profile
    cargo clippy --all-targets -- -D warnings

The package names are `brokk-mj-core` and `brokk-mj-controller`; `-p mj-core`
fails with "package ID specification `mj-core` did not match any packages".

Commit each validated milestone on the current branch with only the files it changed.

## Validation and Acceptance

- With a container target configured and its image absent, `mj daemon` logs "pulled a newer container image" within seconds of starting and the dashboard shows the download and ready notices.
- A Create started during the download shows "Pull image" and does not run a second pull (only one `pull` process on the host).
- A target with `pull_policy = "never"` never triggers a background pull.
- All listed tests pass; `a_missing_image_is_pulled_once_and_not_again_when_present` fails before Milestone 1 and passes after.

## Idempotence and Recovery

Every step is additive. A pull that fails is retried on the next hourly tick. Shutdown cancels in-flight pulls through the shared cancel flag and `CancellableProcessExecutor` kills the child; the gate is released when the blocking thread returns.

## Artifacts and Notes

Test commands and results, run outside the sandbox on the dev profile from
`/home/jonathan/Projects/hel4`. Note the crate names: the packages are
`brokk-mj-core` and `brokk-mj-controller`, not `mj-core` and `mj-controller`.

    $ cargo test -p brokk-mj-core -p brokk-mj-controller
    test result: ok. 1341 passed; 0 failed; 7 ignored   # mj_controller lib
    test result: ok. 341 passed; 0 failed; 0 ignored    # mj_core lib

    $ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 30.80s

The Podman command-form check, with the image absent beforehand:

    $ podman image inspect --format '{{.Id}}' docker.io/library/alpine:3.20
    Error: docker.io/library/alpine:3.20: image not known
    exit=125
    $ podman pull docker.io/library/alpine:3.20
    bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731
    $ podman image inspect --format '{{.Id}}' docker.io/library/alpine:3.20
    bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731
    $ podman image prune -f
    $ podman rmi docker.io/library/alpine:3.20   # leave the store as it was

That is exactly the sequence `refresh_host_image` runs: a failed inspect means
"absent", so `image_id` returns `None` and the pull goes ahead; the second
inspect supplies the new id.

## Interfaces and Dependencies

No new crates. New items: `RefreshWhen`, `ImageHost::AppleContainer`, `TargetTemplate::image_host`, `ProvisionStage::PullingImage` (mj-core); `image_pull_gate::{image_pull_mutex, hold_image_pull, with_image_ready}`, `ImageRefreshOutcome`, `ImageRefreshReport`, the `report` parameter of `spawn_image_refresher` (mj-controller).

## Revision note, 2026-09-17 (Opus)

Implemented all six milestones and brought the living sections up to date.
Three deviations from the plan as written, each recorded in the Decision Log
with its reasoning: `image_host` exists on both `TargetTemplate` families
rather than one; the reporting is split between `refresh_host_image`
(`Started`, `Pulled`) and `record_refresh_result` (`Failed`, with
once-per-change suppression) rather than living entirely in
`spawn_image_refresher`; and the workspace notice rule is tested through the
named `notice_reaches_workspace` function rather than through a full
`runtime_snapshot`, which would need an isolated database. The two mj-core
level tests the plan asked for live in `mj-controller/src/targets/tests.rs`,
because `mj-core/src/targets.rs` has no test module and the controller crate
re-exports all of `mj_core::targets`. The command lines in `Concrete Steps`
were corrected to the real package names. The Apple `container` argument forms
remain unverified against the real CLI, which needs macOS.
