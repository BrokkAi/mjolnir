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

    /// Await `future` on the current runtime from a blocking thread, treating
    /// runtime shutdown as a cancellation.
    pub fn block_on_with<F: Future>(&self, future: F) -> Result<F::Output> {
        if self.signalled.load(Ordering::SeqCst) {
            bail!("runtime is shutting down");
        }
        let _guard = InFlightGuard::register(self);
        // Re-check after registering: a shutdown that started in between has
        // already passed the drain wait, so it must not be given work to wait
        // for.
        if self.signalled.load(Ordering::SeqCst) {
            bail!("runtime is shutting down");
        }
        tokio::runtime::Handle::current().block_on(async {
            tokio::select! {
                biased;
                () = self.signalled() => bail!("runtime is shutting down"),
                output = future => Ok(output),
            }
        })
    }

    /// Signal shutdown, wait up to `grace` for guarded `block_on` calls to
    /// return, then shut `runtime` down.
    pub fn shutdown_with(
        &self,
        runtime: tokio::runtime::Runtime,
        work: BlockingWork,
        grace: Duration,
    ) {
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
        drop(in_flight);
        match work {
            BlockingWork::Await => drop(runtime),
            BlockingWork::Abandon => runtime.shutdown_background(),
        }
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
/// thread, onto its own blocking-pool thread.
///
/// This is [`off_async_worker`] for a whole future: work that mixes `.await`s
/// with long synchronous waits (a session lifecycle runs target commands,
/// copies archives and polls locks between its awaits) runs through
/// [`block_on`] on a blocking-pool thread. The future is boxed first, as
/// `tokio::spawn` would box it, because `block_on` keeps its future on the
/// calling thread's stack. The result is an error when the runtime begins
/// shutting down before the future finishes.
///
/// On a current-thread runtime there is no worker thread to free, and
/// `Handle::block_on` on another thread cannot drive that runtime's timers or
/// I/O, so the future is spawned as an ordinary task there.
pub fn spawn_off_async_workers<F>(future: F) -> tokio::task::JoinHandle<Result<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::spawn_blocking(move || block_on(Box::pin(future)))
    } else {
        tokio::spawn(async move { Ok(future.await) })
    }
}

/// Shut the process's runtime down in the order that keeps [`block_on`]
/// callers safe: raise the shutdown signal, wait up to `grace` for every
/// in-flight `block_on` to return, then shut the runtime down.
pub fn shutdown(runtime: tokio::runtime::Runtime, work: BlockingWork, grace: Duration) {
    SHUTDOWN.shutdown_with(runtime, work, grace);
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

    #[test]
    fn shutdown_cancels_an_in_flight_blocking_await() {
        static STATE: ShutdownState = ShutdownState::new();
        let runtime = runtime();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            tokio::task::spawn_blocking(move || {
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
                    tokio::spawn(async move {
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
            tokio::spawn(async move {
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

    #[test]
    fn off_async_worker_runs_in_place_on_a_current_thread_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(async { off_async_worker(|| 7) }), 7);
        assert_eq!(off_async_worker(|| 8), 8);
    }

    #[test]
    fn block_on_returns_the_future_output() {
        static STATE: ShutdownState = ShutdownState::new();
        let runtime = runtime();
        let value = runtime
            .block_on(async {
                tokio::task::spawn_blocking(|| {
                    STATE.block_on_with(async {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        7
                    })
                })
                .await
                .expect("blocking task")
            })
            .expect("completed");
        assert_eq!(value, 7);
        STATE.shutdown_with(runtime, BlockingWork::Await, Duration::from_secs(1));
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
}
