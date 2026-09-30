//! A stalled runtime cannot report its own stall, so observe it from an OS thread.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::FutureExt;

const TICK: Duration = Duration::from_secs(1);
const STALL_AFTER: Duration = Duration::from_secs(10);
const REPORT_INTERVAL: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_millis(100);
const NOT_SERVING: u64 = u64::MAX;

struct Progress {
    started: Instant,
    runtime: AtomicU64,
    serving: AtomicU64,
    phase: AtomicU8,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum ServingPhase {
    Startup,
    Waiting,
    Targets,
    IdleCheck,
    OwnerCheck,
    Recovery,
    BackgroundPolicy,
    Readiness,
    SuspensionRecovery,
    Client,
    SessionUpdate,
    Shutdown,
}

impl Progress {
    fn phase(&self) -> &'static str {
        match self.phase.load(Ordering::Acquire) {
            value if value == ServingPhase::Startup as u8 => "startup",
            value if value == ServingPhase::Waiting as u8 => "waiting for events",
            value if value == ServingPhase::Targets as u8 => "publish worker targets",
            value if value == ServingPhase::IdleCheck as u8 => "check idle exit",
            value if value == ServingPhase::OwnerCheck as u8 => "check daemon owner",
            value if value == ServingPhase::Recovery as u8 => {
                "publish recovery and upgrade results"
            }
            value if value == ServingPhase::BackgroundPolicy as u8 => "refresh background policies",
            value if value == ServingPhase::Readiness as u8 => "check harness readiness",
            value if value == ServingPhase::SuspensionRecovery as u8 => {
                "publish suspension recovery"
            }
            value if value == ServingPhase::Client as u8 => "accept client",
            value if value == ServingPhase::SessionUpdate as u8 => "publish session update",
            _ => "shutdown",
        }
    }
}

impl Progress {
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn runtime_tick(&self) {
        self.runtime.store(self.now(), Ordering::Release);
    }

    fn serving_tick(&self) {
        self.serving.store(self.now(), Ordering::Release);
    }

    fn stalled(&self, threshold: Duration) -> Option<(u64, Option<u64>)> {
        let now = self.now();
        let runtime_ms = now.saturating_sub(self.runtime.load(Ordering::Acquire));
        let serving = self.serving.load(Ordering::Acquire);
        let serving_ms = (serving != NOT_SERVING).then(|| now.saturating_sub(serving));
        let threshold = threshold.as_millis() as u64;
        (runtime_ms >= threshold || serving_ms.is_some_and(|elapsed| elapsed >= threshold))
            .then_some((runtime_ms, serving_ms))
    }
}

#[derive(Debug)]
enum Report {
    Stalled {
        runtime_ms: u64,
        serving_ms: Option<u64>,
        serving_phase: &'static str,
    },
    Recovered {
        elapsed_ms: u64,
    },
}

pub(super) struct DaemonProgressMonitor {
    progress: Arc<Progress>,
    stop: mpsc::Sender<()>,
    stopped: mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl DaemonProgressMonitor {
    pub(super) fn start() -> Result<Self> {
        let metrics = tokio::runtime::Handle::current().metrics();
        Self::start_reporting(
            TICK,
            STALL_AFTER,
            REPORT_INTERVAL,
            move |report| match report {
                Report::Stalled {
                    runtime_ms,
                    serving_ms,
                    serving_phase,
                } => {
                    tracing::warn!(
                        runtime_last_progress_ms = runtime_ms,
                        serving_last_progress_ms = ?serving_ms,
                        serving_phase,
                        runtime_workers = metrics.num_workers(),
                        runtime_alive_tasks = metrics.num_alive_tasks(),
                        runtime_queue_depth = metrics.global_queue_depth(),
                        blocking_operations = ?mj_core::targets::active_blocking_operations(),
                        "daemon progress stalled; reported from outside the async runtime"
                    );
                }
                Report::Recovered { elapsed_ms } => {
                    tracing::info!(elapsed_ms, "daemon progress recovered after a stall");
                }
            },
        )
    }

    fn start_reporting(
        tick: Duration,
        stall_after: Duration,
        report_interval: Duration,
        mut report: impl FnMut(Report) + Send + 'static,
    ) -> Result<Self> {
        let progress = Arc::new(Progress {
            started: Instant::now(),
            runtime: AtomicU64::new(0),
            serving: AtomicU64::new(NOT_SERVING),
            phase: AtomicU8::new(ServingPhase::Startup as u8),
        });
        let observed = progress.clone();
        let (stop, stopping) = mpsc::channel();
        let (finished, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("mj-daemon-progress".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut stalled_since = None;
                    let mut last_report = None;
                    while matches!(
                        stopping.recv_timeout(tick),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        if let Some((runtime_ms, serving_ms)) = observed.stalled(stall_after) {
                            let now = Instant::now();
                            stalled_since.get_or_insert(now);
                            if last_report
                                .is_none_or(|last: Instant| last.elapsed() >= report_interval)
                            {
                                report(Report::Stalled {
                                    runtime_ms,
                                    serving_ms,
                                    serving_phase: observed.phase(),
                                });
                                last_report = Some(now);
                            }
                        } else if let Some(started) = stalled_since.take() {
                            report(Report::Recovered {
                                elapsed_ms: started.elapsed().as_millis() as u64,
                            });
                            last_report = None;
                        }
                    }
                }));
                if let Err(error) = outcome {
                    tracing::error!(?error, "daemon progress reporter panicked");
                }
                // The receiver belongs to the scoped monitor and can be gone
                // if its bounded shutdown already returned.
                let _ = finished.send(());
            })
            .context("start daemon progress reporter")?;
        let heartbeat_progress = progress.clone();
        let heartbeat = tokio::spawn(async move {
            let result: std::result::Result<(), _> = std::panic::AssertUnwindSafe(async move {
                let mut interval = tokio::time::interval(tick);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    heartbeat_progress.runtime_tick();
                }
            })
            .catch_unwind()
            .await;
            if let Err(error) = result {
                tracing::error!(?error, "daemon progress heartbeat panicked");
            }
        });
        Ok(Self {
            progress,
            stop,
            stopped,
            thread: Some(thread),
            heartbeat,
        })
    }

    pub(super) fn serving_tick(&self) {
        self.progress.serving_tick();
        self.phase(ServingPhase::Waiting);
    }

    pub(super) fn phase(&self, phase: ServingPhase) {
        self.progress.phase.store(phase as u8, Ordering::Release);
        if matches!(phase, ServingPhase::Shutdown) {
            // Shutdown can legitimately drain accepted work after the
            // serving loop ends. Keep observing the runtime alone.
            self.progress.serving.store(NOT_SERVING, Ordering::Release);
        }
    }
}

impl Drop for DaemonProgressMonitor {
    fn drop(&mut self) {
        self.heartbeat.abort();
        let _ = self.stop.send(());
        if self.stopped.recv_timeout(STOP_TIMEOUT).is_ok() {
            if let Some(thread) = self.thread.take()
                && let Err(error) = thread.join()
            {
                tracing::error!(?error, "daemon progress reporter failed during shutdown");
            }
        } else {
            tracing::warn!("daemon progress reporter did not stop within its cleanup budget");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TICK: Duration = Duration::from_millis(5);
    const TEST_STALL: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn diagnostics_report_a_stalled_runtime_before_it_can_run_again() {
        let (tx, rx) = mpsc::channel();
        let mut reported_stall = false;
        let monitor = DaemonProgressMonitor::start_reporting(
            TEST_TICK,
            TEST_STALL,
            TEST_TICK,
            move |report| {
                let operations = mj_core::targets::active_blocking_operations();
                match report {
                    Report::Stalled { .. } if !reported_stall => {
                        // Other tests execute target commands concurrently.
                        // A contended registry deliberately returns None; wait
                        // for a later nonblocking snapshot that sees our guard.
                        if operations.as_ref().is_some_and(|operations| {
                            operations
                                .iter()
                                .any(|operation| operation.purpose == "diagnostics-runtime-blocked")
                        }) {
                            tx.send((report, operations)).unwrap();
                            reported_stall = true;
                        }
                    }
                    Report::Recovered { .. } if reported_stall => {
                        tx.send((report, operations)).unwrap();
                    }
                    _ => {}
                }
            },
        )
        .unwrap();
        monitor.serving_tick();
        tokio::task::yield_now().await;
        let blocked =
            mj_core::targets::BlockingOperation::start("diagnostics-runtime-blocked", "ssh");
        // Blocking this current-thread runtime is the fault under test. The
        // report must arrive without letting another async task execute.
        let (report, operations) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(report, Report::Stalled { runtime_ms, serving_ms: Some(serving_ms), .. }
            if runtime_ms >= 100 && serving_ms >= 100)
        );
        assert!(
            operations
                .unwrap()
                .iter()
                .any(|operation| operation.purpose == "diagnostics-runtime-blocked")
        );
        drop(blocked);
        monitor.serving_tick();
        // Keep the runtime progressing until the observer sees recovery.
        // A brief sleep followed by another blocking receive can hide that
        // recovery window from an OS thread on a heavily loaded machine.
        let (received, rx) = tokio::task::spawn_blocking(move || {
            let received = rx.recv_timeout(Duration::from_secs(5));
            (received, rx)
        })
        .await
        .unwrap();
        let (report, _) = received.unwrap();
        assert!(matches!(report, Report::Recovered { .. }));
        drop(monitor);
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn diagnostics_distinguish_a_stuck_serving_loop_from_a_running_runtime() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let monitor = DaemonProgressMonitor::start_reporting(
            TEST_TICK,
            TEST_STALL,
            Duration::from_secs(1),
            move |report| {
                tx.send(report).unwrap();
            },
        )
        .unwrap();
        monitor.serving_tick();
        monitor.phase(ServingPhase::Recovery);
        let report = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(report, Report::Stalled { runtime_ms, serving_ms: Some(serving_ms), serving_phase: "publish recovery and upgrade results" }
            if runtime_ms < 100 && serving_ms >= 100)
        );
        monitor.serving_tick();
        let report = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(report, Report::Recovered { .. }));
        monitor.phase(ServingPhase::Shutdown);
        tokio::time::sleep(TEST_STALL * 2).await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(monitor);
        assert!(rx.recv().await.is_none());
    }
}
