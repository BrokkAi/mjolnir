# Host-aware container capacity admission

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir can currently start several container sessions that each fit within a host's total CPU and RAM even when their combined allocations exceed that host. On a 24-core, 64 GiB Mac, for example, three 8-core, 32 GiB Apple-container sessions are individually valid but collectively request 96 GiB of RAM. The user can see the host's raw capacity, but not how much of it is already committed, and the launch is not refused.

After this change, a person creating a session sees the host's total capacity, the RAM already committed to live Mjolnir containers, and the RAM left for another session. New container sessions default to 4 CPUs and 8 GiB of RAM. A checkbox on the container sizing step can save the chosen size as the default for that physical host. Creating a session whose requested CPU or RAM does not fit is refused with a message that names the remaining capacity and tells the person they can suspend or stop a session and retry. Mjolnir never stops or suspends a session automatically to make room.

The admission calculation treats RAM as a hard, non-compressible commitment. It does not use swap, memory compression, or a momentary "available memory" reading to claim that more RAM exists. CPU commitments are also summed, while CPU utilization remains a separate runtime observation.

## Progress

- [x] (2026-10-25 00:00Z) Read `.agents/PLANS.md`, the current capacity probes, session sizing types, controller action path, daemon create path, web form, and TUI wizard.
- [x] (2026-10-25 00:00Z) Confirm that the current implementation validates each requested allocation against host totals but never sums active owner-container allocations.
- [x] (2026-10-25) Add the shared host-container capacity model, reserve policy, 4 CPU / 8 GiB default, and minimum useful size.
- [x] (2026-10-25) Add committed and remaining CPU/RAM to web and TUI target projections, including a visible RAM-left value.
- [x] (2026-10-25) Enforce aggregate capacity at the daemon create admission point and return an actionable refusal.
- [x] (2026-10-25) Replace automatic launch-size remembering in the new-session surfaces with an explicit "Save as default sizing" checkbox.
- [x] (2026-10-25) Update web and TUI tests, API documentation, and protocol compatibility coverage.
- [x] (2026-10-25) Run focused tests during implementation, then full unit suites and clippy for every touched crate.
- [x] (2026-10-25) Run the deterministic web suite, unit tests, and documentation check.
- [x] (2026-10-25) Independent review fixes: cover resume and move admission, restore the separate 8-vCPU EC2 default, keep the new-session floor out of resume, avoid zero placeholders for unknown capacity, and floor displayed limits.
- [x] (2026-10-25) Commit the validated change on the current branch.
- [x] (2026-10-25) Commit the independently reviewed fixes on the current branch.
- [x] (2026-10-25) Fix the second review's joined Move lease, same-host Move accounting, and terminal reservation projection regressions.

## Surprises & Discoveries

- Observation: The host probes already obtain exactly the totals needed for admission. Local targets use `sysinfo`, and macOS SSH targets use `sysctl -n hw.memsize` and `sysctl -n hw.logicalcpu`.
  Evidence: `mj-controller/src/pollers/capacity.rs` and `mj-controller/src/targets/resources.rs`.

- Observation: Capacity targets already group all local container targets together and all targets sharing one SSH host together.
  Evidence: `Controller::deployment_capacity_targets` groups local target ids into `local` and SSH targets by host in `mj-controller/src/controller/backend.rs`.

- Observation: Container sub-agents borrow their parent's container and copy the parent's `resource_allocation`. Summing every active session record would charge the same container more than once.
  Evidence: `mj-controller/src/controller/subagents.rs` records the child's allocation from the parent and sets `borrowed_from` on the child's target locator.

- Observation: Apple runs each container in its own lightweight Linux VM. Apple's documentation says the VM uses memory as needed but does not currently return freed pages to macOS promptly. A configured `--memory` value is therefore a safe capacity commitment for worst-case admission even though it is not all touched immediately.
  Evidence: Apple's public `container` technical overview, linked from the repository's Apple-container documentation and reviewed during design.

- Observation: The existing `remember_launch_choices` flag remembers more than sizing: it also seeds profile, target, recent-launch, and subagent defaults. The new checkbox cannot reuse that flag without retaining behavior the user did not select.
  Evidence: `mj-controller/src/daemon/create.rs` and `mj-controller/src/controller.rs`.

- Observation: Putting the new minimum into `SessionResourceAllocation::validate` would make older stores containing smaller live sessions fail to load. The admission floor therefore has a separate `validate_minimum_useful_container` method; general durable validation deliberately remains limited to non-zero values.
  Evidence: `mj-core/src/state.rs` and the existing compatibility tests in `mj-controller/src/database/tests.rs`.

- Observation: The controller and TUI suites pass. The full core suite has one unrelated environment failure in `spawn_detached_leaves_no_zombie_under_a_spawner_that_keeps_running`: the fixture child remains in state `Z` because this container's PID 1 does not reap it. The changed `state` capacity tests pass in both the full run and isolation.
  Evidence: the full core run reported 534 passed, 1 failed, and 2 ignored; the only failure was the unchanged subprocess zombie probe.

- Observation: The CLI unit suite passes, but the full `daemon_startup` integration binary has environment failures where fixture daemons exit according to their logs but remain observable as zombies or retained stores after the test harness attempts SIGKILL. This is the same process-reaping condition and is not touched by the capacity change.
  Evidence: the failing fixtures log daemon shutdown completion before the cleanup assertion reports the process or store still retained.

- Observation: The first implementation applied the new container default to the separate EC2 preference helper. An option list containing 2 and 8 vCPUs then selected 2 because the shared value had become 4.
  Evidence: `preferred_aws_allocation` compared against `BASELINE_CONTAINER_CPUS`; the new regression test uses `[2, 8]` and requires the 8-vCPU option.

- Observation: `ResourceEditor::allocation` is shared by new, resume, and move. Applying the new 2-CPU/4-GiB floor unconditionally made a previously valid smaller session impossible to resume.
  Evidence: the shared editor is called from `validate_wizard_resources` for every wizard type.

- Observation: `HostContainerCapacity::remaining` is deliberately zero when `complete` is false, but early UI formatting treated that sentinel as a real limit.
  Evidence: web and TUI both rendered or validated the zero value before testing `complete`.

- Observation: A nearest-tenth display could overstate a hard byte limit. The shared formatter now floors to a tenth, and both web and controller messages use that same conservative formatting.
  Evidence: `(8 GiB - 1 byte)` displays as `7.9 GiB`.

- Observation: The web surface reserves a Move before acknowledging it, then its spawned action calls the same Move again. Lifecycle joining skips the second action's closure, so a second unconditional lease had no owner and deleted the first reservation.
  Evidence: `start_move_session` reserves before admission and `ControllerAction::Move` later enters `move_session`; only the first closure owns the capacity lease.

- Observation: A same-host Move keeps its source running while it starts. Removing the source record as soon as the lease existed released too much headroom, while admitting the destination against both source and destination together rejected valid growth.
  Evidence: the same-host 8-to-2 CPU transition allowed an 8-CPU new session, and an 8-to-9 CPU transition exceeded a 10-core host only because the source was counted twice.

- Observation: The terminal had its own raw capacity poller and recomputed hard headroom from the records it happened to hold, so it could not see daemon-only resume and Move leases.
  Evidence: `DashboardState::host_container_capacity` used raw usage plus local records even when the daemon published a reservation-adjusted viewer projection.

## Decision Log

- Decision: Use 4 CPUs and 8 GiB as the baseline size for a new container session.
  Rationale: This is materially more useful than the prior 8 CPU / 32 GiB default while still supporting normal repository work, and the user explicitly selected it.
  Date/Author: 2026-10-25 / Codex

- Decision: Require at least 2 CPUs and 4 GiB for a container session.
  Rationale: The design discussion established that very small allocations are not useful. These minimums do not scale with host size because the work performed by an agent is roughly absolute, while a larger host should hold more sessions rather than make each one proportionally larger.
  Date/Author: 2026-10-25 / Codex

- Decision: Reserve 20 percent of physical RAM, with a minimum of 2 GiB, for macOS or the Linux host, and reserve no CPU cores.
  Rationale: RAM is not compressible for admission purposes and the operating system needs hard headroom. CPU is schedulable and will be summed directly against physical logical cores in this change, with overcommit left for a future policy.
  Date/Author: 2026-10-25 / Codex

- Decision: Compute capacity over the whole physical host, not per runtime or per target template.
  Rationale: Apple container, Docker, and Podman targets on one Mac consume the same physical resources, and the existing capacity targets already provide the correct host grouping.
  Date/Author: 2026-10-25 / Codex

- Decision: Count only sessions that own a container. Count a new session while it is `Provisioning`, and keep counting through cleanup states until its target is gone. Do not count sub-agent sessions whose target has `borrowed_from`.
  Rationale: This is the one-owner rule for resource reservations and prevents both double charging and premature release.
  Date/Author: 2026-10-25 / Codex

- Decision: Perform new-session capacity admission in the daemon's create path under one serialized admission lock, after loading fresh controller state.
  Rationale: Two concurrent creates must not both observe the same old committed total. The daemon is the control-plane owner, and all terminal, web, CLI, and API creation paths converge on this create path.
  Date/Author: 2026-10-25 / Codex

- Decision: Never stop, suspend, or destroy a session automatically when capacity is exhausted. Leave the existing explicit Suspend and Destroy actions available and mention them in the refusal.
  Rationale: The user explicitly required no automatic shutdown. Capacity pressure is not authorization to interrupt accepted work.
  Date/Author: 2026-10-25 / Codex

- Decision: Add a separate `remember_container_size` request field rather than changing `remember_launch_choices`.
  Rationale: The checkbox controls only the saved per-host sizing preference. Profile, target, recent-launch, and subagent behavior remain independent.
  Date/Author: 2026-10-25 / Codex

- Decision: Bump the daemon protocol because `CreateSessionRequest` gains a serialized field.
  Rationale: Daemon request structs deny unknown fields. A new client must not send the new field to an older daemon that will reject it.
  Date/Author: 2026-10-25 / Codex

- Decision: Keep the minimum useful size out of the general durable allocation validator.
  Rationale: Existing sessions below the new floor must remain loadable and stoppable. Only new admission and the new-session editors apply the floor.
  Date/Author: 2026-10-25 / Codex

- Decision: Publish the complete capacity projection as an additive optional field while retaining `container_host_limits`.
  Rationale: Older clients ignore the new field, while current web and terminal surfaces use one shared total/reserved/committed/remaining calculation.
  Date/Author: 2026-10-25 / Codex

- Decision: Once the daemon capacity service is active, refuse a new container request that omits its CPU and RAM allocation.
  Rationale: An unrecorded runtime fallback cannot be reserved safely without reinterpreting the backend's size strings. Current clients always send the explicit allocation, and the protocol bump makes that expectation unambiguous.
  Date/Author: 2026-10-25 / Codex

- Decision: Keep a separate 8-vCPU preference for EC2.
  Rationale: Container sizing and EC2 instance selection are independent policies. Lowering the default container to 4 CPUs must not alter the documented EC2 preference.
  Date/Author: 2026-10-25 / Codex

- Decision: Apply the 2-CPU/4-GiB floor only to new-session drafts and admission, not to the shared editor's resume/move mode.
  Rationale: Existing valid sessions must retain and resume their recorded allocation, and the backend must remain loadable for legacy records.
  Date/Author: 2026-10-25 / Codex

- Decision: Use an in-memory capacity lease for resume and move while their durable session record is not yet `Provisioning`.
  Rationale: Both operations can spend minutes in preflight. A lease reserves the destination budget without holding the new-session admission lock for that whole period. The capacity projection overlays the lease, and the lease disappears when the lifecycle ends.
  Date/Author: 2026-10-25 / Codex

- Decision: Test and display `complete: false` before formatting or validating `remaining`.
  Rationale: Zero is a sentinel for unknown capacity in the core value, never a claim that the host has no capacity.
  Date/Author: 2026-10-25 / Codex

- Decision: Floor displayed GiB limits to one decimal.
  Rationale: A displayed hard limit must not be larger than the bytes available. Entering the displayed value must remain admissible.
  Date/Author: 2026-10-25 / Codex

- Decision: Make capacity-lease ownership explicit; a joining caller gets a non-owning lease and cannot remove the reservation.
  Rationale: A web Move is intentionally reserved twice across acknowledgement and action dispatch, but those calls join one lifecycle. Only the caller that inserted the reservation may drop it.
  Date/Author: 2026-10-25 / Codex

- Decision: Skip capacity admission for in-place Moves, and model a non-in-place Move as source plus destination commitments.
  Rationale: An in-place Move reuses the existing environment and adds no capacity. Same-host Moves hold the larger allocation until the lifecycle ends because the source and destination phases must never expose a temporary gap; cross-host Moves charge each physical host separately. Destination admission removes the moving session from the old source count before applying the destination allocation.
  Date/Author: 2026-10-25 / Codex

- Decision: Publish daemon-computed container capacity through `RuntimeMetadata` for terminal surfaces.
  Rationale: The daemon is the only owner of in-memory leases and the shared reserve policy. The TUI may retain raw CPU and memory utilization for display, but it must not derive admission headroom from partial records.
  Date/Author: 2026-10-25 / Codex

## Outcomes & Retrospective

The feature is implemented. New container sessions default to 4 CPUs and 8 GiB, require at least 2 CPUs and 4 GiB, and are admitted only when their allocation fits the host's aggregate CPU and hard RAM budget. The web viewer and TUI show RAM left from physical totals minus a 20 percent OS reserve and owning-container commitments; borrowed sub-agent containers are excluded. The refusal names the remaining resources, explains that the person may explicitly suspend or stop a session, and states that Mjolnir will not do so automatically.

The explicit sizing checkbox is carried through both interactive surfaces, the HTTP API, the new daemon protocol, and the database write. Controller Action serialization and database tests prove that the broader interactive-default flag no longer implies that a container size is saved.

After independent review, all five findings reproduced and were fixed. New-session admission, resume, and user-started moves to container targets now use the serialized host budget; resume and move leases cover their pre-provisioning work. EC2 again prefers 8 vCPUs independently of the 4-CPU container default. Resume keeps legacy allocations below the new-session floor. Unknown capacity is reported as unavailable rather than zero. Displayed GiB limits are conservatively floored.

A second independent review found three further real regressions. Joined web Move leases can no longer delete the owning reservation. Non-in-place Moves now retain their source accounting until source teardown: same-host transitions hold the larger source or destination allocation, cross-host transitions charge both hosts, and in-place transitions require no additional reservation. Destination admission excludes the old source record once, so growth is not double-counted. The runtime feed now carries the daemon's reservation-adjusted host capacity to the TUI, so both new-session validation and target cards use the same headroom as web and daemon admission.

Validation completed: the controller suite passed 2105 tests with 17 ignored, the TUI suite passed 543 tests with 2 ignored, the client suite passed 42 unit tests plus its integration test, the CLI unit suite passed 225 tests, and its ACP integration binary passed 3 tests. The workspace clippy run passed with warnings denied, the deterministic web suite passed 108 tests with 3 environment skips, all 57 web unit tests passed, and the documentation Astro check reported zero diagnostics. The full core suite passed 536 tests and hit only the pre-existing container process-reaping failure described above. The CLI `daemon_startup` binary passed 6 tests and hit 10 of the same pre-existing fixture-daemon cleanup failures. Targeted capacity, database, API, protocol, runtime-feed, web, and TUI tests all pass.

## Context and Orientation

The repository has three relevant layers.

`mj-core` owns durable session records, target configuration, and shared wire-safe state types. `SessionResourceAllocation::Container` stores the CPU count and memory bytes chosen for a session. `SessionRecord` records that allocation, its target template id, lifecycle state, and eventually its `TargetLocator`. `TargetLocator` records whether a container is borrowed from a parent session.

`mj-controller` is the daemon and control plane. It owns the session database, creates session records, provisions targets, and runs background host-capacity probes. A "capacity target" is one physical host or one AWS fleet. A probe returns `DeploymentCapacityUsage`, including total memory bytes and logical CPU cores. The daemon create path is `RuntimeState::start_create_session_inner` in `mj-controller/src/daemon/create.rs`. All new-session surfaces converge there, including the TUI's direct daemon request and the HTTP API's `ControllerAction::New`.

`mj-tui` and the web viewer in `mj-controller/src/web/viewer.js` are presentation surfaces. Both already receive the physical host totals. They must now receive committed and remaining values, display RAM left, and submit the explicit sizing-default checkbox.

The phrase "committed RAM" means the sum of configured memory limits for live container vm owners on one physical host. It does not mean measured process memory. The phrase "RAM left" means the host's usable RAM after the operating-system reserve and committed container RAM have been subtracted. The calculation is intentionally independent of swap and memory compression.

## Plan of Work

### Milestone 1: Shared capacity model and defaults

In `mj-core/src/state.rs`, replace the 8 CPU / 32 GiB baseline with 4 CPUs / 8 GiB. Add public constants for the 2 CPU / 4 GiB minimum useful size and a `HostContainerCapacity` value containing host totals, reserved capacity, committed capacity, and remaining capacity. All four sub-values use the existing `HostContainerSize` shape.

Add functions on `TargetLocator` and `SessionRecord` as needed to answer one question: does this active session currently own a container on a given set of target-template ids? A `Provisioning` or `StartupCleanup` session counts because a container may be under construction or may still exist during cleanup. A session in another active lifecycle state counts when it has an owning target locator. A child with `borrowed_from` does not count. A stopped or parked child does not count.

Add a pure helper that accepts the host's total CPU and memory, the ids of all target templates on that host, and the session records. It computes the OS reserve, sums owner allocations with saturating arithmetic, and returns `HostContainerCapacity`. If an active owner container has no recorded allocation, it saturates committed capacity to the usable budget and marks the capacity as unavailable for further admission; silently treating an unknown limit as zero would reintroduce oversubscription.

Add the minimum-size check as a separate new-admission validator. Keep the existing non-zero checks for malformed persisted data, and keep the floor out of resume and move so older sessions remain usable.

### Milestone 2: Probe-to-surface plumbing and RAM-left projection

Extend the daemon `CapacityFeed` in `mj-controller/src/daemon/capacity.rs` to retain the latest successful `DeploymentCapacityUsage` per probe target. On a failed probe, retain the last successful reading for display and reuse; on a successful probe, replace it. Expose a crate-private method that returns the `DeploymentCapacityTarget` and reading for a target-template id. If no reading exists yet, sample that capacity target once through the existing probe code so a first launch is still checked. Keep all sampling asynchronous and off UI/render loops.

Extend `ViewerTargetCapacity` and `ViewerTarget` in `mj-controller/src/server/viewer_types.rs` with an optional `HostContainerCapacity`. Keep `container_host_limits` for compatibility with already-installed clients and continue filling it with the physical totals.

When building viewer capacity in `mj-controller/src/server_runtime/snapshot.rs`, use the host's target ids and the controller's current sessions to include committed and remaining values. When building each `ViewerTarget`, find its host capacity and publish the same value. The web form must use `container_capacity.remaining` for validation and display, while retaining host totals for the descriptive line.

The target capacity card in `mj-controller/src/web/viewer.js` must show a hard "RAM left" value derived from that committed budget. The new-session container controls must show total CPU/RAM, committed RAM, and RAM left. They must say that RAM left is a hard commitment and is not derived from compressed or reclaimable memory.

In `mj-tui`, add an equivalent lookup on `DashboardState` in `mj-tui/src/wizards/dashboard/targets.rs`. The target step must render total and RAM-left values. New-session validation uses remaining capacity and applies the new-session minimum; resume and move retain their recorded sizing semantics and do not apply that new floor.

### Milestone 3: Atomic admission at daemon create

Add one asynchronous admission mutex to `RuntimeState` in `mj-controller/src/daemon.rs`. In `RuntimeState::start_create_session_inner`, after loading the configuration and validating the target, obtain the host capacity input. For a container target, lock the admission mutex, load fresh controller state, compute `HostContainerCapacity`, and validate the requested owner-container allocation before registering the provisional session. Keep the mutex through the durable creation of the `Provisioning` session so a second concurrent create sees the first reservation.

The check must compare both CPU and RAM against remaining capacity. It must refuse an allocation below the minimum useful size and an allocation that does not fit. The refusal must be a `mj_core::refusal::Refusal` so the HTTP API, web viewer, and terminal receive its wording rather than a generic internal error. The message must include requested CPU/RAM, remaining CPU/RAM, and the fact that Mjolnir will not stop sessions automatically. It may tell the person to suspend or stop a session or choose a smaller size.

Do not add any task that selects, stops, or suspends sessions under pressure. Existing explicit lifecycle actions remain the only way to release capacity.

### Milestone 4: Explicit save-as-default sizing

Add `remember_container_size: bool`, defaulting to false, to:

- `ControllerAction::New` in `mj-controller/src/server/actions.rs`
- `StartSessionRequest` in `mj-controller/src/server/api/types.rs`
- `CreateSessionRequest` in `mj-client/src/daemon.rs`
- `SessionLaunchOptions` in `mj-controller/src/controller.rs`

Thread the value through the web action handler, API session creation, server-runtime phone action, daemon create path, and controller registration. In `mj-controller/src/database/sessions.rs`, gate only the `host_container_sizes` write on `remember_container_size`. Keep profile, target, recent-launch, and subagent-policy writes gated by the existing `remember_launch_choices` flag.

In the web new-session draft, add an unchecked checkbox labelled "Save as default sizing". It sits with the container CPU/RAM controls, survives step navigation, and is submitted as `remember_container_size` in the `new` action. Reset it to unchecked for each fresh draft rather than persisting it in the browser.

In the TUI, add the checkbox to the target step for new sessions only. Add a `WizardControl::RememberContainerSize` and a `NewWizard.remember_container_size` field. The checkbox must be keyboard and mouse operable, included in draft comparison and declare/render order, and absent from resume/move. Pass it through `DashboardAction::CreateSession` and the daemon create request.

For the HTTP API, accept `remember_container_size` as an optional boolean. Do not make command-line `mj new` save a new default unless its caller explicitly opts in. Update `docs/src/content/docs/api-reference.md` and the CLI/TUI documentation where the new default and checkbox are described.

Bump `mj_client::daemon::PROTOCOL_VERSION` by one and add the required historical protocol transcript coverage according to the comments next to `PROTOCOL_VERSION`.

### Milestone 5: Validation and documentation

Add tests only for behavior that can regress without being noticed:

- A tricky-condition test proves owner containers are summed while borrowed sub-agent containers are not.
- An admission test proves concurrent or sequential creates cannot commit more RAM than the host budget allows, and that a failed admission does not create a session or stop an existing one.
- A web behavioral test proves RAM-left is visible, the 4 CPU / 8 GiB default is selected, and the checkbox changes the outgoing `remember_container_size` value.
- A TUI state/input test proves the checkbox toggles and is carried into `DashboardAction::CreateSession`.

Update existing golden snapshots for the new default, labels, checkbox, and host-capacity text. Do not replace an existing golden with a family of examples.

## Concrete Steps

All commands run from the repository root:

    /workspace/4784ec8ae984fe267bff450232bf36ac/mjolnir

After editing the shared model and controller paths, run focused tests with isolated test storage:

    cargo test -p mj-core container_capacity
    cargo test -p mj-controller container_capacity
    cargo test -p mj-tui remember_container_size

For web behavior and golden updates, use the repository's existing web test setup and update only the affected expected files:

    npm --prefix tests/e2e/web test -- --grep "new session"

Before the final commit, run the required dev-profile checks for every touched crate. The final workspace-wide commands are:

    cargo test
    cargo clippy --all-targets -- -D warnings

These commands must run outside the restricted sandbox because the suite uses loopback and Unix sockets. Use an isolated `--instance` for any manual daemon, TUI, CLI, or end-to-end invocation and an isolated `MJ_CONFIG_DIR` / `MJ_DATA_DIR` for migration or database tests.

Expected successful behavior after implementation:

    Host:      24 CPUs, 64.0 GiB RAM
    Reserved:   0 CPUs, 12.8 GiB RAM
    Committed:  8 CPUs, 32.0 GiB RAM
    RAM left:   19.2 GiB
    Default:    4 CPUs,  8.0 GiB RAM

A request for 4 CPUs and 24 GiB is refused because 24 GiB exceeds the 19.2 GiB left. The refusal names the remaining capacity and says that Mjolnir will not stop a session automatically. The same request succeeds after the person explicitly suspends or stops enough owning sessions. Checking "Save as default sizing" before a successful create makes the chosen size appear on the next new-session form for that host; leaving it unchecked leaves the prior saved size or the 4 CPU / 8 GiB baseline unchanged.

## Validation and Acceptance

Acceptance is behavioral:

1. On a host with 64 GiB physical RAM, the projected host budget is 51.2 GiB after the 20 percent reserve. A new session defaults to 4 CPUs and 8 GiB.
2. Two owning containers with 8 GiB each produce 16 GiB committed and 35.2 GiB RAM left. A borrowed sub-agent on either parent does not increase those numbers.
3. A launch that would make committed RAM exceed the host budget fails before any container is created. Existing sessions remain running.
4. A launch that would make committed CPU exceed logical cores fails before any container is created.
5. The web and TUI show the RAM-left number and the hard-admission explanation.
6. The sizing checkbox is off by default. When checked, the exact successful allocation is saved as the physical host's default and is used by a later new-session form. When unchecked, no saved size changes.
7. No code path invokes Suspend, Close, or Destroy because a new launch lacks capacity.

Tests should fail before the implementation for the aggregate-sum, RAM-left, minimum-size, and checkbox cases, and pass afterward. Existing capacity, lifecycle, checkpoint, and TUI wizard tests must continue to pass.

## Idempotence and Recovery

The change requires no schema migration and no destructive operation. Capacity accounting is derived from durable session records and target locators, so restarting the daemon recomputes it from the database rather than restoring an independent counter. A provisional session remains a reservation until its ordinary rollback or cleanup removes it. If a probe is unavailable, the launch is refused rather than assuming unlimited capacity; refreshing a target can be retried safely.

If a focused test creates an isolated instance, keep using that instance for all commands. Do not point manual verification at the live default instance. If a generated web golden changes for unrelated content, regenerate in the repository's documented isolated test mode and inspect the diff before accepting it.

## Artifacts and Notes

The current per-session-only check is:

    request.cpus <= host.cpus
    request.memory_bytes <= host.memory_bytes

The new check is:

    request.cpus >= 2
    request.memory_bytes >= 4 GiB
    committed.cpus + request.cpus <= host.cpus
    committed.memory_bytes + request.memory_bytes
        <= host.memory_bytes - max(2 GiB, host.memory_bytes / 5)

`committed` is the sum of owning container allocations. Borrowed sub-agent containers are excluded because they consume capacity already charged to their parent.

## Interfaces and Dependencies

In `mj-core/src/state.rs`, define or preserve these public interfaces:

    pub const BASELINE_CONTAINER_CPUS: u64 = 4;
    pub const BASELINE_CONTAINER_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    pub const MINIMUM_CONTAINER_CPUS: u64 = 2;
    pub const MINIMUM_CONTAINER_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

    pub struct HostContainerCapacity {
        pub total: HostContainerSize,
        pub reserved: HostContainerSize,
        pub committed: HostContainerSize,
        pub remaining: HostContainerSize,
        pub complete: bool,
    }

    pub fn host_container_capacity(
        total: HostContainerSize,
        target_ids: &[String],
        sessions: &mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    ) -> HostContainerCapacity;

`complete` is false when an active owning container has no recorded allocation. `remaining` is zero in that case and admission must refuse a new session until the condition is resolved.

In `mj-controller/src/daemon/capacity.rs`, add a crate-private lookup on `CapacityFeed` returning the deployment target and latest usage for a target-template id, with samples performed asynchronously when no prior reading exists.

In `RuntimeState`, add a single serialized new-session capacity admission guard and a helper that formats a `Refusal` from a failed fit. No renderer or event loop may call a probe synchronously.

Resume and move use the same guard to install an in-memory lease while their durable record is not yet `Provisioning`. The lease replaces any active commitment for the same session, is overlaid into viewer capacity, and is removed when the lifecycle ends. New sessions hold the guard until their durable session record exists; they do not need a separate lease.

The web and TUI receive `HostContainerCapacity` rather than recomputing reserve policy or target-host grouping independently. This keeps one owner for the rule that determines how much RAM is left.

## Plan Revision Notes

- 2026-10-25: Initial plan created after design discussion. It records the user's choices for 4 CPU / 8 GiB defaults, hard non-compressible RAM accounting, no automatic shutdown, and a per-launch checkbox for saving the host's default size.
- 2026-10-25: Implementation completed. The plan now records the separate minimum-admission validator, additive wire projection, complete validation results, and the environment-only zombie failures. No schema migration was needed because capacity commitments are derived from durable sessions and borrowed-container ownership.
- 2026-10-25: Independent review confirmed all five findings. Added resume/move leases, separated EC2 and container preferences, scoped the minimum to new sessions, corrected unknown-capacity rendering, and switched hard-limit formatting to floor instead of round.
- 2026-10-25: A second independent review confirmed three additional findings. Added non-owning joined leases, source-aware Move capacity reservations with in-place bypass and same-host max accounting, and daemon-computed capacity in the terminal runtime feed.
