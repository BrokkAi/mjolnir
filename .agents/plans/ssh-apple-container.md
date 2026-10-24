# Apple container runtimes on SSH machines

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

## Decisions

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
