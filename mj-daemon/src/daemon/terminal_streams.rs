use super::*;
use crate::server::api::{ApiFailure, TerminalBytes, TerminalStream, TerminalStreams};
use futures::stream;
use mj_client::daemon::login_terminal::{TerminalInput, TerminalOutput};
use mj_client::http::{AdoptionEvent, LoginTerminalRequest, RuntimeChangesRequest};
use mj_client::runtime_feed::RuntimeFrame;
use mj_client::session::BoxFuture;
use mj_controller::controller::AdoptionControl;
use tokio::sync::{mpsc, oneshot, watch};

impl TerminalStreams for RuntimeState {
    fn terminal_runtime_changes(
        self: Arc<Self>,
        request: RuntimeChangesRequest,
    ) -> BoxFuture<'static, Result<RuntimeFrame>> {
        Box::pin(async move {
            self.runtime_changes_for_subagent_parents(
                request.cursor,
                request.wait,
                request.subagent_parents,
            )
            .await
        })
    }

    fn terminal_adopt_recovery(
        self: Arc<Self>,
        request: RecoveryAdoptionRequest,
        shutdown: CancellationToken,
        upgrade_gate: Arc<mj_core::upgrade::Gate>,
    ) -> TerminalStream {
        let control = AdoptionCancellation {
            token: shutdown.child_token(),
            processes: Arc::new(AtomicBool::new(false)),
        };
        let (tx, rx) = mpsc::channel(1);
        let work_control = control.clone();
        mj_core::runtime::spawn(async move {
            let stop = work_control.cancel_when_stopping(&upgrade_gate);
            let work = self.stream_adoption(request, tx, &work_control);
            tokio::pin!(work);
            tokio::select! {
                biased;
                _ = stop => work.await,
                _ = &mut work => {}
            }
        });
        Box::pin(stream::unfold(
            (rx, AdoptionStreamGuard(control)),
            |(mut rx, guard)| async {
                tokio::select! {
                    biased;
                    _ = guard.0.token.cancelled() => None,
                    line = rx.recv() => line.map(|line| (line, (rx, guard))),
                }
            },
        ))
    }

    fn terminal_open_login(
        self: Arc<Self>,
        request: LoginTerminalRequest,
        shutdown: CancellationToken,
    ) -> std::result::Result<(String, TerminalStream), ApiFailure> {
        let LoginTerminalRequest {
            profile_id,
            cols,
            rows,
        } = request;
        let config = self.with_config(Clone::clone);
        let profile = config
            .named_profile(&profile_id)
            .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?;
        let mut command = portable_pty::CommandBuilder::new(
            std::env::current_exe().context("locate daemon login executable")?,
        );
        if let Some(instance) = mj_core::config::instance_name() {
            command.args(["--instance", &instance]);
        }
        command.args(["login", "--daemon-local", "--profile", &profile_id]);
        for (name, value) in profile.environment.resolved() {
            command.env(name, value);
        }
        command.env("MJ_LOGIN_TERMINAL", "1");
        command.env("MJ_CONFIG_DIR", mj_core::config::config_dir());
        command.env("MJ_DATA_DIR", mj_core::config::data_dir());
        self.open_login_stream(command, cols, rows, shutdown)
            .map_err(Into::into)
    }

    fn terminal_send_login_input(
        self: Arc<Self>,
        id: String,
        input: TerminalInput,
    ) -> BoxFuture<'static, std::result::Result<(), ApiFailure>> {
        Box::pin(async move {
            let entry = self.login_terminals.lock().unwrap().get(&id).cloned();
            let closed = || ApiFailure::not_found("login terminal has closed");
            let entry = entry.ok_or_else(closed)?;
            tokio::select! {
                biased;
                _ = entry.cancellation.cancelled() => Err(closed()),
                result = entry.input.send(input) => result.map_err(|_| closed()),
            }
        })
    }
}

#[derive(Clone)]
pub(super) struct LoginInput {
    input: mpsc::Sender<TerminalInput>,
    cancellation: CancellationToken,
}

struct LoginRegistration {
    state: Arc<RuntimeState>,
    id: String,
    cancellation: CancellationToken,
}

impl LoginRegistration {
    fn remove(&self) {
        self.state.login_terminals.lock().unwrap().remove(&self.id);
    }
}

impl RuntimeState {
    pub(super) fn open_login_stream(
        self: Arc<Self>,
        command: portable_pty::CommandBuilder,
        cols: u16,
        rows: u16,
        shutdown: CancellationToken,
    ) -> Result<(String, TerminalStream)> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow!("generate login terminal ID: {error}"))?;
        let id = mj_core::hex::lower_hex(random);
        let (input, input_rx) = mpsc::channel(1);
        let (output, output_rx) = mpsc::channel(1);
        let cancellation = shutdown.child_token();
        self.login_terminals.lock().unwrap().insert(
            id.clone(),
            LoginInput {
                input,
                cancellation: cancellation.clone(),
            },
        );
        let registration = Arc::new(LoginRegistration {
            state: self,
            id: id.clone(),
            cancellation: cancellation.clone(),
        });
        let owner = registration.clone();
        mj_core::runtime::spawn(async move {
            let runner = mj_controller::login_terminal::serve_channels(
                input_rx,
                output.clone(),
                command,
                cols,
                rows,
                cancellation.clone(),
            );
            tokio::pin!(runner);
            let result = tokio::select! {
                _ = cancellation.cancelled() => {
                    owner.remove();
                    runner.await
                }
                result = &mut runner => result,
            };
            owner.remove();
            if let Err(error) = result {
                tracing::warn!(%error, "login terminal runner failed");
                let _ = output
                    .send(TerminalOutput::Failed(format!("{error:#}")))
                    .await;
            }
        });
        // The body alone owns cancellation on disconnect. The runner also
        // removes the input entry on exit or shutdown even if output is stalled.
        let stream_guard = LoginStreamGuard(registration);
        let stream = stream::unfold((output_rx, stream_guard), |(mut rx, guard)| async {
            tokio::select! {
                biased;
                _ = guard.0.cancellation.cancelled() => None,
                frame = rx.recv() => frame.map(|frame| (ndjson_line(&frame, None), (rx, guard))),
            }
        });
        Ok((id, Box::pin(stream)))
    }

    async fn stream_adoption(
        self: Arc<Self>,
        request: RecoveryAdoptionRequest,
        output: mpsc::Sender<std::io::Result<TerminalBytes>>,
        cancellation: &AdoptionCancellation,
    ) {
        let (progress, mut waiting) = watch::channel(None);
        let (installed, mut installation) = oneshot::channel();
        let work_state = self.clone();
        let work_request = request.clone();
        let control = AdoptionControl {
            cancelled: cancellation.token.clone(),
            progress: Some(progress),
            installed: Some(installed),
            lifecycle_gate: self.session_gate.clone(),
        };
        let executor = CancellableProcessExecutor::new(cancellation.processes.clone());
        let mut work = spawn_adoption_operation(async move {
            let mut controller = mj_core::runtime::spawn_blocking(Controller::load)
                .await
                .context("load adoption controller task")??;
            let outcome = controller
                .adopt_orphan_worker_controlled(&work_request, &executor, control)
                .await;
            refresh_runtime_controller(&work_state).await;
            Ok(outcome)
        });
        let mut installing = true;
        let mut waiting_open = true;
        let mut previous_blockers = None;
        let outcome = loop {
            tokio::select! {
                biased;
                result = &mut work => break match result {
                    Ok(result) => result,
                    Err(error) => Err(anyhow!("adoption task failed: {error}")),
                },
                publication = &mut installation, if installing => {
                    installing = false;
                    waiting_open = false;
                    if let Ok(publication) = publication {
                        refresh_runtime_controller(&self).await;
                        send_adoption_result(
                            &output,
                            &cancellation.token,
                            &AdoptionEvent::Installed { session_id: request.session_id.clone() },
                            publication.admission,
                            Some(publication.acknowledged),
                        ).await;
                    }
                }
                result = waiting.changed(), if waiting_open => {
                    if result.is_err() {
                        waiting_open = false;
                        continue;
                    }
                    let blockers = waiting.borrow_and_update().clone();
                    if blockers == previous_blockers || cancellation.token.is_cancelled() {
                        continue;
                    }
                    previous_blockers = blockers.clone();
                    if let Some(blockers) = blockers {
                        let advice = request.clone();
                        let command = mj_core::runtime::spawn_blocking(move || {
                            mj_controller::controller::adoption_destroy_command(&advice.session_id, &advice.target_id, advice.all_instances)
                        }).await;
                        let destroy_command = match command {
                            Ok(Ok(command)) => Some(command),
                            Ok(Err(error)) => {
                                tracing::warn!(%error, "could not prepare adoption destroy advice");
                                None
                            }
                            Err(error) => {
                                tracing::warn!(%error, "adoption destroy advice task failed");
                                None
                            }
                        };
                        let line = ndjson_line(&AdoptionEvent::Waiting { blockers, destroy_command }, None);
                        tokio::select! {
                            _ = cancellation.token.cancelled() => {}
                            _ = output.send(line) => {}
                        }
                    }
                }
            }
        };
        let (result, admission) = match outcome {
            Ok(outcome) => (outcome.result, outcome.admission),
            Err(error) => (Err(error), None),
        };
        let event = match result {
            Ok(()) => AdoptionEvent::Done,
            Err(error) => AdoptionEvent::Failed {
                error: format!("{error:#}"),
            },
        };
        send_adoption_result(&output, &cancellation.token, &event, admission, None).await;
    }
}

struct LoginStreamGuard(Arc<LoginRegistration>);

impl Drop for LoginStreamGuard {
    fn drop(&mut self) {
        self.0.remove();
        self.0.cancellation.cancel();
    }
}

#[derive(Clone)]
struct AdoptionCancellation {
    token: CancellationToken,
    processes: Arc<AtomicBool>,
}

impl AdoptionCancellation {
    fn cancel(&self) {
        self.processes.store(true, Ordering::Release);
        self.token.cancel();
    }

    async fn cancel_when_stopping(&self, upgrade_gate: &mj_core::upgrade::Gate) {
        tokio::select! {
            _ = self.token.cancelled() => {}
            _ = upgrade_gate.wait_until_draining() => {}
        }
        self.cancel();
    }
}

struct AdoptionStreamGuard(AdoptionCancellation);

impl Drop for AdoptionStreamGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// Hyper can stop polling the body while its socket is backpressured. The
// sender holds admission until the bytes leave Hyper's write queue or drain
// cancels the delivery, even when the body itself is no longer being polled.
async fn send_adoption_result<T>(
    output: &mpsc::Sender<std::io::Result<TerminalBytes>>,
    cancellation: &CancellationToken,
    event: &AdoptionEvent,
    admission: T,
    acknowledged: Option<oneshot::Sender<()>>,
) {
    let delivery = (admission, Acknowledge(acknowledged));
    let (on_drop, released) = oneshot::channel();
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {}
        _ = async {
            if output.send(ndjson_line(event, Some(on_drop))).await.is_ok() {
                let _ = released.await;
            }
        } => {}
    }
    // The accepted controller can continue attachment once delivery ends.
    drop(delivery);
}

struct Acknowledge(Option<oneshot::Sender<()>>);

impl Drop for Acknowledge {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn ndjson_line(
    event: &impl serde::Serialize,
    acknowledged: Option<oneshot::Sender<()>>,
) -> std::io::Result<TerminalBytes> {
    struct Line {
        bytes: Vec<u8>,
        _acknowledged: Acknowledge,
    }
    impl AsRef<[u8]> for Line {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }
    let acknowledged = Acknowledge(acknowledged);
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    Ok(TerminalBytes::from_owner(Line {
        bytes,
        _acknowledged: acknowledged,
    }))
}

// Worker quiescence is unbounded. Only the synchronous controller operations
// leave async workers; the wait must never occupy a blocking-pool thread.
pub(super) fn spawn_adoption_operation<F, T>(operation: F) -> tokio::task::JoinHandle<Result<T>>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    mj_core::runtime::spawn(async move {
        use futures::FutureExt;
        let result = match std::panic::AssertUnwindSafe(operation).catch_unwind().await {
            Ok(result) => result,
            Err(panic) => {
                tracing::error!(?panic, "adoption controller task panicked");
                Err(anyhow::anyhow!(
                    "adoption controller task panicked; check session state before retrying"
                ))
            }
        };
        if let Err(error) = &result {
            tracing::error!(%error, "adoption controller task failed");
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn installed_acknowledges_after_the_last_delivery_buffer_releases_admission() {
        let gate = Arc::new(mj_core::upgrade::Gate::default());
        let admission = gate.enter_unless_draining("adoption install").unwrap();
        let (output, mut rx) = mpsc::channel(1);
        let (ack, mut delivered) = oneshot::channel();
        let event = AdoptionEvent::Installed {
            session_id: "worker".into(),
        };
        let cancellation = CancellationToken::new();
        let delivery = send_adoption_result(&output, &cancellation, &event, admission, Some(ack));
        tokio::pin!(delivery);
        assert!(futures::poll!(&mut delivery).is_pending());

        let encoded = rx.try_recv().unwrap().unwrap();
        let pending_write = encoded.clone();
        drop(encoded);
        assert!(futures::poll!(&mut delivery).is_pending());
        assert!(delivered.try_recv().is_err());
        drop(pending_write);

        delivery.await;
        delivered.try_recv().unwrap();
        assert!(gate.try_close());
    }

    #[tokio::test]
    async fn adoption_drain_releases_installed_admission_without_a_reader() {
        for fill_queue_first in [true, false] {
            let gate = Arc::new(mj_core::upgrade::Gate::default());
            let admission = gate.enter_unless_draining("adoption install").unwrap();
            let control = AdoptionCancellation {
                token: CancellationToken::new(),
                processes: Arc::new(AtomicBool::new(false)),
            };
            let (output, rx) = mpsc::channel(1);
            if fill_queue_first {
                output
                    .try_send(Ok(TerminalBytes::from_static(b"waiting")))
                    .unwrap();
            }
            let (ack, delivered) = oneshot::channel();
            let event = AdoptionEvent::Installed {
                session_id: "worker".into(),
            };
            let delivery =
                send_adoption_result(&output, &control.token, &event, admission, Some(ack));
            tokio::pin!(delivery);
            assert!(futures::poll!(&mut delivery).is_pending());
            assert_eq!(rx.len(), 1);
            assert_eq!(gate.active_labels(), ["adoption install"]);

            gate.begin_draining();
            tokio::time::timeout(Duration::from_secs(2), async {
                control.cancel_when_stopping(&gate).await;
                delivery.await;
                delivered.await.unwrap();
            })
            .await
            .expect("draining must release installation without a polling reader");
            assert!(control.processes.load(Ordering::Acquire));
            assert_eq!(rx.len(), 1, "the response body remained unpolled");
            assert!(gate.try_close());
        }
    }
}
