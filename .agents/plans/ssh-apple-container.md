# Apple container runtimes on SSH machines

This ExecPlan is a living document and follows the standard in
[`.agents/PLANS.md`](../PLANS.md). The `Progress`, `Surprises & Discoveries`,
`Decision Log`, and `Outcomes & Retrospective` sections track the implementation
from design through validation.

## Progress

- [x] (2026-10-24) Defined the fused target and state shapes while keeping the
  saved configuration as an `apple-container` runtime on an `ssh` machine.
- [x] (2026-10-24) Implemented configuration resolution, state conversion, and
  the execution-side SSH Apple container target.
- [x] (2026-10-24) Reused the local Apple container lifecycle plans over SSH for
  provisioning, image preparation, smoke tests, checkpoints, worker installation,
  restart, cleanup, and absence checks.
- [x] (2026-10-24) Added the schema revision 98 migration and target-table
  round trip.
- [x] (2026-10-24) Added machine probing, `mj doctor`, recovery scanning, TUI
  labels and filters, build/tool cache exclusions, and host capacity routing.
- [x] (2026-10-24) Integrated current `origin/master`, including the stack-size
  fix for the controller Move recovery test that failed the first PR run.
- [x] (2026-10-24) Made an SSH disconnect during Restart classify the remote
  Apple container as unreachable so its retained target and checkpoint are not
  discarded.
- [x] (2026-10-24) Documented remote installation, noninteractive SSH `PATH`,
  platform requirements, service lifetime, and the supported target matrix.
- [x] (2026-10-24) Passed focused feature and migration tests, the full
  controller and TUI suites, formatting, and workspace clippy.
- [ ] A live Apple-silicon Mac running Apple's `container` daemon was unavailable,
  so hardware-backed provisioning remains unvalidated.

## Purpose

An `apple-container` runtime today runs only on this computer. A user whose
laptop is an Intel Mac, or whose Mjolnir runs on Linux, therefore cannot reach
the Apple-silicon Mac on the desk next to it, even though that Mac has the
`container` CLI, a running daemon, and everything an `apple-container` session
needs. The capability matrix in `docs/src/content/docs/targets.md` lists the
missing row: `apple-container` on an `ssh` machine.

The outcome: a version 15 configuration may name an `apple-container` runtime
on an SSH machine. Settings › Runtimes offers it whenever the probed machine
answers that it is Apple-silicon macOS 26+ with a running Apple container
daemon, `mj doctor` checks it the way it checks local Apple container, and a
session provisions, runs, and is removed over SSH exactly as a local one is.

## Shape of the change

The configuration file keeps runtimes and machines apart, so nothing new
appears there: the runtime is still `kind = "apple-container"`, and the
machine is still `kind = "ssh"`. What changes is `resolve_target`, which used
to refuse that pair. In memory and on the wire the fused target gains a ninth
kind, `ssh-apple-container`, beside `ssh-podman` and `ssh-docker`:

- `mj-core/src/config/targets.rs`: `TargetTemplate::SshAppleContainer { ssh, container }`.
- `mj-core/src/config/machines.rs`: `resolve_target` joins it and
  `stored_target` splits it; `runtime_kind_name` reports `apple-container`.
- `mj-core/src/state.rs`: `TargetLocator::SshAppleContainer { host, container_id, borrowed_from }`,
  recorded so a session reconnects without the configuration.
- `mj-core/src/targets.rs`: the execution-side template, `ImageHost::SshAppleContainer`,
  locator command, worker root, and image refresh.
- `mj-controller/src/targets/{provision,cleanup,preflight}.rs`: `container run`
  over SSH, the disposable smoke test, remote preflight (platform, daemon,
  version), close, and absence confirmation.
- `mj-controller/src/{doctor.rs,setup/machines.rs}`: the check and the machine
  probe that offers the runtime.
- `mj-controller/src/database/`: the locator kind in the store's `CHECK`
  constraint, as a new migration revision.

Everything else already works for a container target once the kind exists:
worker installation, checkpoint transfer, mbx cache mounts, and review all
dispatch on "is this a container target" and on the engine.

## Surprises & Discoveries

- Observation: The initial PR's Linux failure was not caused by this feature.
  Evidence: `controller::move_session::tests::recovery_from_in_place_closing_source_keeps_children_and_skips_stop_hook`
  overflowed its stack on the branch, while the newer `origin/master` commit
  `6f967768c` increased that nested test's stack and the test then passed after
  the merge.
- Observation: The full core suite cannot prove detached-child reaping in this
  Mjolnir container. Evidence: 522 tests passed and
  `subprocess::tests::spawn_detached_leaves_no_zombie_under_a_spawner_that_keeps_running`
  observed a zombie because PID 1 does not reap children here.
- Observation: An SSH Apple target has no target-level recovery plan, so Restart
  checks the retained container through `container list` instead. A lost SSH
  connection must therefore be classified as unreachable rather than a generic
  list failure.

## Decision Log

- **No new configuration key.** The pair "apple-container runtime on an SSH
  machine" is enough. A separate machine kind would duplicate SSH connection
  handling for one engine.
- **Apple container keeps its limits over SSH**: no `workspace_storage`, no
  attachments with copy-on-write (read-only only), and no container upgrade.
  The same validation that rejects those locally rejects them remotely, so a
  remote session cannot claim a capability the engine does not have.
- **The probe decides whether the runtime is offered**, as for Podman and
  Docker: `uname -sm` for `Darwin arm64`, `sw_vers -productVersion` for macOS
  26+, `container --version`, and `container system status`.
- **Migration revision 98** widens the `session_targets.kind` `CHECK`
  constraint. It is additive for existing rows and never rewrites one; a
  daemon older than revision 98 cannot read a session on the new kind, which
  is the same compatibility the previous SSH Docker kind shipped with.
- **Run every local Apple lifecycle command over SSH.** The engine's VM disk
  image belongs to the host, so the remote target is the local command plan
  wrapped with the shared SSH helper. This preserves quoting, stdin secrets,
  cancellation, and connection reuse instead of maintaining a second plan.
- **Treat SSH status 255 as unreachable during Restart.** The retained container
  cannot be inspected when the transport is down, but that is not evidence it
  is absent. Preserve the recovery data and let the user retry after reconnecting.
- **Document the noninteractive SSH environment explicitly.** Mjolnir probes
  and launches with `ssh <destination> <command>`, so login-shell-only `PATH`
  setup is insufficient for finding `container`.

## Validation

- `cargo test -p mj-core config::`, `cargo test -p mj-controller targets::`,
  `cargo test -p mj-controller doctor`, `cargo test -p mj-controller setup`,
  `cargo test -p mj-controller database`.
- `cargo clippy --all-targets -- -D warnings`.
- New tests: config round trip for the runtime/machine pair, provisioning
  argv built over SSH, close plan, remote absence confirmation, machine probe
  offering the runtime, and doctor reporting it.

## Log

- 2026-02-14: Implemented as described. Beyond the files the shape named, the
  new kind also had to be threaded through the sites that dispatch on a
  container locator or target and would otherwise have silently treated a
  remote Apple session as a local or bare one: recovery scan (candidate
  discovery, adoption, same-target comparison), Move's destination transport
  probe, the restart availability probe, checkpoint staging cleanup, pids
  limits, mbx cache-host resolution, and the dashboard's container-session
  filter. `TargetRuntimeSettings::from` also needed the template arm, or a
  remote Apple session would have recorded a `local` connection. The remote
  kind validates exactly as the local one does: same refusal of Podman's
  `workspace_storage`, same acceptance of `platform`, because the engine is the
  same and only its host differs.
- Validation: `cargo test -p brokk-mj-core --lib` (one pre-existing
  environment failure in `subprocess::tests`, a zombie that never reaps in this
  container), `cargo test -p brokk-mj-controller --lib` (one pre-existing stack
  overflow in `controller::move_session::tests`, reproduced on the unmodified
  tree), `cargo test -p brokk-mj-tui --lib`, and
  `cargo clippy --all-targets -- -D warnings`. New tests cover the config round
  trip and the `platform` refusal, provisioning over SSH, the close plan,
  remote absence confirmation, the smoke plan, the machine probe offering the
  runtime only on Apple-silicon macOS, and the doctor check reporting a stopped
  daemon.
- Not validated: no live Apple-silicon Mac was reachable from this environment,
  so nothing here was exercised against a real `container` daemon.

## Context and Orientation

Mjolnir separates a machine (where work runs) from a runtime (how it runs).
An Apple-silicon Mac is represented as an `[machines.<id>]` table with
`kind = "ssh"`, and Apple's container engine is a `[targets.<id>]` table with
`kind = "apple-container"` and `machine = "<machine-id>"`. Before this change,
that pair was rejected locally at configuration load.

The configuration-time type is `mj_core::config::TargetTemplate`. The
execution-time type is `mj_core::targets::TargetTemplate`, and a recorded
session uses `mj_core::targets::TargetLocator`. A locator contains only the
durable identity of a provisioned target; the connection details and launch
environment live in `mj_core::state::TargetRuntimeSettings`. The controller's
SQLite `session_targets` table stores the locator kind in a constrained `kind`
column. Every operation that starts, copies into, restarts, or removes a target
must therefore agree on all three representations.

The local Apple implementation already owns the correct engine-specific argv,
read-only attachment behavior, image refresh, checkpoint commands, and cleanup
ordering. The remote implementation reuses those plans and changes only the
execution boundary to SSH. This is important because duplicating the plan
would make quoting, secret stdin, and cleanup behavior diverge.

## Plan of Work

In `mj-core/src/config/`, union `apple-container` with `SshConnection` into
`SshAppleContainer`, split it back to the same stored pair, and share validation
with the local Apple target. In `mj-core/src/state.rs`,
`mj-core/src/state/target_runtime.rs`, and `mj-core/src/targets/`, add the
corresponding locator, connection, engine, image-host, command, workspace, and
storage behavior.

In `mj-controller/src/targets/`, build provisioning and cleanup by wrapping the
local Apple command plan with the shared SSH command helper. The preflight runs
platform, macOS release, CLI version, and daemon health checks on the remote
host. Recovery scanning lists Apple containers on that host. Worker installation,
startup, upgrading, checkpoint transfer, and cache cleanup dispatch to the
remote SSH boundary.

In `mj-controller/src/database/`, advance the store to revision 98 by recreating
`session_targets` with `ssh-apple-container` allowed in its `kind` check and
advance the compatibility floor because an older reader cannot decode the new
kind. In `mj-controller/src/setup/machines.rs` and `doctor.rs`, offer and verify
the runtime only on Apple-silicon macOS 26 or newer. In the TUI, label and route
the target like the other container runtimes.

## Concrete Steps

Run all commands from the repository root. First narrow the change while
developing:

    cargo test -p brokk-mj-core --lib apple_container
    cargo test -p brokk-mj-controller --lib apple_container
    cargo test -p brokk-mj-controller --lib database::

The expected result is zero failures. These cover the saved config round trip,
local/remote validation parity, provisioning and cleanup command shape, remote
absence confirmation, the smoke plan, machine detection, doctor reporting, and
the migration from revision 97 to 98.

Before submitting, run the full touched-crate checks and clippy:

    cargo test -p brokk-mj-core --lib
    cargo test -p brokk-mj-controller --lib
    cargo test -p brokk-mj-tui --lib
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all --check

The controller and TUI suites must pass. In this Mjolnir disposable container,
the core suite has one environmental failure in
`subprocess::tests::spawn_detached_leaves_no_zombie_under_a_spawner_that_keeps_running`;
the test observes a zombie because this container's PID 1 does not reap it.
The failure occurs in untouched code and is not accepted as a product failure.

## Validation and Acceptance

Configuration acceptance: a version 15 file containing an SSH machine and an
`apple-container` target naming it loads as `SshAppleContainer`, reports the
runtime name as `apple-container`, and saves back to the same two-table shape.
Validation refuses Podman-only `workspace_storage` and accepts `platform`.

Machine acceptance: an SSH host reporting `Darwin arm64`, macOS 26 or newer,
`container --version`, and successful `container system status` is offered an
Apple container runtime. A Linux host receives an unsupported result without
any remote `container` command being attempted.

Lifecycle acceptance: provisioning contains only `ssh` commands and creates the
container with `container run` on the remote host. Smoke testing creates,
executes, and removes a disposable remote container. Close removes the exact
container and its session Git-cache snapshot. An unavailable SSH connection
during Restart yields `Unreachable`, preserving the retained container.

The remaining hardware gap is explicit: without a reachable Apple-silicon Mac,
the tests validate command construction and controller behavior but cannot
prove interoperability with a live Apple `container` daemon.

## Idempotence and Recovery

The configuration loader and saver are idempotent. The database migration runs
inside one `BEGIN IMMEDIATE` transaction and records revision 98 only after the
target table has been recreated and copied. Rerunning startup after a completed
migration is a no-op. Do not edit a revision that has already shipped; a future
schema change must use the next revision.

Remote container creation uses the existing generated session name and ownership
labels. Cleanup and absence checks are scoped to that exact ID. If cleanup
cannot prove absence, retain the session record and recovery data and retry;
do not delete the Mac's runtime storage or stop its shared container service.

## Artifacts and Notes

Focused validation results from 2026-10-24:

    2 passed: remote Apple config round trip and validation parity
    9 passed: remote Apple target, doctor, setup, and lifecycle tests
    214 passed, 2 ignored: controller database tests
    2090 passed, 17 ignored: full controller library suite
    538 passed, 2 ignored: full TUI library suite
    cargo clippy --all-targets -- -D warnings: clean

The exact failing core test and the reason are recorded in
`Surprises & Discoveries` above.

## Interfaces and Dependencies

The implementation adds these durable variants and methods:

    mj_core::config::TargetTemplate::SshAppleContainer {
        ssh: SshConnection,
        container: ContainerTemplate,
    }

    mj_core::state::TargetLocator::SshAppleContainer {
        host: String,
        container_id: String,
        borrowed_from: Option<String>,
    }

    mj_core::targets::TargetTemplate::SshAppleContainer {
        ssh: SshTarget,
        container: ContainerTemplate,
    }

    mj_core::targets::TargetLocator::SshAppleContainer {
        ssh: SshTarget,
        container_id: String,
        borrowed_from: Option<String>,
    }

`ImageHost::SshAppleContainer` uses the existing `ssh_command_owned` helper, and
the controller's `verify_ssh_apple_container` uses the existing validation
command and command executor abstractions. No new crate or dependency is
required.

## Outcomes & Retrospective

The feature is implemented and validated at the controller and command-plan
layers. An SSH Apple target now behaves as a local Apple target whose commands
run on the configured Mac, with durable locator migration, runtime discovery,
doctor diagnosis, recovery scanning, Restart classification, cleanup, and
documentation. Integrating current `master` resolved the unrelated Linux test
failure that blocked the initial PR run.

The only remaining limitation is live-hardware verification. A follow-up with
an Apple-silicon Mac should run `mj doctor --json --smoke`, launch a session,
restart the controller while the worker is running, transfer a checkpoint, and
close the session while a second session continues. Those steps would validate
Apple daemon behavior and SSH login-environment setup end to end.
