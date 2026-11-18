# Fan the workspace out into many small crates under a size budget

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Building and testing Mjolnir is slow because a few crates are very large. Cargo compiles one crate as one unit, most of that work runs on a single thread, and a test build compiles a crate's code and tests together as a second unit. So the largest crate sets the floor for every build that touches it, and more cores do not help. On 2026-11-18 the workspace has twelve crates; six are over 50,000 lines of Rust and one is 249,000. Comparable Rust command-line tools have 48 to 91 crates (uv 91, ruff 55, Nushell 48), almost all far smaller.

After this work no crate is over 50,000 lines of Rust (tests included), and crates that do not need each other sit side by side so Cargo compiles them at the same time and an edit rebuilds only the crates above it. Nothing changes for a user of `mj`. A contributor sees it with `python3 scripts/check-crate-size.py`, which prints every crate's size and fails when one is over the budget, and with `cargo build --timings`, whose report in `target/cargo-timings/` shows many short `brokk-mj-*` bars running in parallel where there were a few long ones.

## Progress

- [x] (2026-11-18) Measured module sizes and the references between modules in every crate over the budget (see Context and Surprises).
- [x] (2026-11-18) `scripts/register-crate.py` registers a new crate on crates.io (placeholder, owners, Trusted Publishing), so adding a crate needs no hand work.
- [x] (2026-11-18) `scripts/check-crate-size.py` reports every crate against the budget and stops a crate under it from growing past it.
- [ ] Milestone 1: the HTTP server leaves `mj-daemon` as `mj-server` (`brokk-mj-server`).
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

## Decision Log

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
- Decision: start in `mj-daemon`, not `mj-controller`.
  Rationale: another session is moving `mj-controller`'s storage layer into `mj-store` (`.agents/plans/split-mj-controller-by-layer.md`, Milestones 3 and 4). Every other group in `mj-controller` depends on the database, so none can become a crate until the store is one, and working there now would collide with that session. `mj-daemon` is finished from that plan's point of view and is the second largest crate.
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

Baseline timings are recorded here when Milestone 1 lands, from `cargo test --no-run -p brokk-mj-daemon --timings` in a fresh clone before and after.

## Interfaces and Dependencies

At the end of Milestone 1: `brokk-mj-server` depends on `brokk-mj-controller`, `brokk-mj-client`, `brokk-mj-core` and the third-party crates its code names (axum, tower, tower-http, hmac and the rest); `brokk-mj-daemon` adds `brokk-mj-server`; `mj_daemon::server` is a re-export of `mj_server::server`, so `mj-cli` is unchanged. `brokk-mj-server` has a `test-support` feature if its code has test-only behaviour that `mj-daemon`'s tests rely on. No third-party dependency is added by any milestone.
