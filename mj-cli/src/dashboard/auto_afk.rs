//! Coalesced reporting of operator input to the daemon.

use std::{future::Future, pin::Pin, time::Duration};

use anyhow::{Context, Result, bail};
use crossterm::event::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const MIN_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const REPORT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ActivitySignal {
    enabled: bool,
    away: bool,
    generation: u64,
    /// Input at or below this value happened before the latest disabled/away
    /// transition and must never be replayed after a later re-enable.
    discarded_through: u64,
}

trait ActivityTransport: Send + 'static {
    fn report(&mut self, generation: u64) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

struct DaemonActivityTransport {
    client: Option<crate::daemon::DaemonClient>,
}

impl ActivityTransport for DaemonActivityTransport {
    fn report(
        &mut self,
        _generation: u64,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            // Keep the stream local until the acknowledgement arrives. If the
            // enclosing timeout cancels this future, dropping it also drops
            // the potentially half-consumed connection.
            let mut client = match self.client.take() {
                Some(client) => client,
                None => crate::daemon::connect_existing().await?,
            };
            let result = client
                .request(mj_client::daemon::DaemonAction::RecordOperatorActivity)
                .await;
            match result {
                Ok(mj_client::daemon::DaemonReply::Done) => {
                    self.client = Some(client);
                    Ok(())
                }
                Ok(reply) => bail!("unexpected activity-report reply {reply:?}"),
                Err(error) => Err(error).context("record operator activity"),
            }
        })
    }
}

/// One supervised sender for the dashboard lifetime. The watch channel keeps
/// only the latest signal, so input during a slow request produces one trailing
/// report rather than a queue of per-event tasks.
pub(super) struct AutoAfkReporter {
    signal: Option<watch::Sender<ActivitySignal>>,
    pub(super) errors: mpsc::Receiver<String>,
    task: Option<JoinHandle<()>>,
    state: ActivitySignal,
}

impl AutoAfkReporter {
    pub(super) fn new(enabled: bool, away: bool) -> Self {
        let state = ActivitySignal {
            enabled,
            away,
            generation: 0,
            discarded_through: 0,
        };
        let (signal, receiver) = watch::channel(state);
        let (error_tx, errors) = mpsc::channel(1);
        let task = tokio::spawn(run_reporter(
            receiver,
            error_tx,
            DaemonActivityTransport { client: None },
            MIN_REPORT_INTERVAL,
            REPORT_TIMEOUT,
        ));
        Self {
            signal: Some(signal),
            errors,
            task: Some(task),
            state,
        }
    }

    pub(super) fn configure(&mut self, enabled: bool, away: bool) {
        let became_gated = (!enabled || away) && (self.state.enabled && !self.state.away);
        self.state.enabled = enabled;
        self.state.away = away;
        if became_gated {
            self.state.discarded_through = self.state.generation;
        }
        if let Some(signal) = &self.signal {
            signal.send_replace(self.state);
        }
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.configure(enabled, self.state.away);
    }

    pub(super) fn input(&mut self) {
        if !self.state.enabled || self.state.away {
            return;
        }
        self.state.generation = self.state.generation.wrapping_add(1);
        if let Some(signal) = &self.signal {
            signal.send_replace(self.state);
        }
    }

    pub(super) async fn shutdown(&mut self) {
        self.signal.take();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "auto-AFK reporter task failed during shutdown");
                }
                Err(_) => task.abort(),
            }
        }
    }
}

impl Drop for AutoAfkReporter {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(super) fn is_operator_input(event: &Event) -> bool {
    matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_))
}

async fn run_reporter<T: ActivityTransport>(
    mut signal: watch::Receiver<ActivitySignal>,
    errors: mpsc::Sender<String>,
    mut transport: T,
    interval: Duration,
    timeout: Duration,
) {
    let mut reported_generation = 0;
    let mut last_report: Option<tokio::time::Instant> = None;
    let mut last_error: Option<String> = None;
    loop {
        if signal.changed().await.is_err() {
            return;
        }
        let current = *signal.borrow_and_update();
        if !eligible_generation(current, reported_generation) {
            continue;
        }
        if let Some(last) = last_report {
            tokio::time::sleep_until(last + interval).await;
        }
        let current = *signal.borrow_and_update();
        if !eligible_generation(current, reported_generation) {
            continue;
        }
        // Mark the generation before sending. A newer signal arriving during
        // the request remains visible in the watch channel for one trailing
        // report once this request completes.
        reported_generation = current.generation;
        last_report = Some(tokio::time::Instant::now());
        let result = tokio::time::timeout(timeout, transport.report(current.generation))
            .await
            .context("daemon did not acknowledge operator activity before its deadline")
            .and_then(|result| result);
        match result {
            Ok(()) => last_error = None,
            Err(error) => {
                let message = format!("Could not report dashboard input: {error:#}");
                if last_error.as_deref() != Some(&message) {
                    let _ = errors.try_send(message.clone());
                    last_error = Some(message);
                }
            }
        }
    }
}

fn eligible_generation(current: ActivitySignal, reported: u64) -> bool {
    current.enabled
        && !current.away
        && current.generation > reported
        && current.generation > current.discarded_through
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone)]
    struct RecordedTransport {
        generations: mpsc::UnboundedSender<u64>,
        first_ack: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    }

    impl ActivityTransport for RecordedTransport {
        fn report(
            &mut self,
            generation: u64,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.generations.send(generation).unwrap();
                if let Some(ack) = self.first_ack.lock().await.take() {
                    ack.await.unwrap();
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn slow_ack_coalesces_burst_into_one_trailing_request() {
        let (signal_tx, signal_rx) = watch::channel(ActivitySignal {
            enabled: true,
            ..ActivitySignal::default()
        });
        let (errors_tx, _errors_rx) = mpsc::channel(1);
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(run_reporter(
            signal_rx,
            errors_tx,
            RecordedTransport {
                generations: observed_tx,
                first_ack: Arc::new(tokio::sync::Mutex::new(Some(ack_rx))),
            },
            Duration::ZERO,
            Duration::from_secs(2),
        ));

        signal_tx.send_replace(ActivitySignal {
            enabled: true,
            generation: 1,
            ..ActivitySignal::default()
        });
        assert_eq!(observed_rx.recv().await, Some(1));
        signal_tx.send_replace(ActivitySignal {
            enabled: true,
            generation: 2,
            ..ActivitySignal::default()
        });
        signal_tx.send_replace(ActivitySignal {
            enabled: true,
            generation: 3,
            ..ActivitySignal::default()
        });
        ack_tx.send(()).unwrap();
        assert_eq!(observed_rx.recv().await, Some(3));
        assert!(observed_rx.try_recv().is_err());

        worker.abort();
    }

    #[tokio::test]
    async fn away_transition_discards_pending_input_too() {
        let (signal_tx, signal_rx) = watch::channel(ActivitySignal {
            enabled: true,
            ..ActivitySignal::default()
        });
        let (errors_tx, _errors_rx) = mpsc::channel(1);
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(run_reporter(
            signal_rx,
            errors_tx,
            RecordedTransport {
                generations: observed_tx,
                first_ack: Arc::new(tokio::sync::Mutex::new(Some(ack_rx))),
            },
            Duration::ZERO,
            Duration::from_secs(2),
        ));
        signal_tx.send_replace(ActivitySignal {
            enabled: true,
            generation: 1,
            ..ActivitySignal::default()
        });
        assert_eq!(observed_rx.recv().await, Some(1));
        signal_tx.send_replace(ActivitySignal {
            enabled: true,
            generation: 2,
            ..ActivitySignal::default()
        });
        signal_tx.send_replace(ActivitySignal {
            enabled: true,
            away: true,
            generation: 2,
            discarded_through: 2,
        });
        ack_tx.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(observed_rx.try_recv().is_err());
        worker.abort();
    }
}
