# Share build caches across raw and container sessions

This ExecPlan is a living document maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


Independent Mjolnir sessions building the same project should reuse completed
Go, Gradle, Turbo, Nx, and Bazel builds. C and C++ projects should use the mbx
compiler cache already supported for Cargo projects. Both raw local/SSH
sessions and Linux Podman/Docker sessions must benefit. The user explicitly
authorized testing with Podman on morannon, and committing and pushing the
completed work to the current branch's upstream.

The observable result is a cache hit when a second isolated checkout builds
unchanged inputs, a cache miss after a relevant input changes, and successful
concurrent builds with each checkout retaining its own working outputs.

## Progress


- [x] (2026-10-09) Inspected current mbx provisioning, persisted placement,
  worker launch, native tool documentation, and existing issue assignments.
- [x] (2026-10-09) Implement durable tool-cache placement and independent machine enablement.
- [x] (2026-10-09) Implement target-side discovery and tool-specific configuration, including
  C/C++ through mbx, without changing project manifests.
- [x] (2026-10-09) Add integration coverage for configuration, migrations, process execution,
  path handling, and concurrent independent checkouts.
- [x] (2026-10-09) Exercise real raw sessions and morannon Podman sessions in a named,
  isolated instance; record cache-hit and invalidation evidence.
- [x] (2026-10-09) Complete affected crate suites and workspace Clippy, review
  the final diff, and prepare the validated change for the authorized commit
  and push to master.

## Surprises & Discoveries


The existing cache admission checks only `HEAD:Cargo.toml` in the primary
repository's host mirror. It supports Linux Podman/Docker containers only and
requires native mbx before even selecting storage. The host's mbx owns its
directory and budget; the older machine directory/budget settings are inactive
when native mbx owns configuration. Native tool caches must not depend on this
Rust-specific admission path.

Go's build-action key includes the package's absolute directory unless
`-trimpath` is enabled. Sharing only `GOCACHE` is insufficient for project
packages in separate worktrees.

Gradle's task-output store can be shared, but the complete Gradle user home also
contains mutable daemon and dependency-cache state. Containers need private
working state. Nx's cache database and its version-dependent sharing rules also
need validation with real Nx before choosing an adapter.

## Decision Log


- Decision: Preserve the mbx store and introduce separate durable placement for
  native tool caches. Rationale: mbx owns its cache layout, configuration,
  scheduling, and workspace cleanup; those facts do not describe Go or task
  caches. Date: 2026-10-09.
- Decision: Configure only build tools already used by a project; do not install
  or adopt Turbo, Nx, Bazel, or a new project build system. Rationale: a task
  cache's correctness depends on the repository's own declared inputs and
  outputs. Date: 2026-10-09.
- Decision: Perform discovery against the actual checkout on the target during
  supervised preparation. Rationale: a mirror's default branch can disagree
  with the branch or recovery checkpoint a session actually builds.
  Date: 2026-10-09.
- Decision: Preserve explicit user cache settings and use only session-owned
  configuration and wrappers. Rationale: raw sessions must not rewrite the
  user's global build configuration. Date: 2026-10-09.

## Outcomes & Retrospective


Implementation and validation are complete. The generated-launcher shell
integration passes, including shell metacharacters, repeated preparation, and
preparing a reviewer checkout from the primary environment. The real-tool
suite demonstrated cache hits, invalidation and concurrent builds for all six
tool families on raw hosts and across separate containers on morannon. No live store, build layout, or
configuration has been changed.

## Context and Orientation


`mj-controller/src/controller/provisioning.rs` creates session environments and
selects mounts before a container is created. `controller/mbx.rs` and its child
modules inspect the host mbx installation, copy it into its shared store,
publish its configuration, and release retired workspaces. `controller/cache_host.rs`
owns local/SSH host command construction. `controller/worker_binary/launch.rs`
builds `WorkerLaunchConfig`; its environment reaches the harness, terminal,
and reviewer. `controller/worker_binary/install.rs` installs private launchers.

`mj-core/src/state.rs` defines persisted session data. The database stores
session placement in `mj-controller/src/database/state_io.rs`; schema changes
and their compatibility declarations live in `database/schema.rs`. A new
persisted field must be preserved across daemon replacement and checked against
older readers and writers.

The worker runs on the target and owns harness preparation.
`mj-worker/src/worker_runtime/harness_launch.rs` resolves the final login and
session environment before launching a harness. Its returned environment is
also used by terminal commands. Filesystem work belongs in supervised blocking
preparation tasks, never the event loop. Private launchers must invoke the real
tool found before their directory enters PATH, avoiding recursive wrapping.

## Plan of Work


First add independent machine-owned storage for native tool caches. Persist it
inside the existing target runtime settings JSON, alongside its fixed host
connection. Schema 79 raises the compatibility floor because old lifecycle
writers would discard the new placement field. Configuration schema 16 adds
`tools_directory`. Select a
local host path or an SSH host path, verify usable local filesystem semantics,
and mount that path read/write in supported containers. Persist the placement
so reattachment and worker replacement use the mounted path even if machine
configuration later changes. Honor the existing per-machine disabled setting
on new sessions and raw sessions. Keep mbx's existing placement and cleanup.

Then add worker-side adapters. Discover manifests in the actual repository
roots. Go receives a shared build cache and path-independent compilation flags.
Gradle receives an enabled task cache with a shared output store and private
mutable state. Turbo and Nx receive their supported cache configuration only
when their project files are present. Bazel receives a session-owned rc file
through a launcher while preserving the repository's rc files and explicit
command flags. CMake and make builds in C/C++ repositories run through `mbx
exec`, including CMake configuration so absolute compiler selections are
intercepted. Cache configuration must reach reviewer and terminal builds too.

Finally update settings and agent guidance to describe the actual scope, prove
cache reuse with real tools, and run the required Rust checks once after the
last implementation round. Test intermediate changes only with focused filters.

## Concrete Steps


Work from the repository root on the current `master` branch. Use plain Cargo
commands through the existing mbx setup; do not change Cargo target placement.
Every `cargo test` invocation runs outside the restricted sandbox. All test
daemons and CLI invocations of new binaries use `--instance tool-cache-test`
and isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. Profiles use copied homes.

Run focused integration tests as the implementation develops, followed by the
complete suites of touched crates and:

    cargo clippy --all-targets -- -D warnings

Record exact final commands and outcomes here as they run. Test fixture caches
and raw/container workspaces must be unique to this task. Do not clear any
existing host cache or stop the live daemon.

## Validation and Acceptance


For every supported adapter, build a real fixture in checkout A and then
checkout B with shared cache placement. Observe the tool's cache-hit output or
absence of compiler execution, and verify the generated output. Change an input
and verify the new output is produced. Run builds concurrently from separate
checkouts and verify neither corrupts the other's files. Include paths with
spaces and explicit user configuration in adapter integration tests.

Use actual raw sessions and morannon Podman sessions in an isolated named
instance to verify that provisioning mounts the right host directory and that
the worker, terminal commands, and harness environment receive the adapter
configuration. Verify disabled caches, missing tools, and existing Rust cache
behavior. Run isolated migration/upgrade regressions if persisted formats change.

## Idempotence and Recovery


Generated files live in each worker's private directory and are published
atomically. Cache stores remain owned and collected by their build tool.
Re-preparation can replace generated configuration without touching manifests
or user files. Stop fixture processes and their process groups before removing
fixture files. Container cleanup is restricted to this test instance's label.

## Artifacts and Notes


Current branch was clean at the start. No existing open issue matched this
feature; unrelated mbx and image-upgrade issues were left untouched. Save live
validation evidence under `.agents/docs/` when available.

## Interfaces and Dependencies


Reuse `CacheHost`, `CommandExecutor`, `targets::command_on_locator`, the shared
subprocess helpers, atomic file publication, and the existing worker launch
environment. Do not add a workspace crate. Add a small native-cache placement
type in `mj-core`, controller placement logic, and a worker adapter module.
Use the repository's established project identity for per-project task-cache
namespaces; compiler caches may share toolchain-keyed entries across projects.

Revision note (2026-10-09): Initial plan records the user's raw/container scope,
morannon test authorization, implementation boundaries, and push authorization.

Revision note (2026-10-09): Native placement fits the existing recorded target
runtime, avoiding a parallel session field and SQL column. Nx 23.2+ must share
its real ~/.nx directory; symlinks are rejected by Nx's ownership checks and
setting only NX_CACHE_DIRECTORY splits the cache from its database. The
controller mounts the host directory at the container HOME. Generated shell
startup files now have one owner, so Git and cache preparation cannot chain
hooks into a recursion cycle. Cache write grants are added only at harness
launch, keeping them out of repository review and checkpoint discovery.
Native machine placement currently follows the existing Linux host support;
other hosts keep their tools' existing native configuration.

### Validation discoveries (2026-10-09)

- Bazel hashes PATH into action keys. Its launcher removes only mj-owned
  session launcher directories while preserving the user toolchain PATH. This
  changed the second checkout from a rebuild to a disk-cache hit.
- Bash expands BASH_ENV, including dollar signs in directory names. The one
  shared shell-hook owner now uses variable indirection to preserve literal
  paths, and never chains two generated hooks into a cycle.
- Nx 23.3 and Turbo 2.11.7 both restored outputs in the second checkout without
  executing the task, then rebuilt after the declared input changed.
- The CMake fixture explicitly selects /usr/bin/cc. The second checkout still
  records one mbx hit and zero misses, proving configure-time interception.
- Git fixture timestamps must be fixed: separate container invocations cross
  second boundaries, and Go correctly includes the resulting VCS revision in
  its build key. The fixture now creates identical commits on both sides.

All six real-tool checks now pass locally, on raw morannon, and across separate
Podman containers. Both actual mj provisioning smoke checks passed and their
disposable sessions were destroyed. Detailed evidence and reproduction are in
`.agents/docs/native-build-cache-validation-20261009.md`. The full controller
suite found two stale minimum-schema assertions (78 rather than 79); those
expectations were updated, with targeted reruns before the remaining suites.

### Final Rust verification

The dev-profile controller suite completed 1,973 tests successfully and found
two expected-schema assertions to update from 78 to 79. Both corrected tests
passed in targeted reruns. The remaining full suites passed: core 495 tests
plus three integration tests, TUI 526 tests, and worker 674 tests plus its
standalone integration binaries. Existing platform/manual tests remain ignored;
the new real-tool integration was run explicitly in all three environments.
Formatting and diff whitespace checks pass. Workspace
`cargo clippy --all-targets -- -D warnings` passed after the final test edits.
The current branch is master with upstream origin/master; the user authorized
committing and pushing this validated implementation.
