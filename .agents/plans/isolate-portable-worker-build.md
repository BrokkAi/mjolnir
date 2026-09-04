# Isolate the portable musl worker build

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

The development wrapper currently compiles the complete `mj` executable once for Linux-musl and again for the host. That makes a controller, chat, or TUI edit rebuild those host-only layers twice even though a deployed target only invokes the worker subcommands. After this change, `brokk-mj-worker` owns a dedicated `mj-worker` executable. Linux builds a native controller and a static musl worker; macOS builds a native controller and native worker for `local-bare` development. Release bundles still carry static Linux workers for managed Linux targets.

The result is visible in `scripts/run.sh`: changing controller-only code rebuilds only the native `mj`, while changing worker-only code rebuilds only `mj-worker`. The deployed command grammar and relay/checkpoint formats do not change.

## Progress

- [x] (2026-09-04 19:00Z) Inspected the current build graph, worker dispatch, artifact resolver, CI, releases, and existing crate-split plan.
- [x] (2026-09-04 20:10Z) Added the dedicated worker executable, moved the Git bridge protocol below the controller, and feature-gated the broker half out of worker builds.
- [x] (2026-09-04 20:35Z) Updated artifact resolution, development/import scripts, CI/reliability workflows, release assembly, installer/npm packaging, and user documentation.
- [x] (2026-09-04 20:50Z) Added controller/worker foundation features; worker builds exclude database, projection/state, termination, local Git discovery, SQLite, and signal-hook.
- [x] (2026-09-04 21:35Z) Passed focused and full validation, recorded artifact/build-graph evidence, updated documentation, and prepared the implementation commit.

## Surprises & Discoveries

- Observation: `scripts/run.sh` cross-compiles `brokk-mjolnir`, whose direct dependencies include worker, controller, chat, and TUI. The current musl and host debug executables are both approximately 270 MB.
  Evidence: `cargo tree --target x86_64-unknown-linux-musl -p brokk-mjolnir --depth 1` and `ls -lh target/{,x86_64-unknown-linux-musl/}debug/mj`.
- Observation: selecting `brokk-mj-worker` already removes roughly half the dependency-tree nodes, but `brokk-mj-core` still compiles all of its unconditional modules and dependencies, including SQLite.
  Evidence: the current unique tree-line counts are roughly 491 for musl `mj` and 230 for `brokk-mj-worker`.
- Observation: macOS can avoid a Linux cross-toolchain for ordinary development by building a native worker in the same Cargo target graph as the native controller. Only managed Linux targets require the portable artifact.
  Evidence: worker runtime code is Unix-gated, while current artifact selection—not the runtime—prevents the native Mac executable from serving `local-bare`.
- Observation: separate packages alone were not enough to keep repeated builds fresh. Cargo shares host-side procedural macro and build dependency artifacts across target builds, so alternating worker-musl and host graphs in one target directory repeatedly dirtied a small tail of the worker graph.
  Evidence: verbose Cargo output reported rebuilt target dependencies after the native build; after moving worker artifacts to `target/worker`, two consecutive `scripts/run.sh -- --version` runs completed their Cargo phases in 1.77/1.46 seconds with no compilation.
- Observation: the dedicated worker is materially smaller even in the unoptimized debug profile.
  Evidence: the old musl `mj` is 270 MB and the new musl `mj-worker` is 89 MB. `readelf` reports neither a program interpreter nor dynamic `NEEDED` entries.
- Observation: the host's optional desktop-inclusive workspace check requires WebKitGTK/GLib development packages that are not installed in this WSL environment. Default workspace checks and lints do not include `mj-desktop` and pass.
  Evidence: `cargo check --workspace --all-targets` stopped in `glib-sys` pkg-config lookup; `cargo check --all-targets` passed.
- Observation: the configured `mbx` Cargo wrapper twice discarded otherwise valid compilation outputs after reporting that an input artifact changed during compilation.
  Evidence: focused tests and the first full-suite attempt reported `mbx[error]: compilation result was discarded`; rerunning through `/home/jonathan/.cargo/bin/cargo` bypassed the cache layer.

## Decision Log

- Decision: use separate native controller and worker artifacts rather than restoring one full musl Linux executable.
  Rationale: this gives Linux and macOS the same production boundary and prevents host-only edits from invalidating the portable worker.
  Date/Author: 2026-09-04, Jonathan/Codex.
- Decision: remove worker execution from the host `mj` binary instead of retaining an embedded compatibility fallback.
  Rationale: a normal dependency from `mj` to `mj-worker` would preserve the compile coupling this change is intended to remove.
  Date/Author: 2026-09-04, Jonathan/Codex.
- Decision: preserve the installed filename `hel` and the `worker ...` argument grammar.
  Rationale: worker process identification, launch scripts, recovery, MCP configuration, and old installed workers all rely on that shape.
  Date/Author: 2026-09-04, Codex.
- Decision: macOS source development builds a native worker and defaults to `local-bare`; no Zig dependency is introduced.
  Rationale: native controller and worker packages share one target graph and compile shared dependencies once. Linux targets remain available with an explicit packaged or overridden musl worker.
  Date/Author: 2026-09-04, Jonathan/Codex.
- Decision: keep worker builds in `target/worker` and teach development artifact lookup both the isolated and legacy shared-target layouts.
  Rationale: the separate target directory prevents host feature/build-dependency churn from invalidating portable artifacts, while the legacy candidate preserves manual build compatibility.
  Date/Author: 2026-09-04, Codex.
- Decision: retain `mj_controller::hel_git_proxy` as a re-export while locating its implementation in the foundation crate and compiling broker-only code only with the controller feature.
  Rationale: the worker needs the bridge endpoint without depending upward on the controller, and the re-export avoids an unnecessary public path break.
  Date/Author: 2026-09-04, Codex.

## Outcomes & Retrospective

The host `mj` no longer contains or depends on the worker runtime. A dedicated
`mj-worker` owns the unchanged installed `hel worker ...` protocol, including
relay, checkpoint, resource, memory/review MCP, and Git commands. Development
and release artifact lookup supports the isolated source layout, packaged
architecture names, native macOS local-bare companion, overrides, verified
downloads, and legacy source artifacts.

The worker's musl graph excludes controller, chat, TUI, CLI, desktop, SQLite,
database/projection/state, host termination, local Git discovery, and the
controller half of the Git bridge. The resulting debug static binary is 89 MB
instead of the former full-musl `mj` at 270 MB. A final consecutive
`scripts/run.sh -- --version` completed the worker and controller Cargo phases
in 1.61 and 1.44 seconds, respectively, with no compilation.

Validation passed with `cargo fmt --check`, both host and isolated-worker
Clippy with warnings denied, the complete `cargo test` suite, npm packaging
tests, shell syntax checks, YAML parsing, release-version verification, worker
package content inspection, static ELF inspection, and dependency-graph
exclusion checks. The only unavailable optional check was compiling the
desktop-inclusive workspace on this WSL host, which lacks its documented
WebKitGTK/GLib system packages; the normal default-member workspace compiled
and tested fully.

## Context and Orientation

`mj-cli/src/main.rs` currently owns both the user-facing controller commands and hidden worker commands. `mj-worker/` is only a library. `mj-controller/src/hel_controller/worker_binary.rs` discovers and installs the worker artifact, while `src/hel_targets.rs` constructs target-side invocations of the installed `hel worker ...` executable. `scripts/run.sh` explicitly cross-compiles the full `mj` binary before running a native `mj`.

Release assets already distinguish session workers externally as `mj-worker-<target-triple>` even though their contents are the full `mj` binary. This change makes the internal Cargo artifact match that existing external role.

## Plan of Work

First add `mj-worker/src/main.rs` and a worker CLI module that owns every target-side command, stderr logging, process-group setup, panic/exit records, and the musl allocator. Move the target-side Git bridge and proxy out of `mj-controller::hel_git_proxy`; keep the controller-side broker there and put framing shared by the two endpoints in a small foundation module. Remove the `Worker` command and the `mj-worker` normal dependency from `brokk-mjolnir`.

Next update worker artifact selection. Managed targets accept only a compatible portable Linux worker. Local-bare accepts a native sibling worker on macOS and the static same-architecture worker on Linux. Development lookup uses `target/<triple>/<profile>/mj-worker`; packaged lookup retains `mj-worker-<triple>`. Environment overrides retain precedence. The running controller itself is no longer a fallback. The old musl development `mj` path may remain a read-only compatibility candidate during migration.

Then change `scripts/run.sh`, CI, reliability harnesses, import scripts, release jobs, package assembly, and documentation. Linux builds native `mj` plus target-musl `mj-worker`. macOS builds native `mj` and native `mj-worker` together for local-bare development. Release macOS bundles add the native worker while retaining both prebuilt Linux-musl workers.

Finally introduce explicit `controller` and `worker` feature surfaces in `brokk-mj-core`. The worker package disables defaults and enables `worker`. Gate whole host-only modules at `src/lib.rs`, make host-only dependencies optional, and split mixed checkpoint/config/target/process modules only where a worker-facing type or operation prevents the gate. Keep serialized contract types at their current public paths. The musl graph must not contain database/state projection, target provisioning, the controller, chat, TUI, desktop, or CLI.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel`. Use `apply_patch` for edits. Preserve the pre-existing uncommitted edits to `src/hel_worker.rs` and `.agents/docs/claude-autonomous-turns.md`, and never stage them unless this task must deliberately integrate the relevant worker change.

Build graph checks:

    cargo tree --target x86_64-unknown-linux-musl -p brokk-mj-worker
    cargo tree --target x86_64-unknown-linux-musl -p brokk-mj-worker | rg 'brokk-mj-(controller|chat|tui|olnir|desktop)|rusqlite'

Validation commands:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    readelf -lW target/worker/x86_64-unknown-linux-musl/debug/mj-worker
    readelf -dW target/worker/x86_64-unknown-linux-musl/debug/mj-worker
    target/worker/x86_64-unknown-linux-musl/debug/mj-worker --version
    node scripts/release-version.mjs check v2.0.0

Every `cargo test` invocation must run outside the restricted sandbox because tests use loopback TCP and Unix sockets.

## Validation and Acceptance

The dedicated worker parses every existing target-side command and passes worker runtime, checkpoint, resource, project-memory, review-MCP, and Git bridge tests. A local-bare session runs with the selected companion and managed targets still install the artifact as `<worker-root>/hel`. Worker recovery and digest-based replacement continue to work across the old and new artifact layouts.

`readelf` reports no program interpreter and no dynamic `NEEDED` entries for the musl worker. `cargo tree` reports no controller, chat, TUI, CLI, desktop, or SQLite dependency in the worker artifact graph. Touch-based timing checks show controller-only changes leave the musl graph fresh, worker-only changes leave native `mj` fresh, and host-only foundation changes do not invalidate the worker-only foundation artifact.

## Idempotence and Recovery

Artifact lookup remains additive during migration: old static `mj` worker artifacts may be read, but new builds only produce `mj-worker`. Each milestone ends with tests and a commit containing only files changed for this task. If a milestone fails, retain the ExecPlan and working tree, document the failure here, and continue from the last green commit. Do not delete target trees or overwrite unrelated working-tree changes.

## Artifacts and Notes

The earlier completed crate-layering work is documented in `.agents/plans/split-core-into-layered-crates.md`. It intentionally left a broad foundation below worker and controller; this plan narrows only the portable worker's view of that foundation.

## Interfaces and Dependencies

`brokk-mj-worker` gains a binary named `mj-worker`. The user-facing `mj` executable loses its hidden worker implementation. The deployed worker continues to accept `worker run`, `worker proxy`, `worker acp-supervisor`, checkpoint capture/export/pack/restore commands, resource installation, memory/review MCP, and Git bridge/proxy commands.

`brokk-mj-core` gains `controller` (default) and `worker` feature surfaces. `brokk-mj-worker` uses `default-features = false, features = ["worker"]`. Relay, checkpoint, archive, and persisted formats do not change.
