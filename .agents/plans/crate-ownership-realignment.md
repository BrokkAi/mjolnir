# Align Mjolnir crate names and ownership

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation advances.

## Purpose / Big Picture

Make the source layout reflect the existing deployment and dependency boundaries. The root becomes a virtual Cargo workspace and its existing package moves to mj-core. Clients can use shared contracts and presentation without compiling controller storage or worker execution. Existing saved sessions, protocols, binaries, and configuration paths continue to work.

## Progress

- [x] 2026-09-12: Inspected dependencies and obtained approval for clean Rust import replacement and CLI ownership cleanup.
- [x] 2026-09-12: Moved core and renamed Rust modules and dependency keys.
- [x] 2026-09-12: Extracted worker execution and controller persistence from shared core.
- [x] 2026-09-12: Removed direct client/chat storage and moved daemon/server ownership out of CLI.
- [x] 2026-09-12: Architecture, package asset synchronization, and release-version checks pass.
- [x] 2026-09-12: Compiler cleanup complete; strict all-target Clippy passes. Added pending/error/retry history coverage and review restoration/error UI tests. All 3,179 pre-existing test names retained.
- [x] 2026-09-12: All nine package content lists contain required assets; canonical/generated notice differences reconciled (core crate ordering only).
- [x] 2026-09-12: Standalone core, client, worker, controller, chat and TUI checks passed. Desktop all-target check passed with local GTK/WebKit development metadata.
- [x] 2026-09-12: Workspace packaging and extracted core build passed; published manifests omit cross-runtime fixture dependencies. Final strict Clippy and generated notice comparison passed.
- [x] 2026-09-12: Full serial suite passed: 3,143 tests passed, 19 ignored, no failures; all seven PTY tests passed. Final diff reviewed for the refactor commit.

## Surprises & Discoveries

Before this refactor, core published as brokk-mj-core but imported as hel. Client has a default SessionHandleBackend::config_result implementation reading SQLite. Chat directly reads history and review storage and has direct write fallbacks. CLI contains daemon, server, and mixed UI/host pollers. Relay protocol.rs includes both shared messages and a DurableRelay-serving function. Projection is pure except that its mutation types currently belong to database. Database implements SQLite traits on shared PaneSize, requiring local conversion after extraction.

Subprocess-isolated tests construct libtest selectors using module_path! and must strip the actual Rust crate name; replacing those string literals with crate:: silently selected zero child tests. All such selectors have been corrected.

Parallel PTY fixtures on this 120-core host repeatedly exhausted their five-second startup deadline before the initial daemon connection completed. An isolated PTY test passed and six independent dashboards reached the ready screen. The final complete suite runs with RUST_TEST_THREADS=1, retaining original responsiveness deadlines and all tests; no product or test-timeout workaround was added.

Desktop initially lacked system development packages, and noninteractive sudo was unavailable. Downloaded the development dependencies into target/ownership-desktop-sysroot, extracted them without installation, and used scoped PKG_CONFIG_PATH/PKG_CONFIG_SYSROOT_DIR for cargo check --locked -p brokk-mj-desktop --all-targets. This checks Rust code and native dependency metadata, not GUI execution.

## Decision Log

2026-09-12: User approved replacing old Rust import paths without aliases. Use mj-core as directory/dependency and mj_core in Rust. Preserve brokk registry package identities. User approved moving CLI daemon/server implementation into controller. Preserve all serialized names, digest domains, environment names and runtime paths. Reuse existing crates; no new package beyond relocating core.

2026-09-12: Client owns connect-existing transport; CLI owns daemon startup/replacement and its existing attachment-maintenance loop. Controller pollers connect to the running daemon without starting an application binary. Generic checkpoint archive algorithms remain shared; controller owns verified transfer and worker owns target command entrypoints. SQLite-specific PaneSize conversion uses a controller-local wrapper rather than adding SQLite back to core.

## Outcomes & Retrospective

The ownership extraction is implemented without a new workspace package or live-session changes. Core owns shared contracts; worker owns execution; controller owns storage and host services; client owns transport/presentation; CLI owns application startup. Validation is complete. The full serial suite passed 3,143 tests (19 existing environment-dependent or optional tests ignored), including all seven PTY behaviors. Strict Clippy, independent package checks, desktop all-target checking, all package asset checks, workspace packaging and the extracted core build passed. The validated refactor is committed with this plan.

## Context and Orientation

Root src currently mixes reusable data, controller SQLite storage, and target worker runtime. mj-worker runs target processes, mj-controller manages sessions, mj-client contains interfaces, mj-chat/mj-tui render and interact, and mj-cli/mj-desktop are application entrypoints. A relay is the worker's durable command/event journal; a projection deterministically converts its events to a materialized transcript. Core must contain shared contracts and pure transformations, not owning runtimes.

## Plan of Work

### Milestone 1: Consistent layout

Move src to mj-core/src and the package/dependencies sections to mj-core/Cargo.toml. Keep workspace and build profiles in root. Change hel and hel-tui dependencies/imports to mj-core/mj_core and mj-tui/mj_tui. Remove hel_ module prefixes throughout Rust sources and rename HelConfig/HelState to Config/State. Update scripts, manifests, fixtures and packaging assets. Verify cargo check and release-version consistency.

### Milestone 2: Runtime owners

Split core worker into shared relay protocol/operational data and worker-owned DurableRelay/journal/scheduler. Keep ACP normalization/surface/step-clock in core; move process execution, terminals and native task followers to worker. Move database and storage-backed state methods plus coordination into controller. Keep projection data/mutations and pure event application in core. Separate generic subprocess/Git/native/archive helpers from controller provisioning and checkpoint transfer and worker checkpoint commands. Preserve shared review contracts while placing runtime execution with its owning process. Remove core controller/worker feature switches and validate packages independently.

### Milestone 3: Client and application boundaries

Make config_result an explicit backend method. Expose asynchronous history/review persistence through client interfaces; implementations execute outside UI loops and keep daemon writes authoritative. Remove direct chat database fallbacks. Move CLI daemon runtime, server integration, listener operations, host pollers and import persistence to controller. Client owns shared management contracts/transport; CLI keeps startup/argument handling and dashboard adapters. Move common transcript snapshots and activity formatting from chat to client so controller never imports UI crates.

### Milestone 4: Verification and commits

Add a Cargo metadata architecture check for directed crate boundaries. Exercise existing transition, serialization, digest, journal, checkpoint, migration, daemon/API, and PTY tests and async storage behavior with hand-written fakes. Run cargo test elevated and cargo clippy --all-targets -- -D warnings, formatting and script checks, independent package builds, explicit desktop check, release-version and package-content checks, and extracted core build. Commit validated coherent checkpoints on current branch, staging only task files.

## Concrete Steps

Work in /home/jonathan/Projects/hel. Use cargo check -p brokk-mj-core and equivalent individual package selections to avoid feature unification concealing missing dependencies. Run cargo test outside sandbox; use normal target storage. Run cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, node scripts/release-version.mjs check and cargo package --locked -p brokk-mj-core. Record actual commands and outcomes below as milestones finish.

## Validation and Acceptance

All default workspace tests and clippy pass. Standalone core/client/chat/TUI builds do not pull in worker or controller. Worker uses core only among workspace production dependencies; controller uses core/client, never chat/TUI. Existing relay and archive fixtures retain byte/semantic compatibility. Storage failures are observable without blocking render/input. Packaged crates contain required documentation, notices and fixtures independent of repository-relative files.

## Idempotence and Recovery

Preserve unrelated untracked .agents/plans/restore-tui-workspaces-and-status.md and mj.sqlite3. Do not reset or overwrite unrelated work. Commit checkpoints permit review and recovery; do not change branches or rebase. No data migration or running-session manipulation is needed.

## Artifacts and Notes

Build and test evidence will be retained in target/ and summarized here. Existing native Kimi task tests must follow runtime ownership and continue proving termination cannot resurrect phantom tasks.

## Interfaces and Dependencies

Core owns shared domain records, config, worker launch and relay/checkpoint contracts, pure projection, archive/native formats and reusable subprocess/Git/filesystem utilities. Worker owns harness processes, task monitors, durable journal/scheduler, terminals and reviewer execution. Controller owns SQLite/migrations, orchestration, targets/recovery, verified transfers, daemon/HTTP and host maintenance. Client owns request/result interfaces and shared presentation. Chat/TUI own UI state. CLI/desktop compose entrypoints. No old Rust-path aliases remain at completion; serialized compatibility identities remain intentional.

Revision 2026-09-12: Recorded implemented ownership boundaries and passing metadata/assets/version checks. Full compiler/test/package validation remains in progress.

Validation checkpoint 2026-09-12: `cargo clippy --all-targets -- -D warnings` passed (target/ownership-clippy-final.log). Standalone core passed. The full test suite and remaining independent package checks are running. Generated notices preserve their upstream mixed line endings through a narrowly scoped .gitattributes entry; sync writes only changed copies.

Validation checkpoint 2026-09-12: All nine `cargo package --locked --allow-dirty --list` checks and `cargo package --locked --allow-dirty --workspace --no-verify` passed. `cargo package --locked --allow-dirty -p brokk-mj-core` built the extracted crate successfully. Standalone checks used separate `cargo check --locked -p PACKAGE --lib` invocations. Strict Clippy passed again in target/ownership-clippy-verified.log. All 3,179 original test names remain, with three new storage tests. Cross-runtime dev dependencies use versionless path declarations so Cargo omits them from published manifests; the architecture script enforces this release boundary.

Final validation 2026-09-12: `RUST_TEST_THREADS=1 cargo test` exited 0 (target/ownership-test-serial.log), with 3,143 passed and 19 ignored. `cargo clippy --all-targets -- -D warnings` exited 0 (target/ownership-clippy-verified.log). Formatting, staged diff checks, architecture/assets/version scripts, and the final generated notice comparison passed. Serial execution is the recorded full-suite validation mode; parallel PTY startup contention on this host remains documented above. Temporary diagnostic instrumentation was removed, and generated diagnostic daemons exited before completion.
