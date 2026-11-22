//! Runtime shutdown coordination for blocking threads that await futures.
//!
//! The multi-thread scheduler cancels its tasks before it shuts the timer
//! driver down, but a `spawn_blocking` thread running `Handle::block_on` sits
//! outside that ordering: it keeps polling the future's sleeps and timeouts
//! while the driver disappears, and tokio's timer entry asserts. Dropping a
//! timer after driver shutdown is safe; only polling one is not. So every such
//! thread registers here, and shutdown cancels those futures and waits for the
//! threads to return before the runtime goes away.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// How to treat blocking-pool work that is not guarded by [`block_on`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockingWork {
    /// Wait for the blocking pool to finish (a plain runtime drop).
    Await,
    /// Leave the remaining blocking work behind (`shutdown_background`).
    Abandon,
    /// Wait up to this long for the blocking pool, then leave what is still
    /// running behind (`shutdown_timeout`). The returned report says whether
    /// anything was left, so the caller can name it.
    AwaitFor(Duration),
}

/// What a runtime shutdown could not finish.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Guarded [`block_on`] calls still running when the grace ran out.
    pub awaits_left: usize,
    /// Whether [`BlockingWork::AwaitFor`] ran out of time with blocking-pool
    /// work still running.
    pub blocking_left: bool,
}

/// Tracks the shutdown signal and the guarded `block_on` calls in flight.
#[derive(Debug)]
pub struct ShutdownState {
    signalled: AtomicBool,
    notify: tokio::sync::Notify,
    in_flight: Mutex<usize>,
    drained: Condvar,
}

impl Default for ShutdownState {
    fn default() -> Self {
        Self::new()
    }
}

static SHUTDOWN: ShutdownState = ShutdownState::new();

impl ShutdownState {
    pub const fn new() -> Self {
        Self {
            signalled: AtomicBool::new(false),
            notify: tokio::sync::Notify::const_new(),
            in_flight: Mutex::new(0),
            drained: Condvar::new(),
        }
    }

    /// Resolves once shutdown has been signalled, whether before or after the
    /// caller starts waiting.
    async fn signalled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Register before re-reading the flag so a signal raised between the
        // two is delivered rather than lost.
        notified.as_mut().enable();
        if self.signalled.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }

    /// Register an in-flight guarded `block_on` before handing work to another
    /// thread. Returns `None` once shutdown has begun so callers never launch a
    /// thread after the drain wait has passed.
    fn try_register(&self) -> Option<InFlightGuard<'_>> {
        if self.signalled.load(Ordering::SeqCst) {
            return None;
        }
        let guard = InFlightGuard::register(self);
        // Re-check after registering: a shutdown that started in between has
        // already passed the drain wait, so it must not be given work to wait
        // for.
        if self.signalled.load(Ordering::SeqCst) {
            return None;
        }
        Some(guard)
    }

    /// Drive `future` with `handle.block_on` under an already-registered
    /// shutdown guard, dropping `handle` before releasing the guard.
    fn block_on_registered<F: Future>(
        &self,
        guard: InFlightGuard<'_>,
        handle: tokio::runtime::Handle,
        future: F,
    ) -> Result<F::Output> {
        let _guard = guard;
        if self.signalled.load(Ordering::SeqCst) {
            drop(handle);
            bail!("runtime is shutting down");
        }
        let result = handle.block_on(async {
            tokio::select! {
                biased;
                () = self.signalled() => bail!("runtime is shutting down"),
                output = future => Ok(output),
            }
        });
        drop(handle);
        result
    }

    /// Await `future` on the current runtime from a blocking thread, treating
    /// runtime shutdown as a cancellation.
    pub fn block_on_with<F: Future>(&self, future: F) -> Result<F::Output> {
        let Some(guard) = self.try_register() else {
            bail!("runtime is shutting down");
        };
        self.block_on_registered(guard, tokio::runtime::Handle::current(), future)
    }

    /// Drive `future` on a dedicated OS thread outside Tokio's `spawn_blocking`
    /// pool so composite async operations that await `spawn_blocking` (or nest
    /// `spawn_off_async_workers`) can never starve the bounded blocking pool.
    fn spawn_off_async_workers_with<F>(
        &'static self,
        future: F,
    ) -> tokio::task::JoinHandle<Result<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = tokio::runtime::Handle::current();
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return spawn(async move { Ok(future.await) });
        }
        let Some(guard) = self.try_register() else {
            return spawn(async { bail!("runtime is shutting down") });
        };
        let owner = BLOCKING_OWNER
            .try_with(tokio_util::task::TaskTracker::token)
            .ok();
        let (tx, rx) = tokio::sync::oneshot::channel::<std::thread::Result<Result<F::Output>>>();
        let spawned = std::thread::Builder::new()
            .name("mj-off-worker".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let _owner = owner;
                    self.block_on_registered(guard, handle, Box::pin(future))
                }));
                let _ = tx.send(outcome);
            });
        match spawned {
            Ok(_) => spawn(async move {
                match rx.await {
                    Ok(Ok(output)) => output,
                    Ok(Err(payload)) => std::panic::resume_unwind(payload),
                    Err(_) => bail!("off-worker thread exited without returning a result"),
                }
            }),
            Err(error) => {
                spawn(
                    async move { Err(anyhow::Error::new(error).context("spawn off-worker thread")) },
                )
            }
        }
    }

    /// Signal shutdown, wait up to `grace` for guarded `block_on` calls to
    /// return, then shut `runtime` down.
    pub fn shutdown_with(
        &self,
        runtime: tokio::runtime::Runtime,
        work: BlockingWork,
        grace: Duration,
    ) -> ShutdownReport {
        let mut report = ShutdownReport::default();
        self.signalled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        let deadline = Instant::now() + grace;
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        while *in_flight > 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                tracing::warn!(
                    in_flight = *in_flight,
                    "shutting the runtime down with blocking awaits still running"
                );
                break;
            };
            let (guard, timeout) = self
                .drained
                .wait_timeout(in_flight, remaining)
                .unwrap_or_else(|e| e.into_inner());
            in_flight = guard;
            if timeout.timed_out() && *in_flight > 0 {
                tracing::warn!(
                    in_flight = *in_flight,
                    "shutting the runtime down with blocking awaits still running"
                );
                break;
            }
        }
        report.awaits_left = *in_flight;
        drop(in_flight);
        match work {
            BlockingWork::Await => drop(runtime),
            BlockingWork::Abandon => runtime.shutdown_background(),
            BlockingWork::AwaitFor(limit) => {
                let started = Instant::now();
                runtime.shutdown_timeout(limit);
                // The pool returns early once its last thread exits, so a wait
                // that used the whole limit left threads running.
                report.blocking_left = started.elapsed() >= limit;
            }
        }
        report
    }
}

struct InFlightGuard<'a> {
    state: &'a ShutdownState,
}

impl<'a> InFlightGuard<'a> {
    fn register(state: &'a ShutdownState) -> Self {
        *state.in_flight.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        Self { state }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let mut in_flight = self
            .state
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *in_flight -= 1;
        if *in_flight == 0 {
            self.state.drained.notify_all();
        }
    }
}

/// Await runtime work from a blocking thread, with runtime shutdown as a
/// cancellation. Errors with "runtime is shutting down" when the process has
/// begun shutting its runtime down before or while the future runs. Must be
/// called from a thread that has a runtime context (`spawn_blocking` threads
/// do).
pub fn block_on<F: Future>(future: F) -> Result<F::Output> {
    SHUTDOWN.block_on_with(future)
}

/// Run blocking work, such as waiting for a child process, without holding
/// one of the runtime's async worker threads.
///
/// Synchronous code that waits on a subprocess is often reached from an
/// `async fn`, several calls down. On a worker thread such a wait takes the
/// thread away from every other task: with as many slow commands as worker
/// threads, nothing else in the process runs, including the code that would
/// cancel them. Here the worker first hands its other tasks to a replacement
/// thread from the blocking pool, so the wait occupies a blocking-pool thread
/// and the async workers keep serving. Cancelling the work is still the
/// caller's job; this only decides which thread waits.
///
/// Outside a runtime, on a blocking-pool thread, or inside
/// [`tokio::runtime::Handle::block_on`] the work simply runs. On a
/// current-thread runtime there is no other thread to hand tasks to, so the
/// work runs in place there too; the daemon uses the multi-thread runtime.
pub fn off_async_worker<R>(work: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// Spawn `future`, whose synchronous waits must not hold an async worker
/// thread, onto its own dedicated thread outside Tokio's blocking pool.
///
/// This is [`off_async_worker`] for a whole future: work that mixes `.await`s
/// with long synchronous waits (a session lifecycle runs target commands,
/// copies archives and polls locks between its awaits) runs through
/// [`block_on`] on a dedicated `mj-off-worker` OS thread. Running outside
/// Tokio's bounded `spawn_blocking` pool makes pool-starvation deadlocks
/// structurally impossible when many lifecycle futures await inner
/// `spawn_blocking` work or nested lifecycles (#1640). The future is boxed
/// first, as `tokio::spawn` would box it, because `block_on` keeps its future
/// on the calling thread's stack. The result is an error when the runtime
/// begins shutting down before the future finishes.
///
/// On a current-thread runtime there is no worker thread to free, and
/// `Handle::block_on` on another thread cannot drive that runtime's timers or
/// I/O, so the future is spawned as an ordinary task there.
pub fn spawn_off_async_workers<F>(future: F) -> tokio::task::JoinHandle<Result<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    SHUTDOWN.spawn_off_async_workers_with(future)
}

/// Start a task, as `tokio::spawn` does, with the future boxed first.
///
/// Tokio generates its task machinery (about thirty functions) once for every
/// type of future it is handed, and every `async` block is its own type. A
/// boxed future has one type for each output type, so that code is generated
/// once for each output type instead of once for each call. Measured on
/// `mj-daemon`'s tests, where task machinery was over half of the generated
/// code: the compile went from 6.9 GB and 64 s to 5.0 GB and 52 s. The cost at
/// run time is one allocation when a task starts.
///
/// `clippy.toml` disallows calling `tokio::spawn` directly so that new code
/// comes through here.
#[track_caller]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let boxed: std::pin::Pin<Box<dyn Future<Output = F::Output> + Send>> = Box::pin(future);
    #[allow(
        clippy::disallowed_methods,
        reason = "this is the one place tasks are handed to tokio"
    )]
    tokio::spawn(boxed)
}

tokio::task_local! {
    static BLOCKING_OWNER: tokio_util::task::TaskTracker;
}

/// Run `future` so that every closure it hands to [`spawn_blocking`] from its
/// own task is counted by `tracker` until the closure returns.
///
/// A thread blocked in a system call cannot be stopped from outside, and
/// dropping the future that awaited it detaches it. An owner that cancels
/// `future` closes `tracker` and waits on it, so nothing the future started is
/// still running when the owner reports it stopped. Tasks the future spawns
/// are their own owners and are not counted.
pub async fn track_blocking_work<F: Future>(
    tracker: tokio_util::task::TaskTracker,
    future: F,
) -> F::Output {
    BLOCKING_OWNER.scope(tracker, future).await
}

/// Run blocking work on tokio's blocking pool, as
/// `tokio::task::spawn_blocking` does, with the closure boxed first for the
/// same reason as [`spawn`]. Inside [`track_blocking_work`] the closure is
/// counted by that tracker until it returns.
#[track_caller]
pub fn spawn_blocking<R>(work: impl FnOnce() -> R + Send + 'static) -> tokio::task::JoinHandle<R>
where
    R: Send + 'static,
{
    let owner = BLOCKING_OWNER
        .try_with(tokio_util::task::TaskTracker::token)
        .ok();
    let boxed: Box<dyn FnOnce() -> R + Send> = Box::new(move || {
        let _owner = owner;
        work()
    });
    #[allow(
        clippy::disallowed_methods,
        reason = "this is the one place blocking work is handed to tokio"
    )]
    tokio::task::spawn_blocking(boxed)
}

/// Shut the process's runtime down in the order that keeps [`block_on`]
/// callers safe: raise the shutdown signal, wait up to `grace` for every
/// in-flight `block_on` to return, then shut the runtime down.
pub fn shutdown(
    runtime: tokio::runtime::Runtime,
    work: BlockingWork,
    grace: Duration,
) -> ShutdownReport {
    SHUTDOWN.shutdown_with(runtime, work, grace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    }

    // Hard-won: 323c8446: Blocking awaits kept polling a dead Tokio timer driver and panicked during shutdown.
    #[test]
    fn shutdown_cancels_an_in_flight_blocking_await() {
        static STATE: ShutdownState = ShutdownState::new();
        let runtime = runtime();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            spawn_blocking(move || {
                started_tx.send(()).ok();
                let result = STATE.block_on_with(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                result_tx.send(result.is_err()).ok();
            });
        });
        started_rx.recv().expect("blocking await started");
        let began = Instant::now();
        STATE.shutdown_with(runtime, BlockingWork::Await, Duration::from_secs(5));
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "shutdown waited for the full grace period"
        );
        assert!(
            result_rx.recv().expect("blocking await reported"),
            "the cancelled await should report the shutdown error"
        );
    }

    #[test]
    fn block_on_after_the_signal_does_not_poll_the_future() {
        static STATE: ShutdownState = ShutdownState::new();
        let runtime = runtime();
        STATE.shutdown_with(runtime, BlockingWork::Abandon, Duration::from_secs(1));
        let polled = Arc::new(AtomicBool::new(false));
        let observer = polled.clone();
        let error = STATE
            .block_on_with(async move {
                observer.store(true, Ordering::SeqCst);
            })
            .expect_err("shutting down");
        assert!(error.to_string().contains("runtime is shutting down"));
        assert!(!polled.load(Ordering::SeqCst), "the future should not run");
    }

    /// Two worker threads, each blocked in a wait that only a third task can
    /// end. Without the hand-off the third task never runs and the waits never
    /// finish; the watchdog then ends them and the timing assertion fails.
    // Hard-won: 34e86406: Lifecycle waits occupied every async worker so the daemon could not serve or cancel launches.
    #[test]
    fn blocking_waits_leave_the_async_workers_serving() {
        let runtime = runtime();
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let watchdog_release = release_tx.clone();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if finished_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                watchdog_release.send_replace(true);
            }
        });
        let started = Instant::now();
        runtime.block_on(async move {
            let waits = (0..3)
                .map(|_| {
                    let mut release = release_rx.clone();
                    spawn(async move {
                        off_async_worker(|| {
                            while !*release.borrow_and_update() {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        });
                    })
                })
                .collect::<Vec<_>>();
            // Let every wait start before the releasing task is spawned.
            std::thread::sleep(Duration::from_millis(100));
            spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                release_tx.send_replace(true);
            })
            .await
            .unwrap();
            for wait in waits {
                wait.await.unwrap();
            }
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "blocking waits held every async worker for {:?}",
            started.elapsed()
        );
        finished_tx.send(()).unwrap();
        watchdog.join().unwrap();
    }

    /// `Abandon` is the dashboard's exit: the grace covers guarded awaits
    /// only, never a disposable blocking read that would delay quitting.
    #[test]
    fn abandon_does_not_wait_for_unguarded_blocking_work() {
        let state = ShutdownState::new();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();

        let started = Instant::now();
        state.shutdown_with(runtime, BlockingWork::Abandon, Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "abandoning shutdown waited {:?}",
            started.elapsed()
        );
        release_tx.send(()).unwrap();
    }

    /// `AwaitFor` is the daemon's exit: blocking work that finishes in time is
    /// waited for, and work that does not is left behind and reported rather
    /// than holding the process open.
    // Hard-won: 3c3137a2: Live daemon handoffs stalled after the epilogue while unowned blocking work kept the process open.
    #[test]
    fn await_for_reports_blocking_work_it_left_running() {
        let state = ShutdownState::new();
        let stuck = runtime();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        stuck.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        started_rx.recv().unwrap();
        let started = Instant::now();
        let report = state.shutdown_with(
            stuck,
            BlockingWork::AwaitFor(Duration::from_millis(200)),
            Duration::from_secs(1),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded shutdown waited {:?}",
            started.elapsed()
        );
        assert!(
            report.blocking_left,
            "the stuck blocking work was not reported"
        );
        release_tx.send(()).unwrap();

        let state = ShutdownState::new();
        let finished = runtime();
        let done = finished.spawn_blocking(|| ());
        finished.block_on(done).unwrap();
        let report = state.shutdown_with(
            finished,
            BlockingWork::AwaitFor(Duration::from_secs(5)),
            Duration::from_secs(1),
        );
        assert_eq!(report, ShutdownReport::default());
    }

    /// More concurrent `spawn_off_async_workers` futures than Tokio's
    /// `max_blocking_threads` must still complete when every one of them
    /// awaits `spawn_blocking` and nested `spawn_off_async_workers`.
    // Hard-won: #1640: Running `spawn_off_async_workers` on Tokio's blocking pool deadlocked when 512+ lifecycles awaited `spawn_blocking`.
    #[test]
    fn off_async_workers_do_not_starve_the_blocking_pool() {
        static STATE: ShutdownState = ShutdownState::new();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let started = Instant::now();
        let results: Vec<usize> = runtime.block_on(async move {
            let tasks = (0..8)
                .map(|index| {
                    let barrier = barrier.clone();
                    STATE.spawn_off_async_workers_with(async move {
                        // Hold every outer task in flight before any inner
                        // `spawn_blocking` runs so a shared blocking pool of
                        // size 2 would deadlock immediately.
                        barrier.wait().await;
                        let leaf = spawn_blocking(move || index + 1).await.expect("leaf");
                        let inline = off_async_worker(|| leaf * 2);
                        STATE
                            .spawn_off_async_workers_with(async move {
                                spawn_blocking(move || inline + 10).await.expect("nested")
                            })
                            .await
                            .expect("nested join")
                            .expect("nested shutdown")
                    })
                })
                .collect::<Vec<_>>();
            let mut outputs = Vec::with_capacity(tasks.len());
            for task in tasks {
                let output = tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .expect("off-worker tasks deadlocked the blocking pool")
                    .expect("task join")
                    .expect("not shutting down");
                outputs.push(output);
            }
            outputs
        });
        assert_eq!(
            results,
            (0..8).map(|i| (i + 1) * 2 + 10).collect::<Vec<_>>()
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "off-worker tasks took {:?}",
            started.elapsed()
        );
        // Shutdown must still cancel and drop an in-flight off-worker future promptly.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let drop_flag = DropFlag(dropped.clone());
        runtime.spawn(async move {
            let _ = STATE
                .spawn_off_async_workers_with(async move {
                    let _drop_flag = drop_flag;
                    entered_tx.send(()).ok();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                })
                .await;
        });
        entered_rx.recv().expect("off-worker entered");
        let shutdown_started = Instant::now();
        let report = STATE.shutdown_with(runtime, BlockingWork::Await, Duration::from_secs(5));
        assert_eq!(report, ShutdownReport::default());
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(2),
            "shutdown waited {:?} for off-worker await",
            shutdown_started.elapsed()
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "shutdown should cancel and drop the in-flight off-worker future"
        );
    }
}
