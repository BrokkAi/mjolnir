# Manage exact harness versions on remote bare workers

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current while implementation proceeds. This document follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

An SSH-bare or EC2 session currently relies on whatever agent harness happens to be installed on the remote machine, and some fallback launchers may install tools into the user's ordinary home or even install Node with sudo. After this change, Mjolnir owns exact harness versions for remote bare targets. A remote session launches an absolute executable from a private, versioned cache, refuses stale or ambient fallbacks, and reports an actionable provisioning error when prerequisites or installation fail.

Existing active turns must not be disrupted by a Mjolnir upgrade. A busy worker may run for hours and keeps a shared lease on its old harness for that entire time. The controller switches it only after the existing quiet-state test succeeds, reloads the same native ACP session with the new worker and harness, and removes an old cached version only after its last lease is gone.

The behavior is demonstrated by focused Rust tests, the restart chaos harness, and a live tmux-driven SSH-bare session on `precision-3260`.

## Progress

- [x] (2026-09-04 17:00Z) Inspected current controller bridge selection, worker startup, reviewer startup, quiet-time worker upgrades, target kinds, configuration overrides, and chaos coverage.
- [x] (2026-09-04 17:05Z) Resolved product choices: manage SSH-bare and EC2 only, use exact pins, require rather than install Node, remove profile executable overrides, fail without fallback, and retain old versions for arbitrarily long busy turns.
- [x] (2026-09-04 20:10Z) Added the shared runtime policy and exact pin registry, and removed the public executable override and its call-site precedence rules.
- [x] (2026-09-04 21:20Z) Implemented atomic target-side installation, absolute launch resolution, overlapping worker/supervisor leases, and lease-aware garbage collection for primary and reviewer harnesses.
- [x] (2026-09-04 21:55Z) Made quiet-time worker replacement preflight the managed harness, preserve a running old worker on failure, and replace both the binary and launch record on success.
- [x] (2026-09-04 22:35Z) Added unit, compatibility, preflight, long-busy-turn, and restart-chaos coverage.
- [x] (2026-09-04 22:50Z) Updated README and user documentation for pins, prerequisites, cache ownership, failure behavior, and removed configuration.
- [x] (2026-09-04 23:47Z) Passed formatting, the full Rust test suite, Clippy with warnings denied, the isolated restart chaos harness, and a live tmux-driven Kimi session on `precision-3260`.
- [x] (2026-09-04 23:50Z) Committed the completed implementation on the current branch for integration and push.

## Surprises & Discoveries

- Observation: The worker already starts both the ACP supervisor and the underlying adapter in the project directory; Kimi itself emits redundant `cd` prefixes for some terminal commands.
  Evidence: `mj-worker/src/hel_worker_runtime/unix.rs` copies `WorkerLaunchConfig.cwd` into `AcpSupervisorSpec`, and `run_acp_supervisor_with_streams` calls `current_dir(&spec.cwd)`.

- Observation: The existing worker-upgrade coordinator already treats busy work as unbounded and retries on the next quiet observation.
  Evidence: `mj-controller/src/hel_worker_upgrade.rs::PolicyState::due` returns false whenever `observation.quiet` is false and has no busy-age override.

- Observation: A binary-only quiet-time replacement is insufficient for the first managed-runtime rollout because an existing launch file has no target-derived runtime policy.
  Evidence: `restart_worker_with_installed_binary` replaces `{worker_root}/hel` but does not replace `{worker_root}/launch.json`, while recovery has a separate launch-refresh plan.

- Observation: Reviewer launch messages must remain wire-compatible with old busy workers.
  Evidence: a new controller can request a review before an old worker reaches a quiet upgrade point, and `ReviewerLaunchConfig` uses `deny_unknown_fields`.

- Observation: First-time Kimi installation on the busy `precision-3260` host can exceed the worker control-socket startup deadline.
  Evidence: the first disposable live launch installed Kimi successfully but timed out waiting for `control.sock`; moving managed-harness preparation into the controller's pre-start file-preparation phase made the next launch connect normally.

- Observation: The ordinary development daemon currently panics during phone-server startup because two rustls `CryptoProvider`s are enabled; this predates and is independent of managed harness startup.
  Evidence: `scripts/run.sh` restarted the stale daemon, whose log showed the rustls provider-selection panic. Live validation therefore used isolated temporary config and data directories with the phone server disabled.

## Decision Log

- Decision: Add the managed/ambient choice only to `WorkerLaunchConfig`; reviewer launches inherit it from the primary worker.
  Rationale: This records target intent explicitly while avoiding a new field that an older busy worker would reject in `ReviewerLaunchConfig`.
  Date/Author: 2026-09-04 / Codex

- Decision: Pin Codex ACP 1.8.0 with Codex CLI 0.151.0, Claude ACP 0.73.0, Kimi Code 0.41.0, Grok 1.0.13, and DeepSeek dsh 0.1.1-rc.2 with ACP server 0.10.0.
  Rationale: These are the repository's vetted adapter versions plus the current exact native Kimi and Grok releases established during planning. Pin definitions live in one shared module so worker and container parity tests cannot drift silently.
  Date/Author: 2026-09-04 / Codex

- Decision: Use version directories under the remote user's Mjolnir cache, atomic staging plus rename, and advisory lease files held by both worker and ACP supervisor.
  Rationale: Content-addressed publication prevents partial installs from becoming runnable, while two overlapping shared leases close the worker-to-supervisor handoff and worker-crash teardown gaps.
  Date/Author: 2026-09-04 / Codex

- Decision: Never impose an elapsed-time limit on a busy worker or its harness lease.
  Rationale: A legitimate turn can take hours. Only operational quietness authorizes replacement; garbage collection uses nonblocking exclusive leases and skips active versions indefinitely.
  Date/Author: 2026-09-04 / Codex

- Decision: Keep local-bare and container launch semantics otherwise unchanged, but remove privileged Node installation and all executable overrides everywhere.
  Rationale: The requested ownership boundary is remote bare targets. Removing sudo installation and custom executables makes failure and precedence deterministic without expanding managed-cache ownership to other target types.
  Date/Author: 2026-09-04 / Codex

- Decision: Prepare a managed harness from the installed worker and installed launch record before starting a newly provisioned worker.
  Rationale: Installation time is provisioning work, not worker readiness. Keeping it outside the control-socket deadline makes slow first installs reliable while cache hits remain cheap.
  Date/Author: 2026-09-04 / Codex

## Outcomes & Retrospective

SSH-bare and EC2 workers now receive an explicit managed-runtime policy and resolve every supported harness to an exact, absolute executable below the remote user's Mjolnir cache. Installation is serialized and atomically published, completed entries are cheap to reuse, and obsolete versions are deleted only when a nonblocking exclusive lease proves that neither a worker nor its ACP supervisor still owns them. Local and container targets retain ambient behavior. The deleted profile executable override now fails configuration parsing instead of silently changing precedence, and no fallback attempts sudo or a distribution package manager.

Worker upgrades still wait without a busy-time ceiling. Once quiet, they preflight the new harness before touching the old process, then replace the binary and launch record together and recover the native ACP session. Preparation failure explicitly releases the live connection back to the session actor. Initial provisioning likewise prepares the installed harness before starting the worker, so a slow first download cannot consume the socket-readiness window.

Validation passed with `cargo fmt --all -- --check`, the complete `cargo test` suite, and `cargo clippy --all-targets -- -D warnings`. The isolated restart chaos harness completed five generations and proved the standalone supervisor retained its managed lease through process-tree teardown. On `precision-3260`, disposable session `2173ea20a4152880d663a6bc9eefc865` returned `LIVE_OK`; its launch record selected `managed_remote`, the Kimi ACP process used `/home/jonathan/.cache/mjolnir/harnesses/kimi/kimi-0.41.0/bin/kimi`, its cwd was the disposable project root, and a nonblocking exclusive-lock probe confirmed the lease was held. The session was stopped after validation; its isolated archived state and shared managed Kimi cache were retained rather than force-deleted.

During integration, current master moved the portable target CLI from `mj` into the dedicated `mj-worker` binary. The hidden `prepare-harness` command was ported to that target-side entry point, and the complete test suite, Clippy, musl worker build, and restart chaos harness all passed again on the merged tree.

One unrelated limitation remains visible: the user's ordinary development daemon cannot currently restart its phone server because of an existing dual-rustls-provider panic. That did not affect the isolated managed-harness validation and is not changed by this work.

## Context and Orientation

`src/hel_config.rs` defines user-facing harness profiles and target templates. `src/hel_worker_launch.rs` defines JSON launch records shared by the controller and target-side worker. `mj-controller/src/hel_controller/worker_binary.rs` turns a selected profile and target into that launch record and currently constructs shell-based bridge fallbacks. `mj-worker/src/hel_worker_runtime/unix.rs` reads the record, discovers a login-shell PATH for bare targets, starts the durable relay, writes an ACP supervisor specification, and launches the bridge through the worker's hidden `acp-supervisor` command. `mj-worker/src/hel_worker_runtime/reviewer.rs` repeats bridge startup for second-opinion roles.

A harness is the user-facing coding agent and its ACP adapter, such as Codex plus `codex-acp`. A managed harness installation is a directory under `$XDG_CACHE_HOME/mjolnir/harnesses`, or `$HOME/.cache/mjolnir/harnesses` when `XDG_CACHE_HOME` is unset. A lease is an advisory file lock: a running process holds a shared lock, while garbage collection must acquire an exclusive lock before deleting that version. An atomic publication is an installation built in a sibling temporary directory and renamed to its final name only after validation and a completion manifest have been written.

`mj-controller/src/hel_worker_upgrade.rs` observes each worker build but schedules replacement only when the relay reports no active prompt, terminal, background command, or queued work. `mj-controller/src/hel_controller/worker_restart.rs` owns the stop/replace/start sequence. The implementation must preserve this quiet gate and must never reinterpret a long-running busy state as a timeout.

## Plan of Work

First, add `HarnessRuntimePolicy` with `Ambient` and `ManagedRemote` variants to `src/hel_worker_launch.rs`, defaulting deserialization to `Ambient` for old launch records. Add it only to `WorkerLaunchConfig`. `worker_launch_config` selects `ManagedRemote` for `TargetLocator::SshBare` and `TargetLocator::AwsEc2`; all other locators select `Ambient`. Move the exact version constants into a shared foundation module that both controller fallback tests and the worker can read.

Remove `HarnessProfile.executable` from `src/hel_config.rs` and delete all branches that pass an executable override into bridge selection, login instructions, quota probing, and reviewer construction. Configuration remains `deny_unknown_fields`, so an old `executable` entry fails explicitly instead of being ignored. Replace messages that recommend the deleted field with PATH or prerequisite guidance. Change the Node preflight shell to require a compatible `node`, `npm`, and `npx` without invoking sudo or a system package manager.

Add a Unix worker runtime module that resolves managed harnesses. It derives the cache root from the worker process environment, uses the shared pin registry to choose an installation identifier, and serializes installation for that identifier with an advisory lock. Npm-backed recipes install from committed lock manifests using `npm ci` in an isolated staging prefix. Codex additionally sets `CODEX_PATH` to the exact managed 0.151.0 CLI. Kimi and Grok invoke their official installers with exact version and isolated destination variables so neither changes shell startup files or the ordinary user home. The module validates the newly installed entry point once, writes a completion manifest, and renames the staging directory into place. A completed cache hit only checks its manifest and entry point, avoiding a repeated expensive version probe.

The resolver returns an absolute command, arguments, additional environment, and an open shared lease. Primary startup retains that lease in `run_daemon`, and reviewer startup retains its own lease for the role lifetime. Extend `AcpSupervisorSpec` with an optional lease path; the supervisor acquires a second shared lock before spawning the bridge and holds it until the bridge process group has been terminated and reaped. This overlap prevents garbage collection during startup or worker-crash teardown. Garbage collection scans only the validated harness-cache root, tries exclusive nonblocking leases, and removes unleased obsolete version directories. Active versions are skipped without waiting or age limits, and cleanup failures are reported from supervised background work.

For existing sessions, extend the quiet-time replacement operation to materialize and atomically install the controller's current launch record along with the worker binary. Before stopping the old worker, run the target-side managed-harness preparation using the current worker build and proposed launch record. If preparation fails, return an upgrade failure and leave the old worker connected. If it succeeds, stop the quiet worker, replace both files, restart, and reload its prior native ACP session. The current backoff policy handles repeated preparation failures. A busy worker is neither prepared synchronously nor stopped; it can retain the old binary and harness for hours until a future quiet observation.

Update the existing restart chaos harness rather than adding an unrelated framework. Fake installers and temporary cache roots make tests deterministic and network-free. Add scenarios for concurrent publication, interrupted staging, supervisor lease survival after worker death, obsolete-version cleanup only after lease release, and a busy observation far beyond normal timeouts. Extend the upgrade fake to prove preparation failure leaves the old connection alive and successful replacement refreshes both binary and launch policy.

Finally, update README and rendered documentation for managed remote harnesses, exact pins, prerequisites, cache location, removal of `profiles.<id>.executable`, and indefinite busy-turn retention. Run all repository checks, then exercise a disposable SSH-bare session on `precision-3260` inside tmux. Inspect the remote process command and cwd, cached completion manifest, held lease, native session continuity after a quiet restart, and old-version cleanup. Do not interfere with unrelated live sessions on that host.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`.

After each coherent milestone, run focused tests such as:

    cargo test -p brokk-mj-worker hel_worker_runtime
    cargo test -p brokk-mj-controller hel_worker_upgrade
    cargo test -p brokk-mj-core hel_config

Run all Cargo tests outside the restricted sandbox because loopback sockets are part of the suite:

    cargo test
    cargo clippy --all-targets -- -D warnings

Run restart chaos in its required isolated environment using the repository's documented invocation discovered from `tests/e2e/session_restart_chaos.sh`; expect its final `chaos: passed` line plus the new lease and busy-upgrade assertions.

For live validation, create a uniquely named tmux session on the controller host, invoke `scripts/run.sh` against the existing `precision-3260` SSH-bare target with a disposable repository/session name, and capture the pane log. The session must show the managed-install provisioning stage, an absolute executable below `.cache/mjolnir/harnesses`, and an ACP process cwd equal to the selected project directory. Stop and clean up only the disposable session after collecting evidence.

## Validation and Acceptance

The feature is accepted when an SSH-bare or EC2 launch never chooses an ambient harness executable, installs or reuses the exact selected pin below the Mjolnir cache, and produces a useful launch error when Node/npm, curl, download, or validation fails. No code path may run sudo or a distro package manager to satisfy these prerequisites.

Two simultaneous first launches of the same pin must publish one complete installation and both must run it. A killed installer must leave only removable staging state. A running primary or reviewer must prevent deletion of its version even when another worker requests garbage collection.

Advancing test time by hours while a worker remains busy must produce no preparation, stop, or replacement. Once the same worker becomes quiet, failed preparation must leave it alive; successful preparation must refresh the launch policy, restart on the current binary and pin, reload the same native session, and make the unleased old version eligible for immediate cleanup.

All focused tests, `cargo test`, `cargo clippy --all-targets -- -D warnings`, restart chaos, and the disposable `precision-3260` tmux scenario must pass. The final commit must contain only files changed for this feature.

## Idempotence and Recovery

Installation and garbage collection are retryable. A complete final directory is reused, an incomplete staging directory is never executed, and publication by a concurrent process is treated as success after validating the winner. Cleanup refuses paths outside the fixed cache root and skips any version whose lease cannot be acquired exclusively.

If live validation fails, retain the tmux log long enough to diagnose it, but remove only the disposable Mjolnir session and its uniquely named test workspace. Do not delete shared caches wholesale and do not stop unrelated workers. A failed quiet-time preparation leaves the previous worker running by design.

## Artifacts and Notes

The current worker-upgrade contract already documents the critical invariant in `README.md`: a session that is never quiet keeps the worker it started with until it is stopped. The managed harness lifetime must follow the same invariant rather than adding a shorter cache-specific deadline.

## Interfaces and Dependencies

`src/hel_worker_launch.rs` will expose a serializable `HarnessRuntimePolicy` and a `harness_runtime` field on `WorkerLaunchConfig`. It defaults to `Ambient` when absent. `ReviewerLaunchConfig` deliberately remains unchanged.

`src/hel_harness_runtime.rs` will expose exact pin metadata and stable installation identifiers for each `HarnessKind`. It contains no process execution. `mj-worker/src/hel_worker_runtime/harness.rs` owns filesystem, subprocess, installation, lease, and garbage-collection behavior.

`AcpSupervisorSpec` gains an optional `harness_lease: Option<PathBuf>` field with a serde default. The supervisor opens and locks that file before bridge spawn. The worker uses the standard library's advisory file-lock API and existing `hel::hel_subprocess` helpers; it must not hand-roll pipe handling.

The npm recipe assets are packaged with `brokk-mj-worker` and embedded or copied without relying on the controller filesystem. No new workspace crate is introduced.

Revision note (2026-09-04): Initial implementation plan created after repository inspection and product decisions; added the required live tmux validation on `precision-3260`.

Revision note (2026-09-04): Recorded the completed implementation, pre-start preparation discovered by live testing, final validation evidence, and the unrelated development-daemon rustls limitation.

Revision note (2026-09-04): Integrated the dedicated portable-worker split from master and revalidated the combined tree and musl chaos artifact.
