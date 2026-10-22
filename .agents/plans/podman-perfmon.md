# Enable perf in Podman session containers

This ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries,
decisions, and outcomes current.

## Purpose / Big Picture

New local and SSH Podman sessions should run `perf stat` and `perf record`
without sudo inside the container. The user requested a hardcoded PERFMON
capability, verified for user jonathan on morannon, followed by push and
installation. Container replacement must retain this behavior.

## Progress

- [x] (2026-10-21) Inspect configuration, launch commands, container replacement,
  build/install instructions, and morannon's existing permissions.
- [x] (2026-10-21) Add the Podman capability to provisioning and replacement,
  accept only managed capabilities during replacement inspection, and update
  container documentation and integration regressions.
- [x] (2026-10-21) Pass the affected launch-argument regression and fast-forward
  master to 35236ab37, which includes another session's already-installed work.
- [ ] Correct morannon's user-scoped seccomp policy and host perf setting
  (blocked by automatic approval review; explicit user approval requested).
- [x] (2026-10-21) Run the generated SSH launch and perf integration test
  against morannon; it reaches perf and confirms the host permission failure.
- [ ] Pass the morannon perf integration test after approved host correction.
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
policy and user-default seccomp changes. The user received the precise changes
and scope in an asynchronous approval question. Do not retry the mutation or
work around the rejection without that approval. Repository validation, push,
and installation remain independently authorized.

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

## Outcomes & Retrospective

The code adds PERFMON to Podman provisioning and replacement through one
managed-capability list. The real replacement regression passes and confirms
capability preservation across failure and recovery. Workspace Clippy passes.
The full controller run completed with 2033 passes, 18 ignored, and two daemon
timing failures; each failed test passed when rerun individually. Morannon's
new real perf integration test launches and cleans up its container correctly,
then fails with `No permission to enable task-clock event` under the unchanged
host policy. Runtime prerequisite changes remain blocked on explicit approval.
Commit 3792baaca was pushed and installed with both matching x86_64 Linux
workers. A normal `mj sessions --json` request coordinated the daemon handoff;
`mj daemon status` subsequently reported the same build after 91 seconds of
uptime. No manual daemon restart or host security-policy mutation was used.
The code and installation are delivered; successful profiling on morannon is
still outstanding until the pending host-settings approval is answered.

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
