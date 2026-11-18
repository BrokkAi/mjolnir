# Split mj-controller into three layered crates so no single compile dominates the build

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

`mj-controller` is one Rust crate of 382,557 lines (253,012 of code and 129,545 in dedicated test files). Cargo compiles a crate as one unit, so every build that touches it runs one compiler process over all of it, and the test build runs a second one over the code and the tests together. That one process is the longest step of every local test build and of CI, and it needs about 11 GB of memory, which does not fit the 7 GB macOS runner (issue #1578).

After this work the same code lives in three crates stacked by dependency: a storage layer at the bottom, the controller in the middle, and the daemon and HTTP server on top. Nothing changes for a user of `mj`. A contributor sees it in three ways. The largest single compile shrinks from 382k lines to about 200k. An edit in the daemon or server no longer recompiles the controller or the store. CI can test each crate on its own runner. Observe it with `cargo build --timings` (the report in `target/cargo-timings/cargo-timing.html` shows three `brokk-mj-*` units where there was one) and with the CI run page, where the `Tests (controller)` job is replaced by one job per crate.

## Progress

- [x] (2026-11-18 05:45Z) Measured the current build and the references that cross each proposed boundary (see Context and Surprises).
- [x] (2026-11-18 06:05Z) Added `scripts/check-controller-layers.py`, which lists the upward reference sites (not yet run in CI).
- [ ] Milestone 1: cut every reference from lower modules up into the top layer, inside the existing crate, and enforce the check (completed: the pure helpers, `ProfileCatalog`, the startup-step types and the session predicates, 125 sites down to 54; remaining: the modules that take the daemon's `RuntimeState`, the HTTP API types named by lower code, and lower tests that drive top-layer code, listed under Surprises).
- [ ] Owner publishes placeholder `brokk-mj-daemon` and `brokk-mj-store` crates and configures their trusted publishers (blocks Milestones 2 and 4 from landing on master; see Decision Log).
- [ ] Milestone 2: move the top layer into `mj-daemon` (`brokk-mj-daemon`).
- [ ] Milestone 3: cut every reference from the storage modules up into the rest, inside `mj-controller`.
- [ ] Milestone 4: move the storage layer into `mj-store` (`brokk-mj-store`).
- [ ] Milestone 5: one CI test shard per crate; measure and record the result.

## Surprises & Discoveries

- Observation: the compile has no single hot spot. On an idle 24-thread machine the `mj-controller` test build takes 125 s: type checking 20 s, borrow checking 16 s, coherence checking 10 s, collecting generic instantiations 24 s, generating LLVM input 41 s, LLVM itself 45 s (in parallel with the previous step), linking 3 s. Peak memory is 11.1 GB.
  Evidence: `RUSTC_BOOTSTRAP=1 CARGO_INCREMENTAL=0 cargo rustc -p brokk-mj-controller --lib --profile test -- -Ztime-passes`.
- Observation: more cores do not help. Pinned to four threads (two cores) the same unit took 262 s; on 24 threads next to other compiles it took 255 s. The unit is mostly serial work, so only a smaller unit makes it faster.
  Evidence: `cargo test --no-run -p brokk-mj-controller --tests --timings` under `taskset -c 6,18,7,19` with `CARGO_BUILD_JOBS=4`.
- Observation: 47 of the crate's 62 top-level modules form one dependency cycle (244k lines of code), so no module can move out until references are cut.
  Evidence: a strongly-connected-component pass over `crate::` paths, test files excluded.

- Observation: the remaining upward references are not stray helpers. `github_watch::service`, `web_push::run`, `mailbox_outbox::run`, `controller::mbx::service`, `review_host::environment` and `worker_client::credential_sync` each hold or take the daemon's `RuntimeState`, so they are daemon services filed under lower modules. `credential_sync` asks it six questions (`credential_targets`, `credential_session_ended`, `credential_session_ids`, `credential_sync_skips`, `credential_target_is_current`, `credential_profile_source_is_current`). Each needs its service half moved up to the top layer and its types and notifications left below; `review_host` and `github_watch` have almost no lower users and can move up whole.
  Evidence: `python3 scripts/check-controller-layers.py --all` and `grep -n RuntimeState` in those files.
- Observation: a `crate::` path count understates the work, because a file that imports a name once and uses it fifty times counts as one site. The compiler is the real check once files move.
  Evidence: `github_watch/service.rs` has one counted site and about 40 uses of `state`.

## Decision Log

- Decision: split by layer into three crates, not by feature area.
  Rationale: the measured references run mostly downward already. About 100 reference sites point from lower modules into the top layer and about 45 from the storage modules into the rest, against thousands pointing down. A layered split needs only those cut. Feature areas (quota, GitHub, review) are small; the size is in `controller`, `daemon`, `database` and the two server modules.
  Date/Author: 2026-11-18, Claude. Ryan Svihla asked for work on mj-controller's build cost; the crate boundaries were proposed here and he confirmed going ahead the same day.
- Decision: new crates are justified under the repository rule that a crate needs a compilation boundary.
  Rationale: the compile of this one unit is the measured bottleneck for CI, for local builds and for the macOS lane's memory. The rule forbids crates made only to reorganize code; this is the case it allows.
  Date/Author: 2026-11-18, Claude.
- Decision: cut references inside the existing crate first, and move files only when a layer has none left.
  Rationale: other contributors commit to `mj-controller` many times a day. Small in-crate cuts merge easily and each one is correct on its own. The file move then touches no logic, so Git follows it as renames and a concurrent edit merges into the moved file.
  Date/Author: 2026-11-18, Claude.
- Decision: top layer first, storage second.
  Rationale: the top layer is 119k lines against 61k, and the daemon and server are where an edit most often forces a rebuild of code that did not change.
  Date/Author: 2026-11-18, Claude.

- Decision: the new crates are `brokk-mj-store` and `brokk-mj-daemon`, and both names must exist on crates.io, with a trusted publisher configured, before the commit that creates either crate reaches master.
  Rationale: `.github/workflows/publish.yml` authenticates with `rust-lang/crates-io-auth-action`, that is, crates.io trusted publishing, which is configured in the settings of a crate that already exists. `nightly.yml` releases master automatically, and `publish.yml` publishes crates in dependency order and stops at the first failure, so a crate the workflow may not publish would leave a release with `brokk-mj-core` published and `brokk-mj-controller` and `brokk-mj-mjolnir` missing. The owner publishes a `0.0.0` placeholder of each name with a personal token, then adds the trusted publisher (repository `BrokkAi/mjolnir`, workflow `publish.yml`, environment `crates-io`) and the same owners as `brokk-mj-controller`. Both names were free on 2026-11-18.
  Date/Author: 2026-11-18, Ryan Svihla confirmed the split and raised the manual first publish; Claude checked the workflow.

## Outcomes & Retrospective

Nothing shipped yet.

## Context and Orientation

The workspace root `Cargo.toml` lists the crates. `mj-controller/` (package `brokk-mj-controller`, library `mj_controller`) is the daemon side of Mjolnir: `mj daemon-run` in `mj-cli/` starts it. Its modules are declared in `mj-controller/src/lib.rs`. A "top-level module" below means a name declared there, such as `daemon` (the file `mj-controller/src/daemon.rs` plus the directory `mj-controller/src/daemon/`).

A "reference site" is one place in a source file that names an item from another top-level module through a `crate::` path. A "layer" is a set of top-level modules; a reference is "upward" when it points from a lower layer to a higher one. A crate can depend only on crates below it, so every upward reference must go before the layers can become crates.

The three layers, with lines of code plus lines in dedicated test files:

The top layer, to become `mj-daemon`: `daemon` (32,734 + 18,853), `server` (16,370 + 13,593), `server_runtime` (16,198 + 10,606), `pollers` (4,509 + 4,157), `web_viewer`, `recovery` and `worker_upgrade`. About 72k + 47k lines. `doctor` stays in the middle because `setup` uses it.

The storage layer, to become `mj-store`: `database` (33,023 + 8,935), `targets` (8,481 + 5,763), `upgrade`, `recovery_gate`, `worker_lifecycle`, `target_storage`, `image_pull_gate`, `streamed_child`, `termination`, `launch_timing` and `test_log`. About 46k + 15k lines.

The middle layer stays in `mj-controller`: `controller` (63,165 + 38,956), `session_manager`, `worker_client`, `review_host`, `doctor`, `setup`, `quota`, `import`, `sessionwiki` and the remaining small modules. About 135k + 67k lines.

Upward references into the top layer today, by the item named: `daemon::RuntimeState` (about 20 sites, in `worker_client`, `review_host`, `controller`, `github_watch`, `mailbox_outbox`, `web_push`); `server_runtime::profile_catalog::ProfileCatalog` (about 40, mostly `ProfileCatalog::new` in `controller` tests); `daemon::StartupStep` and `daemon::startup_followup` (about 12, in `database`, `controller`, `review_host`); `daemon::delegation::deliver` (4, `controller`); `server::api` types `StartFollowup`, `SubagentCandidates`, `SubagentBackend`, `WaitObservation`, `ApiSession`, `ViewerSnapshot`, `ViewerMoveRecovery` (about 25); `server_runtime::api::{ExportRuntime, ApiBackend, agent_working_directory_at}` (5); `pollers` helpers `session_target_is_pollable`, `interrupted_lifecycle_cause`, `refresh_host_image`, `credential_sync_target_is_current`, `QUOTA_REFRESH_INTERVAL` (about 10); `server::{install_rustls_crypto_provider, load_or_create_cookie_key, cookie_key_path}` (8); `recovery::backoff_delay` (1). Regenerate this list with the check script from Milestone 1; the numbers move as others commit.

Outside the crate, `mj-cli` names `mj_controller::{database, server, controller, doctor, targets, session_manager, import, pollers, recovery_gate, review_host, ...}`, `mj-worker` tests name `mj_controller::targets`, and `mj-core` mentions `pollers` in two comments. The package name appears in `.github/workflows/ci.yml` (the package list in the `licenses` job and the test shards), `.github/workflows/publish.yml` (publication order), `.github/workflows/windows.yml`, `scripts/check-crate-boundaries.mjs` (allowed dependencies per crate), `licenses/deny.toml`, `CONTRIBUTING.md`, the root `Cargo.toml` and `mj-worker/Cargo.toml`.

Tests in one crate cannot see `#[cfg(test)]` items of another. The fixtures in `mj-controller/src/controller/test_support.rs` and `database::install_isolated_test_writer` are used by tests in all three layers. They become `#[doc(hidden)] pub` behind a Cargo feature named `test-support` on the crate that owns them, which the higher crate enables in `[dev-dependencies]`, the way `test-hooks` on `brokk-mj-core` already works.

## Plan of Work

Milestone 1 removes the upward references into the top layer while everything is still one crate. For each item listed above, decide by what it is. A plain type or helper that the lower code owns in practice moves down to the module that uses it, and the top layer names it from there (`StartupStep`, `backoff_delay`, the keychain and cookie-key helpers, `install_rustls_crypto_provider`, `QUOTA_REFRESH_INTERVAL`, `ProfileCatalog` if its own dependencies allow). A request or response type that belongs to the HTTP API but is produced by the controller moves to the module that produces it. Where lower code calls into the daemon's live state (`RuntimeState`), read each site: pass the narrower value the callee needs, or move the function up to the daemon if it is daemon logic that happens to sit in a lower file. Do not introduce a trait only to invert a dependency while a move would do. A lower test that needs a top-layer item moves up with it. Add `scripts/check-controller-layers.py`, which lists upward reference sites given the layer table and exits non-zero when there are any, and run it in the `licenses` job of `.github/workflows/ci.yml` beside `check-crate-boundaries.mjs`, with the storage boundary reported but not yet enforced. At the end the script prints no upward reference into the top layer and the crate's tests pass unchanged.

Milestone 2 creates `mj-daemon/` with `git mv` of the top-layer files, a `Cargo.toml` modelled on `mj-controller/Cargo.toml`, and a `lib.rs` declaring the moved modules. In moved files, `crate::<lower module>` becomes `mj_controller::<lower module>`; lower items they use become `pub`; shared fixtures go behind `test-support`. `mj-cli` depends on both crates and names moved modules as `mj_daemon::...`. Update every file named in Context that lists crates, placing `brokk-mj-daemon` after `brokk-mj-controller` in the publication order. No behavior changes and no test is added or removed: the count of tests run by `cargo test` across the two crates equals the count before.

Milestones 3 and 4 repeat the two steps for the storage layer, with `brokk-mj-store` published before `brokk-mj-controller`. `database` names `mailbox_outbox::notify_mailbox_outbox_changed`, `quota` types, `daemon::StartupStep`, `web_push::SubscriptionKeys`, `import::setup_style_id`, `session_manager::new_command_id` and `review_host::partial_review_events`; `targets` names `controller::StoppedWorker` and `controller::run_control_command`. Then enforce the storage boundary in the check script.

Milestone 5 replaces the `controller` entry of the `tests` matrix in `.github/workflows/ci.yml` with one entry per crate, keeps the rule that the `libraries` shard takes every member not named elsewhere, and records before and after timings here.

## Validation and Acceptance

After every milestone, from the repository root and outside the sandbox: `cargo test -p brokk-mj-controller` (plus `-p brokk-mj-daemon` and `-p brokk-mj-store` once they exist) reports the same passed, failed and ignored tests as before the milestone; `cargo clippy --all-targets -- -D warnings` is clean; `node scripts/check-crate-boundaries.mjs` and `python3 scripts/check-controller-layers.py` pass. At the time of writing 24 `mj-controller` tests fail on master (`controller::move_session`, `controller::checkpoint` and five `daemon::tests` suspend tests, all with "relay proxy disconnected during project_memory_snapshot"); they are not caused by this work, so compare failure sets, not just exit codes.

After Milestones 2 and 4, also run `cargo test` for `brokk-mjolnir` and `brokk-mj-worker`, which depend on the moved modules, and `cargo package --locked --allow-dirty --list -p <new crate>` to confirm the crate packages with its `LICENSE`.

Acceptance for the whole plan: `cargo build --timings` shows no `brokk-mj-*` test unit above about half of today's 125 s on the same machine; touching `mj-daemon/src/daemon.rs` and rebuilding leaves `brokk-mj-controller` and `brokk-mj-store` fresh; peak memory of the largest compile is under 7 GB; the slowest CI test job is faster than the 6.4 minute build plus test run of today's `Tests (controller)`.

## Idempotence and Recovery

Each cut in Milestones 1 and 3 is its own commit and leaves the crate building, so the work can stop and resume at any commit. The check script reports what remains. A file move is a single commit per crate; if upstream moves on while it is in progress, merge `origin/master` (do not rebase): edits to a moved file follow the rename, and a new upward reference shows up in the check script.

## Interfaces and Dependencies

At the end: `brokk-mj-store` depends on `brokk-mj-core`, `brokk-mj-checkpoint`, `brokk-mj-transcript` and whatever third-party crates its modules use; `brokk-mj-controller` adds `brokk-mj-store`; `brokk-mj-daemon` depends on both; `brokk-mjolnir` depends on all three. `brokk-mj-store` and `brokk-mj-controller` each have a `test-support` feature exposing their test fixtures. No new third-party dependency is introduced.
