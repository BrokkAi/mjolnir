# Replace tokio with smol behind one runtime module

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir (`mj`) is async Rust on the tokio runtime. Nothing in mj needs tokio's throughput. The daemon runs tens of sessions, each a few sockets, child processes and timers. But tokio is woven through the whole workspace: about 12,500 `.await`s, about 260 `tokio::select!` sites, about 460 lines that use tokio-util's `CancellationToken`, 1,574 `#[tokio::test]` tests, and three third-party stacks that only run on tokio (axum-server for the HTTP API, reqwest for outbound HTTP, tower-http for response compression). Today the choice of runtime is made in about a thousand places. After this work it is made in one module, `mj-core/src/runtime.rs`, and that module uses smol's crates (`async-executor`, `async-io`, `async-net`, `async-process`, `async-signal`, `blocking`, `async-channel`, `async-lock`, `event-listener`, `futures-lite`) in place of tokio.

What a contributor gains, stated honestly:

- The dependency graph loses tokio, tokio-util, tokio-stream, tokio-rustls, axum-server, reqwest with its hyper client and h2, and tower-http's compression. `cargo tree --workspace --target all -i tokio` reports that no package matches.
- The `test-util` feature of tokio stops giving every test build a second tokio and a second copy of hyper, h2, axum and reqwest. `.agents/docs/build-performance-analysis.md` measured that rebuild at 85 s of CPU per test build.
- `mj-core/src/runtime.rs` loses the shutdown coordination it needs today only because tokio's timer driver must not be polled after the runtime shuts down. smol's reactor is one process-wide thread that never shuts down.
- The runtime can be changed again by editing one module.

What it does not buy: the switch does not by itself make generated code smaller. The prototype in Milestone 0 measured that smol's executor generates about 26% more code than tokio for every distinct future type it is handed. The large saving comes from boxing each future before handing it to the runtime (94% less per spawn, with either runtime). Master already does this for `spawn` and `spawn_blocking` (commit `d35e38c`, which took the `mj-daemon` test compile from 120 s to 71 s). This plan keeps and extends that boxing. Do not start this work expecting the runtime swap alone to speed up the build.

To see it working at the end: the commands in Validation and Acceptance build and test the workspace with no tokio in `Cargo.lock`. A daemon, the TUI, the web viewer and a real session run in an isolated instance. The reliability smoke scenario passes. The build timings and binary sizes are recorded next to the baseline in `.agents/docs/build-performance-analysis.md`.

## Progress

- [x] (2026-11-19) Milestone 0, prototype: measured code generated per spawned future under tokio and smol, boxed and unboxed (see Surprises & Discoveries and Artifacts).
- [x] (2026-11-19) Inventory of tokio use across the workspace (see Context and Orientation).
- [ ] Milestone 1: every runtime facility that code outside `mj-core/src/runtime.rs` uses is reached through that module, still implemented with tokio; clippy disallows the direct tokio paths.
- [ ] Milestone 2: tests run through `mj_core::runtime::test` and `test_paused` instead of `#[tokio::test]`; virtual time is owned by the runtime module.
- [ ] Milestone 3: the HTTP server is served by hyper through the runtime module (axum keeps the routes, axum-server is gone); server-sent-event keep-alive and response compression no longer need tokio.
- [ ] Milestone 4: outbound HTTP uses ureq on blocking threads; reqwest is gone from the workspace. `brokk-anvil-client` is converted and published the same way.
- [ ] Milestone 5a, prototype: smol executor, timers and the paused test clock in a scratch branch, proven on ten representative tests from `mj-daemon`, `mj-session-runtime` and `mj-worker`.
- [ ] Milestone 5: the runtime module is implemented with smol; tokio's runtime, timer, net, process, fs, io and signal are no longer used anywhere.
- [ ] Milestone 6: tokio's synchronization types and tokio-util are replaced; tokio leaves `Cargo.lock`; a CI check keeps it out.
- [ ] Milestone 7: before-and-after measurements recorded; `Outcomes & Retrospective` written.

## Surprises & Discoveries

- Observation: smol's executor generates more code per distinct spawned future than tokio, and boxing before spawning removes almost all of it in both.
  Evidence: a 200-spawn prototype (see Artifacts), debug build with `debug = "line-tables-only"`, `.text` growth divided by 200: tokio 20,893 bytes per spawn, smol (`async-executor` 1.x) 26,386, tokio boxed 1,334, smol boxed 1,443. The empty program is 589 KB of `.text` on tokio and 344 KB on smol.

- Observation: the ACP library is already runtime-neutral and already brings much of smol.
  Evidence: `agent-client-protocol` 5.0.0 depends on `futures` and `futures-concurrency`, not tokio. `cargo tree -i blocking` shows it pulling `blocking`, and through it `async-task`, `async-channel`, `event-listener` and `futures-lite`, into every workspace crate today.

- Observation: tokio's `JoinHandle` and smol's `Task` disagree on what dropping the handle means. Dropping a tokio handle detaches the task, which keeps running; dropping an `async_task::Task` cancels it. mj relies on the tokio meaning in many places and asks for cancel-on-drop explicitly with `tokio_util::task::AbortOnDropHandle` (11 sites) or `.abort()` (183 lines).
  Evidence: `grep -rE 'AbortOnDropHandle|\.abort\(\)' --include=*.rs mj-*`.

- Observation: two pieces of the HTTP server need tokio even though axum's router does not. axum's server-sent-event `KeepAlive` is compiled only with axum's `tokio` feature, and tower-http's `compression-gzip` and `compression-br` features depend on tokio and tokio-util.
  Evidence: `axum-0.8.9/src/response/sse.rs` lines 574 to 600 (`#[cfg(feature = "tokio")]`, `tokio::time::Sleep`). The `compression-br` feature in `tower-http-0.6.11/Cargo.toml` lists `"dep:tokio"`. mj applies `CompressionLayer` in `mj-server/src/server/routes.rs`.

- Observation: axum-server does more for mj than accept connections. It serves HTTP/1.1 and HTTP/2 through hyper-util's `auto::Builder`, advertises both through TLS ALPN, and reloads the certificate in place; mj uses the reload for renewed Tailscale certificates.
  Evidence: `axum-server-0.7.3/src/server.rs` (`Builder::new(TokioExecutor::new())`, `serve_connection_with_upgrades`), `axum-server-0.7.3/src/tls_rustls/mod.rs` line 292 (`alpn_protocols = vec![b"h2", b"http/1.1"]`), and `mj-daemon/src/server_runtime/args.rs` line 140 (`reload_from_pem_file`).

- Observation: `brokk-anvil-client` 0.28.6, Brokk's own crate published from a separate repository, uses async reqwest with default features. That brings tokio and also `aws-lc-sys`.
  Evidence: `cargo tree -p brokk-anvil-client -e normal` lists reqwest, hyper and tokio.

- Observation: master already boxes every spawn. `mj_core::runtime::spawn` and `mj_core::runtime::spawn_blocking` exist, and `clippy.toml` disallows `tokio::spawn` and `tokio::task::spawn_blocking` elsewhere. They still return `tokio::task::JoinHandle`, so their callers still name tokio.
  Evidence: commit `d35e38c` ("Box tasks before tokio gets them, and compile lifecycle admission once").

## Decision Log

- Decision: replace tokio with the smol family of crates, used directly rather than through the `smol` facade crate.
  Rationale: async-std, the other general-purpose runtime, is discontinued and points its users to smol. io_uring runtimes (glommio, monoio) are Linux-only, and mj ships on macOS and Windows. smol is small independent crates (`async-executor`, `async-io`, `async-net`, `async-process`, `async-signal`, `blocking`, `async-channel`, `async-lock`, `event-listener`, `futures-lite`), several of which the ACP library already compiles into every crate. Depending on them directly lets each crate take only what it uses.
  Date/Author: 2026-11-19, Ryan Svihla chose a different async library over removing async; Claude chose smol.

- Decision: build the seam first and swap second. Milestones 1 to 4 move every use of tokio behind `mj-core/src/runtime.rs` while tokio still implements it. Only Milestone 5 changes the implementation.
  Rationale: each milestone then leaves a working tree on a known runtime, and the semantic differences (task drop, timers, test time, select) are confronted in one module instead of a thousand files at once. It also follows the repository rule that one owner decides each fact: today every file decides which runtime it talks to.
  Date/Author: 2026-11-19, Claude.

- Decision: keep boxing every future and closure at the seam, in both runtimes.
  Rationale: the prototype shows the per-type code of either runtime is the expensive part; boxing removes 94% of it at the cost of one allocation per spawn (measured on master at about 80 ns per spawn and 50 ns per `spawn_blocking` in a release build).
  Date/Author: 2026-11-19, Claude.

- Decision: the seam's task handle keeps tokio's meaning. Dropping a `mj_core::runtime::JoinHandle` detaches the task. Cancel-on-drop is a separate, explicit type, `mj_core::runtime::AbortOnDrop`.
  Rationale: changing what dropping a handle means would silently cancel work that call sites today expect to continue, for example a background task whose handle is not kept. Making cancel-on-drop opt-in keeps every call site's behavior. With smol, `JoinHandle` holds an `Option<async_task::Task>` and calls `detach()` in its `Drop`.
  Date/Author: 2026-11-19, Claude.

- Decision: no attribute macro for tests. A test is a plain `#[test] fn` whose body calls `mj_core::runtime::test(async move { ... })` or `test_paused(...)`.
  Rationale: an attribute macro needs a proc-macro crate, a new workspace crate created only for syntax. A plain function also boxes the test's future, so each test binary compiles one copy of the runtime's driver loop instead of one per test (about 490 copies in the `mj-daemon` test binary today). The rewrite is mechanical.
  Date/Author: 2026-11-19, Claude.

- Decision: keep axum's `Router`, extractors and handlers, and serve the router with hyper-util's `auto` connection builder (HTTP/1.1 and HTTP/2, as axum-server does) over the runtime module's TCP stream, with TLS from `futures-rustls`. axum-server goes.
  Rationale: the 95 routes and their handlers are mj's API and are runtime-neutral. Only the accept loop, TLS, certificate reload and graceful shutdown are tokio-bound, and they are a few hundred lines that mj can own. HTTP/2 stays because browsers use it over TLS and would otherwise be limited to six connections per site, each live event stream holding one. `smol-hyper` 0.1 supplies `FuturesIo`, the adapter between hyper's I/O traits and `futures-io`. hyper-util's `server-auto` and `service` features do not depend on tokio.
  Date/Author: 2026-11-19, Claude.

- Decision: outbound HTTP uses `ureq` 3 (blocking) called through `mj_core::runtime::spawn_blocking`, not hyper's client.
  Rationale: mj's outbound HTTP is low volume (usage endpoints, GitHub, downloads of harnesses and worker binaries). ureq brings proxies from the environment, redirects, cookies, JSON, multipart and gzip without a hand-written client. Its `rustls-no-provider` feature keeps ring as the process-wide rustls provider, matching the workspace's existing rule against aws-lc.
  Date/Author: 2026-11-19, Claude.

- Decision: replace tokio's synchronization types last (Milestone 6), as a separate milestone.
  Rationale: `tokio::sync` and tokio-util's `CancellationToken` do not need tokio's runtime, so they keep working after the swap. Their replacements have different semantics in places (a tokio `Notify` stores one permit; an `event-listener` `Event` does not). Isolating them keeps those differences out of the runtime swap. A tree that stops after Milestone 5, with tokio compiled with only its `sync` feature, is a valid resting point but not the goal.
  Date/Author: 2026-11-19, Claude.

- Decision: the executor runs `min(available_parallelism, 16)` threads, the same as the tokio runtime it replaces in `mj-cli/src/main.rs` (`MAX_RUNTIME_THREADS`).
  Rationale: that cap is measured, not speculative: on a 120-CPU host idle runtime workers' wakeups cost more than they added. Keep the measured value and its comment; add no other caps.
  Date/Author: 2026-11-19, Claude.

- Decision: no wire format, stored format or protocol changes. The relay frames, the client-daemon frames, the HTTP API and the checkpoint formats stay byte for byte as they are.
  Rationale: workers outlive daemons, and old and new processes must keep talking during upgrades. This work changes only how bytes are moved. The golden tests and the upgrade regressions prove it.
  Date/Author: 2026-11-19, Claude.

## Outcomes & Retrospective

Nothing shipped yet beyond the prototype and the inventory.

## Context and Orientation

Terms used in this plan. A future is a value representing work that is not finished; `async fn` and `async { }` blocks produce futures. A runtime is the library that runs futures. It has three parts: an executor (threads that poll futures that are ready to make progress), a reactor (one place that waits on many sockets, pipes and timers at once and wakes the futures waiting on them), and a blocking pool (ordinary threads for work that would otherwise stall the executor, such as file I/O). To spawn is to hand a future to the executor as an independent task. A join handle is the value returned by spawning, through which the spawner waits for or cancels the task. Monomorphization is the compiler generating a separate copy of generic code for every concrete type it is used with; a runtime's task machinery is generic over the future type, so every distinct spawned future type gets its own copy unless the future is boxed (put on the heap behind a pointer to a trait object, `Pin<Box<dyn Future + Send>>`, so all of them share one type). Server-sent events (SSE) are an HTTP response that stays open and streams events to a browser; mj's web viewer uses them.

mj runs as three kinds of process. The client is `mj`, the CLI and the TUI dashboard, in `mj-cli/`. The daemon is `mj daemon-run`, the control plane: `mj-daemon/`, `mj-server/` (its HTTP API and web viewer), `mj-controller/` and the crates under it. The worker is `mj-worker`, one per session, in `mj-worker/` (library, package `brokk-mj-worker-runtime`) and `mj-worker-bin/` (binary). They talk in three ways. Client to daemon is a framed, authenticated protocol over loopback TCP, implemented in `mj-client/src/daemon.rs` (`read_frame`, `write_frame`, `DaemonClient::connect`) with `tokio::net::TcpStream`. Daemon to worker is the relay, a framed protocol over Unix sockets (`mj-core/src/relay/`, socket binding in `mj-core/src/local_sockets.rs`) or over SSH for remote targets. Worker to agent harness is ACP (Agent Client Protocol), JSON-RPC over the harness child process's standard input and output, in `mj-worker/src/acp/`, using the runtime-neutral `agent-client-protocol` crate. The daemon also serves an HTTP API and the web viewer with axum: `mj-server/src/server.rs` starts it with `axum_server::from_tcp` or `axum_server::from_tcp_rustls`, and `mj-server/src/server/routes.rs` builds the router and applies `tower_http::compression::CompressionLayer`.

Tokio runtimes are built in `mj-cli/src/main.rs` (`run`, a multi-thread runtime of `min(cores, MAX_RUNTIME_THREADS)` threads that every `mj` command, including the daemon, runs in), `mj-worker-bin/src/main.rs` (the worker's runtime), `mj-core/src/runtime.rs`, `mj-cli/src/dashboard/io.rs`, `mj-core/src/harness_install.rs` (current-thread runtimes for synchronous callers), and in tests. `mj-core/src/runtime.rs` is the existing runtime module. It holds `spawn` and `spawn_blocking`, which box before calling tokio; `block_on`, `off_async_worker` and `spawn_off_async_workers`, which run futures from blocking threads; and `ShutdownState` and `shutdown`, which make a runtime shut down without polling timers on a stopped driver. `clippy.toml` already disallows `tokio::spawn` and `tokio::task::spawn_blocking` outside that module.

What uses tokio today, counted with `grep -rE '<pattern>' --include=*.rs mj-*` on 2026-11-19 (lines, not uses; a file that imports a type and then uses its short name counts once):

- Synchronization: `tokio::sync::mpsc` 213 lines in 53 files, `oneshot` 178, `watch` 168, `Notify` 95 (`notify_one` 86 calls, `notify_waiters` 12), `Mutex` 50, `Semaphore` 33, `RwLock` 2; tokio-util's `CancellationToken` 463 lines in 88 files, `DropGuard` 8, `TaskTracker` 8, `AbortOnDropHandle` 11 imports.
- Time: `tokio::time::timeout` 854 lines, `sleep` 407, `Instant` 235, `interval` 35.
- Tasks: `JoinHandle` 149 lines, `JoinSet` 77, `.abort()` 183, `tokio::select!` 261 (67 of them `biased;`, about 100 branches with an `if` precondition).
- I/O: `tokio::io` 271 lines in 46 files, `tokio::net::TcpListener` 71, `TcpStream` 8, `UnixStream` 24, `UnixListener` 4, `tokio::fs` 35, `tokio::process` 67 lines in 22 files (11 in `mj-worker`), `tokio::signal` in 7 files.
- Third-party crates that only run on tokio: axum-server (`mj-server`, `mj-daemon`, `mj-cli`), reqwest (11 crates, most in `mj-accounts`, `mj-cli`, `mj-daemon`), tower-http compression (`mj-server`), and `brokk-anvil-client`.
- Tests: 1,574 `#[tokio::test]`: 1,431 plain, 110 `start_paused = true`, and 33 with a `flavor` or `worker_threads`. Virtual-time calls (`start_paused`, `tokio::time::pause`, `tokio::time::advance`) are in `mj-daemon` (70), `mj-session-runtime` (46), `mj-worker` (32), `mj-profile-catalog` (13), `mj-cli` (13), `mj-server` (12), `mj-controller` (3) and `mj-accounts` (1).
- The TUI reads terminal input with crossterm's `EventStream` (`mj-cli/src/dashboard.rs`), which is runtime-neutral.

Rules from `AGENTS.md` that constrain this work. The TUI and web control surfaces never block their event loops. Spawned task failures are reported, not dropped. Child processes run through the shared subprocess helpers in `mj-core/src/subprocess.rs`, output is drained while input is fed, and teardown kills the process group before deleting files. Daemon handoff waits only for daemon-owned work, in seconds. Tests run in isolated instances (`--instance <name>`). Every `cargo test` runs outside the sandbox.

## Plan of Work

### Milestone 0 (done): prototype

The prototype answered whether switching runtimes shrinks generated code. It does not; boxing does. Its shape is in Artifacts so it can be rerun against new versions.

### Milestone 1: one runtime module, still tokio

At the end of this milestone, no file outside `mj-core/src/runtime.rs` (and the new files under `mj-core/src/runtime/`) names `tokio::runtime`, `tokio::task`, `tokio::time`, `tokio::net`, `tokio::process`, `tokio::fs`, `tokio::signal`, `tokio::io` or `tokio::select!`. Behavior does not change. Only `tokio::sync` and tokio-util's sync types are still named directly; Milestone 6 handles them.

Turn `mj-core/src/runtime.rs` into a module with submodules: `runtime/task.rs`, `runtime/time.rs`, `runtime/net.rs`, `runtime/process.rs`, `runtime/fs.rs`, `runtime/signal.rs`, `runtime/io.rs`. Keep the existing public items where they are.

In `task.rs` define `JoinHandle<T>`. It wraps `tokio::task::JoinHandle<T>`, detaches on drop, and has `abort(&self)`, `is_finished(&self)`, and an `await` that yields `Result<T, JoinError>`. Define `JoinError`, which says whether the task panicked or was cancelled. Define `AbortOnDrop<T>`, which aborts on drop, and `JoinSet<T>` with `spawn`, `join_next`, `abort_all`, `len` and `is_empty`, the methods mj calls today. `spawn` and `spawn_blocking` return these types. Change callers from `tokio::task::JoinHandle` to `mj_core::runtime::JoinHandle`, from `tokio_util::task::AbortOnDropHandle::new(handle)` to `handle.abort_on_drop()`, and from `tokio::task::JoinSet` to `mj_core::runtime::JoinSet`. `TaskTracker` (8 lines) becomes a `runtime::TaskTracker` with the same three methods mj uses; read the call sites before writing it.

In `time.rs` define `sleep(Duration)`, `sleep_until(Instant)`, `timeout(Duration, future) -> Result<T, Elapsed>`, `interval(Duration) -> Interval` (with `tick()` and the missed-tick behavior each call site sets today; read them), and `Instant`, a type with `now()` and the arithmetic mj uses. Every one of these must read time from the runtime module, so that Milestone 2 can pause it. Where code reads `std::time::Instant::now()` to compare against a runtime deadline, change it to `runtime::Instant::now()`. The compiler finds the mismatches.

In `net.rs` define `TcpListener`, `TcpStream`, `UnixListener` and `UnixStream` wrappers that implement `futures_io::AsyncRead` and `AsyncWrite`. With tokio underneath, use `tokio_util::compat` inside the module. Change `mj-client/src/daemon.rs` and the relay code to `futures_lite::io::{AsyncReadExt, AsyncWriteExt}`. The same applies to `io.rs`: code above the seam uses `futures-io` traits, never `tokio::io`. `mj_core::local_sockets` keeps producing standard library sockets; `runtime::net` converts them.

In `process.rs` define `Command`, built from a `std::process::Command` so callers keep setting `process_group` and `pre_exec` through the standard library, and `Child` with `wait`, `kill`, `id`, and `stdin`/`stdout`/`stderr` as `futures-io` streams. Move the shared helpers in `mj-core/src/subprocess.rs` onto it first, then the other 21 files.

`fs.rs` is `spawn_blocking` around `std::fs`: write each function mj calls (`read`, `write`, `read_to_string`, `create_dir_all`, `rename`, `remove_file`, `metadata`) as one line over `spawn_blocking`. `signal.rs` covers what the 7 files use (`ctrl_c`, and on Unix `SIGTERM`, `SIGHUP` and `SIGINT` streams).

`select!` stays tokio's macro in this milestone, re-exported as `mj_core::runtime::select!`, so call sites change only their path. It is the riskiest translation and gets its own step in Milestone 5.

Extend `clippy.toml`'s `disallowed-methods` and add `disallowed-types` entries for the tokio paths above, each with a reason that names the runtime module. The one module that implements them carries the scoped `#[allow(...)]` with a reason, as `spawn` does now.

Proof: `cargo clippy --all-targets -- -D warnings` is clean; `grep -rnE 'tokio::(runtime|task|time|net|process|fs|signal|io)::|tokio::select!' --include=*.rs mj-* | grep -v 'mj-core/src/runtime'` prints nothing; every test suite passes; the reliability smoke scenario passes.

### Milestone 2: tests and virtual time through the runtime module

Add `mj_core::runtime::test<F: Future>(future: F) -> F::Output` and `test_paused`, each boxing the future and running it to completion on a fresh runtime. `test` runs a single-threaded runtime, which is what a plain `#[tokio::test]` runs today, so the 1,431 plain tests keep their scheduling. `test_paused` is single-threaded with paused time. Add `runtime::time::advance(Duration)`, `pause()` and `resume()`. In this milestone they call tokio's own paused clock, so behavior is identical.

Rewrite the 1,574 tests with a script, kept in `scripts/tokio-migration/rewrite-tests.py` until the work is done. `#[tokio::test]` followed by `async fn name() -> R { body }` becomes `#[test]`, `fn name() -> R { mj_core::runtime::test(async move { body }) }`. `start_paused = true` becomes `test_paused`. The 28 tests with `flavor = "multi_thread"` become `test_with(TestRuntime { threads: N, paused: false }, ...)`, with `N` from their `worker_threads` or, where none is given, tokio's default of one thread per CPU (`std::thread::available_parallelism`). The 5 with `flavor = "current_thread"` become plain `test`. The script must leave a `// Hard-won:` comment above the attribute, and any other comment, where it is. Run each crate's tests after rewriting it, with the same test count as before (`cargo test -p <pkg> --lib -- --list | grep -c ': test$'`).

Proof: no `#[tokio::test]` remains; test counts per crate match; the paused-time tests pass; the `mj-daemon` test binary contains one `block_on` instantiation of the test runner (`nm -C <binary> | grep -c 'CoreGuard.*block_on'` drops from about 490 to a handful).

### Milestone 3: serve HTTP without axum-server

In `mj-server/src/server.rs`, replace `axum_server::from_tcp` and `from_tcp_rustls` with an accept loop in a new `mj-server/src/server/serve.rs`, and keep everything axum-server does for mj today.

The loop takes connections from `runtime::net::TcpListener`. It serves each one with `hyper_util::server::conn::auto::Builder::new(RuntimeExecutor).serve_connection_with_upgrades(io, service)`, which is exactly what axum-server 0.7 does (`axum-server-0.7.3/src/server.rs`). That builder speaks both HTTP/1.1 and HTTP/2. HTTP/2 must stay: over TLS, browsers negotiate it, and on HTTP/1.1 a browser opens at most six connections to one site, each live event stream holds one, and several viewer tabs would stall. `io` is the stream adapted with `smol_hyper::rt::FuturesIo`, and `service` is `hyper_util::service::TowerToHyperService::new(router.clone())`. `RuntimeExecutor` implements `hyper::rt::Executor` over `mj_core::runtime::spawn`; HTTP/2 uses it to run each stream. `RuntimeTimer` implements `hyper::rt::Timer` over `mj_core::runtime::time`. Both live in `serve.rs`, so the server never names a runtime.

For TLS, wrap each accepted connection in a `futures_rustls::TlsAcceptor`, with `futures-rustls` compiled `default-features = false, features = ["ring", "tls12"]`. Build the `rustls::ServerConfig` the way `RustlsConfig::from_pem_file` does today, including `alpn_protocols = [h2, http/1.1]` (`axum-server-0.7.3/src/tls_rustls/mod.rs` line 292). `mj-daemon/src/server_runtime/args.rs` calls `reload_from_pem_file` when a renewed Tailscale certificate arrives. Keep that: hold the config as an `Arc<rustls::ServerConfig>` in a `std::sync::RwLock` that reload replaces. Each new connection reads the current one; existing connections keep theirs.

Keep the `Handle` that the daemon uses to stop the server, as a small type in `serve.rs` with the same methods (`graceful_shutdown`, `shutdown`). It stops accepting and calls `graceful_shutdown()` on every open connection. Record the open connections in one owner (a `runtime::JoinSet`), not in flags.

Turn off axum's `tokio` feature: `axum = { version = "0.8", default-features = false, features = [<every default feature mj uses except "tokio">] }`, listing them from `axum-0.8.9/Cargo.toml`'s `default`. The SSE keep-alive in `mj-server/src/server.rs`, `server/handlers.rs` and `server/api/events.rs` (`KeepAlive::default()`) becomes a stream combinator in `mj-server/src/server/sse.rs`. It merges a `runtime::time::interval` of 15 seconds, axum's default `KeepAlive` interval, emitting `Event::default().comment("")`, with the event stream, ending when the event stream ends.

Replace `CompressionLayer` with `mj-server/src/server/compression.rs`, an axum middleware that compresses with the same codecs and level as today (gzip through `flate2`, brotli through the `brotli` crate already in the tree, `CompressionLevel::Fastest`). Apply it to exactly the responses tower-http compresses today, which its `DefaultPredicate` decides: a body over 32 bytes, not `text/event-stream`, not an image other than SVG, not gRPC, and not a response that already carries `Content-Encoding`. That includes streamed bodies such as the file downloads in `mj-server/src/server/api/files.rs`, whose comment notes that compression removes `Content-Length`. So the middleware compresses a streamed body chunk by chunk as it flows (an encoder over the body stream, flushed per chunk), never by buffering it whole. The web viewer goldens, the asset tests and the file download tests prove the bytes.

Proof: `cargo tree -i axum-server` and `cargo tree -i tower-http -e features` show neither axum-server nor tower-http's compression features. `mj-server` and `mj-daemon` tests pass. In an isolated instance the web viewer loads, streams events, and serves compressed assets (`curl -sI -H 'Accept-Encoding: gzip' https://127.0.0.1:<port>/viewer.js` shows `content-encoding: gzip`), with TLS on and off. With TLS on, `curl -sI --http2 https://<host>:<port>/` reports `HTTP/2`, and replacing the certificate files while the daemon runs makes new connections present the new certificate (`openssl s_client -connect <host>:<port> </dev/null | openssl x509 -noout -enddate`).

### Milestone 4: outbound HTTP with ureq

Add `mj_core::http`, the one owner of outbound HTTP. It holds an `http::Client` built once per process from `ureq::Agent` with `rustls-no-provider` (ring is installed by `mj_server::server::install_rustls_crypto_provider` at startup, as today), the proxy from the environment (`ureq::Proxy::try_from_env()`, matching reqwest's `system-proxy` behavior), and the timeouts each call site sets today. Its async methods (`get_json`, `post_json`, `get_bytes`, `download_to_file`) run the ureq call in `runtime::spawn_blocking`. Streaming downloads write to the destination file from the blocking thread and report progress through an `async_channel`. Move each crate's reqwest calls onto it. `mj-accounts` has the most (44 lines), then `mj-cli` (34) and `mj-daemon` (20). The tests that start an axum or `tiny_http` stand-in server keep working because the wire is HTTP.

`brokk-anvil-client` is published from Brokk's separate repository. Convert it the same way there: reqwest becomes ureq with `default-features = false`, and `jsonschema` gets `default-features = false` without `resolve-http`. Publish it and raise the version in `[workspace.dependencies]`. Until it is published, Milestone 6 cannot finish, but Milestones 5 and 6 can proceed otherwise.

Proof: `cargo tree -i reqwest` shows only `brokk-anvil-client` (then nothing once it is published). Login, quota and usage readers, worker binary and harness downloads, and GitHub calls work in an isolated instance with `HTTPS_PROXY` set and unset.

### Milestone 5a: prototype the smol runtime module (prototyping)

On a scratch branch, implement `runtime/task.rs`, `runtime/time.rs` and the test runner with smol, and port ten tests that exercise the hard parts. Pick at least three paused-time tests from `mj-daemon/src/daemon/tests.rs`, two from `mj-session-runtime`, two from `mj-worker` that start a child process, and three that cancel work with `select!`. Promote the prototype if those tests pass unchanged except for the `select!` translation; otherwise record why in Surprises & Discoveries and revise this plan.

The executor is one `async_executor::Executor<'static>` in a `OnceLock`, run by `min(available_parallelism, 16)` threads that each loop on `block_on(executor.run(future::pending()))`. Timers are `async_io::Timer` when time is real. The paused clock is the hard part. tokio's paused clock advances by itself when every task is waiting and no I/O is ready, and the 110 paused tests rely on that. `test_paused` therefore runs its own single-threaded executor and drives it in a loop. It ticks the executor until `try_tick()` returns false, then lets ready I/O wake its tasks, then, if nothing ran, advances the virtual clock to the earliest registered timer and wakes it. The middle step is this prototype's main open question. tokio parks its I/O driver with a zero timeout at that point, but `async-io` has no public call that polls its reactor once without blocking. Candidates are running the loop inside `async_io::block_on`, which drives the reactor while its future is pending, or counting outstanding reactor registrations as "not idle". A fixed sleep before advancing is not acceptable; it is a guessed timeout that makes tests slow and flaky. Record what works in Surprises & Discoveries. Timers in paused mode are entries in the runtime module's own ordered map of `(deadline, waker)`, not `async_io::Timer`. Write this as a state machine with explicit states (running, idle-checking-io, advancing) in `runtime/test_clock.rs`, per `AGENTS.md`'s concurrency rules.

Also prove panics: a task that panics must make its `JoinHandle` return `JoinError::Panicked`. Use `async_task::Builder::new().propagate_panic(true)` if `async-executor` does not already, and test that a panic in a detached task is logged, not lost.

### Milestone 5: the runtime module on smol

Replace the tokio implementation of every `runtime` submodule with the prototype's. `net.rs` uses `async-net`, `process.rs` uses `async-process` (built with `async_process::Command::from(std::process::Command)`, which keeps `process_group` and `pre_exec`), `signal.rs` uses `async-signal` (on Windows it supports only `SIGINT`, which is the Ctrl+C that tokio's `ctrl_c` handles there today; check that no Windows path in the 7 signal files listens for anything else), `fs.rs` uses `blocking::unblock`, and `spawn_blocking` uses `blocking::unblock`. `block_on` is `async_io::block_on`. Delete `ShutdownState`'s timer-driver coordination and keep `ShutdownReport`, `BlockingWork` and `shutdown`. Daemon handoff still needs to wait for its own bounded work and report what it left. Reimplement them over the executor and the set of in-flight `block_on` calls, and keep the daemon handoff and restart tests green.

Translate the 261 `select!` sites from tokio's macro to `futures::select_biased!` (for the 67 `biased;` sites) and `futures::select!` (the rest), fusing each future (`.fuse()`), pinning it (`std::pin::pin!`), and replacing each `if precondition` branch with a future that is `Fuse::terminated()` when the condition is false. Do this file by file, each with its tests run. A branch that tokio's macro cancelled by dropping must still be dropped at the same point. Read each site; do not script it. Then remove the `select!` re-export.

Replace the runtime construction in `mj-cli/src/main.rs`, `mj-worker-bin/src/main.rs`, `mj-cli/src/dashboard/io.rs` and `mj-core/src/harness_install.rs` with `runtime::Runtime::new(threads)` and `runtime::block_on`.

Proof: `cargo tree --workspace --target all -i tokio -e features` shows tokio with only its `sync` feature. All suites pass on Linux; dispatch `nightly.yml` on master after landing to run macOS and Windows. The e2e scripts in Validation pass.

### Milestone 6: synchronization types, and tokio leaves

Add `mj-core/src/runtime/sync.rs` and move every user to it. `mpsc` becomes `async_channel` (bounded and unbounded). `oneshot` becomes a small `oneshot` over `async_channel::bounded(1)` that keeps the sender-dropped error. `Mutex`, `RwLock` and `Semaphore` become `async-lock`'s. `watch` becomes `runtime::sync::watch`, a value with a version counter and an `event_listener::Event`, with `send`, `send_modify`, `borrow`, `changed` and `subscribe`. `Notify` becomes `runtime::sync::Notify`, which keeps tokio's stored permit: `notify_one` with no waiter stores one permit, and the next `notified()` consumes it. Test that permit directly; it is the semantic that `event-listener` lacks. `CancellationToken`, `DropGuard` and child tokens become `runtime::sync::CancellationToken` with the same method names, so the 463 lines change only their import. `ReceiverStream` and `ReaderStream` (3 sites) become `async_channel::Receiver`, which is already a stream, and a five-line reader stream.

Remove tokio, tokio-util and tokio-stream from every `Cargo.toml` and from `[workspace.dependencies]`. Add `scripts/check-no-tokio.sh`, which fails when `cargo tree --workspace --target all -i tokio` finds the package. Run it in the CI `licenses` job beside `check-crate-boundaries.mjs`.

Proof: the check passes; `grep -c '^name = "tokio' Cargo.lock` prints 0; all suites pass.

### Milestone 7: measure

Repeat the measurements of `.agents/docs/build-performance-analysis.md` on the same kind of machine (cold `cargo build --timings`, then `cargo test --no-run --timings`, then `cargo test -p brokk-mj-tui --no-run`), plus `readelf -SW target/debug/mj` for `.text`, and record them in Artifacts with the baseline beside them. Write Outcomes & Retrospective.

## Concrete Steps

Run everything from the repository root, `/path/to/mjolnir`. Tests run outside the sandbox, and anything that starts a daemon uses an isolated instance.

Before each milestone, record the test counts so the after-state can be compared:

    for p in $(grep -h '^name = "brokk' */Cargo.toml | cut -d'"' -f2); do
      printf '%s ' $p; cargo test -q -p $p -- --list 2>/dev/null | grep -c ': test$'
    done > /tmp/test-counts-before.txt

After each file or group of files in Milestones 1 to 6:

    cargo check -p <package> --all-targets
    cargo test -p <package> <module filter>
    cargo clippy -p <package> --all-targets -- -D warnings

After each milestone:

    cargo clippy --all-targets -- -D warnings
    cargo test --no-fail-fast
    cargo build -p brokk-mjolnir
    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    tests/e2e/run-reliability.sh --scenario multi-client-happy-path --seed 1 ./target/debug/mj

The Milestone 1 seam check, expected to print nothing:

    grep -rnE 'tokio::(runtime|task|time|net|process|fs|signal|io)::|tokio::select!' --include=*.rs mj-* | grep -v 'mj-core/src/runtime'

The Milestone 6 check, expected to fail to find tokio:

    cargo tree --workspace --target all -i tokio
    error: package ID specification `tokio` did not match any packages

## Validation and Acceptance

Behavior, not compilation, is the acceptance. At the end of each milestone the following must hold, run against a build of that milestone in an isolated instance (`mj --instance notokio ...`), and the instance's daemon is stopped afterwards with `mj --instance notokio daemon stop`.

The whole test suite passes with the same test count per package as before (compare with `/tmp/test-counts-before.txt`). The test `bridge_fallback_pins_match_the_agent_dev_containerfile` and the golden tests for the web viewer, the TUI and the daemon API pass unchanged. That proves no output changed.

A real session works end to end: `mj --instance notokio new` (or the TUI) starts a session on the local target, the agent answers a prompt, the TUI shows the turn, the web viewer at the daemon's address shows the same turn live, and closing the session stops its worker and harness process group.

The reliability smoke scenario passes: `tests/e2e/run-reliability.sh --scenario multi-client-happy-path --seed 1 ./target/debug/mj`.

Upgrade and handoff hold. `python3 tests/e2e/create_survives_restart.py` and `python3 tests/e2e/fetch_restart.py` pass, and a daemon built from the milestone attaches to a worker started by a daemon from master. That shows the relay bytes are unchanged.

From Milestone 3: the web viewer streams events over SSE through a proxy that drops idle connections after 30 seconds, proving the keep-alive works, and serves gzip to a client that asks for it, with TLS enabled and disabled.

From Milestone 4: logins, quota readers and harness downloads work with `HTTPS_PROXY` pointed at a local proxy and with it unset.

From Milestone 5: dispatch `nightly.yml` on master after landing and confirm the macOS and Windows lanes pass, since check-in CI is Linux only.

## Idempotence and Recovery

Every milestone is a series of commits, each of which builds and passes its tests, so work can stop after any commit. A milestone changes how bytes move, never which bytes. Reverting a commit restores the previous behavior, and no database migration, stored format or protocol version is involved, so a reverted build talks to every daemon and worker that a non-reverted one does. The test rewrite script in Milestone 2 is idempotent: it only rewrites `#[tokio::test]`, which it removes. Run it per crate and commit per crate. If master moves during the work, merge `origin/master`, never rebase; new tokio uses that arrive by merge fail the clippy rules from Milestone 1 and are moved behind the module in the merge commit.

## Artifacts and Notes

Prototype from Milestone 0, to recreate: a binary crate with features `rt-tokio` (tokio with `rt-multi-thread`), `rt-smol` (`async-executor` 1 and `futures-lite` 2), `many` and `boxed`. With `many`, it defines 200 functions that each spawn a distinct `async move` block awaiting a small `async fn` twice. With `boxed`, the spawner boxes the future to `Pin<Box<dyn Future<Output = u64> + Send>>` before handing it over. Build in the dev profile with `debug = "line-tables-only"` and read `.text` from `readelf -SW target/debug/<bin>`.

    variant                       .text bytes   per spawn (vs. none)
    tokio, no spawns                  588,737
    tokio, 200 spawns               4,767,329   20,893
    tokio, 200 spawns, boxed          855,489    1,334
    smol, no spawns                   343,891
    smol, 200 spawns                5,621,139   26,386
    smol, 200 spawns, boxed           632,403    1,443

The spawner used in each runtime:

    // tokio
    self.0.spawn(f)          // self.0: tokio::runtime::Runtime
    // smol
    self.0.spawn(f)          // self.0: Arc<async_executor::Executor<'static>>
    // boxed, either runtime
    let b: Pin<Box<dyn Future<Output = u64> + Send>> = Box::pin(f);
    self.0.spawn(b)

Baseline to compare against in Milestone 7, from `.agents/docs/build-performance-analysis.md` (4 vCPU, no compiler cache, before `d35e38c`): cold `cargo build` 5m07s; `cargo test --no-run` after it 6m38s; `cargo test -p brokk-mj-tui --no-run` after both 110 s; `target/debug/mj` 843 MB with 191 MB of `.text`; tokio's task machinery 16.7% of `mj`'s machine code.

## Interfaces and Dependencies

New workspace dependencies, all declared once in `[workspace.dependencies]` of the root `Cargo.toml`: `async-executor = "1"`, `async-io = "2"`, `async-net = "2"`, `async-process = "2"`, `async-signal = "0.2"`, `blocking = "1"`, `async-channel = "2"`, `async-lock = "3"`, `event-listener = "5"`, `futures-lite = "2"`, `futures = "0.3"` (for `select!`), `hyper = { version = "1", features = ["http1", "http2", "server"] }`, `hyper-util = { version = "0.1", default-features = false, features = ["service", "server-auto"] }`, `smol-hyper = { version = "0.1", default-features = false }` (for `smol_hyper::rt::FuturesIo` only; the executor and timer are mj's own), `futures-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12"] }`, `ureq = { version = "3", default-features = false, features = ["rustls-no-provider", "json", "gzip", "brotli", "multipart", "cookies"] }`. Check each against the versions in `Cargo.lock` when the milestone starts and keep `cargo deny` in the `licenses` job passing. Removed by the end: `tokio`, `tokio-util`, `tokio-stream`, `axum-server`, `reqwest`, `tower-http`'s compression features.

The runtime module's public surface at the end of Milestone 1, in `mj-core/src/runtime.rs` and its submodules (names fixed; bodies change in Milestone 5):

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
        where F: Future + Send + 'static, F::Output: Send + 'static;
    pub fn spawn_blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> JoinHandle<R>;
    pub fn block_on<F: Future>(future: F) -> anyhow::Result<F::Output>;
    pub struct JoinHandle<T>;   // detaches on drop; abort(), is_finished(), abort_on_drop(); awaits to Result<T, JoinError>
    pub struct AbortOnDrop<T>;  // aborts on drop
    pub struct JoinSet<T>;      // spawn, join_next, abort_all, len, is_empty
    pub enum JoinError { Panicked(String), Cancelled }
    pub mod time { pub fn sleep(d: Duration) -> Sleep; pub fn sleep_until(t: Instant) -> Sleep;
                   pub async fn timeout<F: Future>(d: Duration, f: F) -> Result<F::Output, Elapsed>;
                   pub fn interval(d: Duration) -> Interval; pub struct Instant; }
    pub mod net { pub struct TcpListener; pub struct TcpStream; pub struct UnixListener; pub struct UnixStream; }
    pub mod process { pub struct Command; pub struct Child; }
    pub mod fs { /* async wrappers over std::fs used by mj */ }
    pub mod signal { pub async fn ctrl_c() -> std::io::Result<()>; /* Unix signal streams */ }

At the end of Milestone 2, also:

    pub fn test<F: Future>(future: F) -> F::Output;
    pub fn test_paused<F: Future>(future: F) -> F::Output;
    pub fn test_with<F: Future>(runtime: TestRuntime, future: F) -> F::Output;
    pub struct TestRuntime { pub threads: usize, pub paused: bool }
    pub mod time { pub fn pause(); pub fn resume(); pub async fn advance(d: Duration); }

At the end of Milestone 6, also `pub mod sync` with `mpsc`, `oneshot`, `watch`, `Notify`, `Mutex`, `RwLock`, `Semaphore`, `CancellationToken` and `DropGuard`, with the method names mj uses from tokio today.

Revision note, 2026-11-19: first version, written after the inventory, the Milestone 0 prototype, and reading commit `d35e38c`, which had already boxed every spawn behind `mj_core::runtime`.
