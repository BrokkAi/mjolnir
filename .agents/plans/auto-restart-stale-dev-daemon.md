# Automatically Replace a Stale Development Daemon

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Running `scripts/run.sh` rebuilds both the native `mj` client and its portable Linux worker, but an already-running daemon can continue serving the new dashboard from an older executable. The old daemon then resolves workers relative to its own build directory and can report that no worker exists even though the wrapper just built one. After this change, a client launched through `scripts/run.sh` detects once, before its first daemon connection, whether the registered daemon is running the exact native executable Cargo just selected. If not, it gracefully replaces that daemon and continues without requiring `scripts/run.sh -- daemon restart`.

## Progress

- [x] (2026-09-04 17:44Z) Confirmed the failure mechanism from the live processes and files: the serving daemon retained an old NFS inode under a `.nfs*` name while the current native and musl executables existed at their normal paths.
- [x] (2026-09-04 17:44Z) Read the required Morannon NFS recovery runbook and left mounts, NFS configuration, and build-cache placement unchanged.
- [x] (2026-09-04 17:49Z) Added one-shot stale-executable detection to the daemon connection path, opted into it from `scripts/run.sh`, and kept the private switch out of the replacement daemon's environment.
- [x] (2026-09-04 17:50Z) Added behavior-focused tests for same-executable and replaced-executable identity, including the NFS-style renamed-open-file shape and a live `/proc` self check.
- [x] (2026-09-04 17:57Z) Passed the focused daemon tests, an isolated stale-daemon replacement exercise, the full workspace suite, clippy, formatting, shell syntax, and the portable musl build; reviewed the final diff for the three plan-owned files before commit.

## Surprises & Discoveries

- Observation: The worker was built successfully in the Morannon Cargo target directory, but the daemon could not find it.
  Evidence: The current files were `.../debug/mj` and `.../x86_64-unknown-linux-musl/debug/mj`, both timestamped 12:31, while daemon PID 1836777 resolved through `/proc/1836777/exe` to `.../debug/.nfs0000000001dc8010000137d3`, the executable it opened at 09:49.

- Observation: NFS preserves an unlinked executable by renaming it to `.nfs*`, rather than exposing Linux's usual ` (deleted)` suffix.
  Evidence: `/proc/1836777/exe` followed an existing `.nfs*` file, so the current replaced-binary check considered the old executable present and worker sibling lookup used `.nfs*` as the worker name.

- Observation: The old development build used for the isolated exercise had a pre-existing rustls provider panic in its phone task during shutdown, but the management stop still terminated it and allowed immediate replacement.
  Evidence: The stale daemon printed the known `CryptoProvider` panic and exited nonzero; the invoking current client printed the stale-build restart diagnostic, returned a valid empty recovery scan, and published replacement PID 3642418. This feature does not alter that unrelated old-build behavior.

## Decision Log

- Decision: Compare Linux executable file identity, meaning filesystem device and inode, rather than paths, timestamps, package versions, or hashes.
  Rationale: A running NFS executable can acquire a `.nfs*` path and a rebuilt executable can reuse the original path. Device and inode distinguish those two open files cheaply and correctly without hashing a roughly 280 MB debug binary on every client start. Hard links to the same executable remain correctly classified as current.
  Date/Author: 2026-09-04 / Codex

- Decision: Make stale-daemon replacement an explicit development-wrapper policy and perform the check at most once per client process.
  Rationale: Released clients from different installations or worktrees should not fight over a same-protocol daemon merely because their executable files differ. `scripts/run.sh` establishes that its newly built executable is authoritative, while a process-local asynchronous one-shot prevents dashboard pollers from repeatedly touching `/proc` or racing the initial replacement.
  Date/Author: 2026-09-04 / Codex

- Decision: Reuse the frozen management protocol and existing graceful daemon replacement rather than adding fields to daemon metadata.
  Rationale: Adding executable identity to the metadata would be rejected by older clients because that JSON structure denies unknown fields. The management status/stop wire format is deliberately stable across protocol versions, and `/proc/<pid>/exe` already supplies the identity needed on Linux without a schema change.
  Date/Author: 2026-09-04 / Codex

## Outcomes & Retrospective

`scripts/run.sh` now makes its freshly selected native executable authoritative for the first daemon-backed operation. A daemon running another inode, including an NFS-retained `.nfs*` file, is stopped through the stable management protocol and replaced; an already-current daemon is left alone. The opt-in check runs once per client process and is removed from the replacement daemon's environment, so daemon children and ordinary released invocations retain their prior behavior. The implementation changed no wire or metadata shape and passed native and musl builds, all tests, clippy, and an isolated live replacement exercise.

## Context and Orientation

`scripts/run.sh` is the development entry point. It builds `mj` for the native host through `cargo run` and separately builds the static musl target used as the worker inside containers and remote Linux targets. The daemon is a persistent controller process implemented in `mj-cli/src/daemon.rs`; dashboards are short-lived clients that call `daemon::connect_or_start`. A daemon remains alive after a dashboard exits, and session workers remain alive across a graceful daemon restart.

The existing `connect_or_start` function reads owner-only daemon metadata, gracefully replaces a daemon when its protocol number differs, otherwise connects to it, and starts a daemon from the current executable if none answers. It deliberately does not distinguish two development builds with the same package and protocol versions. On Linux, `/proc/<pid>/exe` is a kernel-provided reference to the exact executable file held open by process `<pid>`. Comparing its device and inode to `/proc/self/exe` determines whether two processes execute the same file even when NFS has renamed one open file.

The opt-in must be private to the development wrapper. `scripts/run.sh` will export a narrowly named environment switch before executing Cargo. `connect_or_start` will consult that switch and run a process-wide asynchronous one-shot before its normal protocol check. The one-shot must either establish that the daemon is current, gracefully replace a stale daemon, or leave an already-dead daemon for the normal start path. An unexpected permission or metadata error must be reported with the daemon PID and path context rather than silently ignored.

## Plan of Work

First, add a small Linux-only executable identity helper near daemon process management in `mj-cli/src/daemon.rs`. It will stat `/proc/self/exe` and `/proc/<daemon-pid>/exe` and compare `std::os::unix::fs::MetadataExt::dev` and `ino`. A missing daemon process is not a stale executable: normal connection recovery will handle it. Non-Linux builds will leave this development-only policy inactive because `scripts/run.sh` exists to build Linux workers and the incident is specifically Linux/NFS executable replacement.

Second, add a static `tokio::sync::OnceCell` guarding a `maybe_replace_stale_development_daemon` function. When the wrapper's environment switch is absent, it returns immediately. When present, it reads daemon metadata, checks executable identity, and reuses the existing graceful management stop-and-wait replacement if the inode differs. Rename the replacement helper if necessary so its name describes both incompatible-protocol and stale-executable callers. Make disappearance during the check safe: if another client already stopped the recorded daemon, the normal connection/start loop should proceed instead of producing a false failure.

Third, update `scripts/run.sh` to export the switch for the native `cargo run`. Retain its musl build and profile selection. Update the header comment so it promises both worker rebuilding and replacement of a stale daemon, and states that active detached workers survive the graceful daemon replacement.

Fourth, add colocated tests in `mj-cli/src/daemon.rs`. Use temporary small files to prove that a hard link is the same executable identity, then rename the old file to an NFS-style `.nfs*` name, create a replacement at the original path, and prove that the identities differ. Exercise the replacement decision independently of process-global environment where practical. Preserve the frozen cross-version management-wire tests unchanged.

Finally, format and run the focused daemon tests, then run the repository-required full test and clippy commands outside the restricted sandbox. Inspect the final diff, record exact evidence here, and commit the implementation and plan on the current branch.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel2`.

After editing, run:

    cargo fmt --all -- --check
    cargo test -p brokk-mjolnir daemon
    cargo test
    cargo clippy --all-targets -- -D warnings

Every `cargo test` command must run with elevated permissions because the suite uses loopback TCP and Unix sockets. Expected results are exit status zero and no clippy warnings.

Inspect before committing:

    git status --short
    git diff --check
    git diff -- .agents/plans/auto-restart-stale-dev-daemon.md scripts/run.sh mj-cli/src/daemon.rs

Then stage only those files and commit them on the current branch.

## Validation and Acceptance

The identity test must prove three behaviors: two names for one inode are current; replacing the canonical path creates a different identity; and retaining the old inode under a `.nfs*` name does not confuse the comparison. Existing protocol-mismatch and cross-version management tests must still pass.

For an end-to-end manual check, start the development daemon through `scripts/run.sh`, leave a session worker active, rebuild or replace the native `mj`, and run `scripts/run.sh` again. The old daemon PID must exit gracefully, a new daemon from the just-built native executable must appear, the existing worker must remain reconnectable, and creating a container session must select the existing `target/x86_64-unknown-linux-musl/debug/mj` (or the equivalent configured Cargo target directory) instead of reporting `no Linux worker`.

A second `scripts/run.sh` invocation without rebuilding the native executable must leave the daemon PID unchanged. Invoking a released `mj` without the development switch must retain current behavior and must not replace a same-protocol daemon merely because it comes from another file.

## Idempotence and Recovery

The source edits and tests are safe to repeat. The replacement uses the existing graceful stop path, whose shutdown is bounded and leaves detached workers active. If the recorded daemon disappears between identity inspection and stop, retry the normal connection path rather than signalling an unverified replacement PID. Do not delete `.nfs*` files: NFS removes them after the processes holding them open exit. Do not alter Morannon mounts or NFS settings for this feature.

## Artifacts and Notes

The failure evidence that motivated the feature is:

    daemon PID: 1836777
    daemon executable: .../debug/.nfs0000000001dc8010000137d3
    current native executable: .../debug/mj
    current worker: .../x86_64-unknown-linux-musl/debug/mj

The desired wrapper behavior is a single concise diagnostic when replacement occurs, followed by the normal dashboard startup. No diagnostic is needed when the daemon already uses the current executable.

Validation evidence:

    cargo test -p brokk-mjolnir daemon::tests::
    test result: ok. 26 passed; 0 failed

    isolated stale daemon PID 3641517
    Mjolnir daemon 3641517 is using an older development build; restarting it.
    replacement daemon PID 3642418
    second opted-in connection: no restart diagnostic; PID unchanged

    cargo test
    test result: ok across every workspace test binary
    core: 736 passed, 4 ignored
    TUI: 280 passed, 2 ignored
    worker: 90 passed, 1 ignored
    CLI: 131 passed

    cargo clippy --all-targets -- -D warnings
    Finished `dev` profile; exit status 0

    cargo build --target x86_64-unknown-linux-musl -p brokk-mjolnir --bin mj
    Finished `dev` profile; exit status 0

## Interfaces and Dependencies

No new crate dependency or public configuration is needed. `mj-cli/src/daemon.rs` will keep `connect_or_start() -> Result<DaemonClient>` as its callers' interface. It will gain an internal process-wide asynchronous one-shot and Linux device/inode comparison. `scripts/run.sh` will be the sole producer of the private development environment switch. Existing `ManagementClient::stop_and_wait`, the frozen `DaemonAction::Stop`, and detached daemon spawning remain the restart mechanism.

Revision note (2026-09-04 17:44Z): Created this plan after confirming that `scripts/run.sh` built the correct musl worker but a persistent daemon continued from an older NFS-retained executable inode.

Revision note (2026-09-04 17:57Z): Recorded the completed one-shot executable-identity design, race handling, isolated live replacement result, and full native and musl validation before committing the implementation.
