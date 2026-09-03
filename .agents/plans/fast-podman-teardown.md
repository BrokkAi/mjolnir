# Make Podman session teardown fast and observable

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This document is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Stopping a Podman-backed session currently blocks until Podman deletes the container's writable overlay. Cargo workspaces can contain millions of files, making that deletion take more than a minute even though the agent and relay are already stopped. After this change, every new Podman session stores its complete `/workspace` in its own Podman named volume by default, the controller marks the session stopped as soon as its verified checkpoint exists and the container is confirmed not running, and container/storage deletion continues in a supervised daemon task with visible progress and failure reporting. Ordinary rootless Podman hosts require no helper. Operators who want native host storage such as one ZFS dataset per session may opt into a generic helper protocol; the repository contains a safe example script but does not install it.

## Progress

- [x] (2026-09-03 18:00Z) Reproduced and attributed the original delay to synchronous `podman rm` of a Cargo-heavy writable overlay.
- [x] (2026-09-03 18:00Z) Chose one full-workspace Podman named volume per session as the portable default and a generic host-helper override for ZFS.
- [x] (2026-09-03 21:10Z) Added version-2 configuration, runtime/durable locators, and schema-22 persistence for Podman workspace storage.
- [x] (2026-09-03 21:35Z) Provisioned isolated storage for local and SSH Podman targets, including exact identity validation and failed-launch rollback.
- [x] (2026-09-03 22:35Z) Split Podman quiescence from supervised asynchronous storage cleanup, including restart recovery, visible stages, and bounded shutdown draining.
- [x] (2026-09-03 21:42Z) Added the optional root-owned ZFS helper example and target planning coverage.
- [ ] Run focused tests, the full test suite, and clippy; commit coherent validated checkpoints.

## Surprises & Discoveries

- Observation: The measured stop spent about 68 of 76 seconds after the container process died, inside Podman storage removal.
  Evidence: Podman events for session `91fc44af…` showed `died` at 17:13:20Z and `remove` at 17:14:28Z.
- Observation: Public container configuration has no arbitrary run-argument escape hatch, although the internal runtime template does.
  Evidence: `src/hel_config.rs` exposes image, pull policy, platform, CPU, memory, and environment; `src/hel_targets.rs` alone has `extra_run_args`.
- Observation: Podman targets always use `/workspace`, so mounting the whole directory isolates repository-local Cargo targets without introducing `CARGO_TARGET_DIR`.
  Evidence: `workspace_for`, clone planning, worker startup, checkpoint export, and restore all use the same container workspace constant.
- Observation: An mbx build that overlaps source edits can leave stale metadata newer than the edited source even though mbx refuses to cache it.
  Evidence: the core compile began at 15:26:26, source changed at 15:27, and stale `.rmeta` landed at 15:28; reported upstream as mr-boxington discussion 325. This was a build-tool diagnosis only and did not change the storage design.
- Observation: Podman 4.0 supports the `U` volume option but does not advertise `nocopy`, which appears in current Podman documentation.
  Evidence: the versioned 4.0 `podman-run(1)` option list includes `U` but not `[no]copy`; the fresh named volume needs no copy suppression.

## Decision Log

- Decision: Default new local and SSH Podman sessions to a named volume mounted at `/workspace`.
  Rationale: Named volumes need no host privilege or helper, keep the large file tree out of the container overlay, and isolate concurrent sessions.
  Date/Author: 2026-09-03 / Sol and Jonathan
- Decision: Mark a session stopped after a verified checkpoint and confirmed target death, then remove storage asynchronously.
  Rationale: Recoverability and the absence of a live target define a stopped session; slow unlink work is cleanup, not quiescence.
  Date/Author: 2026-09-03 / Sol and Jonathan
- Decision: Keep native ZFS support behind a generic helper command and place a reference shell implementation in `examples/`.
  Rationale: Mj remains host-filesystem agnostic, no setuid component is introduced, and Morannon-specific setup stays optional and reproducible.
  Date/Author: 2026-09-03 / Sol and Jonathan
- Decision: Do not include reflink cloning, tmpfs, a shared Cargo target, or Docker storage changes.
  Rationale: These do not solve the synchronous Podman teardown boundary and were explicitly removed from scope.
  Date/Author: 2026-09-03 / Sol and Jonathan

## Outcomes & Retrospective

Configuration, persistence, provisioning, rollback, decomposed cleanup plans, the daemon handoff, and the example helper are implemented. Focused configuration, database, target-planning, controller-lifecycle, and daemon tests pass. Full repository validation and a host-level Podman smoke check remain.

## Context and Orientation

`src/hel_config.rs` defines user-facing TOML target templates. `src/hel_targets.rs` turns those templates into commands and locators for provision, reconnect, and close. `src/hel_state.rs` plus `src/hel_database.rs` persist the live target locator. `mj-controller/src/hel_controller/lifecycle.rs` enforces checkpoint-before-destroy semantics. `mj-cli/src/daemon.rs` supervises lifecycle operations and publishes runtime snapshots consumed by the dashboard.

A quiesced target is a container that is absent or confirmed not running. Deferred cleanup means removing the stopped container object, its workspace volume or host-managed directory, and its Git cache after the durable session has become `Stopped`.

## Plan of Work

Add a user-facing `PodmanWorkspaceStorage` enum with `podman-volume` as the default, `host-helper { root, helper }`, and explicit `container-layer`. It belongs only to local and SSH Podman target variants. Raise config version to 2 while accepting version 1 and upgrading it in memory. Add a matching runtime storage specification and a durable locator describing the actual volume name or host path. Raise the database schema to 22 and store the locator in a nullable JSON column; null decodes as the legacy container-layer mode.

For default provisioning, derive `<container-resource-name>-workspace`, create it explicitly with Mj ownership/session labels, and mount it at `/workspace` with `rw,U`. Wrap storage creation and `podman run` in one guarded target-creation command so existing provisioning rollback semantics remain valid; if `podman run` fails, remove the newly created empty storage before returning failure. Do the equivalent for the host-helper mode. Never fall back silently to the container layer.

Split target close planning into synchronous quiescence and deferred cleanup. Podman quiescence verifies ownership labels, stops the exact container immediately, and verifies it is absent or not running. After the controller persists `Stopped`, the daemon owns the cleanup plan in a supervised task keyed by session ID. Cleanup removes the container before the volume/helper storage, reports each stage and duration, and then removes Git cache state. Resume and permanent deletion wait for same-session cleanup; deterministic labels and names permit idempotent reconciliation after interruption. Non-Podman targets retain their current synchronous close behavior.

Extend daemon runtime snapshots with cleanup status and add teardown variants to stage reporting. The dashboard shows `Stopping target` before the durable transition and `Removing container storage` while deferred cleanup runs. Shutdown drains cleanup tasks for up to eight seconds under the existing ten-second watchdog and tells the caller which session/stage it is waiting for; remaining supervised process groups are cancelled and reported.

Add `examples/mj-zfs-workspace-helper.sh`. It accepts only `create`, `destroy`, or `status` plus one validated Mj resource name, derives a child dataset beneath operator-edited fixed constants, and uses absolute commands without `eval`. It documents copying the file to a root-owned `/usr/local/libexec` path and granting only that command through `sudo -n`. It is not installed or enabled automatically.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Make configuration/state changes first and run their focused tests. Then implement target planning and lifecycle supervision with focused module tests. Finally run:

    cargo test
    cargo clippy --all-targets -- -D warnings

Every `cargo test` command must run with elevated sandbox permissions because the suite uses loopback sockets. Update this plan after each validated checkpoint, stage only files changed for this task, and commit directly to the current branch without pushing.

## Validation and Acceptance

Tests must prove version-1 config migration, version-2 round trips, schema-21 migration, separate volume names for concurrent sessions, local and SSH Podman mounts, failed-launch rollback, container-before-volume deletion, target-death-before-`Stopped`, unrelated lifecycle concurrency, same-session cleanup coordination, visible cleanup failure, balanced stage notifications, and bounded informative daemon shutdown.

A manual ordinary-Podman check creates a session whose workspace contains a large Cargo-style file tree and shows that the session reaches `Stopped` before volume deletion completes. A Morannon check installs the example helper manually, selects `host-helper`, confirms two sessions receive separate datasets mounted at `/workspace`, and confirms stopping destroys only the matching dataset.

## Idempotence and Recovery

Volume and helper cleanup commands must be safe to retry and must validate Mj ownership before deletion. A failed cleanup leaves the stopped session recoverable from its verified checkpoint and exposes an actionable error. Provisioning must refuse an unlabeled or mismatched deterministic resource instead of adopting or deleting it. Existing live locators without storage metadata continue to use container-layer cleanup.

## Artifacts and Notes

The original measured close was approximately seven seconds of checkpoint work, less than one second of relay close, and sixty-eight seconds of `podman rm` storage deletion. The implementation is successful when the last phase no longer lies on the transition to `Stopped` and its progress is no longer silent.

Revision note (2026-09-03): recorded the completed storage/provisioning milestone, its passing focused tests, and the unrelated mbx stale-output diagnosis so a resumed implementation does not rediscover either state.

## Interfaces and Dependencies

No new third-party dependency is required. The public TOML shape is:

    workspace_storage = { kind = "podman-volume" }
    workspace_storage = { kind = "host-helper", root = "/absolute/root", helper = ["sudo", "-n", "/usr/local/libexec/mj-zfs-workspace-helper"] }
    workspace_storage = { kind = "container-layer" }

The helper protocol is `HELPER create RESOURCE`, `HELPER destroy RESOURCE`, and `HELPER status RESOURCE`; status writes exactly `present` or `absent`. The daemon snapshot exposes cleanup session identity, stage, start time, and optional failure. Podman remains the only runtime changed by this work.
