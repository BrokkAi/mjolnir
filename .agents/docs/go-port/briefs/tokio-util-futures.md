# Async runtime, part 3: tokio-util, futures, tokio-stream, rayon

Packages `go/crates/tokioutil` (tokio-util 0.7.18), `go/crates/futures`
(futures 0.3.32), `go/crates/tokiostream` (tokio-stream 0.1.19),
`go/crates/rayon` (rayon 1.12.0). Oracle module `tokio_oracle.rs`, feature
`tokio` (it has all four crates). Start only after the tokio core exists: read
`go/crates/tokio/ARCHITECTURE.md` and
`.agents/docs/go-port/tokio-contract.md` first, and build on the core's
future, waker and scope machinery rather than on raw goroutines and channels.

Grep the Rust sources for what is used and cover all of it.

- `tokioutil`: `sync::CancellationToken` (about 470 uses): `new`, `cancel`,
  `is_cancelled`, `cancelled()` as both `Cancelled(ctx)` and
  `CancelledFuture()` (resolving also when the token was cancelled earlier),
  `cancelled_owned`, `child_token` (cancelled with its parent, not the
  reverse), `clone`, `drop_guard` and `DropGuard` (`Drop()` cancels,
  `disarm`), `run_until_cancelled` if used. `sync::PollSemaphore` and
  `ReusableBoxFuture` only if used. `task::TaskTracker` (21 uses: `spawn`,
  `close`, `wait`, `is_closed`, `len`), `task::AbortOnDropHandle` (25 uses),
  `task::JoinMap` if used. `io` (`ReaderStream`, `StreamReader`, `SyncIoBridge`
  as used), `compat` (the futures-io adapters used), `codec` (`LinesCodec`,
  `Framed*` as used).
- `futures`: `future::{join_all, try_join_all, select_all, select, Either,
  ready, pending, poll_fn, BoxFuture, FutureExt (boxed, map, then, shared,
  now_or_never, catch_unwind, fuse), Shared}`, `stream::{Stream, StreamExt
  (next, map, filter, filter_map, then, collect, for_each, buffer_unordered,
  buffered, take, skip, chain, fuse, boxed, peekable, flat_map), iter, unfold,
  once, FuturesUnordered, FuturesOrdered, BoxStream}`, `channel` if used,
  `executor::block_on`, `poll!`, `pin_mut!`, `select!`/`join!` if the futures
  versions (not tokio's) are used anywhere. A `Stream` is an interface with
  `Next(ctx) rs.Option[T]` and a `NextFuture()` for select arms. `Shared`
  runs its future once and gives every clone the result; note the timing rule
  recorded in the owner's notes: the body runs at first poll, not at
  construction. `join_all` and `FuturesOrdered` keep input order;
  `FuturesUnordered` and `buffer_unordered` yield in completion order, with
  tokio/futures' polling order deciding ties on a current-thread runtime.
- `tokiostream`: `StreamExt` methods used (`next`, `timeout`, `merge`,
  `throttle` as used), `wrappers::{ReceiverStream, UnboundedReceiverStream,
  WatchStream, IntervalStream, LinesStream, ...}` as used.
- `rayon`: `join`, `par_iter().map(..).collect()` and the other adapters used
  (results in input order), `ThreadPoolBuilder` (`num_threads`, `build`,
  `install`), `current_num_threads`. Panics in workers propagate to the caller
  as rayon's do.

Tests: port each crate's tests for what you cover
(`tokio-util/tests/sync_cancellation_token.rs`, `task_tracker.rs`,
`abort_on_drop.rs`, `mpsc.rs`; the futures-util tests for the combinators
used), and trace tests against the oracle for the current-thread ordering of
`FuturesUnordered`, `buffer_unordered`, `select_all`, `Shared` with several
waiters, and cancellation propagation through child tokens.
