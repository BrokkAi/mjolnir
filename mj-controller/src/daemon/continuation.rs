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
    QuotaPrepared(Result<mj_core::continuation::QuotaRecovery>),
    Submitted(Result<Box<ManagedSessionView>>),
}

struct Pending {
    _upgrade_work: crate::upgrade::Work,
    diagnostic: Option<mj_core::jev::Attempt>,
    submitting: bool,
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
        && s.operational.continuation.quota_recovery.as_ref().is_none_or(|r| r.submitted)
        && s.operational.continuation.eligible()
        && mj_core::activity::is_quiet(&s.operational.facts())
        && s.operational.background_commands.is_empty()
        && !s.operational.goal.active()
        && s.materialized.pending_elicitations.is_empty()
        && s.materialized.last_turn_outcome.as_ref().is_some_and(|t| matches!(&t.outcome,
            TurnOutcomeKind::Completed { stop_reason } if classify_prompt_completion(stop_reason) == PromptCompletion::Finished))
}

fn quota_eligible(view: &ManagedSessionView) -> bool {
    let Some(s) = &view.snapshot else {
        return false;
    };
    let c = &s.operational.continuation;
    view.connected && s.operational.relay_protocol_version.is_some_and(|v| v >= 20)
        && !c.quota_suppressed && c.user_command_id.is_some() && c.completed_command_id.is_some()
        && mj_core::activity::is_quiet(&s.operational.facts())
        && s.operational.background_commands.is_empty() && !s.operational.goal.active()
        && s.materialized.pending_elicitations.is_empty()
        && s.materialized.last_turn_outcome.as_ref().is_some_and(|t| {
            c.completed_command_id.as_ref() == Some(&t.command_id)
                && matches!(&t.outcome, TurnOutcomeKind::Completed { stop_reason }
                    if !mj_core::relay::is_capacity_stop_reason(stop_reason)
                    && !matches!(classify_prompt_completion(stop_reason), PromptCompletion::Cancelled))
        })
}

fn check_eligible(view: &ManagedSessionView) -> bool {
    eligible(view)
        || (quota_eligible(view)
            && view.snapshot.as_ref().is_some_and(|s| {
                let c = &s.operational.continuation;
                c.quota_recovery.as_ref().is_none_or(|r| {
                    c.completed_command_id.as_ref() != Some(&r.completed_command_id)
                })
            }))
}

fn allowed(state: &RuntimeState, session: &str) -> bool {
    let enabled = {
        let controller = state
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        controller.config.automatic_continuation_enabled()
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
    log: Option<mj_core::jev::DecisionLog>,
    control: SessionManagerControl,
    allowed: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    live: Arc<dyn Fn() -> BTreeSet<String> + Send + Sync>,
    review: ReviewObserver,
    quota: quota::Resolver,
    profile: ProfileLookup,
}
type ProfileLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
type ReviewObserver = Arc<dyn Fn(&str, &ManagedSessionView) + Send + Sync>;
type Classifier = Arc<
    dyn Fn(
            mj_core::continuation::ContinuationEvidence,
            Option<mj_core::jev::Attempt>,
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
    } else if view.snapshot.as_ref().is_none_or(|s| {
        s.operational
            .continuation
            .quota_recovery
            .as_ref()
            .is_none_or(|r| r.submitted)
    }) {
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
        log: match mj_core::jev::DecisionLog::open(mj_core::jev::controller_log_dir()) {
            Ok(log) => Some(log),
            Err(error) => {
                tracing::warn!(%error, "Jev diagnostic log unavailable");
                None
            }
        },
        control: state.session_manager.clone(),
        allowed: {
            let state = state.clone();
            Arc::new(move |id| allowed(&state, id))
        },
        live: {
            let state = state.clone();
            Arc::new(move || state.live_session_ids())
        },
        quota: {
            let state = state.clone();
            let service = Arc::new(quota::Service::default());
            Arc::new(move |id, view| {
                let state = state.clone();
                let service = service.clone();
                Box::pin(async move { quota::prepare(&state, &service, &id, &view).await })
            })
        },
        profile: {
            let state = state.clone();
            Arc::new(move |id| {
                state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .state
                    .sessions
                    .get(id)
                    .map(|s| s.last_profile.clone())
            })
        },
        review: Arc::new(move |id, view| state.review_host().observe(id, view)),
    };
    spawn_in(
        environment,
        input,
        cancellation,
        Arc::new(|evidence, diagnostic| {
            Box::pin(
                async move { crate::continuation::classify(&evidence, diagnostic.as_ref()).await },
            )
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
        let mut latest = BTreeMap::<String, ManagedSessionView>::new();
        let mut retry_after = BTreeMap::<String, std::time::Instant>::new();
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
                    latest.insert(id.clone(), view.clone());
                    let completed = view.snapshot.as_ref().and_then(|s| {
                        s.materialized
                            .last_turn_outcome
                            .as_ref()
                            .map(|t| t.command_id.clone())
                    });
                    let previous = seen.insert(id.clone(), completed.clone());
                    if let Some(p) = pending.get_mut(&id) {
                        let unchanged = check_eligible(&view)
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
                        let previous = pending.remove(&id).expect("pending request");
                        if !previous.submitting {
                            if let Some(diagnostic) = &previous.diagnostic {
                                diagnostic.finish("stale", "Session evidence or runtime state changed before this check completed; no action was taken.");
                            }
                            previous.abort.abort();
                        }
                        // An already-dispatched submission retains its bounded task
                        // to record the worker's actual acceptance or rejection.
                        // Its atomic frontier guard still lets new input win.
                    }
                    let new_completion = previous.is_some_and(|old| old != completed) && completed.is_some();
                    if new_completion && check_eligible(&view) && (environment.allowed)(&id) {
                        let Ok(upgrade_work) = crate::upgrade::activity("automatic continuation") else {
                            publish(&tx, &environment, id, view, false);
                            continue;
                        };
                        generation = generation.wrapping_add(1);
                        let epoch = generation;
                        let session = id.clone();
                        let snapshot = view.snapshot.as_ref().expect("eligible snapshot").clone();
                        let diagnostic = environment.log.as_ref().map(|log| log.start(&id, "continuation",
                            "Is the turn blocked by subscription quota, or does authorized unfinished work remain?",
                            "All real user instructions since context reset and whole recent assistant messages. Tool history is excluded; assistant_history_omitted reports older omitted assistant context."));
                        let request_diagnostic = diagnostic.clone();
                        let classify = classifier.clone();
                        let task_work = upgrade_work.clone();
                        let abort = jobs.spawn(async move {
                            let _task_work = task_work;
                            let result = async {
                                let evidence_work = _task_work.clone();
                                let evidence = tokio::task::spawn_blocking(move || {
                                    let _evidence_work = evidence_work;
                                    let quota_message = quota::message(&snapshot.materialized);
                                    let materialized = snapshot.materialized;
                                    let ordinary = if snapshot.window.omitted_items > 0 {
                                        crate::database::load_continuation_evidence(
                                            &materialized.session_id,
                                            materialized.applied_event_ordinal,
                                            &materialized.applied_event_digest,
                                        )
                                    } else { crate::continuation::evidence(&materialized) };
                                    let mut evidence = ordinary.unwrap_or_else(|error| {
                                        tracing::debug!(%error, "ordinary continuation evidence unavailable; checking only current quota message");
                                        mj_core::continuation::ContinuationEvidence {
                                            messages: Vec::new(), assistant_history_omitted: true, quota_message: None,
                                        }
                                    });
                                    evidence.quota_message = quota_message;
                                    if evidence.validate().is_err() && evidence.quota_message.is_some() {
                                        evidence.messages.clear();
                                        evidence.assistant_history_omitted = true;
                                    }
                                    evidence.validate()?;
                                    Ok::<_, anyhow::Error>(evidence)
                                })
                                .await
                                .context("collect continuation evidence")??;
                                classify(evidence, request_diagnostic).await
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
                                _upgrade_work: upgrade_work,
                                diagnostic,
                                submitting: false,
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
                                Outcome::QuotaPrepared(result) => {
                                    match result {
                                        Ok(recovery) if (environment.allowed)(&id) && quota_eligible(&p.view) => {
                                            let env = environment.clone(); let session = id.clone(); let view = p.view.clone();
                                            let task_work = p._upgrade_work.clone();
                                            let abort = jobs.spawn(async move {
                                                let _task_work = task_work;
                                                let result = quota::schedule(&env, &session, view, recovery).await;
                                                (session, epoch, Outcome::Submitted(result))
                                            });
                                            pending.insert(id, Pending { abort, submitting: true, ..p });
                                        }
                                        result => {
                                            tracing::warn!(session=%id, ?result, "quota recovery could not be scheduled");
                                            publish(&tx, &environment, id, p.view, false);
                                        }
                                    }
                                    continue;
                                }
                                Outcome::Submitted(result) => {
                                    match result {
                                        Ok(view) => {
                                            latest.insert(id.clone(), (*view).clone());
                                            tx.send(SessionManagerUpdate {
                                                session_id: id,
                                                view: *view,
                                            });
                                        }
                                        Err(error) => {
                                            if let Some(diagnostic) = &p.diagnostic {
                                                diagnostic.update(None, serde_json::json!({"submission_error":format!("{error:#}")}));
                                                diagnostic.finish("failed", "Automatic continuation could not be confirmed. No further continuation was submitted by this check.");
                                            }
                                            tracing::warn!(session=%id,%error,"automatic continuation not submitted");
                                            publish(&tx, &environment, id, p.view, false);
                                        }
                                    }
                                    continue;
                                }
                                Outcome::Classified(result) => result,
                            };
                            if let Some(diagnostic) = &p.diagnostic {
                                match &result {
                                    Ok(verdict) => diagnostic.update(Some(if verdict.is_quota_limit() { "Jev identified a current subscription quota limit." } else if verdict.should_continue() {
                                        "Jev assessed that already-requested work remains and needs no new input."
                                    } else { "Jev did not confidently establish both unfinished work and no need for user input." }), serde_json::json!({"result":{"quota_limit":verdict.quota_limit,"unfinished":verdict.unfinished,"no_input_needed":verdict.no_input_needed}})),
                                    Err(error) => diagnostic.update(Some("No usable Jev answer."), serde_json::json!({"error":format!("{error:#}")})),
                                }
                            }
                            if result.as_ref().is_ok_and(|v| v.is_quota_limit())
                                && quota_eligible(&p.view) && (environment.allowed)(&id) {
                                let env = environment.clone();
                                let session = id.clone();
                                let view = p.view.clone();
                                let task_work = p._upgrade_work.clone();
                                            let abort = jobs.spawn(async move {
                                                let _task_work = task_work;
                                    let result = (env.quota)(session.clone(), view).await;
                                    (session, epoch, Outcome::QuotaPrepared(result))
                                });
                                if let Some(diagnostic) = &p.diagnostic {
                                    diagnostic.finish("applied", "Quota exhaustion identified; resolving the reset deadline without an LLM.");
                                }
                                pending.insert(id, Pending { abort, submitting: false, ..p });
                                continue;
                            }
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
                                let diagnostic = p.diagnostic.clone();
                                let task_work = p._upgrade_work.clone();
                                            let abort = jobs.spawn(async move {
                                                let _task_work = task_work;
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
                                        if let Some(diagnostic) = &diagnostic {
                                            diagnostic.update(None, serde_json::json!({"command_id":command_id, "attempt":c.attempts + 1, "expected_ordinal":snapshot.operational.latest_ordinal}));
                                        }
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
                                        if let Some(diagnostic) = &diagnostic {
                                            diagnostic.finish("applied", "The worker accepted mj's automatic continuation of already-requested work.");
                                        }
                                        handle.sync_now().await?;
                                        Ok::<_, anyhow::Error>(Box::new(handle.view()))
                                    })
                                    .await
                                    .context("continuation submission timed out")
                                    .and_then(|result| result);
                                    if let Err(error) = &outcome {
                                        tracing::warn!(session=%submit_id, %error, "automatic continuation submission or refresh failed");
                                        if let Some(diagnostic) = &diagnostic {
                                            diagnostic.update(None, serde_json::json!({"submission_error":format!("{error:#}")}));
                                            diagnostic.finish("failed", "Mj could not confirm automatic continuation. The submission failed or the worker rejected its guard.");
                                        }
                                    }
                                    (submit_id, epoch, Outcome::Submitted(outcome))
                                });
                                pending.insert(id, Pending { abort, submitting: true, ..p });
                            } else {
                                if let Some(diagnostic) = &p.diagnostic {
                                    let (status, action) = match &result {
                                        Err(_) => ("failed", "Mj left the session ready for operator input because the check failed."),
                                        Ok(v) if !v.should_continue() => ("uncertain", "Both scores must reach 90%. Mj left the session ready for operator input."),
                                        _ => ("stale", "The session is no longer eligible; mj did not continue it."),
                                    };
                                    diagnostic.finish(status, action);
                                }
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
                        if !p.submitting {
                            if let Some(diagnostic) = &p.diagnostic {
                                diagnostic.finish("cancelled", "Continuation was disabled or a session lifecycle operation took ownership.");
                            }
                            p.abort.abort();
                        }
                        publish(&tx, &environment, id, p.view, false);
                    }
                    for (id, view) in &latest {
                        if pending.contains_key(id) || retry_after.get(id).is_some_and(|t| *t > std::time::Instant::now()) { continue; }
                        let Some(snapshot) = &view.snapshot else { continue; };
                        let Some(recovery) = snapshot.operational.continuation.quota_recovery.as_ref().filter(|r| !r.submitted) else { continue; };
                        let clear = !(environment.allowed)(id) || (environment.profile)(id).as_deref() != Some(&recovery.profile_id);
                        if !clear && (!quota_eligible(view) || recovery.retry_at_ms.is_none_or(|t| t > mj_core::clock::epoch_millis())) { continue; }
                        if !view.connected { continue; }
                        generation = generation.wrapping_add(1);
                        let epoch = generation;
                        let Ok(upgrade_work) = crate::upgrade::activity("quota continuation") else { continue };
                        let env = environment.clone(); let session = id.clone(); let current = view.clone();
                        let task_work = upgrade_work.clone();
                        let abort = jobs.spawn(async move {
                            let _task_work = task_work;
                            let result = quota::resume(&env, &session, &current, clear).await;
                            (session, epoch, Outcome::Submitted(result))
                        });
                        retry_after.insert(id.clone(), std::time::Instant::now() + Duration::from_secs(30));
                        pending.insert(id.clone(), Pending { _upgrade_work: upgrade_work, diagnostic: None, submitting: true, generation: epoch, abort,
                            view: view.clone(), user: recovery.user_command_id.clone(), completed: recovery.completed_command_id.clone(), evidence_ordinal: evidence_ordinal(view) });
                    }
                    let live = (environment.live)();
                    seen.retain(|id, _| live.contains(id));
                    latest.retain(|id, _| live.contains(id));
                    retry_after.retain(|id, _| live.contains(id));
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

mod quota;
