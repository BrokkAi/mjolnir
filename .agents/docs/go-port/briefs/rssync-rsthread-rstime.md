# `go/rs/rssync` (`std::sync`), `go/rs/rsthread` (`std::thread`), `go/rs/rstime` (`std::time`)

Oracle module `go/port/oracle/src/std_time.rs`, feature `std_time`.

Reproduce Rust 1.99.1 (`std/src/sync/`, `std/src/thread/`, `std/src/time.rs`,
`core/src/time.rs`). Grep the Rust sources for exactly what is used (`Mutex`
about 1,500 uses, `OnceLock` 149, `LazyLock` 78, `Condvar`, `RwLock`, atomics,
`std::sync::mpsc`, `thread::spawn` 102, `thread::scope` 44, `Instant` 1,100,
`SystemTime` 190, `Duration` 3,300).

`rssync`:
- `Mutex[T]` owns its value. It is a handle created by `rssync.MutexNew(v)`
  (pointer to shared state) so moving the struct that holds it keeps one
  mutex; an uninitialised one panics clearly. `m.Lock()` returns a
  `*MutexGuard[T]` with `Get() *T` and an idempotent `Drop()` that unlocks.
  Poisoning is real: a goroutine that panics while holding the guard poisons
  it (detect it in a deferred `Drop()` during the panic, re-panicking with the
  same value). `m.Lock()` is `.lock().unwrap()` (panics on poison with
  `unwrap`'s text), `m.LockExpect(msg)`, `m.LockInner()`
  (`unwrap_or_else(|e| e.into_inner())`), `m.LockResult()`, `m.TryLock()` with
  `TryLockError`, `m.With(func(*T))` for a guard held for one statement,
  `IsPoisoned`, `ClearPoison`, `IntoInner`, `GetMut`.
- `RwLock[T]` likewise, `Condvar` (`Wait`, `WaitTimeout` with the timed-out
  flag, `WaitWhile`, `WaitTimeoutWhile`, `NotifyOne`, `NotifyAll`), `Once`,
  `OnceLock[T]`, `LazyLock[T]` (`rssync.LazyLockNew(f)`, `Force`), `Barrier`,
  atomics with Rust's method names taking an ignored `Ordering`, and
  `std::sync::mpsc` with Rust's disconnect semantics and error text if used.
  The declarations generator writes statics as `rssync.OnceLockNew[T]()`,
  `rssync.MutexNew[T](v)`, `rssync.AtomicU64New(v)`: keep those constructor
  names.
- Every resource holder has an idempotent `Drop()`.

`rsthread`: `rsthread.Spawn(f) *JoinHandle[T]`, `Builder`, `Join() (T,
*rs.PanicInfo)` where a panic in the thread does not end the process (it is
recovered, the hook runs with the thread's name, and `Join` gets it),
`IsFinished`, `Thread()` with `Name`/`Id`, `Scope` with Rust's rule that it
joins all threads and re-panics (`a scoped thread panicked`), `Sleep`,
`YieldNow`, `Park`/`Unpark`/`ParkTimeout`, `Current`, `AvailableParallelism`,
`Panicking` (best effort). No goroutine started here dies with an unrecovered
panic.

`rstime`: `Duration` is Go's `time.Duration`; Rust's methods as functions with
Rust's results (`DurationFromSecs`, `FromMillis`, `FromMicros`, `FromNanos`,
`FromSecsF64` and its error text, the `As*` and `Subsec*` accessors,
`Checked*`, `Saturating*`, `MulF64`, `DivF64`, `AbsDiff`, `IsZero`, `MAX`,
`ZERO`), `DurationDebug` with Rust's exact algorithm (share one implementation
with the formatter), a report of every place the Rust code uses durations
beyond Go's range, `Instant` (monotonic, Rust's saturating and checked
arithmetic and panics, comparable), `SystemTime` as `time.Time` with
`SystemTimeError` and its text.

Tests: the std tests that apply (sync primitives, poison, mpsc, threads,
time) and differential tests for `Duration` arithmetic, conversions and
`Debug` over edge and random values. No `-race` (CGO is disabled).
