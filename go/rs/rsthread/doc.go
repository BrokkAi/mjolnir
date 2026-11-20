// Package rsthread stands in for Rust's std::thread (Rust 1.99.1:
// std/src/thread/ and the platform parts under std/src/sys/thread/).
//
// # Threads are goroutines
//
//	thread::spawn(f)                         rsthread.Spawn(f)  // *JoinHandle[T]
//	Builder::new().name(n).spawn(f)          rsthread.BuilderSpawn(rsthread.BuilderNew().Name(n), f)
//	handle.join()                            v, panicked := handle.Join()  // (T, *rs.PanicInfo)
//	handle.join().unwrap() / .expect(m)      handle.JoinUnwrap() / handle.JoinExpect(m)
//	handle.is_finished(), handle.thread()    handle.IsFinished(), handle.Thread()
//	thread::scope(|s| { s.spawn(f); .. })    rsthread.Scope(func(s *rsthread.ScopeHandle) T { rsthread.ScopeSpawn(s, f); .. })
//	thread::current()                        rsthread.Current()  // Thread: Id(), Name(), Unpark()
//	thread::sleep(d), thread::yield_now()    rsthread.Sleep(d), rsthread.YieldNow()
//	thread::park(), park_timeout(d)          rsthread.Park(), rsthread.ParkTimeout(d)
//	thread::panicking()                      rsthread.Panicking()
//	thread::available_parallelism()          n, err := rsthread.AvailableParallelism()
//	thread_local! { static K: T = init }     var K = rsthread.LocalKeyNew(func() T { return init })
//	K.with(|v| ..)                           K.With(func(v *T) {..}), rsthread.LocalKeyWith(K, func(v *T) R {..})
//	K.get(), K.set(v), K.replace(v)          rsthread.LocalKeyCellGet(K), LocalKeyCellSet, LocalKeyCellReplace (LocalKeyRefCell* for a RefCell)
//
// A spawned thread is a goroutine. A panic in it does not end the process:
// it is recovered where the goroutine starts, the panic hook runs with the
// thread's name, and Join returns it. A goroutine that ends through
// runtime.Goexit is reported to Join as a panic too, since Rust has no such
// ending and a Join must not wait for ever. No goroutine this package starts
// ends with an unrecovered panic.
//
// Join returns a *rs.PanicInfo, not an error: never pass its results to
// rs.Unwrap, which would see a nil *rs.PanicInfo as a non-nil error. A
// JoinHandle is spent by Join, as Rust's join consumes it.
//
// Scope waits for every thread spawned in it, joined or not, then continues
// a panic of its function, or panics with "a scoped thread panicked" if a
// thread panicked and nobody joined it. The type of the scope is ScopeHandle
// because the function std::thread::scope has the name Scope (as io::stdin is
// rsio.Stdin and its type rsio.StdinHandle).
//
// # The current thread
//
// Thread ids are Rust's: the main goroutine is the thread "main" with
// ThreadId 1, a spawned thread gets the next id when it is spawned, and any
// other goroutine (a test, for one) is an unnamed thread of its own with the
// next id the first time it asks. current.go keeps the table from goroutine
// to thread. When go/crates/tokio builds, its goroutine table is the owner of
// that fact (a current-thread runtime runs all its tasks, which are
// separate goroutines, on one thread), and current.go must ask it instead;
// until then a task is its own thread here. Thread-local values (LocalKey)
// and park tokens follow the same table.
//
// # Differences that follow from goroutines
//
//   - Panicking reads the goroutine's stack for the Go runtime's panic frame.
//     It is right in deferred calls during a panic, in what they call, and in
//     the panic hook, and false under runtime.Goexit; in the deferred
//     function that recovers, it stays true after recover until that
//     function returns.
//   - The panic hook runs after the panicking goroutine's deferred calls,
//     where Rust runs it before unwinding (see rs.RunPanicHook).
//   - Builder.StackSize is kept and shown, and otherwise ignored: a
//     goroutine's stack grows. Thread names are not given to the operating
//     system.
//   - Park returns only when the token is available; Rust allows it to
//     return spuriously, so code that is right in Rust is right here.
//   - Sleep, ParkTimeout and every other wait use Go's clock, as std's do;
//     they do not follow tokio's paused clock.
//   - AvailableParallelism follows Rust's algorithm: on Linux the CPUs in
//     the process's affinity mask, lowered to the cgroup v1 or v2 CPU quota
//     (Rust's cgroups module, ported); on macOS the hw.activecpu count; on
//     Windows GetSystemInfo's processor count.
//   - Thread-local values are dropped when a thread Spawn started ends,
//     the most recently made first. A goroutine rsthread did not start never
//     tells it that it ended, so its values are never dropped, and its entry
//     in the thread table is never removed.
//
// # Not covered
//
// The unstable or deprecated parts of std::thread: spawn hooks and
// Builder::no_hooks, Builder::spawn_unchecked, sleep_until, sleep_ms and
// park_timeout_ms, Thread::into_raw and from_raw, current_id, ThreadId::as_u64,
// add_spawn_hook. AvailableParallelism on operating systems other than
// Linux, macOS and Windows panics with rs.Todo.
package rsthread
