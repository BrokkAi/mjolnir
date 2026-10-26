# Let detection decide what the session pickers offer

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir detects agent profiles (a configured coding agent such as Claude Code or Codex, under `[profiles.<id>]` in `config.toml`) and runtimes (where a session's agent runs: bare on a machine, or in Podman, Docker, or Apple container, under `[targets.<id>]`). Before this change detection only ever added entries, and only at a few moments: profiles once per dashboard launch, and an SSH machine's runtimes only while that machine had none at all. Nothing ever noticed that something had gone away. The session pickers offered whatever the configuration named, then probed each runtime separately when the picker opened, showing rows marked "checking availability…" and greyed "unavailable" rows that could not be picked, and kept a good answer for thirty minutes. A runtime installed later on a machine that already had one was never found; the user had to press Detect runtimes in Settings or write the `[targets.<id>]` table by hand.

After this change one check decides what exists. It runs in the background when the dashboard starts and each time the new-session or resume picker opens. It looks at this computer directly (agent homes, agent commands on `PATH`, provider API keys, and the local container engines), and at each SSH machine with exactly one SSH call that runs every probe and disconnects. It adds what is new: a newly installed agent becomes a profile, and a newly working engine on any machine becomes a runtime. The pickers list only what the latest check found present. Nothing is ever shown as "checking"; the picker shows the last answer, and the screen changes only when an answer changes. What is missing is not listed, and one line under the list names it, says why, and says how to fix it. A runtime can be switched off with `enabled = false`, so a runtime the user does not want is not added back.

To see it: in an isolated instance (`mj --instance avail`) with Docker stopped, open New session (`n`). The target list has no `docker` row and the help under it reads `docker: Docker is not running. Start Docker Desktop…`. Start Docker and open the picker again: the row is there, with no "checking" step in between.

## Progress

- [x] (2026-10-26 05:39Z) Mapped the current detection paths, the per-target readiness checks in the pickers, the machine probe, and the configuration shape. Created branch `detection-availability`.
- [x] (2026-10-26 07:10Z) Milestone 1: one SSH call per machine probe (`record_machine`, `RecordedMachine` in `mj-controller/src/setup/machines.rs`), EC2 probe through the AWS checks, `profile_presence` and `check_availability` (`mj-controller/src/setup/availability.rs`). Every check adds missing runtime kinds on every machine. The doctor golden is unchanged byte for byte.
- [x] (2026-10-26 07:10Z) Milestone 2: `targets.<id>.enabled` (configuration version 19, `Config::disabled_targets`, `enabled_targets`, `enabled_target`, `ensure_target_enabled`), refused by launch, resume and move, left out of the daemon's viewer projection, kept when a standard local runtime is switched off, and an Enabled checkbox on each Settings runtime. Remaining for the pickers: Milestone 3.
- [ ] Milestone 3: the dashboard owns one availability snapshot, replaces target readiness and the machine-runtime check, and the pickers list only present entries.
- [ ] Milestone 4: tests, documentation, full validation, live check in an isolated instance.
- [ ] Pull request, review, fixes, merge.

## Surprises & Discoveries

- Observation: the pickers already had a per-target availability system, which is the source of the "checking availability…" rows and the thirty-minute success cache.
  Evidence: `mj-tui/src/wizards/dashboard/targets.rs` (`begin_target_readiness_checks`, `target_readiness_rejection`), `TARGET_READINESS_TTL` in `mj-tui/src/wizards.rs`, dispatched per target in `mj-cli/src/dashboard/actions.rs` (`DashboardAction::CheckTargetReadiness`).

- Observation: a machine probe opened four to seven SSH connections in sequence: `true`, `uname -sm`, the batched Podman script, `docker version`, and on an Apple-silicon Mac `uname`, `sw_vers`, `container --version`, `container system status`.
  Evidence: `probe_ssh_machine` in `mj-controller/src/setup/machines.rs`.

- Observation: six controller tests fail on this macOS host before and after the change: five use `rsync --protect-args`, which macOS's bundled openrsync lacks, and `ec2_move_preview_defers_destination_checks_and_resolves_inherited_container_sizing` fails on unmodified master too.
  Evidence: `rsync: unrecognized option '--protect-args'`; the EC2 test fails with the change stashed.

- Observation: `HarnessProfile::ensure_ready` is already the one owner of "can a harness start from this profile"; every launch asks it. Profile presence is that plus "the installation is still here", not a second readiness rule.
  Evidence: `mj-core/src/config/harness.rs`, `ensure_ready`.

- Observation: Mjolnir installs and runs its own pinned agent bridges, so a profile does not need the user's agent command to run. The user's credentials live in the profile's home directory.
  Evidence: `mj-core/src/harness_runtime.rs`, "Exact harness versions used by Mjolnir-managed installations."

## Decision Log

- Decision: checks run at dashboard start and on opening the new-session or resume picker, in the background, and at no other time. There is no timer and no reaction to configuration changes.
  Rationale: the user chose this. Configuration writes come from many places, including detection itself; reacting to them invites loops, and a timer adds work nobody asked for. Opening the picker is the moment the answer matters.
  Date/Author: 2026-10-26, user and Claude.

- Decision: a remote machine is checked with one SSH call that runs every probe and disconnects. The answers are fed to the existing verifiers through a replaying `CommandExecutor`, so each runtime is still judged and worded by the code a launch uses.
  Rationale: the user asked for "ssh, run all checks, disconnect". Replaying keeps one implementation of every judgement instead of writing a second parser per engine.
  Date/Author: 2026-10-26, Claude.

- Decision: the pickers never show a "checking" state. Each entry is present, missing, or not yet known; not yet known is listed, because the configuration is evidence the entry existed when it was written, and the first check after start usually answers within a second. Only missing entries are left out.
  Rationale: the user rejected progress markers that flash when the answer usually does not change, and rejected offering what is not there. Hiding not-yet-known entries would empty the profile list at start (which opens Settings) and would move the cursor off the recent target while a host answers.
  Date/Author: 2026-10-26, Claude.

- Decision: a missing entry is left out of the list and named once under it, with the reason and the fix. Unavailable rows are no longer shown greyed.
  Rationale: people do not want to pick something that is not there; a silent disappearance would be as bad as a dead row.
  Date/Author: 2026-10-26, user and Claude.

- Decision: a profile is present when its harness home exists (for a Codex profile file, that file exists), when it is supplied with the API key its harness reads, or when the harness command is on `PATH`. These are the same signals discovery uses to add a profile, decided by one function used by both.
  Rationale: Mjolnir runs its own pinned bridges, so an uninstalled agent command with credentials still in the home still works, while a profile with no home, no key and no command cannot. One predicate means detection never adds what availability calls missing.
  Date/Author: 2026-10-26, Claude.

- Decision: every check adds any runtime kind a machine newly runs, on every machine, not only machines without runtimes. Runtimes gain `enabled = false`, as profiles have, and a disabled runtime counts as present for the decision, so it is never added again.
  Rationale: this reverses the #1329 rule that a machine with a runtime is never probed for more. The user asked that detection find what is there without editing configuration. Profiles already work this way.
  Date/Author: 2026-10-26, user and Claude.

- Decision: configuration version 19 is breaking.
  Rationale: an older build reading `enabled = false` on a container runtime ignores the unknown key (container settings are flattened, which turns off `deny_unknown_fields`) and a save drops it, so the runtime would come back enabled; on a bare runtime an older build refuses the file. Older writers cannot preserve the switch.
  Date/Author: 2026-10-26, Claude.

## Outcomes & Retrospective

To be written at completion.

## Context and Orientation

The configuration lives in `mj-core/src/config.rs` (`Config`, read and written through `StoredConfig`) and `mj-core/src/config/machines.rs` (`Machine`, `StoredTarget`). In memory every runtime is a `TargetTemplate` (`mj-core/src/config/targets.rs`) that fuses host and runtime, for example `SshDocker { ssh, container }`. `Config::with_local_targets` adds candidate local runtimes (`localhost`, `podman`, `docker`, and on macOS `apple-container`) that the file does not name; their ids are in `Config::default_targets` and they are never written unless the user changes them. `Config::machine_runtime_additions` decides which probed runtimes a machine gets; `Config::enabled_profiles` lists profiles not switched off.

Detection is in `mj-controller/src/setup.rs` (profile discovery: `discover_profiles`, `discover_installed_harnesses`), `mj-controller/src/setup/machines.rs` (machine probing: `probe_machine`, `probe_machines`, `add_machine_runtimes`) and `mj-controller/src/setup/startup.rs` (first-run setup and `add_detected_profiles`). Each runtime probe returns a `RuntimeProbe` with a doctor `CheckStatus`, so setup and `mj doctor` word a problem the same way. Remote engine verifiers are in `mj-controller/src/targets/preflight.rs` (`verify_ssh_podman`, `verify_ssh_docker`, `verify_ssh_apple_container`); every command runs through a `CommandExecutor` (`mj-controller/src/targets.rs`).

The dashboard is a state machine in `mj-tui` (`DashboardState`, `mj-tui/src/lib.rs`) driven by `mj-cli/src/dashboard.rs`, which runs blocking work in background tasks (`mj-cli/src/dashboard/io/spawn.rs`) and feeds results back as `DashboardIoUpdate`s. The new-session and resume pickers are the wizards in `mj-tui/src/wizards*`; they index profiles through `Config::enabled_profiles` and runtimes through `config.targets`, and filter the rows shown through `DashboardState::offered_target_indices`. Before this change the target step's rows came from `target_readiness` (per-target checks), and machines without runtimes were probed by `take_machine_runtime_check` in `mj-tui/src/machine_runtimes.rs`.

## Plan of Work

Milestone 1 is in `mj-controller`. `probe_ssh_machine` builds one remote script from the exact remote command lines the verifiers send (each built by the same builder the verifier uses), runs it through one `ssh` command made by `ssh_validation_command`, and parses the framed output with the existing probe framing (`__mj_probe_begin__` blocks, as in `SSH_PODMAN_PREFLIGHT_SCRIPT`). A `ReplayExecutor` then answers each verifier's `CommandSpec` by its remote command line from the recording; a command that was not recorded is an error, which a test drives so the recorded list and the verifiers cannot drift. When `ssh` itself fails, the connectivity check receives the transport failure and classifies it as before. An EC2 machine is probed with the AWS credential and launch-template checks a launch runs, so its bare runtime is offered and added only when they pass. A new `profile_presence` decides whether a configured profile's installation is there, and `discover_installed_harnesses` uses the same `PATH` lookup. A new `setup::check_availability(config_path, executor)` runs discovery and adds profiles, probes local engines and every machine at once, adds new runtimes on every machine, and returns an `AvailabilityReport` with profile presence, local engine probes, machine probes and what it added.

Milestone 2 adds `Config::disabled_targets`, read from and written to `enabled = false` on a `[targets.<id>]` table, with `Config::enabled_targets` and `Config::enabled_target`, configuration version 19, and refusal at every launch, resume and move path that already refuses a disabled profile. A disabled standard local runtime is written to the file. The Settings Runtimes page gets the Enabled checkbox profiles have. The daemon's launch options leave disabled runtimes out.

Milestone 3 replaces `target_readiness`, `machine_runtime_checks` and the absent-engine cache with one `Availability` snapshot on `DashboardState`, filled from `AvailabilityReport`. One function answers whether a profile or runtime is offered. Opening a picker starts a check when none is in flight. The profile and target steps list only offered entries; the help under each list names each missing entry once. When an answer removes the entry a wizard has selected, the selection moves to the first offered entry. The Targets pane and session rows read the same snapshot.

Milestone 4 adds the tests named under Validation, updates `docs/` (configuration reference, targets, SSH) and the help texts, runs the full checks, and runs a live check.

## Concrete Steps

Run everything from the repository root. Tests run outside the sandbox:

    cargo test -p mj-controller setup::
    cargo test -p mj-tui wizards::
    cargo test -p mj-core config::
    cargo clippy --all-targets -- -D warnings

## Validation and Acceptance

The machine probe test records the single `ssh` call a fake host receives and checks that every verifier was answered from it: exactly one `ssh` command per machine, and a run against a recording that lacks a command fails. A configuration test checks that a version-18 file loads, that `enabled = false` round-trips on bare and container runtimes, and that a disabled standard local runtime is written. A golden render of the new-session target step with one missing local engine and one unreachable machine shows no "checking" text, no row for either, and one help line for each. A golden render of the profile step shows a missing profile left out and named. Acceptance in an isolated instance is described under Purpose.

## Idempotence and Recovery

Every check is additive and decided under the configuration lock against the latest file, so repeated or concurrent checks add each runtime once. Nothing deletes configuration. Cancelling a check on quit leaves the file as the last completed write left it.

## Artifacts and Notes

None yet.

## Interfaces and Dependencies

In `mj-controller/src/setup.rs`:

    pub fn profile_presence(profile: &HarnessProfile) -> Presence;
    pub fn check_availability(config_path: &Path, executor: &(impl CommandExecutor + Sync), cancelled: &dyn Fn() -> bool) -> Result<AvailabilityReport>;

In `mj-core/src/config.rs`:

    pub disabled_targets: BTreeSet<String>,   // field on Config
    pub fn enabled_targets(&self) -> impl Iterator<Item = (&str, &TargetTemplate)>;
    pub fn enabled_target(&self, id: &str) -> Option<&TargetTemplate>;
