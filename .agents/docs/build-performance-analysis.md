# Why the Mjolnir build is slow and large

Measured 2026-11-19 on commit `1766637`, on a 4-vCPU, 15 GB Linux container with
rustc 1.99.1, no compiler cache, dev profile. Absolute times scale with the
machine; the ratios and the causes do not.

## Summary

The build is slow for three reasons, in order of how much agent time they cost:

1. **Feature variants.** Test-only Cargo features (`test-hooks`, `test-support`,
   `golden`, `test-identity`), `github-history`, and tokio's `test-util` make
   almost every different `cargo` command resolve a different feature set. Each
   feature set is a separate compilation of `mj-core` and every crate above it,
   stored as a separate copy on disk. A test build after a full `cargo build`
   reuses almost none of the workspace. Switching `cargo test -p` from one
   crate to another with everything already built still costs 70 to 110
   seconds.
2. **Generated code volume.** Most compile time in the large units is code
   generation, not parsing or type checking. About 40 to 45 percent of the
   machine code comes from async state machines and tokio's per-spawned-future
   task machinery, and about 22 to 24 percent from serde. One unit, the
   `mj-daemon` library test, takes 234 s and is 60 percent of the test build's
   wall time.
3. **Static linking of many large executables.** There are 46 executables over
   50 MB, totalling 11.3 GB. The debug `mj` is 843 MB, the daemon test binary
   945 MB. Feature variants keep a second build of both `mj` and `mj-worker`.

Splitting crates by line count does not address any of these.
`.agents/plans/fan-out-crates-under-size-budget.md` records it already:
after Milestone 1 the whole test build went from 114 s to 118 s. Each split
also adds `test-hooks`/`test-support` plumbing, which feeds cause 1. The one
change in that work that clearly paid off was taking the commit hash off the
compile path (83 s to 12 s after a commit).

## Measurements

| Step (run in this order) | Wall | Notes |
|---|---|---|
| Cold `cargo build` | 5m07s | target/ 8.7 GB (deps 4.2 GB, incremental 4.0 GB) |
| `cargo test --no-run` right after | 6m38s | target/ grows to 32 GB (deps 16 GB, incremental 15 GB) |
| `cargo test -p brokk-mj-tui --no-run` after both | 110 s | recompiled about 100 crates, including libc, serde, tokio, rustls, mj-core |
| `cargo test -p brokk-mj-store --no-run` | 88 s | mj-core, tokenizers, rusqlite and the chain above |
| `cargo test -p brokk-mj-targets --no-run` | 72 s | mj-core, mj-transcript, mj-client |
| `cargo test -p brokk-mj-client --no-run` | 99 s | also built mj-worker (107K lines) through a dev-dependency |
| `touch mj-core/src/lib.rs; cargo build` | 77 s | all 27 workspace crates rerun with no content change |
| `touch mj-daemon/src/lib.rs; cargo test -p brokk-mj-daemon --no-run` | 26 s | incremental works when only the top crate changes |

### Cold build critical path

    aws-lc-sys build script (C)      16.6 →  71.9   55.3 s
    rustls, reqwest                  72   →  81.8
    brokk-mj-core                    79.4 → 138.6   59.2 s
    transcript, client, targets, store, accounts, import, sessionwiki,
      session-runtime, controller, server (chain, partly pipelined)
    brokk-mj-daemon                 208.9 → 275.5   66.6 s
    mj binary                       275.5 → 306.8   31.2 s

`aws-lc-sys` should not be built at all. The workspace manifest says it uses
ring "instead of aws-lc-rs, which needs a C toolchain". But
`brokk-anvil-client` 0.28.6 depends on `reqwest` and `jsonschema` with their
default features, which turn on `rustls/aws-lc-rs`. That puts a 55 s C build in
front of `mj-core` on every cold build.

`tikv-jemalloc-sys` takes 96 s, the longest single unit, but it is off the
critical path. It only costs CPU on a 4-core machine.

### The test build after a full build

`cargo test --no-run` compiled 151 units:

- 71 third-party units (85 s): tokio's `test-util` feature, from 17
  dev-dependency lines, gives a second tokio, so hyper, h2, axum and reqwest
  compile again.
- 30 workspace library units (504 s): every library that `cargo build` had
  just built, compiled again because its feature set differs.
- 46 test targets (927 s): the only work that has to happen.

The critical path was 164 s of library rebuilds, then the 234 s `mj-daemon`
library test unit. Without the variants, that unit could start at once.

### Feature sets of `mj-core` by command

`cargo tree -p <pkg> -i brokk-mj-core -f '{f}'` gives at least seven
different builds of `mj-core`, and each one rebuilds everything above it:

    cargo build (default members)        default, github-history
    test -p mj-core                      default
    test -p mj-checkpoint / mj-review    golden
    test -p mj-chat / mj-client / mj-tui golden, test-identity
    test -p mj-targets                   golden, test-hooks, test-identity
    test -p mj-transcript                github-history, golden
    test -p (most other crates), --workspace
                                         github-history, golden, test-hooks, test-identity

What those features gate across the workspace: `golden` 4 sites, `test-identity`
5, `test-hooks` 21, `github-history` 4. A few dozen lines decide whether about
900,000 lines are compiled once or seven times. `test-support` (204 sites in 75
files across 15 crates) does the same for each crate above `mj-targets`.

The repository's own instructions make this worse. AGENTS.md asks for
`cargo test -p <crate> <filter>` and `cargo clippy -p <crate> --all-targets`
per round. Those are exactly the commands that land on a new feature set.

Cargo has `resolver.feature-unification = "workspace"`, which would remove
the per-`-p` variation, but it is unstable in cargo 1.99 and ignored on
stable. It also would not unify `cargo build` with `cargo test`, because
dev-dependency features apply only to test builds.

### Dev-dependency back-edges

Several low crates' tests depend on high crates:

- `mj-client`, `mj-chat`, `mj-worker-staging` (dev) → `mj-worker` (107K lines)
- `mj-worker`, `mj-worker-bin` (dev) → `mj-controller` and everything below it

So testing a small crate compiles a large one, in that crate's feature variant.
The crate graph says these crates are independent; the test graph says they
are not.

### What the machine code is made of

Symbols of the debug `mj` (185 MB of function code, 865K symbols) and of the
`mj-daemon` test binary (194 MB), grouped by what generated them. The first
matching rule wins, so the categories are approximate:

| Source | `mj` | daemon tests |
|---|---|---|
| async state machines (`{{closure}}` bodies) | 22.1% | 26.1% |
| serde impls, visitors, serde_json | 21.5% | 24.4% |
| tokio task machinery, instantiated per spawned future type | 16.7% | 19.0% |
| drop glue | 1.8% | 2.1% |
| everything else | 37.9% | 28.4% |

- tokio's task harness (`poll_future`, `set_stage`, `take_output`, …) is
  instantiated once for each distinct future type passed to `spawn`. There are
  about 2,800 in `mj`, roughly 11 KB of code each, plus about 1,280
  `spawn_blocking_inner` copies.
- Each `#[tokio::test]` instantiates the runtime's `block_on` for its own
  future: about 490 copies in the daemon test binary.
- About 11 MB of serde code is `Content` buffering, which serde generates for
  internally tagged, untagged, and `#[serde(flatten)]` types. The largest
  contributors are `mj_core::relay::snapshot`, `mj_client::daemon::DaemonAction`,
  the ACP schema, `mj_core::relay::protocol`, and `mj_core::config`.
- `.strtab` is 230 MB in `mj`, larger than `.text` (191 MB): symbol names of
  deeply nested generic and future types.

## Does mj need tokio?

Not for throughput. mj runs dozens of sessions, each a few sockets, child
processes and timers; threads and blocking I/O would handle that load.

Removing it would be a rewrite, though, not a build fix. The workspace has
about 12,500 `.await`s, about 275 `select!`s (much of the cancellation
logic), about 430 `spawn_blocking`s, the axum HTTP API and web viewer
(hyper/tokio), and reqwest. Every ownership and cancellation decision in the
daemon and worker would have to be redone. The ACP crate itself does not
require tokio.

What costs build time is not the tokio library, which compiles once in about
5 s. It is the per-type code that async generates. Most of that can be cut
while keeping tokio:

- Route spawns through one helper that boxes the future
  (`Pin<Box<dyn Future<Output = T> + Send>>`). That collapses about 2,800
  task-harness copies into a handful. Do the same for `spawn_blocking` with a
  boxed `FnOnce`.
- Replace `#[tokio::test]` with one shared runner that takes a boxed future,
  so each test binary has one `block_on`, not hundreds.
- Keep async at the edges that require it (HTTP server, HTTP client, ACP
  transport, relay sockets). Write new filesystem, git, and process code as
  plain synchronous code on threads rather than `async fn` plus
  `spawn_blocking`.

## What to do, in order

Each step is measurable with the commands above. Re-run them after each one.

1. **Make every command resolve the same feature set.** This has the
   largest payoff, it is mechanical, and it changes no behavior.
   - `golden`, `test-identity`: compile the code always, as
     `#[doc(hidden)] pub`. The linker drops it from binaries that do not call it.
   - `test-hooks`: give the libraries a runtime-installed hook, the way the
     build identity is installed at program start. Only the `mj` binary crate,
     a leaf, decides whether to install it, so release binaries stay
     hook-free and nothing below `mj-cli` varies.
   - `test-support`: compile fixture modules unconditionally as
     `#[doc(hidden)] pub mod test_fixtures`. Move test-only behavior, such as the
     `mj-server` counters, behind runtime state or into the leaf crate.
   - `github-history`: its only production caller is
     `mj-daemon/src/github_watch/service.rs` (`build_evidence`), yet eight
     manifests enable it (`mj-controller`, `mj-store`, `mj-import`,
     `mj-session-runtime`, `mj-sessionwiki`, `mj-server`, `mj-daemon`,
     `mj-github-relevance`). The crate splits copied the dependency line along
     with the code. Move the tokenizer and history evidence out of `mj-core` and
     `mj-transcript` into `mj-github-relevance`, which already depends on
     `tokenizers`, and delete the feature.
   - tokio and the other shared dependencies: put the union of features,
     including `test-util`, on the `[workspace.dependencies]` entry, and stop
     adding features per crate.
   - Then add a CI check that `cargo tree -p <pkg> -e normal,dev -i <crate> -f '{f}'`
     prints the same features for every package and for `cargo build`.

   Expected effect, from the measurements above: a test build after a build
   compiles only test targets (−504 s CPU of library rebuilds, −85 s of
   third-party rebuilds, and the 164 s wait before the daemon test unit
   starts). Switching `-p` targets costs only that crate's test unit instead
   of 70 to 110 s. target/ loses about one full copy of the workspace.
2. **Remove the dev-dependency back-edges** listed above. Then a small
   crate's tests stop building `mj-worker` or `mj-controller`.
3. **Fix `brokk-anvil-client`'s features** (separate repository): use
   `reqwest` and `jsonschema` with `default-features = false` and
   `rustls-no-provider`. That removes `aws-lc-sys` and saves about 55 s from
   every cold build's critical path.
4. **Cut generated code** in the `mj-daemon`, `mj-worker` and `mj-core`
   test units, choosing changes by measured code volume, not line count.
   Use the boxed spawn helper, the shared test runner, and fewer serde
   `flatten`/untagged/internally tagged types on the largest types above.
   Measure with `cargo llvm-lines` on the unit before and after.
5. **Fewer, smaller executables.** Merge `mj-cli`'s 10 integration-test files
   and `mj-worker-bin`'s 7 into one test binary each (`tests/it/main.rs`).
   That saves about 15 links and about 3 GB. Consider
   `[profile.dev.package."*"] debug = false` for third-party crates after
   checking that dependency frames in backtraces are not needed.
6. **Pause the crate fan-out** until step 1 lands and the table above has
   been re-measured. More crates add per-invocation fixed cost (the no-op
   `mj-core` touch reran 27 crates in 77 s) and more feature plumbing.
   Splitting still makes sense where it removes a real dependency, as
   `mj-github-relevance` did for ONNX Runtime and tokenizers, or where it
   takes a large codegen unit off the critical path.

## How these numbers were taken

    cargo build --timings                      # cold
    cargo test --no-run --timings              # right after
    cargo test -p <pkg> --no-run               # per package, after both
    cargo tree -q -p <pkg> -i brokk-mj-core -f '{f}' --prefix none --depth 0
    nm -S --defined-only -C target/debug/mj    # grouped by generic function
    readelf -SW target/debug/mj                # section sizes

Unit durations and the critical path come from the `UNIT_DATA` table in
`target/cargo-timings/cargo-timing-*.html`. Each unit's `unblocked_units`
names the units it released.
