//! A supervised completion gate: user input always wins over automatic work.
use super::*;
use crate::session_manager::{
    CoalescedUpdateSender, ManagedSessionView, SessionManagerUpdate, SessionManagerUpdates,
    coalesced_update_channel,
};
use mj_core::activity::ActivityState;
use mj_core::relay::{RelayCommand, RelayCursor};
use mj_core::state::{PromptCompletion, TurnOutcomeKind, classify_prompt_completion};
use tokio::task::{AbortHandle, JoinSet};

enum Outcome {
    Classified(Result<mj_core::continuation::ContinuationVerdict>),
    Submitted(Result<Box<ManagedSessionView>>),
}

struct Pending {
    generation: u64,
    abort: AbortHandle,
    view: ManagedSessionView,
    user: String,
    completed: String,
    evidence_ordinal: Option<u64>,
}

fn evidence_ordinal(view: &ManagedSessionView) -> Option<u64> {
    view.snapshot
        .as_ref()?
        .materialized
        .transcript
        .iter()
        .rev()
        .find(|item| {
            matches!(
                item.body,
                mj_core::transcript::TranscriptBody::User { .. }
                    | mj_core::transcript::TranscriptBody::Agent { .. }
            )
        })
        .map(|item| item.latest_content_event_ordinal.unwrap_or(item.position))
}

fn eligible(view: &ManagedSessionView) -> bool {
    let Some(s) = &view.snapshot else {
        return false;
    };
    view.connected && s.operational.relay_protocol_version.is_some_and(|v| v >= 18)
        && s.operational.continuation.eligible()
        && mj_core::activity::is_quiet(&s.operational.facts())
        && s.operational.background_commands.is_empty()
        && !s.operational.goal.active()
        && s.materialized.pending_elicitations.is_empty()
        && s.materialized.last_turn_outcome.as_ref().is_some_and(|t| matches!(&t.outcome,
            TurnOutcomeKind::Completed { stop_reason } if classify_prompt_completion(stop_reason) == PromptCompletion::Finished))
}

fn allowed(state: &RuntimeState, session: &str) -> bool {
    let enabled = {
        let controller = state
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        controller.config.continuation.enabled
            && controller.state.sessions.contains_key(session)
            && !controller.state.subagents.contains_key(session)
    };
    enabled
        && !crate::controller::move_session::move_owns_session(session)
        && !state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(session)
        && crate::review_host::prompt_refusal(session).is_none()
}

#[derive(Clone)]
struct Environment {
    control: SessionManagerControl,
    allowed: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    live: Arc<dyn Fn() -> BTreeSet<String> + Send + Sync>,
    review: ReviewObserver,
}
type ReviewObserver = Arc<dyn Fn(&str, &ManagedSessionView) + Send + Sync>;
type Classifier = Arc<
    dyn Fn(
            mj_core::continuation::ContinuationEvidence,
        ) -> futures::future::BoxFuture<
            'static,
            Result<mj_core::continuation::ContinuationVerdict>,
        > + Send
        + Sync,
>;

fn publish(
    tx: &CoalescedUpdateSender,
    environment: &Environment,
    session_id: String,
    mut view: ManagedSessionView,
    checking: bool,
) {
    if checking {
        if let Some(snapshot) = &mut view.snapshot {
            snapshot.operational.activity = Some(ActivityState::CheckingContinuation);
        }
    } else {
        (environment.review)(&session_id, &view);
    }
    tx.send(SessionManagerUpdate { session_id, view });
}

pub(super) fn spawn(
    state: Arc<RuntimeState>,
    input: SessionManagerUpdates,
    cancellation: CancellationToken,
) -> (SessionManagerUpdates, tokio::task::JoinHandle<Result<()>>) {
    let environment = Environment {
        control: state.session_manager.clone(),
        allowed: {
            let state = state.clone();
            Arc::new(move |id| allowed(&state, id))
        },
        live: {
            let state = state.clone();
            Arc::new(move || state.live_session_ids())
        },
        review: Arc::new(move |id, view| state.review_host().observe(id, view)),
    };
    spawn_in(
        environment,
        input,
        cancellation,
        Arc::new(|evidence| {
            Box::pin(async move { crate::continuation::classify(&evidence).await })
        }),
    )
}

fn spawn_in(
    environment: Environment,
    mut input: SessionManagerUpdates,
    cancellation: CancellationToken,
    classifier: Classifier,
) -> (SessionManagerUpdates, tokio::task::JoinHandle<Result<()>>) {
    let (tx, rx) = coalesced_update_channel();
    let task = tokio::spawn(async move {
        let mut seen = BTreeMap::<String, Option<String>>::new();
        let mut pending = BTreeMap::<String, Pending>::new();
        let mut jobs = JoinSet::new();

        let mut generation = 0_u64;
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break,
                update = input.recv() => {
                    let Some(update) = update else { break };
                    let id = update.session_id;
                    let view = update.view;
                    let completed = view.snapshot.as_ref().and_then(|s| {
                        s.materialized
                            .last_turn_outcome
                            .as_ref()
                            .map(|t| t.command_id.clone())
                    });
                    let previous = seen.insert(id.clone(), completed.clone());
                    if let Some(p) = pending.get_mut(&id) {
                        let unchanged = eligible(&view)
                            && (environment.allowed)(&id)
                            && view.snapshot.as_ref().is_some_and(|s| {
                                s.operational.continuation.user_command_id.as_ref() == Some(&p.user)
                                    && s.operational.continuation.completed_command_id.as_ref()
                                        == Some(&p.completed)
                            })
                            && evidence_ordinal(&view) == p.evidence_ordinal;
                        if unchanged {
                            p.view = view.clone();
                            publish(&tx, &environment, id, view, true);
                            continue;
                        }
                        pending.remove(&id).expect("pending request").abort.abort();
                    }
                    let new_completion = previous.is_some_and(|old| old != completed) && completed.is_some();
                    if new_completion && eligible(&view) && (environment.allowed)(&id) {
                        generation = generation.wrapping_add(1);
                        let epoch = generation;
                        let session = id.clone();
                        let snapshot = view.snapshot.as_ref().expect("eligible snapshot").clone();
                        let classify = classifier.clone();
                        let abort = jobs.spawn(async move {
                            let result = async {
                                let evidence = tokio::task::spawn_blocking(move || {
                                    let materialized = snapshot.materialized;
                                    if snapshot.window.omitted_items > 0 {
                                        return crate::database::load_continuation_evidence(
                                            &materialized.session_id,
                                            materialized.applied_event_ordinal,
                                            &materialized.applied_event_digest,
                                        );
                                    }
                                    crate::continuation::evidence(&materialized)
                                })
                                .await
                                .context("collect continuation evidence")??;
                                classify(evidence).await
                            }
                            .await;
                            (session, epoch, Outcome::Classified(result))
                        });
                        let s = &view
                            .snapshot
                            .as_ref()
                            .expect("eligible snapshot")
                            .operational
                            .continuation;
                        pending.insert(
                            id.clone(),
                            Pending {
                                generation: epoch,
                                abort,
                                view: view.clone(),
                                user: s.user_command_id.clone().unwrap(),
                                completed: s.completed_command_id.clone().unwrap(),
                                evidence_ordinal: evidence_ordinal(&view),
                            },
                        );
                        publish(&tx, &environment, id, view, true);
                    } else {
                        publish(&tx, &environment, id, view, false);
                    }
                }
                result = jobs.join_next(), if !jobs.is_empty() => {
                    match result {
                        Some(Ok((id, epoch, result))) => {
                            if pending.get(&id).is_none_or(|p| p.generation != epoch) {
                                continue;
                            }
                            let p = pending.remove(&id).expect("current request");
                            let result = match result {
                                Outcome::Submitted(result) => {
                                    match result {
                                        Ok(view) => {
                                            tx.send(SessionManagerUpdate {
                                                session_id: id,
                                                view: *view,
                                            });
                                        }
                                        Err(error) => {
                                            tracing::warn!(session=%id,%error,"automatic continuation not submitted");
                                            publish(&tx, &environment, id, p.view, false);
                                        }
                                    }
                                    continue;
                                }
                                Outcome::Classified(result) => result,
                            };
                            let continuing = result.as_ref().is_ok_and(|v| v.should_continue())
                                && eligible(&p.view)
                                && (environment.allowed)(&id);
                            tracing::info!(target:"mj_jev",session=%id,generation=epoch,?result,continuing,"continuation decision");
                            if continuing {
                                let control = environment.control.clone();
                                let environment = environment.clone();
                                let submit_id = id.clone();
                                let user = p.user.clone();
                                let completed = p.completed.clone();
                                let evidence_frontier = p.evidence_ordinal;
                                let abort = jobs.spawn(async move {
                                    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
                                        let handle = control
                                            .wait_for_session(&submit_id, Duration::from_secs(5))
                                            .await?;
                                        let current = handle.view();
                                        ensure!(
                                            eligible(&current)
                                                && (environment.allowed)(&submit_id)
                                                && evidence_ordinal(&current) == evidence_frontier,
                                            "continuation no longer eligible"
                                        );
                                        let snapshot = current.snapshot.as_ref().unwrap();
                                        let c = &snapshot.operational.continuation;
                                        ensure!(
                                            c.user_command_id.as_ref() == Some(&user)
                                                && c.completed_command_id.as_ref() == Some(&completed),
                                            "continuation evidence changed"
                                        );
                                        let command_id = format!(
                                            "auto-continue-{}-{}",
                                            snapshot
                                                .materialized
                                                .last_turn_outcome
                                                .as_ref()
                                                .unwrap()
                                                .completed_ordinal,
                                            c.attempts + 1
                                        );
                                        handle
                                            .submit(
                                                command_id,
                                                RelayCommand::ContinueAuthorizedWork {
                                                    expected: RelayCursor {
                                                        ordinal: snapshot.operational.latest_ordinal,
                                                        digest: snapshot.operational.latest_digest.clone(),
                                                    },
                                                    user_command_id: user,
                                                    completed_command_id: completed,
                                                    attempt: c.attempts + 1,
                                                },
                                            )
                                            .await?;
                                        handle.sync_now().await?;
                                        Ok::<_, anyhow::Error>(Box::new(handle.view()))
                                    })
                                    .await
                                    .context("continuation submission timed out")
                                    .and_then(|result| result);
                                    (submit_id, epoch, Outcome::Submitted(outcome))
                                });
                                pending.insert(id, Pending { abort, ..p });
                            } else {
                                publish(&tx, &environment, id, p.view, false);
                            }
                        }
                        Some(Err(error)) if !error.is_cancelled() => {
                            tracing::error!(%error,"continuation classifier task failed");
                            let failed = pending
                                .iter()
                                .find(|(_, p)| p.abort.id() == error.id())
                                .map(|(id, _)| id.clone());
                            if let Some(id) = failed {
                                let p = pending.remove(&id).unwrap();
                                publish(&tx, &environment, id, p.view, false);
                            }
                        }
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    let invalid: Vec<_> = pending
                        .keys()
                        .filter(|id| !(environment.allowed)(id))
                        .cloned()
                        .collect();
                    for id in invalid {
                        let p = pending.remove(&id).unwrap();
                        p.abort.abort();
                        publish(&tx, &environment, id, p.view, false);
                    }
                    let live = (environment.live)();
                    seen.retain(|id, _| live.contains(id));
                }
            }
        }
        jobs.shutdown().await;

        Ok(())
    });
    (rx, task)
}

#[cfg(test)]
mod tests;
