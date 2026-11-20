//! Push-review operations that change refs: an override, a baseline move and a
//! review-off acceptance.
//!
//! Each runs as a task, because it waits on the worker. The task does the
//! worker's part and nothing else. Every decision about review state belongs to
//! the host's task, so the operation asks for it: which verdict an override
//! answers, which roots it holds while it changes refs, and what the stored row
//! becomes. The host answers one request at a time, in the order of its other
//! events, and releases the operation's roots when it ends on any path.

use super::*;
use futures::FutureExt;

/// How an operation's task reaches the host.
#[derive(Clone)]
pub struct OperationLink {
    pub id: u64,
    pub events: mpsc::UnboundedSender<HostEvent>,
}

fn host_stopped() -> StartRefusal {
    StartRefusal("the review host stopped".into())
}

impl OperationLink {
    pub async fn ask(&self, request: OperationRequest) -> Result<OperationAnswer, StartRefusal> {
        let (reply, answer) = oneshot::channel();
        self.events
            .send(HostEvent::OperationRequest {
                operation: self.id,
                request,
                reply,
            })
            .map_err(|_| host_stopped())?;
        answer.await.map_err(|_| host_stopped())?
    }

    /// Asks for a change and waits until the host's save of it is durable.
    pub async fn ask_saved(&self, request: OperationRequest) -> Result<(), StartRefusal> {
        match self.ask(request).await? {
            OperationAnswer::Saved(saved) => saved
                .await
                .map_err(|_| {
                    StartRefusal("review persistence stopped before the change was recorded".into())
                })?
                .map_err(StartRefusal),
            _ => Ok(()),
        }
    }
}

/// The relay protocol of the session's worker, as its latest view reports it.
pub async fn session_protocol(
    control: &SessionManagerControl,
    session_id: &str,
) -> Result<u32, StartRefusal> {
    Ok(control
        .session(session_id.to_owned())
        .await
        .map_err(|error| {
            StartRefusal(format!(
                "{error:#}; resume the session before requesting review"
            ))
        })?
        .view()
        .snapshot
        .and_then(|snapshot| snapshot.operational.relay_protocol_version)
        .unwrap_or(0))
}

/// The repository roots `path` selects, or the working directory's when it
/// names none, from a worker that can resolve them without a capture.
pub async fn resolve_roots(
    control: &SessionManagerControl,
    session_id: &str,
    paths: Vec<std::path::PathBuf>,
) -> Result<Vec<std::path::PathBuf>, StartRefusal> {
    let outcome = resolve::push_action(
        control,
        session_id,
        ReviewerAction::ResolveRepositories { paths },
    )
    .await
    .map_err(|error| StartRefusal(format!("{error:#}")))?;
    match outcome {
        ReviewerOutcome::Repositories { roots } => Ok(roots),
        _ => Err(StartRefusal(
            "the worker did not resolve the repositories".into(),
        )),
    }
}

impl HostState {
    /// Why a push-review state change cannot start now, with how to proceed.
    /// Whether a review owns the roots it would change is decided later, with
    /// the roots known.
    pub fn push_operation_refusal(&self, session_id: &str) -> Option<&'static str> {
        if !(self.config)(session_id).is_push() {
            Some("this session's review trigger is not push; this tool only works for push review")
        } else if mj_session_runtime::session_manager::move_ownership::move_owns_session(session_id)
        {
            Some("session is moving; try again after the move finishes")
        } else {
            None
        }
    }

    /// Runs one push-review operation in a task. The task holds the session's
    /// background work off its worker until it ends.
    pub fn spawn_operation<F>(
        &mut self,
        session_id: String,
        reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>>,
        name: &'static str,
        run: impl FnOnce(SessionManagerControl, OperationLink, String) -> F,
    ) where
        F: std::future::Future<Output = Result<serde_json::Value, StartRefusal>> + Send + 'static,
    {
        self.next_epoch = self.next_epoch.saturating_add(1);
        let link = OperationLink {
            id: self.next_epoch,
            events: self.events.clone(),
        };
        let id = link.id;
        let events = self.events.clone();
        let owner = session_id.clone();
        let operation = run(self.control.clone(), link, owner.clone());
        let environment = self.environment.clone();
        let (completed, result) = oneshot::channel();
        let task = mj_core::runtime::spawn(async move {
            let held = {
                let owner = owner.clone();
                async move {
                    // Each of these operations sends reviewer work to the
                    // worker. A recovery copy already running is waited for,
                    // and the next one stays out until the operation ends.
                    let deadline =
                        tokio::time::Instant::now() + super::reviewer::BACKGROUND_WORK_WAIT;
                    let _background =
                        super::reviewer::hold_background_work(&environment, &owner, deadline)
                            .await?;
                    operation.await
                }
            };
            let result = std::panic::AssertUnwindSafe(held)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    tracing::error!(session_id = %owner, "push review {name} task panicked");
                    Err(StartRefusal(format!("push review {name} task panicked")))
                });
            let _ = completed.send(result);
            let _ = events.send(HostEvent::OperationFinished { operation: id });
        });
        self.operations.insert(
            id,
            Operation {
                session_id: session_id.clone(),
                name,
                task,
                result,
                reply,
            },
        );
        self.publish(&session_id);
    }

    /// Decides what an operation's task asked for.
    pub fn operation_request(
        &mut self,
        operation: u64,
        request: OperationRequest,
        reply: oneshot::Sender<Result<OperationAnswer, StartRefusal>>,
    ) {
        let Some((session_id, name)) = self
            .operations
            .get(&operation)
            .map(|held| (held.session_id.clone(), held.name))
        else {
            let _ = reply.send(Err(StartRefusal("the operation already ended".into())));
            return;
        };
        let _ = reply.send(self.decide(operation, &session_id, name, request));
    }

    fn decide(
        &mut self,
        operation: u64,
        session_id: &str,
        name: &'static str,
        request: OperationRequest,
    ) -> Result<OperationAnswer, StartRefusal> {
        match request {
            OperationRequest::ReserveOverride { protocol, roots } => {
                let record = self.select_findings(session_id, protocol, roots.as_deref())?;
                self.reserve(
                    operation,
                    session_id,
                    name,
                    protocol,
                    record.trees.keys().cloned().collect(),
                    Some(record.review_id.clone()),
                )?;
                Ok(OperationAnswer::Reserved {
                    findings: Some(record),
                })
            }
            OperationRequest::ReserveBase { protocol, root } => {
                self.reserve(
                    operation,
                    session_id,
                    name,
                    protocol,
                    root.into_iter().collect(),
                    None,
                )?;
                Ok(OperationAnswer::Reserved { findings: None })
            }
            OperationRequest::ReserveCaptured { protocol, roots } => {
                self.reserve(operation, session_id, name, protocol, roots, None)?;
                Ok(OperationAnswer::Reserved { findings: None })
            }
            OperationRequest::DeclineFindings { overrides } => {
                let selected = self
                    .review_state
                    .get(session_id)
                    .and_then(|owner| {
                        owner
                            .reservations
                            .iter()
                            .find(|held| held.operation == operation)
                            .and_then(|held| held.findings.clone())
                            .and_then(|id| {
                                owner
                                    .part
                                    .push_findings
                                    .iter()
                                    .find(|record| record.review_id == id)
                                    .cloned()
                            })
                    })
                    .ok_or_else(|| {
                        StartRefusal(
                            "the findings verdict is gone; call request_review again".into(),
                        )
                    })?;
                if let Some(owner) = self.review_state.get_mut(session_id) {
                    owner
                        .part
                        .decline_findings(overrides, selected.trees.keys().cloned().collect());
                }
                self.saved("push review override state", session_id)
            }
            OperationRequest::FinishOverride => {
                if let Some(owner) = self.review_state.get_mut(session_id)
                    && let Some(held) = owner
                        .reservations
                        .iter_mut()
                        .find(|held| held.operation == operation)
                    && let Some(id) = held.findings.clone()
                    && let Some(position) = owner
                        .part
                        .push_findings
                        .iter()
                        .position(|record| record.review_id == id)
                {
                    held.retired = Some(owner.part.push_findings.remove(position));
                }
                self.saved("push review override state", session_id)
            }
            OperationRequest::FinishBase { root, changed } => {
                if !changed {
                    return Ok(OperationAnswer::Done);
                }
                // Only this repository starts a new range; other declined
                // ranges remain.
                if let Some(owner) = self.review_state.get_mut(session_id) {
                    owner.part.advance_review_bases(std::slice::from_ref(&root));
                }
                self.saved("push review baseline state", session_id)
            }
        }
    }

    fn saved(
        &self,
        operation: &'static str,
        session_id: &str,
    ) -> Result<OperationAnswer, StartRefusal> {
        self.persist_acknowledged(operation, session_id)
            .map(OperationAnswer::Saved)
            .map_err(|error| {
                StartRefusal(format!(
                    "review persistence stopped before the change was recorded: {error}"
                ))
            })
    }

    /// An operation ended on some path. It releases its roots, and counts a
    /// base move if it held any, because a failed override or baseline move
    /// can already have written a ref.
    pub fn operation_finished(&mut self, operation: u64) {
        let Some(Operation {
            session_id,
            mut result,
            reply,
            ..
        }) = self.operations.remove(&operation)
        else {
            return;
        };
        let response = result
            .try_recv()
            .unwrap_or_else(|_| Err(StartRefusal("push review operation task stopped".into())));
        if let Some(held) = self.release(operation, &session_id) {
            if let (Err(_), Some(retired), Some(owner)) = (
                &response,
                held.retired,
                self.review_state.get_mut(&session_id),
            ) {
                // The verdict's retirement did not become durable, so it is not
                // done: a retry must still find the verdict.
                owner.part.push_findings.push(retired);
            }
            self.moved_base(&session_id);
        }
        let _ = reply.send(response);
        self.publish(&session_id);
    }
}
