# Download configured container images when the daemon starts

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

When a person creates their first session on a container target, the container engine has to download the image first, often several gigabytes, and the Create sits in "Provisioning" for minutes. After this change the daemon starts downloading every configured container image it does not already have a couple of seconds after it starts, so by the time the person opens the New Session wizard the image is usually there. A Create issued while that download is still running waits for it (showing a "Pull image" stage) instead of starting a second download. A failed download is reported once in the dashboard notices. Images whose pull policy is `never` are not touched.

## Progress

- [ ] Milestone 1: `RefreshWhen` on `ImageRefresh`, `ImageHost::AppleContainer`, `TargetTemplate::image_host`, plan merge in `image_refresh_plan`, present-skip in `refresh_host_image`; tests.
- [ ] Milestone 2: `IMAGE_REFRESH_DELAY` to 2 s with a rewritten comment; `the_first_refresh_runs_at_startup`.
- [ ] Milestone 3: `image_pull_gate` module + tests; the refresher holds the gate.
- [ ] Milestone 4: `ProvisionStage::PullingImage`; provisioning hooks in `podman_image_user` and `provision_target_creation`; test.
- [ ] Milestone 5: report closure, daemon-global notices, once-per-change failure suppression; tests.
- [ ] Milestone 6: docs; full `cargo test` and clippy; manual check with a fresh Podman store.

## Surprises & Discoveries

- Observation: on Podman the first-session download does not happen in `container run` but earlier, in the image-user probe (`podman run --rm --entrypoint '' <image> sh -c 'id -u; id -g'`).
  Evidence: `mj-controller/src/controller/provisioning.rs:1086-1125` (`podman_image_user`) → `mj-controller/src/targets/preflight.rs:691-710` (`probe_image_user`). Docker has no probe; its pull happens in `docker run --pull=missing` (`targets/container.rs:293-300`).

## Decision Log

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

(To be written at completion.)

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

    cargo test -p mj-core -p mj-controller -- image      # focused, after each milestone
    cargo test                                           # full suite, outside the sandbox, dev profile
    cargo clippy --all-targets -- -D warnings

Commit each validated milestone on the current branch with only the files it changed.

## Validation and Acceptance

- With a container target configured and its image absent, `mj daemon` logs "pulled a newer container image" within seconds of starting and the dashboard shows the download and ready notices.
- A Create started during the download shows "Pull image" and does not run a second pull (only one `pull` process on the host).
- A target with `pull_policy = "never"` never triggers a background pull.
- All listed tests pass; `a_missing_image_is_pulled_once_and_not_again_when_present` fails before Milestone 1 and passes after.

## Idempotence and Recovery

Every step is additive. A pull that fails is retried on the next hourly tick. Shutdown cancels in-flight pulls through the shared cancel flag and `CancellableProcessExecutor` kills the child; the gate is released when the blocking thread returns.

## Artifacts and Notes

(Add test transcripts and the manual check here.)

## Interfaces and Dependencies

No new crates. New items: `RefreshWhen`, `ImageHost::AppleContainer`, `TargetTemplate::image_host`, `ProvisionStage::PullingImage` (mj-core); `image_pull_gate::{image_pull_mutex, hold_image_pull, with_image_ready}`, `ImageRefreshOutcome`, `ImageRefreshReport`, the `report` parameter of `spawn_image_refresher` (mj-controller).
