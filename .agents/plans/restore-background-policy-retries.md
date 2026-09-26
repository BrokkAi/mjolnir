# Retry background work when an idle session has no new events

This ExecPlan follows `.agents/PLANS.md` and must be updated as implementation
and validation proceed.

## Purpose / Big Picture

After a turn ends, Mjolnir must eventually make its recovery checkpoint even
when a concurrent worker-version check temporarily owns that session. A quiet
session must also retry eligible background work when a delay expires. Neither
operation should depend on the user sending another prompt.

## Progress

- [x] (2026-09-26) Downloaded master reliability run 36227014873 and identified
  the failed checkpoint boundary and a separate browser validation failure.
- [x] (2026-09-26) Prepared the Windows-only unused-function correction and
  directory-only browser validation correction with focused regressions.
- [x] (2026-09-26) Reproduced the checkpoint failure with observation diagnostics.
- [x] (2026-09-26) Added periodic reevaluation from current daemon records and cached worker
  facts, without republishing unchanged UI snapshots.
- [x] (2026-09-26) Verified retry after contention and retirement of removed sessions.
- [x] (2026-09-26) Passed crash boundaries, browser convergence, Cargo tests, and Clippy.
- [x] (2026-09-26) Completed the implementation and local validation; hand release
  preparation and publication to the existing `RELEASING.md` workflow.

## Surprises & Discoveries

The failed session's turn ended at 07:37:00 UTC and its last worker view arrived
at 07:37:01.052. A worker-version check reported that its worker was already
current at 07:37:02.420. The checkpoint never began during the next 18 seconds.
The recovery and worker-upgrade coordinators share a per-session gate, and a
failed gate acquisition discards that observation. Session actors intentionally
do not republish unchanged views. Thus the assumption in background-policy
comments that idle observations arrive every sync tick is no longer valid.

The Windows failure is an environment-filter helper compiled on every platform
whose callers are all Unix-only. The browser failure is `invalid id` because
local-directory preflight supplies an empty bundle ID and server validation
requires a bundle even for directory-based targets.

## Decision Log

Use the daemon's current records to reevaluate background policies periodically.
Do not have coordinators retain stale session records and retry them forever:
a session may have been destroyed or moved since the last worker observation.
Keep only derived worker facts in the daemon; they are rebuilt when workers
reattach. Continue to use each coordinator's existing admission gate, deadlines,
and retry policy. This preserves atomic upgrade admission and avoids doing any
filesystem or process work on the daemon event loop.

Do not restore redundant UI publications as a heartbeat. Background scheduling
and visible state changes are separate concerns. The periodic observation only
queues work; supervised background tasks still perform checkpointing and worker
replacement.

## Outcomes & Retrospective

Implementation and local validation are complete. Quiet sessions now retry
background work without needing another user prompt or worker event. The
checkpoint crash regression and browser convergence both pass. Windows
compilation will be confirmed by the standard remote CI before tagging. The ACP issue preceding this
release work is committed as `a05e5f5f`; its real-harness acceptance evidence is
in `.agents/docs/acp-real-harness-validation.md`. Latest master was merged before
these CI repairs.

## Context and Orientation

The daemon is Mjolnir's control process. `RuntimeState` in
`mj-controller/src/daemon.rs` holds its current session records and derived
worker views. `publish_session` in `mj-controller/src/daemon/snapshot.rs` sends
observations to `RecoveryCoordinator` in `mj-controller/src/recovery.rs` and
`WorkerUpgradeCoordinator` in `mj-controller/src/worker_upgrade.rs`. The shared
gate is implemented in `mj-controller/src/recovery_gate.rs`. It prevents a
checkpoint, worker replacement, and foreground lifecycle work from touching
the same worker simultaneously.

`mj-controller/src/daemon/process.rs` runs the daemon event loop and already
polls background results. Its worker views come from session actors which
deduplicate unchanged views. A policy observation is a small description of
current session activity, not a command to perform a checkpoint immediately.

## Plan of Work

First build the CLI and portable worker with `test-hooks`. These hooks pause a
test process at named durability boundaries so the isolated test can kill it
and verify restart. Add temporary debug logging in the recovery coordinator to
distinguish ineligibility from contention, then reproduce the existing failure.

Store compact derived background-policy facts alongside the daemon's worker
view cache. Update them whenever a worker view changes. Extract the existing
observation construction so both a new view and a periodic daemon tick can use
it. Every periodic pass must use current controller records and configuration,
drop entries for removed or non-live sessions, and refuse disconnected workers.
The pass must do no filesystem reads, network calls, or subprocess work.

Add a daemon regression which observes a completed idle turn, reevaluates it
without another worker event, and verifies that eligible work is offered again.
Also remove or retire the session and prove it no longer produces work. Keep
the existing coordinator policy and gate tests. Remove diagnostic-only logging
once the cause is demonstrated.

## Concrete Steps

Run all commands from `/home/ryan/code/mjolnir`. Cargo tests require execution
outside the restricted sandbox. All application runs use the existing isolated
test harnesses with fresh configuration and data directories and a named test
instance selected through `MJ_INSTANCE`.

    cargo build --bin mj --features test-hooks
    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker --features test-hooks
    MJ_INSTANCE=ci-repair-checkpoint MJ_CHAOS_ISOLATED=1 tests/e2e/run-test-hook-chaos.sh ./target/debug/mj --hook checkpoint_archive_before_database_publication --seed 700006
    MJ_INSTANCE=ci-repair-browser tests/e2e/run-browser-reliability.sh --seed 700001 ./target/debug/mj
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Then run the complete named crash-boundary matrix, commit only changed files,
and follow `RELEASING.md`. Push authorization comes from the user's explicit
release request. Use the current branch; do not create or switch branches.

## Validation and Acceptance

The checkpoint scenario must reach its named hook, kill and restart the daemon,
and retain exactly one copy of the acknowledged prompt. It must end with
`leaks=0`. Browser convergence must create a local session without a bundle and
finish the subsequent browser/TUI assertions. The full Rust suite, Clippy, and
format checks must pass. The exact versioned commit must have green CI before
it is tagged, and failed scheduled reliability jobs must pass on the repaired
commit as well.

## Idempotence and Recovery

Each test harness creates disposable roots and cleans up its own processes.
Retain failure artifacts under `target/reliability-artifacts`. Never target the
default instance or live user store. If a tool-server restart interrupts a
supervisor, inspect surviving processes before repeating or cleaning up work.
Do not move a published release tag; follow the documented workflow recovery.

## Artifacts and Notes

Downloaded evidence is in `/tmp/mj-master-reliability-36227014873`. Diagnostic
build logs are `target/release-validation/build.log` and
`target/worker/release-validation-build.log`. The failed remote job is
<https://github.com/BrokkAi/mjolnir/actions/runs/36227014873>.

## Interfaces and Dependencies

Keep `RecoveryObservation`, `WorkerUpgradeObservation`, and the existing gate
interfaces. The new cache and observation helper are private daemon details;
no database migration or wire-format change is required. Use Tokio's existing
interval/select pattern with missed-tick behavior set to delay so a slow loop
does not accumulate a burst of maintenance passes.

Plan created after identifying lost idle observations as the likely shared
cause of the durability timeout and missing delayed retries.

Reproduction confirmed the gate race at 08:56 UTC: the completed turn was due
on every observation through 08:56:16.226, with the shared gate busy. The
worker check finished at 08:56:16.851 and no further view arrived. The named
checkpoint hook timed out. Evidence is under
`target/reliability-artifacts/test-hook-checkpoint_archive_before_database_publication-seed-700006-1031822`.
The temporary diagnostics were removed after confirmation.

The first local rerun exhausted the shared `/tmp` tmpfs. Subsequent isolated
tests use `TMPDIR=/tmp/mjv`, a short symlink to `target/test-scratch`, preserving
Unix-socket path limits while putting fixture data on the workspace disk.
Browser creation also needs to register the directory through the existing
quick-bundle helper after validation, since stored sessions require a valid
bundle ID. The action regression covers both steps and their ordering.

Local repaired durability matrix: all six named boundaries passed, including
`checkpoint_archive_before_database_publication`, with `leaks=0` in every case.
Artifacts end in seed-700001 through seed-700006, process suffix `1044897`.
The browser/TUI convergence scenario also passed (`clients=2`, `sse_reconnect=1`,
`leaks=0`), followed by all 113 deterministic browser checks. Its artifacts are
`target/reliability-artifacts/browser-tui-convergence-seed-700001-1045066`.
The full `cargo test` run and `cargo clippy --all-targets -- -D warnings` both
passed in the dev profile; `cargo fmt --all -- --check` also passed. Cargo
temporary fixtures used `/var/tmp`, outside the Git checkout: the earlier
disk-backed root inside `target` invalidated two non-Git-directory fixtures
because Git discovered the enclosing checkout. That environmental run is
retained as `target/release-validation/cargo-test-invalid-temp-root.log`.
Passing logs are `cargo-test.log` and `clippy.log` in the same directory.

The prepared 2.22.0 commit's remote checks exposed two previously masked
test defects. Windows progressed past the worker helper, then found a shell
fixture test with no Unix guard; that guard was duplicated on the preceding
test. Move the duplicate to the intended shell test. The remote crash matrix
passed all six durability hooks, then its topology script killed the second
bridge during ACP initialization because a PID and restart marker do not
prove readiness. Wait for each generation's reported `acp_ready` and native
session before injecting the next death. The corrected topology script passes
all five generations and the supervisor lease check locally; its evidence is
`target/reliability-artifacts/worker-topology-fixed`. Remote browser/TUI
convergence passed on the prepared commit. These corrections change test
compilation and timing only, not application behavior.

Follow-up validation passed: full `cargo test`, all-target Clippy with denied
warnings, Rust formatting, shell syntax, and the isolated `active-stop`
scenario (`leaks=0`). Logs are `cargo-test-windows-followup.log`,
`clippy-windows-followup.log`, `topology-fixed.log`, and `active-stop-fixed.log`
in `target/release-validation`. The active-stop run used matching 2.22.0
host binaries after its initial setup correctly rejected an older 2.21.0
portable worker; that setup failure made no application assertion.

With the shell fixture compiling, Windows then reported three test helpers
whose only callers are Unix-only. Matching guards on those helpers preserve
all supported tests. Full Cargo tests and all-target Clippy passed again; see
`cargo-test-helper-guards.log` and `clippy-helper-guards.log`. The corrected
remote crash matrix, browser convergence, and highly parallel suite are green
in reliability run 36232667089.
