# Stop a new commit from recompiling the whole workspace

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Every build of Mjolnir is stamped with the Git commit it was built from. Today that stamp is compiled into `mj-core`, the crate every other crate depends on. A new commit therefore makes `mj-core` stale even when no source file changed, and Cargo then recompiles every crate in the workspace. "Commit, then run the tests" costs a full rebuild, about two minutes on a 32-thread machine, and the compiler cache cannot share the workspace's crates between checkouts at different commits.

After this work the stamp lives in one tiny crate that only the two programs, `mj` and `mj-worker`, depend on, and library code is handed the stamp when the program starts. A commit that changes no source rebuilds that crate and the two thin programs. To see it: build, make an empty commit with `git commit --allow-empty -m probe`, run `cargo build` again, and read the `Compiling` lines; before this work they list every `brokk-mj-*` crate, afterwards only `brokk-mj-build-id`, `brokk-mj-worker` and `brokk-mjolnir`. Nothing changes for a user: the stamp has the same values and the same meaning.

## Progress

- [x] (2026-11-18) Confirmed the cause and that Cargo recompiles dependents even when a build script reruns with identical output (see Surprises).
- [x] (2026-11-18) Milestone 1: library code reads the build identity through `mj_core::worker_build::{build_id, worker_build_id, commit_time, worker_build_stamp}`; `mj` and `mj-worker` install it first thing in `main`. Values still come from `mj-core`'s own constants, so behaviour is unchanged: `mj --version` prints the same, and the 23 tests of `mj-cli/tests/daemon_startup.rs` pass.
- [x] (2026-11-18) Milestone 2: the worker program is package `brokk-mj-worker` in `mj-worker-bin/`; the library is package `brokk-mj-worker-runtime`, still in `mj-worker/` and still named `mj_worker` in code.
- [ ] Milestone 3: the stamp and the script that computes it move to `mj-build-id`; `mj-core` no longer watches Git in a development build.
- [ ] Milestone 4: measure an empty commit before and after; record it here.

## Surprises & Discoveries

- Observation: Cargo recompiles a crate, and everything that depends on it, whenever the crate's build script reruns, even if the script prints exactly what it printed before.
  Evidence: a two-crate scratch workspace whose script prints a constant `cargo:rustc-env` and names a trigger file in `cargo:rerun-if-changed`; touching the trigger printed `Compiling a` and `Compiling b`.
- Observation: a package's library is rebuilt when any dependency of the package is, including one that only the package's program uses. So the worker library (107,000 lines) would rebuild on every commit as long as it shares a package with the worker program, and so would the tests of every crate that uses that library in its tests (`mj-controller`, `mj-daemon`, `mj-server`).
  Evidence: Cargo passes every `[dependencies]` entry to each target of the package.

- Observation: some tests rely on the stamp's bytes being in the test executable. Four `controller::move_session` tests run a child of their own test binary and name that binary as `MJ_WORKER_BINARY`; the controller then reads the worker stamp out of the file. While the stamp was a string constant referenced by library code it was linked into every test binary. Built at run time it is not, and the tests failed with "missing worker build stamp". The test fallback in `worker_build::installed` now references the constant so the bytes stay in the file.
  Evidence: `MJ_WORKER_BINARY does not match the running mj ... missing worker build stamp (legacy or invalid worker)`.
- Observation: `mj-client`, `mj-chat` and `mj-tui` tests reach the identity without `mj-core`'s `test-hooks` feature, so the test fallback has its own feature, `test-identity`, which `test-hooks` implies and those three enable in their dev-dependencies.
  Evidence: their `[dev-dependencies]` enabled only `golden`.

## Decision Log

- Decision: library code receives the identity at run time from the program, instead of each crate compiling it in.
  Rationale: anything that compiles the stamp in must be recompiled when it changes, and Cargo has no way to skip dependents whose inputs did not really change. The alternatives were weighed: computing the worker identity from the working tree instead of from `HEAD` would remove the dependence on commits but changes what the identity means (uncommitted edits would count); supplying it at link time needs weak symbols, which stable Rust lacks.
  Date/Author: 2026-11-18, Claude; Ryan Svihla asked for the rebuild to be fixed.
- Decision: a program that reads the identity before installing it stops with a clear message; a test binary, which has no `main`, gets a fixed test identity.
  Rationale: a silent default in a real program would be a daemon that cannot be ordered against another build, which is the failure the stamp exists to prevent. Tests need a value that does not change per commit, or test builds would be stale after every commit again.
  Date/Author: 2026-11-18, Claude.
- Decision: the worker library moves to a new package and keeps the library name `mj_worker`; the package `brokk-mj-worker` keeps the program.
  Rationale: `cargo install brokk-mj-worker`, the release workflow, the container image and the build scripts all build the program from that package by name. Keeping the library's name means no `mj_worker::` path changes anywhere.
  Date/Author: 2026-11-18, Claude.
- Decision: the program moved, not the library. The library stays in `mj-worker/` under the new package name `brokk-mj-worker-runtime`; `src/main.rs` and the seven integration tests that run the program moved to `mj-worker-bin/`, which carries the package name `brokk-mj-worker`.
  Rationale: `services/jev-proxy`, `tests/e2e/antigravity_owned_mcp_grants.py` and several scripts read files under `mj-worker/src/` by path, and the library is 106,000 lines against the program's 1,100. Moving the small side leaves those paths valid. The directory and package names no longer match for these two, which `CONTRIBUTING.md` states.
  Date/Author: 2026-11-18, Claude.
- Decision: the worker's ACP client name is the constant `brokk-mj-worker`.
  Rationale: `mj-worker/src/acp/session.rs` sent `env!("CARGO_PKG_NAME")` to agents as its client name. Renaming the library's package would have changed what agents and their logs see.
  Date/Author: 2026-11-18, Claude.

## Outcomes & Retrospective

Nothing shipped yet.

## Context and Orientation

A "build script" is a `build.rs` file that Cargo runs before compiling a crate; what it prints can set environment variables for that compilation, and `cargo:rerun-if-changed=<path>` lines tell Cargo when to run it again. `mj-core/build.rs` reads the current commit by following `.git/HEAD` (so it reruns whenever `HEAD` moves) and sets four values that `mj-core/src/worker_build.rs` turns into constants with `env!`:

`BUILD_ID` is the package version, `+`, and the commit hash. `BUILD_COMMIT_TIME` is that commit's time. Together they are the daemon's identity: `mj_client::build_identity::this_build()` (in `mj-client/src/build_identity.rs`) uses them so that `mj` replaces a running daemon only with a newer build. `WORKER_BUILD_ID` is the version, `+`, and a digest of the Git trees at `HEAD` of the paths listed in `mj-core/worker-build-inputs.txt`, computed by `mj-core/worker_build_inputs.py`; it changes only when the worker's own inputs change. The controller compares it with the stamp inside a worker binary to decide whether that worker matches. `WORKER_BUILD_STAMP` is that identity between NUL bytes; `mj-worker/src/main.rs` references it so the bytes are in the worker executable, where `worker_build::worker_build_from_bytes` finds them.

The same script also sets the default container image (`MJ_AGENT_DEV_IMAGE`, `MJ_AGENT_DEV_IMAGE_RELEASE`, `MJ_RELEASE_IMAGE_CARRIES_WORKER`, read in `mj-core/src/config.rs`). Those depend on the release channel (`MJ_BUILD_CHANNEL=release`, or a `.cargo_vcs_info.json` file in a crates.io package), not on the commit, except that `MJ_RELEASE_IMAGE_CARRIES_WORKER` compares the worker digest with the commit hash and is always false outside a release. With the `test-hooks` feature the script also writes a fake worker script stamped with the worker identity to `OUT_DIR/fake-worker.sh`.

About 40 places read the constants: `mj-controller/src/controller/worker_binary/` and `mj-controller/src/doctor.rs` (the expected worker identity), `mj-worker/src/microvm/`, `mj-worker/src/relay.rs`, `mj-worker/src/main.rs`, `mj-core/src/worker_launch.rs`, `mj-client/src/build_identity.rs`, `mj-cli/src/daemon/container.rs`, and tests in `mj-controller`, `mj-daemon` and `mj-cli/tests/daemon_startup.rs`.

`mj-worker/` is one package, `brokk-mj-worker`, with a library `mj_worker` and a program `mj-worker` (`src/main.rs`). `mj-controller`, `mj-daemon` and `mj-server` use the library in their tests.

## Plan of Work

Milestone 1 changes how the identity is read without changing where it comes from. In `mj-core/src/worker_build.rs` add a `BuildIdentity` struct holding `build_id`, `worker_build_id` and `commit_time` as `&'static str`, a process-wide `OnceLock` of it, `pub fn install(identity: BuildIdentity)` which fails if called twice with different values, and three accessors `build_id()`, `worker_build_id()` and `commit_time()`. An accessor called before `install` panics with "the program did not install its build identity" in a normal build; under `cfg(test)` or the `test-hooks` feature it initializes the lock from the crate's own constants instead. Replace every reader of the three constants outside this file with the accessors, call `install` first thing in `main` of `mj-cli/src/main.rs` and `mj-worker/src/main.rs` with the constants, and make the constants private to the file except `WORKER_BUILD_STAMP`, which the worker program still references. Acceptance: the workspace builds, the tests that passed before pass, and `mj --version` and `mj-worker --version` print what they printed before.

Milestone 2 separates the worker program from its library. Create `mj-worker-bin/` as package `brokk-mj-worker` with `src/main.rs` and the integration tests that run the program, depending on the library; rename the package in `mj-worker/` to `brokk-mj-worker-runtime`, keeping the library name `mj_worker`. Every manifest that names `brokk-mj-worker` for its library names the runtime instead. Add the new directory to `mj-core/worker-build-inputs.txt`, to the lists that name crates (the same list as for any new crate: root `Cargo.toml`, `.dockerignore`, `containers/Containerfile.agent-dev`, `scripts/check-crate-boundaries.mjs`, `licenses/deny.toml`, the publication order in `.github/workflows/publish.yml`, the test matrix in `.github/workflows/ci.yml`, `CONTRIBUTING.md`), and register the name with `scripts/register-crate.py`. Acceptance: `cargo build -p brokk-mj-worker --bin mj-worker` produces the same program, `scripts/build-linux-worker.sh` is unchanged, and the worker's tests run from the runtime package.

Milestone 3 moves the stamp. Create `mj-build-id/` (package `brokk-mj-build-id`) with a `build.rs` holding the Git logic from `mj-core/build.rs` (`git_revision`, `commit_time`, `worker_inputs_id`, with `worker_build_inputs.py` and `worker-build-inputs.txt` moving beside it) and a `lib.rs` exporting the three constants, `WORKER_BUILD_STAMP`, and `fn identity() -> mj_core::worker_build::BuildIdentity`. Only `mj-cli` and `mj-worker` depend on it; their `main` installs `mj_build_id::identity()`. `mj-core/build.rs` keeps the container image values and runs the Git logic only when the build is a release, where it needs the comparison and a rebuild per commit costs nothing; in a development build it prints the two fixed non-release values and watches no Git file. `mj-core`'s test identity becomes a fixed constant (the package version, `+`, forty zeros) and the fake worker fixture is stamped with it. `mj-cli/tests/daemon_startup.rs` reads the real values from `mj_build_id`. Acceptance is the empty-commit check in Purpose, and the isolated upgrade tests in `mj-cli/tests/daemon_startup.rs` pass.

## Concrete Steps

From the repository root, outside the sandbox, after each milestone:

    cargo check --workspace --all-targets
    cargo test -p brokk-mj-core worker_build
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo clippy --workspace --all-targets -- -D warnings

For Milestone 4:

    cargo build
    git commit --allow-empty -m probe
    cargo build 2>&1 | grep Compiling
    git reset --soft HEAD~1

## Validation and Acceptance

The stamp is unchanged: `mj --version` before and after each milestone prints the same string at the same commit; `strings target/debug/mj-worker | grep MJ-WORKER-BUILD` shows the same identity. `cargo test -p brokk-mjolnir --test daemon_startup` passes: those tests start real `mj` binaries in isolated instances and check that a newer build replaces an older daemon and that an older one does not. The tests of `mj-controller`'s `controller::worker_binary` module, which stage and compare worker stamps, pass. After Milestone 3 an empty commit followed by `cargo build` compiles only `brokk-mj-build-id`, `brokk-mj-worker` and `brokk-mjolnir`, and `cargo test --no-run -p brokk-mj-daemon` after an empty commit compiles nothing.

## Idempotence and Recovery

Each milestone is its own commits and leaves the workspace building and the stamp's values unchanged, so the work can stop after any of them. Milestone 1 alone is a pure refactor. If Milestone 3 has to be reverted, Milestones 1 and 2 stay valid: `main` goes back to installing `mj-core`'s constants.

## Interfaces and Dependencies

At the end, in `mj-core/src/worker_build.rs`:

    pub struct BuildIdentity {
        pub build_id: &'static str,
        pub worker_build_id: &'static str,
        pub commit_time: &'static str,
    }
    pub fn install(identity: BuildIdentity);
    pub fn build_id() -> &'static str;
    pub fn worker_build_id() -> &'static str;
    pub fn commit_time() -> &'static str;

and in `mj-build-id/src/lib.rs`:

    pub const BUILD_ID: &str;
    pub const WORKER_BUILD_ID: &str;
    pub const BUILD_COMMIT_TIME: &str;
    pub const WORKER_BUILD_STAMP: &str;
    pub fn identity() -> mj_core::worker_build::BuildIdentity;

`brokk-mj-build-id` depends on `brokk-mj-core` (for the struct) and is depended on only by `brokk-mjolnir` and `brokk-mj-worker`. No third-party dependency is added.
