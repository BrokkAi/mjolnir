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
