# Create a machine's runtimes by probing it (#1329)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir's configuration separates machines (hosts: this computer, SSH hosts, EC2 launch templates, under `[machines.<id>]` in `config.toml`) from runtimes (where a session's harness runs on a machine: `bare`, `podman`, `docker`, `apple-container`, under `[targets.<id>]`). Sessions are launched on runtimes, never on machines. Before this change, adding an SSH machine under Settings › Setup › Machines created no runtime, so the host never appeared in the session picker, and nothing said why: `mj doctor` checked runtimes only, the Machines page showed the machine as if ready, and Settings' Detect runtimes only probed this computer.

After this change, saving a machine probes it over its SSH connection: whether it answers, which operating system and architecture it runs (a bare runtime needs a Mjolnir worker for that platform: Linux or macOS on x86_64 or arm64), and whether rootless Podman and Docker pass the same preflight a launch runs. The runtimes it can run are written to the configuration: a bare runtime named after the machine, plus `<machine>-podman` and `<machine>-docker` for each engine found. Machines already configured without a runtime get the same treatment whenever the dashboard starts, and Detect runtimes covers every machine in the Settings draft. A machine that still has no usable runtime says why: inline on the Settings Machines page, under the target table in the new-session and resume pickers, in a dashboard notice when its probe ends, and in a `machine.<id>` check in `mj doctor`.

To see it: start `mj --instance <name>` with a configuration that has an SSH machine and no runtime for it. Within a few seconds the notice reads `Added runtimes for builder: builder, builder-docker.` and the picker lists both. With an unreachable host, the notice and the picker say `Machine builder has no runtime: it did not answer. ...` with the SSH error and its fix, and `mj doctor` prints a fixable `Runtimes for machine builder` check.

## Progress

- [x] (2026-10-22) Read the configuration model (`mj-core/src/config/machines.rs`), the doctor's SSH checks, setup discovery, the Settings save path and the picker.
- [x] (2026-10-22) Milestone 1: `mj-core` answers which machine a runtime runs on and decides which probed runtimes a machine gets (`target_machine_id`, `runtime_kind_name`, `Config::machine_runtimes`, `Config::machines_without_runtimes`, `Config::machine_runtime_additions`).
- [x] (2026-10-22) Milestone 2: `mj-controller` probes a machine (`setup::probe_machine`, `setup::probe_machines`) and adds runtimes under the configuration lock (`setup::add_machine_runtimes`); `mj doctor` reports `machine.<id>`; `mj setup` adds machine runtimes.
- [x] (2026-10-22) Milestone 3: the dashboard probes machines without runtimes automatically (`take_machine_runtime_check`), Detect runtimes probes the draft's machines, the Machines page and the pickers show the reason.
- [x] (2026-10-22) Milestone 4: tests, documentation (`docs/SSH.md`, `targets.md`, `cli-reference.md`, the Settings help text), full validation, and a live check in an isolated instance.

## Surprises & Discoveries

- Observation: the Settings draft is the stored shape, where a runtime names its machine, but `Config::setup_additions` serializes additions through a `Config` with no machines. Serialized that way, a fused SSH runtime gets a machine synthesized from its host name (`stored_target` creates one), not the machine it was found on.
  Evidence: `stored_target` → `ssh_machine_id` inserts `unique_config_id(machines, &ssh.host)` when no machine holds the connection. Detect runtimes therefore serializes machine additions with the draft's own machines.

- Observation: `std::thread::scope` re-raises a thread's panic only for threads that were not joined. Joining each probe and discarding `Err` would drop a panic silently.
  Evidence: `std::thread::scope` documentation; `probe_machines` re-raises with `std::panic::resume_unwind`.

- Observation: the Machines page help text has to fit two rows at 60 columns (`every_setting_description_fits_its_two_rows`); the first wording overflowed.
  Evidence: the test listed `machines`; the help now reads "Add an SSH host or EC2 launch template; saving one adds its runtimes. Its runtimes share its build cache settings."

- Observation: in the live run the probe's notice put the unreachable machine before the added runtime, because probes come back in machine-id order. The added runtimes now lead the notice.
  Evidence: Recent messages showed "Machine nas has no runtime: ... Added runtimes for spot: spot."

## Decision Log

- Decision: the decision "which runtimes does this machine get" has one implementation, `Config::machine_runtime_additions` in `mj-core`, used both under the configuration file's lock and against the Settings draft.
  Rationale: the file writer (startup, after a save, `mj setup`) and the draft (Detect runtimes) must agree: a kind the machine already runs is not added twice, and what a probe found applies only to the machine as it was probed. `mj-tui` does not depend on `mj-controller`, so the shared decision lives in `mj-core`.
  Date/Author: 2026-10-22, Claude.

- Decision: probing happens outside the lock and the decision inside it. `add_machine_runtimes` re-reads the latest file under `Config::update_to` and adds runtimes only to a machine that still has none and is still equal to the probed definition.
  Rationale: SSH probes take seconds and must not hold the configuration lock. Deciding under the lock makes concurrent dashboards, a concurrent Settings save, and a machine edited mid-probe all safe by construction: the second writer sees runtimes and adds nothing, and an edited machine is not given runtimes found on its old host.
  Date/Author: 2026-10-22, Claude.

- Decision: the automatic path (startup, after a save, after any configuration change) only fills machines that have no runtime at all; Detect runtimes adds any kind a machine lacks.
  Rationale: a user who deliberately removed one runtime of a machine keeps that choice across launches; a machine with none is not a session target, which is the state the issue asks to repair. Detect runtimes is an explicit request to find everything.
  Date/Author: 2026-10-22, Claude.

- Decision: the save itself stays fast and unchanged; the probe runs after it as a cancellable background task started by the dashboard's `take_machine_runtime_check`, which sees the saved machine arrive through the runtime configuration feed.
  Rationale: `spawn_setup_save` is a critical (non-cancellable) operation, and quitting must stay responsive. One trigger (`take_machine_runtime_check`) covers startup, saves, hand edits of `config.toml` and other clients' edits, because each of them reaches the dashboard as a configuration with a machine that has no runtime. The trigger waits while Settings is open, because Settings holds its own draft of the machines.
  Date/Author: 2026-10-22, Claude.

- Decision: a probe never asks the user anything.
  Rationale: the issue allows a question only when the probe is ambiguous. Each runtime is judged by the same preflight a launch runs (`verify_ssh_podman`, `verify_ssh_docker`, the `uname -sm` platform check), so a runtime is added exactly when a session could start on it, and an unanswered host adds nothing and reports why. No outcome needs a choice.
  Date/Author: 2026-10-22, Claude.

- Decision: an EC2 machine gets its bare runtime without a probe.
  Rationale: an EC2 machine is a launch template; there is no host until a session launches one, and EC2 runs a bare harness only (`resolve_target` rejects containers there).
  Date/Author: 2026-10-22, Claude.

- Decision: a machine's bare runtime is offered when its platform is one the worker supports, independent of whether this build has that worker artifact at hand.
  Rationale: the runtime is right for the machine either way; a missing artifact is this installation's problem, which `mj doctor`'s worker checks already report per runtime.
  Date/Author: 2026-10-22, Claude.

- Decision: the web viewer's launch options are unchanged.
  Rationale: the issue is a terminal Settings issue (`area:ui`), and machines are edited only in the terminal Settings screen or by hand. The automatic probe gives every reachable machine its runtimes, which then appear in every picker, the web one included. Reporting a machine without a runtime in `/api/v1/options` is an API change across the daemon, `mj-client` and the web viewer, and is left for a follow-up.
  Date/Author: 2026-10-22, Claude.

## Outcomes & Retrospective

Saving an SSH or EC2 machine now gives it the runtimes it can run, machines already configured without one are repaired at every dashboard start and by `mj setup`, Detect runtimes covers every machine in the draft, and a machine that still has none says why on the Settings Machines page, in the new-session and resume pickers, in a notice, and in `mj doctor`. The live run in an isolated instance wrote `[targets.spot]` (`kind = "bare"`, `machine = "spot"`) about a second after the dashboard started, listed `spot` in the picker, and reported the refused SSH connection of `nas` with its fix in the notice, the picker, the first-run summary and `mj doctor`.

Remaining: the web viewer's launch options do not name a machine without a runtime (see the Decision Log); a reachable SSH host was not available in this container, so the SSH success path is covered by the doctor golden and the file-level test rather than a live host.

## Context and Orientation

The configuration lives in `mj-core/src/config.rs` (`Config`) and `mj-core/src/config/machines.rs` (`Machine`, `StoredTarget`). In memory every runtime is a fused `TargetTemplate` (`mj-core/src/config/targets.rs`) such as `SshBare { ssh, permissions, workspace_prefix }` or `SshDocker { ssh, container }`, which carries a copy of its machine's connection. The file stores the runtime with a `machine = "<id>"` field instead; on save, `stored_target` matches a fused runtime to its machine by the SSH connection, which is unique per machine. `target_machine_id` performs the same match without changing anything.

`mj-controller/src/setup.rs` is first-run and Settings discovery. Its `RuntimeProbe` (a runtime kind, a doctor `CheckStatus`, a detail and a remediation) is what local discovery returns, built from the doctor's own checks so that setup and `mj doctor` word a problem identically. `mj-controller/src/setup/machines.rs` adds the machine probe on the same type. `mj-controller/src/doctor.rs` holds `ssh_connectivity` (an `ssh -o BatchMode=yes <host> true` probe with classified failures) and the remote Podman and Docker wording, now shared through `ssh_podman_failure` and `ssh_docker_failure_remediation`.

The terminal dashboard is `mj-tui` (state and rendering) driven by `mj-cli/src/dashboard*` (event loop and background I/O). Background work is requested as a `DashboardAction`, run by `mj-cli/src/dashboard/io/spawn.rs`, and returned as a `DashboardIoUpdate`. The event loop in `mj-cli/src/dashboard.rs` polls `take_*` methods on `DashboardState` after every action, so state decides when background work starts and records it as in flight before it runs. The configuration the dashboard shows arrives from the daemon's runtime feed, which reloads `config.toml` every 500 ms; clients, not the daemon, write the file through `Config::update_to`, which holds a file lock across read, edit and write.

## Plan of Work

Milestone 1 adds the configuration helpers in `mj-core`. Milestone 2 adds `probe_machine`, `probe_machines` and `add_machine_runtimes` in `mj-controller/src/setup/machines.rs`, the `machine_runtime_checks` in `mj-controller/src/doctor.rs`, and the call in `run_setup_command` (`mj-controller/src/setup/startup.rs`). Milestone 3 adds `mj-tui/src/machine_runtimes.rs` (the dashboard's record of each machine's check, its trigger, and the reason text), the `CheckMachineRuntimes` action and `machines` on `DiscoverSetup` in `mj-tui/src/lib.rs`, the machine rows and Detect runtimes merge in `mj-tui/src/setup.rs`, the picker lines in `mj-tui/src/wizards/render.rs` and `lone_target`, and the CLI plumbing in `mj-cli/src/dashboard/{actions.rs,io.rs,io/spawn.rs}` and `mj-cli/src/dashboard.rs`. Milestone 4 adds the tests listed under Validation, updates `docs/SSH.md` and the Settings help text, and validates.

## Concrete Steps

From the repository root:

    cargo test -p brokk-mj-core config
    cargo test -p brokk-mj-controller setup:: doctor::
    cargo test -p brokk-mj-tui machine_runtime setup:: golden_wizard_target_step
    cargo test -p brokk-mjolnir dashboard::
    cargo clippy --all-targets -- -D warnings

Run `cargo test` outside the sandbox; the suites use loopback sockets.

## Validation and Acceptance

`golden_doctor_reports_external_aws_ssh_docker_and_podman_observations` (`mj-controller/src/doctor/tests.rs`) covers the doctor's machine checks: a machine that can run a bare runtime and Docker, one that does not answer, one whose platform and engines are all unusable, and an EC2 machine. `mj-controller/src/setup/machines/tests.rs` writes a real `config.toml` and shows that runtimes are added under the lock in the stored shape, not added twice, and not added to a machine that changed or gained a runtime while it was probed. `mj-tui/src/machine_runtimes.rs` tests the dashboard's transitions: a check starts once per machine definition, not while Settings is open, drops stale answers, and starts again when the machine changes or loses its runtimes. `golden_wizard_target_step` gains a scenario in which the target step is kept and names a machine without a runtime. A Settings test shows Detect runtimes adding a machine's runtimes to the draft under that machine.

Live check: an isolated instance (`mj --instance machine-runtimes`, with `MJ_CONFIG_DIR` and `MJ_DATA_DIR` unset so the instance flag applies) with an SSH machine that does not answer reports the reason in the notice, the picker and `mj doctor --instance machine-runtimes`.

## Idempotence and Recovery

Every write is additive and decided under the configuration lock, so repeating a probe, or running several dashboards at once, adds each runtime once. A probe that is cancelled (the dashboard quits) writes nothing. No database or configuration schema changes; `config.toml` gains ordinary `[targets.<id>]` entries that every supported version reads.

## Artifacts and Notes

Validation on 2026-10-22: `cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-tui` (2042 controller, 508 core, 532 TUI tests) and `cargo test -p brokk-mjolnir` passed; `cargo clippy --all-targets -- -D warnings` is clean.

Live check, isolated instance `machine-runtimes` (with `MJ_CONFIG_DIR`, `MJ_DATA_DIR` and `MJ_SESSION_ID` unset):

    $ mj --instance machine-runtimes doctor
    fixable Runtimes for machine nas: Machine nas has no runtime, so no session can run there, and it did not answer: `ssh dev@127.0.0.1 true` failed: ssh: connect to host 127.0.0.1 port 22: Connection refused
    fixable Runtimes for machine spot: Machine spot has no runtime, so no session can run there. It can run these runtimes: Bare.
      remediation: Start `mj` or run `mj setup` to add them.

    (dashboard started; about a second later config.toml gained)
    [targets.spot]
    kind = "bare"
    machine = "spot"

    New session · 2/4 target
      localhost  raw localhost
      spot       AWS EC2        unavailable
      Machine nas has no runtime: it did not answer. `ssh dev@127.0.0.1 true` failed ssh: connect to host 127.0.0.1 port 22: Connection refused. ...

The instance's daemon was stopped and its configuration and data directories removed afterwards.

## Interfaces and Dependencies

In `mj-core/src/config/machines.rs`:

    pub fn target_machine_id<'a>(target: &TargetTemplate, machines: &'a BTreeMap<String, Machine>) -> Option<&'a str>;
    pub const fn runtime_kind_name(target: &TargetTemplate) -> &'static str;

In `mj-core/src/config.rs`, on `Config`:

    pub fn machine_runtimes<'a>(&'a self, machine_id: &'a str) -> impl Iterator<Item = (&'a str, &'a TargetTemplate)> + 'a;
    pub fn machines_without_runtimes(&self) -> impl Iterator<Item = (&str, &Machine)>;
    pub fn machine_runtime_additions(&self, machine_id: &str, probed: &Machine, found: impl IntoIterator<Item = (String, TargetTemplate)>) -> BTreeMap<String, TargetTemplate>;

In `mj-controller/src/setup/machines.rs` (re-exported from `mj_controller::setup`):

    pub struct MachineProbe { pub machine_id: String, pub machine: Machine, pub outcome: MachineProbeOutcome }
    pub enum MachineProbeOutcome { Answered(Vec<RuntimeProbe>), Unreachable { detail: String, remediation: String } }
    pub fn probe_machine(machine_id: &str, machine: &Machine, executor: &impl CommandExecutor) -> Option<MachineProbe>;
    pub fn probe_machines(machines: &[(String, Machine)], executor: &(impl CommandExecutor + Sync)) -> Vec<MachineProbe>;
    pub fn add_machine_runtimes(config_path: &Path, probes: &[MachineProbe], cancelled: &dyn Fn() -> bool) -> Result<BTreeMap<String, Vec<String>>>;

In `mj-tui` (re-exported from the crate root): `MachineRuntimeProbe`, `MachineFinding`, `MachineRuntimesChecked`, and on `DashboardState` `take_machine_runtime_check` and `apply_machine_runtime_check`.
