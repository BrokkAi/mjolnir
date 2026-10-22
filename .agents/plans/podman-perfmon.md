# Enable perf in Podman session containers

This ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries,
decisions, and outcomes current.

## Purpose / Big Picture

New local and SSH Podman sessions should run `perf stat` and `perf record`
without sudo inside the container. The user requested a hardcoded PERFMON
capability, verified for user jonathan on morannon, followed by push and
installation. Container replacement must retain this behavior.
The user subsequently requested restarting session
`d8443f17cb24506c5ee26277fcf319f4` and telling it that perf is available.

## Progress

- [x] (2026-10-21) Inspect configuration, launch commands, container replacement,
  build/install instructions, and morannon's existing permissions.
- [x] (2026-10-21) Add the Podman capability to provisioning and replacement,
  accept only managed capabilities during replacement inspection, and update
  container documentation and integration regressions.
- [x] (2026-10-21) Pass the affected launch-argument regression and fast-forward
  master to 35236ab37, which includes another session's already-installed work.
- [x] (2026-10-22) Apply the user's explicitly approved persistent host perf
  setting and user-scoped seccomp correction. Verify boot-time sysctl
  precedence, persistent paths, and preservation of other syscall rules.
- [x] (2026-10-22) Run fresh rootless containers: ordinary perf stat, perf
  record, and perf script succeed with PERFMON and seccomp enabled; perf
  remains denied without PERFMON.
- [x] (2026-10-21) Run the generated SSH launch and perf integration test
  against morannon; it reaches perf and confirms the host permission failure.
- [x] (2026-10-22) Pass the generated morannon perf integration test after
  approved host correction (4.61 seconds, seccomp enabled).
- [x] (2026-10-22) Snapshot the requested session's writable layer, then
  complete its explicit storage-preserving upgrade with CAP_PERFMON.
- [x] (2026-10-22) Verify perf in the actual replacement container, deliver
  the requested notification, and observe the session independently confirm
  that perf stat, perf record, and perf script produce samples.
- [x] (2026-10-21) Pass the real storage-preserving replacement regression.
- [x] (2026-10-21) Run the full controller suite and pass workspace Clippy,
  formatting and diff checks. Two timing failures passed individual reruns.
- [x] (2026-10-22 04:03Z) Commit 3792baaca on master, push upstream, install
  through scripts/install.sh, and verify normal daemon handoff. Daemon 3893067
  reports this build and remained up for more than 90 seconds.

## Surprises & Discoveries

Morannon runs Podman 5.7.0 rootless as jonathan, uid 1000. Its host setting
`kernel.perf_event_paranoid=2` permits user-space-only events but refuses
ordinary perf defaults. `/etc/sysctl.d/60-perf.conf` persists that setting.

Its packaged `/usr/share/containers/seccomp.json` has an allow rule for
`perf_event_open` when CAP_PERFMON is present, but two overlapping deny rules
do not exclude CAP_PERFMON. A disposable rootless container with PERFMON
cannot even record `task-clock:u`. A diagnostic with seccomp disabled can,
proving seccomp is an independent blocker. The permanent correction must
retain seccomp rather than disable it. No user or system containers.conf
exists at initial inspection.

Automatic approval review rejected the exact prepared persistent host changes:
the original request was judged insufficient authorization for host-wide perf
policy and user-default seccomp changes. The user then explicitly approved
persisting `kernel.perf_event_paranoid=1` and correcting their user's seccomp
profile, with a requirement that both survive reboot. That approval authorized
the host changes applied in the follow-up turn.

## Decision Log

- Decision: Apply PERFMON to Podman only, through one capability list used
  by provisioning, replacement, and replacement inspection.
  Rationale: The requested runtime is Podman. Replacements must recognize
  managed capabilities without accepting arbitrary privilege overrides.
  Date/Author: 2026-10-21, Codex.
- Decision: Correct jonathan's seccomp profile on morannon and persist host
  perf_event_paranoid=1.
  Rationale: A rootless capability cannot override host perf permissions.
  Keep every other seccomp restriction and continue excluding system-wide
  profiling through the host's per-process policy.
  Date/Author: 2026-10-21, Codex.
- Decision: Use the supported explicit container upgrade for the requested
  session, with a local snapshot of its writable layer first.
  Rationale: Ordinary worker Restart retains its container and cannot add a
  capability. The actual workspace and caches are independently mounted;
  the snapshot also preserves manually installed packages and layer files.
  Date/Author: 2026-10-22, Codex.

## Outcomes & Retrospective

The code adds PERFMON to Podman provisioning and replacement through one
managed-capability list. The real replacement regression passes and confirms
capability preservation across failure and recovery. Workspace Clippy passes.
The full controller run completed with 2033 passes, 18 ignored, and two daemon
timing failures; each failed test passed when rerun individually. Morannon's
new real perf integration test launches and cleans up its container correctly,
then fails with `No permission to enable task-clock event` under the unchanged
host policy. After explicit user approval, `/etc/sysctl.d/60-perf.conf` now
persists level 1, and `/home/jonathan/.config/containers/containers.conf`
selects `/home/jonathan/.config/containers/seccomp-perfmon.json`. The profile
splits perf_event_open out of grouped deny rules and excludes CAP_PERFMON
from both relevant denies. Other syscall rules are identical to the packaged
profile, and seccomp remains enabled. Boot-time configuration resolves to
the sole perf assignment of 1 through the active systemd-sysctl service.
Fresh rootless containers now complete perf stat, perf record, and perf script;
one smoke recorded 452 lines of sample output. The no-PERFMON control still
fails with the original permission error. The generated mj launch integration
test now passes against morannon with seccomp enabled.
Commit 3792baaca was pushed and installed with both matching x86_64 Linux
workers. A normal `mj sessions --json` request coordinated the daemon handoff;
`mj daemon status` subsequently reported the same build after 91 seconds of
uptime. No manual daemon restart was needed. Code and installation are
delivered and the approved host correction is persistent. Session
`d8443f17cb24506c5ee26277fcf319f4` completed the supported explicit upgrade
from agent-dev:2.52.0 to the cached agent-dev:latest. Its replacement has
CAP_PERFMON and retains the original workspace and temporary volumes and
cache mounts. Its previous writable layer is preserved as local image
`localhost/mj-session-backup/d8443f17cb24:pre-perfmon-20261022T045708Z`
(image ID fd69e5385b71c9eec3ee07b509256f12060922b676702adb8b611818694b2339).
Ordinary perf stat, perf record, and perf script succeeded inside this exact
replacement with seccomp mode 2, producing 439 sample-output lines. The
notification is delivered in transcript position 256520; the session then
independently verified profiling and confirmed success at position 256557.
All requested work is complete. No reboot was performed; persistence was
verified through the effective boot sysctl configuration and Podman's durable
per-user configuration loaded by fresh containers.

## Context and Orientation

`mj-controller/src/targets/container.rs` builds Podman launch arguments.
`container_upgrade.rs` inspects existing containers and starts replacements
on their retained volumes. Its previous empty-CapAdd requirement would reject
all newly created PERFMON containers, so both paths must agree.
`mj-controller/src/targets/tests.rs` holds real engine tests; the new ignored
test accepts `MJ_PERF_TEST_HOST=morannon` to exercise the same generated SSH
launch command as normal sessions. It records samples and reads them back.
`docs/src/content/docs/containers.md` documents the prerequisites.

## Plan of Work

Use a single `container_capabilities(engine)` list to supply PERFMON to Podman
launches and replacements. Inspection accepts that list, with or without
Podman's CAP_ prefix, and continues rejecting other overrides. Extend the
existing real replacement regression to observe capabilities before and
after replacement. Add a real perf integration test with explicit cleanup.

On morannon, copy the packaged seccomp profile into jonathan's containers
configuration directory. Split perf_event_open out of grouped deny rules
and add CAP_PERFMON to their capability exclusions, preserving all other
rules. Configure that profile through jonathan's containers.conf. Change
the existing perf sysctl file from 2 to 1 and apply that single setting.

## Concrete Steps

From the repository root, run Cargo tests outside the restricted sandbox:

    MJ_INSTANCE=perfmon-regression MJ_PERF_TEST_HOST=morannon cargo test -p brokk-mj-controller podman_session_container_can_record_perf_samples -- --ignored --nocapture
    MJ_INSTANCE=container-upgrade-regression cargo test -p brokk-mj-controller replacement_recovery_reattaches_the_same_storage_without_a_checkpoint -- --ignored --nocapture
    MJ_INSTANCE=perfmon-regression cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all --check
    git diff --check

Use existing mbx-managed Cargo output without changing target directories.
Commit only task files on the current master branch. Push to origin/master;
if it moved, fetch and merge, never rebase. Compare the installed binary's
build with HEAD before `scripts/install.sh` to avoid overwriting newer work.
Do not change HEAD during the install build. Verify the installed version and
daemon metadata after ordinary startup coordinates the handoff.

## Validation and Acceptance

The SSH integration test must launch a real disposable container on morannon,
complete unqualified `perf stat`, record nonempty samples with `perf record`,
and read them with `perf script`. The replacement regression must preserve
the capability and existing workspace and cache storage across its injected
failure and retry. The controller suite and Clippy must pass in dev profile.
Installation must leave the default daemon healthy with the new build.

## Idempotence and Recovery

Tests create unique containers and remove only their own resources. Every mj
test uses a named instance; no test build operates on the default store.
Check for concurrent host configuration changes before writing. Restore the
previous perf level of 2 and remove only the created user configuration files
if the host correction cannot be completed safely. Never touch live sessions
to change capabilities: existing containers acquire the new launch policy
only when recreated explicitly.

## Artifacts and Notes

Initial morannon probes returned `No permission to enable task-clock event`
both with and without PERFMON. With PERFMON and diagnostic seccomp bypass,
`perf stat -e task-clock:u -- true` succeeded. The final solution must succeed
with seccomp enabled and ordinary perf events.

## Interfaces and Dependencies

No public configuration, store schema, protocol, or dependency changes.
`container_capabilities(&str) -> &'static [&'static str]` is private to the
targets module. Host dependencies are Podman, OpenSSH, and the image's perf.

Initial plan recorded after confirming both independent host blockers.
Updated with the initial regression result, upstream synchronization, and the
explicit approval boundary imposed by automatic review.
Updated after the real upgrade regression and required validation completed;
recorded the two timing-sensitive tests' successful individual reruns.
Recorded the actual generated SSH integration test's confirmed host-policy
failure; no host security settings have been changed.
Recorded the pushed implementation, completed installation, and stable normal
daemon handoff. This final planning-only update does not change installed code.
Updated after explicit approval: recorded the persistent host settings,
boot-path verification, successful profiling smoke, and unchanged syscall
restrictions outside perf_event_open.
Recorded successful generated-launch verification and the user's additional
request to restart and notify the specified live session.
Recorded the completed upgrade, preserved layer snapshot, successful exact-
container verification, and delivered notification with independent confirmation.
