# Fan the workspace out into many small crates under a size budget

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Building and testing Mjolnir is slow because a few crates are very large. Cargo compiles one crate as one unit, most of that work runs on a single thread, and a test build compiles a crate's code and tests together as a second unit. So the largest crate sets the floor for every build that touches it, and more cores do not help. On 2026-11-18 the workspace has twelve crates; six are over 50,000 lines of Rust and one is 249,000. Comparable Rust command-line tools have 48 to 91 crates (uv 91, ruff 55, Nushell 48), almost all far smaller.

After this work no crate is over 50,000 lines of Rust (tests included), and crates that do not need each other sit side by side so Cargo compiles them at the same time and an edit rebuilds only the crates above it. Nothing changes for a user of `mj`. A contributor sees it with `python3 scripts/check-crate-size.py`, which prints every crate's size and fails when one is over the budget, and with `cargo build --timings`, whose report in `target/cargo-timings/` shows many short `brokk-mj-*` bars running in parallel where there were a few long ones.

## Progress

- [x] (2026-11-18) Measured module sizes and the references between modules in every crate over the budget (see Context and Surprises).
- [x] (2026-11-18) `scripts/register-crate.py` registers a new crate on crates.io (placeholder, owners, Trusted Publishing), so adding a crate needs no hand work.
- [x] (2026-11-18) `scripts/check-crate-size.py` reports every crate against the budget and stops a crate under it from growing past it.
- [x] (2026-11-18) Milestone 1: the HTTP server is `mj-server` (`brokk-mj-server`), 29,241 lines; `mj-daemon` is 104,360. The 964 tests are the same 964, 279 of them now in `mj-server`.
- [ ] Measure what the `mj-daemon` test unit generates code for (see the code generation entry in Surprises) before choosing Milestone 2's cuts.
- [x] (2026-11-18) Milestone 2a: no production code in `mj-controller`'s targets layer or store layer names a module above it (16 sites at the start). `scripts/check-controller-layers.py` enforces both boundaries in CI.
- [x] (2026-11-18) Milestone 2b: no test crosses those boundaries either (22 sites at the start). `test_name` and `IsolatedTest` are `test_isolation`, in the targets layer; the store's record fixtures are `database::test_fixtures`; two tests of controller functions left `targets/tests.rs` and the test that renders a stored window through `compaction` sits with `compaction`'s tests. The same 1,845 tests run.
- [x] (2026-11-18) Milestone 2c, first half: the targets layer is `mj-targets` (`brokk-mj-targets`), 18,115 lines; `mj-controller` is 230,766. The same 1,845 tests run, 175 of them in `mj-targets`.
- [x] (2026-11-18) Milestone 2c, second half: the store layer is `mj-store` (`brokk-mj-store`), 45,605 lines and under the budget; `mj-controller` is 185,161. The same 1,845 tests run: 175 in `mj-targets`, 302 in `mj-store`, 1,368 in `mj-controller`. `scripts/check-controller-layers.py` is deleted, since the compiler enforces both boundaries now.
- [x] (2026-11-18) A commit no longer recompiles the workspace: after an empty commit `cargo build` compiles three small units in 12 s where it compiled everything in 83 s (`.agents/plans/build-identity-off-the-compile-path.md`).
- [x] (2026-11-18) Milestone 4, first two groups: `mj-accounts` (`brokk-mj-accounts`, 17,074 lines: logins, the login terminal, quota and usage readers) and `mj-github-relevance` (`brokk-mj-github-relevance`: the GitHub classifier with its ONNX Runtime and tokenizer dependencies, which only the daemon uses). `mj-controller` is 164,848 lines. Its 1,368 tests are now 158 + 10 + 1,200.
- [x] (2026-11-18) Milestone 4, import group: `mj-import` (`brokk-mj-import`, 14,486 lines: `import`, `project_catalog`, `compaction`, `utility_llm`, `handoff`, `project_picker`). `mj-controller` is 150,284 lines and its 1,200 tests are 133 + 1,067.
- [x] (2026-11-18) Milestone 4, above the controller: `setup` and `doctor` are `mj-setup` (`brokk-mj-setup`, 10,600 lines), which depends on `mj-controller`; `mj-daemon` and `mj-cli` name it directly. `mj-controller`'s 1,067 tests are 83 + 984.
- [ ] Milestone 4, `sessionwiki` (7,400 lines): `session_manager/standalone.rs` calls `sessionwiki::history::execute` to answer workers' history queries, and `sessionwiki.rs` calls `Controller::load` in five places, so the three form a loop. Split it: the read-only query side (`history`, `search_query`, `transcript_grep`, `tags`) below the controller, the sync side that loads controller state above it.
- [ ] Milestone 4, session runtime: `worker_client`, `session_manager`, `project_memory_merge`, `checkpoint_transfer`, `wait_policy` (about 25,000 lines); they name `controller` 26 times.
- [ ] Milestone 4, worker staging: `controller/worker_binary` (18,600 lines) and `controller/mbx`; then the remaining lifecycle operations in `controller/` by operation.
- [x] (2026-11-18) `cargo machete` 0.9.2, the version CI pins, is installed for local use at `~/.cache/mjolnir/build/cargo-machete/bin/cargo-machete`; run it from the repository root after every move.
- [ ] Milestone 2: the review host and the pollers leave `mj-daemon`.
- [ ] Milestone 3: `mj-daemon`'s `daemon` and `server_runtime` modules are separated and the crate is under the budget.
- [ ] Milestone 4: `mj-controller` fans out above the store (accounts, import, worker staging, session runtime); starts when the storage layer is its own crate.
- [ ] Milestone 5: `mj-worker` splits into relay, ACP bridge and runtime.
- [ ] Milestone 6: `mj-core` splits so that the bottom of the graph is no longer one 90,000-line unit.
- [ ] Milestone 7: `mj-tui` and `mj-chat` are under the budget.
- [ ] Milestone 8: `OVER_BUDGET` in `scripts/check-crate-size.py` is empty; record before and after timings.

## Surprises & Discoveries

- Observation: tests are close to half of the big crates, so a test build is about twice the size of the library build. `mj-controller` has 140,754 lines of code and 108,030 of tests; `mj-daemon` 66,353 and 67,216; `mj-worker` 50,614 and 55,454.
  Evidence: a count of `tests.rs`, `tests/` directories and trailing `#[cfg(test)] mod tests` blocks against everything else under each `src/`.
- Observation: `mj-daemon`'s HTTP server is almost free to move. The `server` module (13,765 lines of code, 15,445 of tests) names the rest of the crate in three places in production code: `crate::daemon::lifecycle_failure_message` (a pure formatting function), `crate::server_runtime::phone_prompt_blocks` (a pure function over a server type), and `crate::review_host::{RuntimeReviewView, VerdictKind}`, which are re-exports of `mj_client::review`.
  Evidence: `grep -rnE "crate::(daemon|server_runtime|review_host|pollers|web_viewer|recovery|observe|github_watch|worker_upgrade)" mj-daemon/src/server.rs mj-daemon/src/server/`.
- Observation: `mj-controller` looks like one cycle but the cycle is thin. Counting each `crate::` path, 27 of its 60 top-level modules are mutually reachable. Grouped into seven candidate crates (store, targets, accounts, import, worker staging, session runtime, controller), the references that point from a lower group to a higher one are a few dozen symbols, not thousands.
  Evidence: a pass that maps each uniquely named public type and path-qualified function to the module defining it and counts uses elsewhere. It over-counts common names (`session`, `result`, `ensure`), so the compiler is the real check once files move.
- Observation: the mbx compiler cache already serves rustc in session clones. A cold `cargo test --no-run -p brokk-mj-daemon` in a fresh clone reported `753 hits, 191 misses`, so third-party crates are mostly reused and the workspace's own crates are what a fresh clone pays for.
  Evidence: the `mbx[progress]` line in that build's output.

- Observation: lines moved out do not buy time in proportion, because code generation, not parsing or type checking, is most of a test build. Moving 22% of `mj-daemon`'s lines out took its test unit from 63.4 s to 56.7 s, about 10%. Of those 56.7 s on an idle 32-thread machine, type checking is 9.0 s, borrow checking 6.6 s and coherence checking 5.5 s, while collecting generic instantiations is 12.1 s, generating LLVM input 19.3 s and LLVM itself 19.1 s (in parallel with the previous step). The server's own test unit is 12.8 s. So the remaining cost is concentrated in what the daemon's code and its 16,238-line `daemon/tests.rs` instantiate, and the next cut should be chosen by generated code, not by line count.
  Evidence: `RUSTC_BOOTSTRAP=1 CARGO_INCREMENTAL=0 cargo rustc -p brokk-mj-daemon --lib --profile test -- -Ztime-passes`, and `cargo test --no-run -p brokk-mj-daemon --timings` before and after the move.
- Observation: a new commit rebuilds every workspace crate. `mj-core/build.rs` passes the commit time to the compiler as `MJ_BUILD_COMMIT_TIME`, and `mj-core/src/worker_build.rs` reads it with `env!`, so `mj-core` and everything above it is stale after each commit even when no source changed.
  Evidence: `mbx[warning]: incremental state was not reused: compiler environment input changed: MJ_BUILD_COMMIT_TIME` on the first build after a commit.
- Observation: making the server a crate needed 30 items widened from `pub(crate)` to `pub` and one feature. The daemon's tests read two counters in `server/viewer_types.rs` that existed only under `cfg(test)`; they are now behind `mj-server`'s `test-support` feature, which `mj-daemon` enables in its dev-dependencies.
  Evidence: the errors from `cargo check -p brokk-mj-daemon --all-targets` after the move.

- Observation: a test that hands a file's path to a child process breaks silently when the file moves crates. Five `worker_client` tests write a stand-in `aws` script that prints `tests/fixtures/aws-sts-assume-role-access-denied.txt` by its path under `CARGO_MANIFEST_DIR`; the compiler found every `include_str!` of the moved fixtures, but not that. The fixtures are now constants in `mj_targets::aws_assume_role::test_fixtures` and the tests write the text to a scratch file. After a move, search for the fixture's file name, not only for compile errors.
  Evidence: `the AWS CLI exited with status 254` in those five tests after the move.

- Observation: a group chosen by subject can hide a module that belongs elsewhere. `github_student` was first placed with the quota readers because it names `quota::retry_after_header`; it is in fact an on-device inference model that pulls in ONNX Runtime bindings and a tokenizer, and nothing in `mj-controller` uses it. As its own crate it needs only `mj-core` and `mj-transcript`, and `mj-controller` no longer depends on those libraries. Check a candidate's third-party dependencies, not only its `crate::` references, before grouping it.
  Evidence: `grep -rlE "\bort::|tokenizers::"` over the group named only `github_student/`.
- Observation: after a move, the crate that gave up the code keeps the dependencies only that code used, and nothing local reports it; the `cargo machete` job in CI does. Eleven such dependencies failed that job for eight pushes before it was noticed.
  Evidence: the "Unused dependencies" job on commits `ed3554170` through `f2f99b9f0`.

## Decision Log
- Decision: in a file that moves to a new crate, every `pub(crate)` becomes `pub`.
  Rationale: widening only what the compiler rejects leaves crate-private inherent methods that callers above can no longer see, and a call to one silently binds to a trait method of the same name when one is in scope. Widening everything means nothing that was visible becomes invisible. The cost is a larger public surface on a crate that has no users outside this workspace. (`mj-server` was moved before this rule and widened item by item; no such rebinding was found there, since the build has warnings denied.)
  Date/Author: 2026-11-18, Claude.

- Decision: a budget of 50,000 lines of Rust per crate, tests included, checked in CI.
  Rationale: the test build is the unit that hurts and it contains the tests. 50,000 is about a fifth of today's largest unit and is a size the smaller crates here already compile at in seconds. The check reports the six crates that are over today without failing on them, because a hard cap would block unrelated work in crates other people edit daily; it fails only when a crate under the budget grows past it, or when a listed crate has shrunk enough to be removed from the list.
  Date/Author: 2026-11-18, Claude, at Ryan Svihla's request to be far more aggressive than a three-layer split.
- Decision: fan out, not stack. Prefer a crate that does not depend on its siblings, and accept a trait at a boundary when that is what makes two crates independent.
  Rationale: a chain of layers only helps an edit at the top; siblings compile in parallel and do not rebuild each other. rustc split its own cyclic compiler crates the same way, by having lower crates call through an interface the upper crate fills in. `server_runtime::api::ExportRuntime` and `worker_client::CredentialSyncRuntime` are existing traits of this kind here. The daemon still owns every fact; a trait changes who names the type, not who decides.
  Date/Author: 2026-11-18, Claude; Ryan Svihla chose this over the layered split.
- Decision: keep publishing every crate to crates.io.
  Rationale: `cargo install brokk-mjolnir` is documented, and crates.io forbids path-only dependencies, so each internal crate must be published. uv and ruff do the same with dozens of internal crates. `scripts/register-crate.py` makes the registration one command.
  Date/Author: 2026-11-18, Ryan Svihla asked for the script; Claude.
- Decision: a moved module keeps its path. The new crate's `lib.rs` declares the module under its old name (`mj_server::server`) and the crate it left re-exports it (`pub use mj_server::server;` in `mj-daemon/src/lib.rs`).
  Rationale: no file in the moved module and no caller changes its `crate::server::...` paths, so Git records pure renames and a concurrent edit by another contributor merges into the moved file. The stutter in the path is the price.
  Date/Author: 2026-11-18, Claude.
- Decision: this plan takes over the storage work of `.agents/plans/split-mj-controller-by-layer.md` (its Milestones 3 to 5), and splits that plan's one storage crate into two: targets below, store above.
  Rationale: Ryan Svihla stopped the session that was carrying out that plan. Its storage layer put `targets` and `database` in one crate of about 61,000 lines, but `targets` names nothing in `database`, and `database` takes only four plain types from `targets`. As two crates, `mj-targets` (about 18,000 lines) compiles first and everything else that needs targets but not the database can depend on it alone. Every other group in `mj-controller` depends on one or both, so these two come before the rest of Milestone 4.
  Date/Author: 2026-11-18, Claude.
- Decision: two traits at the targets boundary, `targets::StoppedWorkerProof` and `targets::ControlWorkerStager`.
  Rationale: `targets::cleanup` took the controller's `StoppedWorker` as proof that a worker's managed Stop completed, and `WorkerControlPlan::execute` called up into `controller::worker_binary` to stage the control worker when a script reports status 78. Neither can move: the proof is built from the controller's reclamation state, and staging needs the worker store. The proof is now a trait the controller's type implements, which keeps its construction private to the controller. `execute` takes the stager as an argument, so a caller cannot run a control plan without the means to stage it; the retry loop itself (`run_staged_control_command`) moved down into `targets`. A process-wide hook was rejected because a plan could then run with no stager installed.
  Date/Author: 2026-11-18, Claude.
- Decision: tests stay beside the code they test.
  Rationale: this is what most large Rust projects do, and it is cheap once crates are small. Moving 130,000 lines of tests into separate test crates was considered as a faster route to a smaller test unit and set aside; reconsider it only for a crate that cannot be split.
  Date/Author: 2026-11-18, Claude.

## Outcomes & Retrospective

Nothing shipped yet.

## Context and Orientation

The workspace root `Cargo.toml` lists the crates. A crate's package name starts with `brokk-` (`brokk-mj-daemon`), its directory and library name do not (`mj-daemon/`, `mj_daemon`). The sizes below are lines of Rust under `src/` on 2026-11-18, as printed by `python3 scripts/check-crate-size.py`:

    248,881  brokk-mj-controller   the daemon side below the daemon process: database, targets, sessions
    133,601  brokk-mj-daemon       the daemon process: runtime state, HTTP API, web viewer, pollers
    107,226  brokk-mj-worker       the per-session worker: relay, ACP bridge, runtime
     90,689  brokk-mj-tui          the terminal dashboard
     89,793  brokk-mj-core         shared types, configuration, targets; everything depends on it
     51,271  brokk-mj-chat         the conversation view
     43,932  brokk-mjolnir         the `mj` binary (`mj-cli/`)

A "top-level module" is a name declared in a crate's `lib.rs`, such as `server` (the file `src/server.rs` plus the directory `src/server/`). An "upward reference" is a place where code that should end up in a lower crate names an item that will live in a higher one; a crate can only depend on crates below it, so each must be removed before the files move. There are three ways to remove one, in order of preference: move a plain type or helper down to the code that owns it in practice; move a function up when it is really the higher layer's logic filed in a lower file; or have the lower code ask through a trait that the higher crate implements.

`mj-daemon` has these modules (code + tests): `daemon` (26,560 + 26,454), `server` (13,765 + 15,445), `server_runtime` (11,389 + 10,897), `review_host` (5,328 + 5,876), `pollers` (3,829 + 4,651), `github_watch` (1,591 + 2,270), and the small `observe`, `web_viewer`, `recovery`, `worker_upgrade` and `controller_tests`. `server` is the axum HTTP API with its request and response types; `server_runtime` implements that API against the daemon's live state (`daemon::RuntimeState`); `review_host` runs turn reviews; `pollers` holds the periodic background loops.

`mj-controller`'s largest modules are `controller` (51,116 + 49,041, of which `controller/worker_binary` is 18,626 in all, `controller/move_session` 13,392 and `controller/checkpoint` 8,476), `database` (27,028 + 15,072), `targets` (8,277 + 5,989), `worker_client` (6,374 + 6,079), `session_manager` (6,588 + 5,030), `import` (5,328 + 3,043), `doctor`, `sessionwiki`, and about fifty small ones.

Adding a crate touches more than the code. The list, from the commit that created `mj-daemon` (`64013d622`): the `members`, `default-members` and `[workspace.dependencies]` entries in the root `Cargo.toml`; `.dockerignore` and the worker stage's `COPY` lines in `containers/Containerfile.agent-dev` (`scripts/release-workflow.test.mjs` checks they agree); the `allowed` table in `scripts/check-crate-boundaries.mjs`; `licenses/deny.toml`; the publication order in `.github/workflows/publish.yml`, after the crates it depends on; the test matrix in `.github/workflows/ci.yml` and the package list in `.github/workflows/windows.yml`; the crate list in `CONTRIBUTING.md`; and a `LICENSE` symlink in the crate directory. The crate must also exist on crates.io before the commit reaches master, because the nightly release publishes master and `.github/workflows/publish.yml` stops at the first crate it may not publish: run `scripts/register-crate.py brokk-mj-<name> --description "<one line>"`.

Tests in one crate cannot see `#[cfg(test)]` items of another. Where a moved module has code that behaves differently under test, or fixtures that tests in a higher crate use, the gate becomes `#[cfg(any(test, feature = "test-support"))]` and the higher crate enables that feature in `[dev-dependencies]`, as `mj-daemon` already does for `mj-controller`.

Making a `pub(crate)` method invisible to a caller can change which method a call resolves to: if a trait in scope has a method of the same name, the call silently binds to the trait method. When items become `pub` for a move, build with warnings denied; the `unconditional_recursion` lint is what caught this the first time.

## Plan of Work

Milestone 1 moves `server` out of `mj-daemon` into a new crate `mj-server` (package `brokk-mj-server`, library `mj_server`). First remove the three upward references inside `mj-daemon`: `server/viewer_types.rs` and `server/tests.rs` name `mj_client::review` directly; `lifecycle_failure_message` moves from `daemon.rs` to `server/actions.rs` as a public function and `daemon.rs` calls it there; `phone_prompt_blocks` moves from `server_runtime/snapshot.rs` to `server/validation.rs`. Then `git mv` `mj-daemon/src/server.rs`, `mj-daemon/src/server/`, and the `web/`, `icons/` and `fonts/` directories that `server/assets.rs` embeds, into `mj-server/src/`; write `mj-server/Cargo.toml` from `mj-daemon/Cargo.toml` keeping only the dependencies the moved code names; `mj-server/src/lib.rs` declares `pub mod server;`; `mj-daemon/src/lib.rs` replaces `pub mod server;` with `pub use mj_server::server;`. Items of `server` that the daemon uses and that were `pub(crate)` or `pub(super)` become `pub`. Golden files under `mj-daemon/tests/golden/` that only server tests read move to `mj-server/tests/golden/`. Update every file in the list in Context, and move the font path in `scripts/generate-supplemental-third-party-notices.mjs`. At the end `cargo test -p brokk-mj-server -p brokk-mj-daemon` runs the same tests that `cargo test -p brokk-mj-daemon` ran before, and `mj-daemon` is about 29,000 lines smaller.

Milestone 2a to 2c make the two lowest layers of `mj-controller` into crates. The layers are the `LAYERS` table in `scripts/check-controller-layers.py`: targets (`targets`, `target_storage`, `image_pull_gate`, `streamed_child`, `termination`, `tailscale`, `aws_credentials`, `aws_assume_role`, `signing_key`, `test_log`) and store (`database`, `upgrade`, `recovery_gate`, `worker_lifecycle`, `web_push`, `mailbox_outbox`). The production cuts are done. For the tests: `test_name` and `IsolatedTest` from `controller/test_support.rs` are general test helpers and move to the targets layer behind `test-support`; `checkpoint_test_session` moves to the store layer; the two tests in `targets/tests.rs` that exercise `controller::removable_profile_root` and `controller::target_profile_home_for_test` move to `controller/tests.rs`; the one database test that renders through `compaction` moves beside `compaction`'s tests. Then each layer moves with `git mv` exactly as `server` did, with `mj-controller` re-exporting every moved module under its old name (`pub use mj_targets::targets;`).

Milestone 2 does the same for `review_host`, which names `daemon::RuntimeState` and `server_runtime::api::ApiBackend` in `review_host/environment.rs`, and for `pollers` with `github_watch`, `recovery` and `worker_upgrade`, which name `daemon::RuntimeSessionView` and a few daemon functions. The types they read move down; where they need the daemon's live state they ask through a trait that `RuntimeState` implements. Each becomes a crate beside `mj-server`, depending on `mj-controller` and not on each other, so the three compile at once.

Milestone 3 separates `daemon` (53,000 lines) from `server_runtime` (22,000). They refer to each other about fifty times. `server_runtime` is the implementation of the API traits over `RuntimeState`, so the direction is `server_runtime` above `daemon`: find what `daemon` takes from `server_runtime` and move it down or behind the API traits in `mj-server`. If `daemon` alone is still over the budget, its largest submodules (`continuation`, `delegation`, session lifecycle) are the next candidates.

Milestone 4 fans out `mj-controller` once `brokk-mj-store` exists. The candidate crates, each depending on the store and `mj-core` and not on each other where the references allow: targets (`targets`, `target_storage`, `streamed_child`, `image_pull_gate`, `termination`, `worker_lifecycle`, `tailscale`, the AWS modules; about 17,000 lines), accounts (`quota`, `quota_runtime`, the per-harness usage readers, `profile_catalog`, `profile_health`, `profile_scheduler`, the login modules, `github_student`, `utility_llm`; about 22,000), import (`import`, `sessionwiki`, `compaction`, `project_memory_merge`, `handoff`, `project_catalog`, `project_picker`, `session_images`; about 22,000), worker staging (`controller/worker_binary`, `controller/mbx`, `controller/tool_cache`, `controller/cache_host`; about 24,000), and session runtime (`worker_client`, `session_manager`, `checkpoint_transfer`; about 24,000). What remains in `mj-controller` is the lifecycle operations (`move_session`, `checkpoint`, `resume`, `provisioning`, `worktree`), about 70,000 lines, which then splits by operation. Start each group by listing its upward references with the compiler: move the files into a scratch crate on a throwaway branch, read the errors, and cut those references on master inside `mj-controller` before the real move.

Milestones 5 to 7 apply the same method to `mj-worker` (its `relay`, `acp` and `worker_runtime` modules are each about 25,000 lines and `microvm` is nearly independent), `mj-core` (40 of its 78 modules are mutually reachable through `config`, `targets`, `relay` and `state`; the aim is a small types crate at the very bottom so the rest can compile in parallel), and `mj-tui` and `mj-chat`.

## Concrete Steps

For each crate that is created, from the repository root:

    python3 scripts/register-crate.py --check brokk-mj-<name> --description "<one line>"
    # cut the upward references inside the old crate, commit each
    git mv <old>/src/<module>.rs <old>/src/<module> mj-<name>/src/
    # write mj-<name>/Cargo.toml and src/lib.rs, add the re-export, update the files listed in Context
    cargo check -p brokk-mj-<name> -p <old package> --all-targets
    cargo test -p brokk-mj-<name>
    cargo clippy -p brokk-mj-<name> -p <old package> --all-targets -- -D warnings
    node scripts/check-crate-boundaries.mjs && python3 scripts/check-crate-size.py
    python3 scripts/register-crate.py brokk-mj-<name> --description "<one line>"

Register the crate for real only when the commit that creates it is ready to push, since a crates.io name cannot be deleted.

## Validation and Acceptance

After each milestone, outside the sandbox: the tests of the crates involved report the same set of passed, failed and ignored tests as before the move (compare names, since some tests fail on master for unrelated reasons); `cargo clippy --all-targets -- -D warnings` is clean for those crates; `node scripts/check-crate-boundaries.mjs`, `node --test scripts/release-workflow.test.mjs` and `python3 scripts/check-crate-size.py` pass; `cargo package --locked --allow-dirty --list -p <new crate>` lists its `LICENSE` and embedded assets.

Acceptance for the whole plan: `python3 scripts/check-crate-size.py` prints no crate over 50,000 lines and its `OVER_BUDGET` set is empty; touching one file in a leaf crate and running `cargo build` leaves its sibling crates fresh; `cargo test --no-run --timings` on the same machine shows no `brokk-mj-*` unit taking more than a quarter of the baseline recorded in Artifacts.

## Idempotence and Recovery

Each cut is its own commit and leaves the workspace building, so the work can stop at any commit. A file move is one commit per crate. If master moves while a move is in progress, merge `origin/master` (do not rebase): edits to a moved file follow the rename, and a new upward reference shows up as a compile error in the new crate. `scripts/register-crate.py` reads the state of the crate first and can be run again after a failure.

## Artifacts and Notes

Milestone 1, `cargo test --no-run --timings` in a fresh clone on a 32-thread machine, third-party crates served by the mbx cache. Seconds per unit:

                                        before    after
    brokk-mj-core (lib)                   19.6     21.5
    brokk-mj-controller (lib)             24.1     28.9
    brokk-mj-server (lib)                    -      8.2
    brokk-mj-server (lib test)               -     12.8
    brokk-mj-daemon (lib test)            63.4     64.4   (56.7 when nothing else compiles beside it)
    whole build                            114      118

The whole build did not get shorter: the daemon's test unit is still the last and longest step, and it now waits for the server library first. The gain so far is that a daemon edit no longer recompiles 29,000 lines of server and its tests.

## Interfaces and Dependencies

At the end of Milestone 1: `brokk-mj-server` depends on `brokk-mj-controller`, `brokk-mj-client`, `brokk-mj-core` and the third-party crates its code names (axum, tower, tower-http, hmac and the rest); `brokk-mj-daemon` adds `brokk-mj-server`; `mj_daemon::server` is a re-export of `mj_server::server`, so `mj-cli` is unchanged. `brokk-mj-server` has a `test-support` feature if its code has test-only behaviour that `mj-daemon`'s tests rely on. No third-party dependency is added by any milestone.
