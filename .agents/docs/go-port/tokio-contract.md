# The async runtime: how `go/crates/tokio` reproduces tokio

Mjolnir's Rust code has 3,500 async functions, 265 `select!` blocks, 1,574
`#[tokio::test]` tests (111 of them on a paused clock) and 131 destructors
whose effect depends on where a future stops. Goroutines with `context` and
channels do not behave like that: goroutines run in parallel where a
current-thread tokio runtime interleaves tasks only at awaits; a cancelled
`context` is a flag to poll where a dropped future simply stops at its await
and runs its destructors; Go channels close, queue and wake differently from
tokio's; Go's clock cannot be paused.

`go/crates/tokio` is therefore not a set of wrappers. It is a small runtime
with tokio's observable behaviour, on which product code is translated call
for call (`.agents/docs/go-port/translation-rules.md`, rule 6). This document
fixes the model and the interface. The implementation follows tokio 1.52.3's
source (`~/.cargo/registry/src/*/tokio-1.52.3/src/`) for every rule not
spelled out here.

## The model

### Tasks, futures and the unit of scheduling

A **task** is what `tokio::spawn` creates, and the root future a runtime's
`block_on` drives is treated the same way. A task is run by goroutines, but
at most one goroutine of a task makes progress at a time, exactly as one
thread polls a task at a time.

A **future** in product code is a `tokio.Fn[T]`: `func(ctx context.Context) T`.
It is lazy; calling it is awaiting it. A future that must make progress while
its owner does something else (an arm of a `select!`, a `join!` operand, a
future pinned and polled in a loop, the body of a `timeout`) is run as a
**sub-future**: its own goroutine inside the same task, with its own
cancellation scope, started by `tokio.Start`, `tokio.ArmFn`, `tokio.Join*` or
`tokio.TimeTimeout`. The owner and its sub-futures take turns: the owner
"polls" a sub-future by letting its goroutine run until that goroutine parks
at an await or returns, which is what one call to `Future::poll` does.

`ctx` identifies the running future: it carries the task, the cancellation
scope and the runtime. It is created by the runtime and passed down; product
code never builds, stores or inspects one.

### Await points

An **await point** is any call into the runtime that can wait: receiving,
sending on a full channel, acquiring, sleeping, joining, yielding, I/O,
`tokio.Select`. At an await point the runtime does, in order:

1. If the current scope has been cancelled (its future was dropped: the task
   was aborted, another `select!` arm won, a `timeout` fired, the owner
   called `Drop()` on a started future, the runtime is shutting down), the
   goroutine ends here with `runtime.Goexit()`. Its deferred calls run, in
   order, and nothing after the await point executes. This is Rust's
   drop-at-await, and it is why every destructor in the port is a `defer`.
   `Goexit` is not a panic: `recover` and `rs.CatchUnwind` do not see it.
2. If the operation can complete now, it completes and returns without
   yielding, unless the task's cooperative budget is spent (below).
3. Otherwise the goroutine registers a waker with the resource and parks.
   When woken it starts again at step 1.

An operation that is abandoned at step 1 or while parked has had no effect: a
`recv` that was cancelled took no message, as tokio's cancel-safe operations
guarantee. Operations tokio documents as not cancel-safe behave as tokio's do.

Tokio's cooperative budget is reproduced (`src/task/coop/`): each task gets
128 units when it is polled, the operations that consume budget in tokio
consume it here, and an operation that finds the budget spent yields the task
to the back of the run queue before completing.

### Flavours

**Current-thread** (`Builder::new_current_thread`, and the default of
`#[tokio::test]`): the runtime has one run token. A goroutine belonging to the
runtime runs only while it holds the token, takes it in the order tokio's
current-thread scheduler would poll (`src/runtime/scheduler/current_thread/`:
the run queue order for spawned and woken tasks, where `yield_now` places a
task, the root future's turn, the event interval), and gives it up when it
parks or finishes. Between two await points a task runs without any other
task of the runtime running, so code and tests that rely on "the task I
spawned has not run yet" behave as they do in Rust. Blocking the goroutine in
a plain system call or a `std` lock holds the token, as blocking the only
thread does.

**Multi-thread** (`Builder::new_multi_thread`, `flavor = "multi_thread"`):
tasks run in parallel as goroutines and there is no run token. The rule that
one goroutine of a task runs at a time still holds.

`spawn_blocking` work runs on its own goroutine outside the token.
`block_in_place` runs its function in place on the multi-thread flavour and
panics with tokio's message on the current-thread flavour.

### The runtime context of a goroutine

Rust finds "the current runtime" in a thread-local. Async functions here get
it from `ctx`. Synchronous code that tokio lets reach the runtime through the
thread-local (`Handle::current()`, `Handle::try_current()`, `tokio::spawn`
from a non-async function, `time::pause`, a `Drop` that spawns) uses
`tokio.RuntimeHandleCurrent()` and `tokio.RuntimeHandleTryCurrent()`, which
look the calling goroutine up in a table the runtime keeps of the goroutines
it started (keyed by goroutine id, read from `runtime.Stack`). The same table
backs `tokio.TaskLocalKey` (`task_local!`), `rsthread.LocalKey`
(`thread_local!`) and the tracing span stack. `Runtime.Enter()` registers the
calling goroutine for the life of its guard.

In builds with the `mj_debug` tag every await point checks that the `ctx` it
was given belongs to the goroutine calling it, and panics if not. A wrong
`ctx` (one captured from an enclosing function instead of the closure's own)
is the most likely mistake in translated code, and this turns it into an
immediate failure in tests.

### Panics

A panic in a task does not end the process. The runtime recovers it at the
task's root, runs the panic hook (`rs.RunPanicHook`), and resolves the task's
`JoinHandle` with a `JoinError` for which `IsPanic()` holds, with tokio's
`Display` text. A panic in a sub-future propagates to the owner that polls
it, as a panic inside `poll` does.

### Time

The runtime owns its clock. `tokio.TimeInstantNow`, `TimeSleep`,
`TimeTimeout`, `TimeInterval` read and wait on it. Timers have tokio's
resolution: a deadline is rounded up to the next millisecond as tokio's timer
wheel does. With the clock paused (`start_paused`, `time::pause`) time moves
only by `TimeAdvance`, or automatically to the next timer when the runtime
has nothing runnable, under tokio's rules (`src/time/clock.rs`: auto-advance
is held off while blocking tasks the runtime is waiting for are running, and
after `advance`). `TimeAdvance` yields as tokio's does. Pausing is allowed on
the current-thread flavour only, with tokio's panic text otherwise.

### Shutdown

Dropping or shutting down a runtime cancels every task at its next (or
current) await point, runs their deferred calls, and then waits for blocking
tasks according to the method used (`Drop`, `ShutdownTimeout`,
`ShutdownBackground`), as tokio does.

## The interface

Names follow the port's rule over the path Rust code writes. Each awaitable
operation has two forms: a method taking `ctx` that waits (what `.await`
translates to), and a `...Future()` method returning a `tokio.Future[T]` for
use as a `select!` arm or anywhere a future value is needed.

Core:

    type Fn[T any] func(ctx context.Context) T
    type Future[T any] interface{ ... }            // something Select can poll
    func Await[T any](ctx context.Context, f Future[T]) T
    func Start[T any](ctx context.Context, fn Fn[T]) *Started[T]   // a pinned, running future; Started[T] is a Future[T]; Drop() cancels and waits
    func Ready[T any](value T) Future[T]
    func Pending[T any]() Future[T]

Select (`select!`), returning the index of the arm that completed:

    func Select(ctx context.Context, mode SelectMode, arms ...SelectArm) int
    const SelectUnbiased, SelectBiased SelectMode
    func Arm[T any](f Future[T], out *T) SelectArm
    func ArmIf[T any](enabled bool, f Future[T], out *T) SelectArm
    func ArmMatch[T any](f Future[T], out *T, matches func(*T) bool) SelectArm   // a refutable pattern
    func ArmFn[T any](fn Fn[T], out *T) SelectArm
    func ArmFnIf[T any](enabled bool, fn Fn[T], out *T) SelectArm
    func ArmFnMatch[T any](fn Fn[T], out *T, matches func(*T) bool) SelectArm
    func ArmElse() SelectArm

`Select` polls as the macro does: all enabled arms in declaration order when
biased, from a pseudo-random start otherwise; an arm whose pattern does not
match is disabled and the others continue; when every arm is disabled the
`else` arm is taken, and without one it panics with tokio's message. `out`
may be nil when the arm's value is not used. Futures created for the arms
that did not win are dropped in declaration order before `Select` returns. A
`Started` future passed to `Arm` is borrowed, not dropped.

Join: `Join2`..`Join5`, `TryJoin2`..`TryJoin5` (`join!`, `try_join!`: all
operands polled on the calling task, in order).

Tasks: `Spawn[T](ctx, fn) *TaskJoinHandle[T]`, `TaskSpawnBlocking[T](ctx, f
func() T)`, `TaskBlockInPlace`, `TaskYieldNow(ctx)`, `TaskJoinHandle[T]`
(`Await(ctx) (T, *TaskJoinError)`, `Future()`, `Abort()`, `AbortHandle()`,
`IsFinished()`, `Id()`; dropping a handle detaches the task), `TaskJoinError`,
`TaskJoinSet[T]` (dropping it aborts its tasks), `TaskAbortHandle`, `TaskId`,
`TaskLocalKey[T]`.

Runtime: `RuntimeBuilderNewMultiThread()`, `RuntimeBuilderNewCurrentThread()`
and the builder methods the code calls, `RuntimeRuntimeNew()`,
`RuntimeBlockOn[T](rt, fn)`, `RuntimeSpawn[T](rt, fn)`, `rt.Handle()`,
`rt.Enter()`, `rt.ShutdownTimeout(d)`, `rt.ShutdownBackground()`, `rt.Drop()`,
`RuntimeHandleCurrent()`, `RuntimeHandleTryCurrent()`, `HandleSpawn`,
`HandleBlockOn`, `HandleSpawnBlocking`, `handle.RuntimeFlavor()`.

Sync (`tokio::sync`): `SyncMpscChannel`, `SyncMpscUnboundedChannel` and their
senders, receivers, permits and errors; `SyncOneshotChannel`;
`SyncWatchChannel`; `SyncNotify`; `SyncSemaphore` with borrowed and owned
permits; `SyncMutex`, `SyncRwLock` with borrowed and owned guards;
`SyncOnceCell`; `SyncBarrier`. `broadcast` is not used by Mjolnir and is not
provided. Every handle whose drop matters in tokio (senders, receivers,
permits, guards) has `Clone()` where tokio's is `Clone` and an idempotent
`Drop()`; a handle that becomes unreachable without `Drop()` is dropped by a
cleanup as a safety net, and reported when `MJ_RS_LEAK_CHECK=1`.

Time (`tokio::time`): `TimeSleep(ctx, d)`, `TimeSleepFuture(d)`,
`TimeSleepUntil`, `TimeTimeout[T](ctx, d, fn) (T, error)`, `TimeTimeoutAt`,
`TimeInterval`, `TimeIntervalAt` with `MissedTickBehavior`, `TimeInstant`,
`TimePause`, `TimeAdvance(ctx, d)`, `TimeResume`, `TimeErrorElapsed`.

Tests:

    func Test(t *testing.T, options TestOptions, body func(ctx context.Context))
    type TestOptions struct { Flavor TestFlavor; WorkerThreads int; StartPaused bool }

`Test` builds the runtime `#[tokio::test(...)]` would, runs `body` as its
root future, and shuts the runtime down. A panic in `body` fails the test
with Rust's panic text.

I/O, networking, processes, files and signals (`tokio::io`, `net`, `process`,
`fs`, `signal`) are built on this core as a second step; their readiness
waits are await points like any other.
