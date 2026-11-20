# Async runtime, part 1: the core of `go/crates/tokio`

Read `.agents/docs/go-port/tokio-contract.md` twice: it is the specification,
and it is not negotiable. Then rule 6 of the translation rules, to see how
product code calls the package. Oracle module `go/port/oracle/src/tokio_oracle.rs`,
feature `tokio`. Reference: tokio 1.52.3 (`~/.cargo/registry/src/*/tokio-1.52.3/`,
with its 170 test files under `tests/`).

Every one of Mjolnir's 3,500 async functions runs on this, and 1,574 tests
depend on its scheduling being tokio's. Fidelity comes before everything; take
the time the design needs. Two rules from the contract that are easy to get
wrong: a function takes `ctx` if and only if it is `async` in Rust (so
`tokio.Spawn(fn)`, `tokio.Start(fn)`, `TimeInstantNow()`, channel constructors,
`TrySend`/`TryRecv`, `Abort()` and so on take no `ctx` and find the runtime
through the goroutine table); and `tokio.Start(fn)` models `tokio::pin!`: it
creates the pinned future without running it, and the future is polled when
awaited or selected.

Scope (I/O, net, process, fs, signal and tokio-util come later, built by
others on your internals):

1. The machinery: tasks, sub-futures with their own cancellation scopes, one
   goroutine of a task running at a time, await points (cancellation check and
   `runtime.Goexit()`, immediate completion, waker registration and parking),
   the cooperative budget, panics recovered at the task root into `JoinError`,
   the goroutine table (ids from `runtime.Stack`) behind
   `RuntimeHandleCurrent()`, `RuntimeHandleTryCurrent()`, task-locals and
   thread-locals, and the `mj_debug` check that an await point's `ctx` belongs
   to the calling goroutine.
2. Both flavours: current-thread with the run token and tokio's exact queue
   order (`src/runtime/scheduler/current_thread/mod.rs`: where spawned and
   woken tasks go, `yield_now` and the defer list, when the root future is
   polled, the event interval, wakes from other threads), and multi-thread.
3. `Runtime`, `Builder`, `Handle`, `EnterGuard`, the three shutdown forms,
   `block_on` (including `Handle::block_on` from a `spawn_blocking` thread, as
   `mj-core/src/runtime.rs` does), `spawn_blocking`, `block_in_place`.
4. `Fn`, `Future`, `Await`, `Start`/`Started`, `Ready`, `Pending`, `Select`
   with every arm form and tokio's polling, disabling, `else` and drop order,
   `Join2..5`, `TryJoin2..5`.
5. Tasks: `Spawn`, `TaskJoinHandle`, `TaskJoinError` (tokio's `Display` and
   `Debug`), `TaskAbortHandle`, `TaskJoinSet` (all of it; drop aborts),
   `TaskId`, `TaskYieldNow`, `TaskLocalKey`.
6. Time: the runtime clock, the timer queue with millisecond rounding,
   `TimeSleep`, `TimeSleepUntil`, `TimeTimeout`, `TimeTimeoutAt`,
   `TimeInterval`/`TimeIntervalAt` with the three `MissedTickBehavior`s and
   `reset*`, `TimeInstant`, `TimePause`, `TimeAdvance`, `TimeResume`,
   auto-advance with tokio's inhibition rules, `TimeErrorElapsed`.
7. Sync: bounded and unbounded `mpsc` (capacity, permits, closing from either
   side, `closed()`, sender clones and drops, `recv_many`, try and blocking
   forms, fairness, error text), `oneshot`, `watch` (versions, `changed`,
   `borrow_and_update`, `has_changed`, `wait_for`, `send_modify`,
   `send_if_modified`, `send_replace`, `subscribe`, `mark_*`, closing, receiver
   counts), `Notify` (permits, `notify_one`, `notify_waiters`, `notify_last`,
   and `Notified::enable`, which `mj-core/src/runtime.rs` relies on),
   `Semaphore` (FIFO, owned forms, `add_permits`, `close`, `forget`), `Mutex`
   and `RwLock` with owned guards, `OnceCell`, `Barrier`. Grep `mj-*/` for every
   method used. No `broadcast`.
8. `tokio.Test` with `TestOptions`, and helpers tests need to observe
   scheduling.
9. The leak safety net (`runtime.AddCleanup` on handles whose drop matters)
   with its `MJ_RS_LEAK_CHECK=1` report.

Write `go/crates/tokio/ARCHITECTURE.md` for the people who build I/O,
tokio-util and futures on this: how to implement a new awaitable resource, how
sub-futures are polled, how the run token moves, how the clock works, and the
invariants. Keep it accurate.

Tests are the point:
- Port tokio's tests for everything covered: at least `sync_mpsc`,
  `sync_oneshot`, `sync_watch`, `sync_notify`, `sync_semaphore`,
  `sync_semaphore_owned`, `sync_mutex`, `sync_mutex_owned`, `sync_rwlock`,
  `sync_once_cell`, `sync_barrier`, `sync_errors`, `macros_select`,
  `macros_join`, `macros_try_join`, `macros_pin`, `task_abort`,
  `task_join_set`, `task_local`, `task_id`, `task_yield_now`, `task_blocking`,
  `task_panic`, `rt_basic`, `rt_common` (both flavours), `rt_handle`,
  `rt_handle_block_on`, `rt_time_start_paused`, `rt_panic`, `time_pause`,
  `time_interval`, `time_sleep`, `time_timeout`, `time_rt`, and the unit tests
  in `src/` for the pieces ported. Build the equivalent of
  `tokio_test::task::spawn` and `assert_pending!`/`assert_ready!` over the
  sub-future machinery so those tests port faithfully.
- Scheduling-order tests against the oracle: programs whose event trace is
  fully determined in tokio, implemented in Rust (oracle) and Go, requiring
  identical traces: spawn order, wake order, `yield_now`, biased and unbiased
  select, cancellation by abort and by losing a select (destructors run, code
  after the await does not), nested selects, `JoinSet`, budget exhaustion,
  paused-clock programs, and a `spawn_blocking` thread calling
  `Handle::block_on`.
- Multi-thread stress tests for every primitive, run repeatedly: no lost
  wake-ups, no deadlocks, `runtime.NumGoroutine()` back to baseline after a
  runtime is dropped.

Priority if the scope cannot be finished in one go: machinery, both flavours,
Select/Join/Start, tasks and JoinSet, time, mpsc/oneshot/watch/Notify, then the
rest of sync. Never leave a primitive that looks finished and is not.
