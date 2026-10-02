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

/// Workers from this protocol on record turns the harness ends on its own and
/// admit continuation by [`mj_core::activity::driver_present`] instead of raw
/// background counts, and without a plan-mode exclusion.
const ENDED_TURN_PROTOCOL: u32 = 23;

fn current_rules(s: &mj_core::state::ManagedSessionSnapshot) -> bool {
    s.operational
        .relay_protocol_version
        .is_some_and(|v| v >= ENDED_TURN_PROTOCOL)
}

/// The turn a check is about: the last prompted turn, or one the harness
/// started and ended on its own.
pub(super) struct EndedTurn<'a> {
    /// The id `continuation.completed_command_id` carries for this turn.
    pub key: &'a str,
    pub start_position: Option<u64>,
    pub completed_ordinal: u64,
    pub completed_at_ms: i64,
    /// How a prompted turn ended; a self-started turn has no outcome.
    pub outcome: Option<&'a TurnOutcomeKind>,
    pub diagnostic: Option<&'a mj_core::diagnostic::TurnDiagnostic>,
}

impl EndedTurn<'_> {
    fn self_started(&self) -> bool {
        self.outcome.is_none()
    }
    fn finished(&self) -> bool {
        self.outcome.is_none_or(|outcome| {
            matches!(outcome, TurnOutcomeKind::Completed { stop_reason }
                if classify_prompt_completion(stop_reason) == PromptCompletion::Finished)
        })
    }
    fn quota_candidate(&self) -> bool {
        self.outcome.is_none_or(|outcome| {
            matches!(outcome, TurnOutcomeKind::Completed { stop_reason }
                if !mj_core::relay::is_capacity_stop_reason(stop_reason)
                && !matches!(classify_prompt_completion(stop_reason), PromptCompletion::Cancelled))
        })
    }
}

pub(super) fn ended_turn(s: &mj_core::state::ManagedSessionSnapshot) -> Option<EndedTurn<'_>> {
    let c = &s.operational.continuation;
    if current_rules(s)
        && let Some(turn) = &c.harness_turn
        && c.completed_command_id.as_deref() == Some(turn.id.as_str())
    {
        return Some(EndedTurn {
            key: &turn.id,
            start_position: Some(turn.start_position),
            completed_ordinal: turn.settled_ordinal,
            completed_at_ms: turn.settled_at_ms,
            outcome: None,
            diagnostic: None,
        });
    }
    s.materialized
        .last_turn_outcome
        .as_ref()
        .map(|t| EndedTurn {
            key: &t.command_id,
            start_position: t.turn_start_position,
            completed_ordinal: t.completed_ordinal,
            completed_at_ms: t.completed_at_ms,
            outcome: Some(&t.outcome),
            diagnostic: t.diagnostic.as_ref(),
        })
}

/// What older workers still require before automatic work, and so what their
/// sessions are still held to.
fn legacy_quiet(s: &mj_core::state::ManagedSessionSnapshot) -> bool {
    mj_core::activity::is_quiet(&s.operational.facts())
        && s.operational.background_commands.is_empty()
        && !s.operational.goal.active()
}

/// Everything ordinary continuation needs except that nothing else is about
/// to move the session on.
fn continuation_allowed(view: &ManagedSessionView) -> bool {
    let Some(s) = &view.snapshot else {
        return false;
    };
    view.connected
        && s.operational
            .relay_protocol_version
            .is_some_and(|v| v >= 18)
        && !s.operational.goal.budget_limited()
        && s.operational
            .continuation
            .quota_recovery
            .as_ref()
            .is_none_or(|r| r.submitted)
        && s.operational.continuation.eligible()
        && s.materialized.pending_elicitations.is_empty()
        && ended_turn(s).is_some_and(|t| unified(s) || t.finished())
}

/// A background command, agent or goal will move the session on, so an
/// ordinary nudge waits until it stops.
fn driven(view: &ManagedSessionView) -> bool {
    view.snapshot.as_ref().is_some_and(|s| {
        current_rules(s) && mj_core::activity::driver_present(&s.operational.facts())
    })
}

/// What [`driven`] saw, for the decision log.
fn driver_name(view: &ManagedSessionView) -> &'static str {
    let Some(s) = &view.snapshot else {
        return "unknown";
    };
    match mj_core::activity::classify(&s.operational.facts()) {
        ActivityState::Background { .. } => "background",
        ActivityState::Goal => "goal",
        ActivityState::Retry => "retry",
        _ => "turn",
    }
}

fn eligible(view: &ManagedSessionView) -> bool {
    let Some(s) = &view.snapshot else {
        return false;
    };
    continuation_allowed(view)
        && if current_rules(s) {
            let facts = s.operational.facts();
            mj_core::activity::can_submit(&facts) && !mj_core::activity::driver_present(&facts)
        } else {
            legacy_quiet(s)
        }
}

/// Everything a quota recovery needs except the chance to submit a prompt.
fn quota_allowed(view: &ManagedSessionView) -> bool {
    quota_unallowed(view).is_empty()
}

/// What [`quota_allowed`] found missing, by name.
fn quota_unallowed(view: &ManagedSessionView) -> Vec<&'static str> {
    let Some(s) = &view.snapshot else {
        return vec!["no session snapshot"];
    };
    let c = &s.operational.continuation;
    let turn_matches = ended_turn(s).is_some_and(|t| {
        c.completed_command_id.as_deref() == Some(t.key) && (unified(s) || t.quota_candidate())
    });
    [
        (!view.connected, "session is not connected"),
        (
            s.operational.relay_protocol_version.is_none_or(|v| v < 20),
            "worker is too old for quota recovery",
        ),
        (
            c.quota_suppressed,
            "quota recovery is suppressed (a user action cancelled it)",
        ),
        (c.user_command_id.is_none(), "no user request to continue"),
        (s.operational.goal.budget_limited(), "goal budget is spent"),
        (
            !s.materialized.pending_elicitations.is_empty(),
            "a question is waiting for the user",
        ),
        (!turn_matches, "the blocked turn is no longer the last one"),
    ]
    .into_iter()
    .filter_map(|(blocked, name)| blocked.then_some(name))
    .collect()
}

/// A quota recovery may be recorded: a turn, background work or a goal does
/// not stop that, since none of them gets past the limit.
fn quota_schedulable(view: &ManagedSessionView) -> bool {
    quota_allowed(view)
        && view
            .snapshot
            .as_ref()
            .is_some_and(|s| current_rules(s) || legacy_quiet(s))
}

/// Due quota recoveries the loop declined to submit. A due deadline that
/// passes with nothing happening looks like a lost recovery, so each refusal
/// is logged at info once a minute with the facts that refused it, and written
/// to the decision log whenever the reasons change.
#[derive(Default)]
struct QuotaSkips(BTreeMap<String, (std::time::Instant, String)>);

impl QuotaSkips {
    const EVERY: Duration = Duration::from_secs(60);

    fn note(
        &mut self,
        log: Option<&mj_core::jev::DecisionLog>,
        session: &str,
        recovery: &mj_core::continuation::QuotaRecovery,
        reasons: &[&str],
    ) {
        let reason = reasons.join("; ");
        let now = std::time::Instant::now();
        let previous = self.0.get(session);
        if previous
            .is_some_and(|(at, seen)| *seen == reason && now.duration_since(*at) < Self::EVERY)
        {
            return;
        }
        let changed = previous.is_none_or(|(_, seen)| *seen != reason);
        tracing::info!(
            session,
            retry_at_ms = ?recovery.retry_at_ms,
            reason = %reason,
            "a due quota recovery was not submitted"
        );
        if changed && let Some(log) = log {
            let attempt = log.start(
                session,
                "quota-recovery",
                "Can the quota recovery that is due be submitted now?",
                "Session activity facts and the stored quota recovery.",
            );
            attempt.update(
                Some("Not yet."),
                serde_json::json!({"retry_at_ms": recovery.retry_at_ms, "refused_by": reasons}),
            );
            attempt.finish(
                "deferred",
                &format!("The due quota recovery was not submitted: {reason}. It is checked again every minute."),
            );
        }
        self.0.insert(session.to_owned(), (now, reason));
    }

    fn forget(&mut self, session: &str) {
        self.0.remove(session);
    }

    fn retain(&mut self, live: &BTreeSet<String>) {
        self.0.retain(|id, _| live.contains(id));
    }
}

/// A due quota recovery may be submitted now.
fn quota_resumable(view: &ManagedSessionView) -> bool {
    quota_resume_blockers(view).is_empty()
}

/// Why a due quota recovery cannot be submitted now, by name; empty when it can.
fn quota_resume_blockers(view: &ManagedSessionView) -> Vec<&'static str> {
    let mut blockers = quota_unallowed(view);
    if let Some(s) = &view.snapshot {
        if current_rules(s) {
            blockers.extend(mj_core::activity::submit_blockers(&s.operational.facts()));
        } else if !legacy_quiet(s) {
            blockers.push("the session is not quiet");
        }
    }
    blockers
}

fn unified(s: &mj_core::state::ManagedSessionSnapshot) -> bool {
    s.operational
        .relay_protocol_version
        .is_some_and(|v| v >= mj_core::assessment::PROTOCOL)
}

fn check_eligible(view: &ManagedSessionView) -> bool {
    let Some(s) = &view.snapshot else {
        return false;
    };
    if unified(s) {
        return s.operational.assessment.as_ref().is_some_and(|a| {
            a.current()
                && match a.action {
                    Some(mj_core::assessment::Action::Continue) => continuation_allowed(view),
                    Some(mj_core::assessment::Action::RecoverQuota) => {
                        quota_schedulable(view)
                            && s.operational
                                .continuation
                                .quota_recovery
                                .as_ref()
                                .is_none_or(|r| r.completed_command_id != a.turn_id)
                    }
                    _ => false,
                }
        });
    }
    // Current workers are checked whatever else is running; the answer is
    // acted on only when it can be.
    let ordinary = if current_rules(s) {
        continuation_allowed(view)
    } else {
        eligible(view)
    };
    ordinary
        || (quota_schedulable(view) && {
            let c = &s.operational.continuation;
            c.quota_recovery
                .as_ref()
                .is_none_or(|r| c.completed_command_id.as_ref() != Some(&r.completed_command_id))
        })
}

fn allowed(state: &RuntimeState, session: &str) -> bool {
    let enabled = {
        let controller_owner = state.owner();
        let controller = controller_owner.controller();
        controller.config.automatic_continuation_enabled()
            && controller.state.sessions.contains_key(session)
            && !controller.state.subagents.contains_key(session)
    };
    enabled
        && !crate::controller::move_session::move_owns_session(session)
        && !state.owner().lifecycle.contains_key(session)
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

/// Why the continuation service stopped forwarding session updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FeedEnd {
    /// The daemon asked the service to stop.
    Shutdown,
    /// The session manager closed its update channel while no shutdown was
    /// requested. The daemon stops the session manager only after it has
    /// joined this service, so this is never part of a graceful shutdown.
    SessionManagerStopped,
}

/// Session updates as the serving loop receives them, after the continuation
/// service has seen them.
///
/// The service is the only sender on this channel and decides, at one point,
/// why it stopped. A closed channel therefore says nothing by itself: the
/// feed reports the service's decision instead of guessing a cause.
pub(super) struct UpdateFeed {
    updates: SessionManagerUpdates,
    service: Option<tokio::task::JoinHandle<Result<FeedEnd>>>,
}

impl UpdateFeed {
    fn new(
        updates: SessionManagerUpdates,
        service: tokio::task::JoinHandle<Result<FeedEnd>>,
    ) -> Self {
        Self {
            updates,
            service: Some(service),
        }
    }

    /// The next update. `Ok(None)` means the service stopped because shutdown
    /// was requested. An error means the session manager stopped without a
    /// shutdown request, or the service failed. After the end is reported,
    /// the feed stays pending. Cancel safe.
    pub(super) async fn next(&mut self) -> Result<Option<SessionManagerUpdate>> {
        if let Some(update) = self.updates.recv().await {
            return Ok(Some(update));
        }
        let Some(service) = self.service.as_mut() else {
            return std::future::pending().await;
        };
        // The service holds the only sender and drops it as it finishes, so
        // its result is ready or about to be.
        let end = service.await;
        self.service = None;
        match end.context("continuation service task failed")?? {
            FeedEnd::Shutdown => Ok(None),
            FeedEnd::SessionManagerStopped => bail!("controller daemon session manager stopped"),
        }
    }

    /// Wait for the service after the daemon has cancelled it. An end that
    /// [`Self::next`] already reported is not reported again.
    pub(super) async fn join(self) -> Result<()> {
        let Some(service) = self.service else {
            return Ok(());
        };
        match service
            .await
            .context("continuation service task failed")??
        {
            FeedEnd::Shutdown => Ok(()),
            FeedEnd::SessionManagerStopped => bail!("controller daemon session manager stopped"),
        }
    }
}

pub(super) fn spawn(
    state: Arc<RuntimeState>,
    input: SessionManagerUpdates,
    cancellation: CancellationToken,
) -> UpdateFeed {
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
                    .owner()
                    .controller()
                    .state
                    .sessions
                    .get(id)
                    .map(|s| s.last_profile.clone())
            })
        },
        review: Arc::new(move |id, view| state.review_host().observe(id, view)),
    };
    let (updates, service) = spawn_in(
        environment,
        input,
        cancellation,
        Arc::new(|evidence, diagnostic| {
            Box::pin(
                async move { crate::continuation::classify(&evidence, diagnostic.as_ref()).await },
            )
        }),
    );
    UpdateFeed::new(updates, service)
}

fn spawn_in(
    environment: Environment,
    input: SessionManagerUpdates,
    cancellation: CancellationToken,
    classifier: Classifier,
) -> (
    SessionManagerUpdates,
    tokio::task::JoinHandle<Result<FeedEnd>>,
) {
    spawn_with_gate(
        environment,
        input,
        cancellation,
        classifier,
        crate::upgrade::gate().clone(),
    )
}

fn spawn_with_gate(
    environment: Environment,
    mut input: SessionManagerUpdates,
    cancellation: CancellationToken,
    classifier: Classifier,
    gate: Arc<crate::upgrade::Gate>,
) -> (
    SessionManagerUpdates,
    tokio::task::JoinHandle<Result<FeedEnd>>,
) {
    let (tx, rx) = coalesced_update_channel();
    let task = tokio::spawn(async move {
        let mut seen = BTreeMap::<String, Option<String>>::new();
        // Ended turns whose continuation waits for a background command,
        // agent or goal to stop moving the session on.
        let mut deferred = BTreeMap::<String, String>::new();
        // A refused admission must not consume a completed-turn trigger.
        let mut admission_deferred = BTreeMap::<String, Option<String>>::new();
        let mut pending = BTreeMap::<String, Pending>::new();
        let mut latest = BTreeMap::<String, ManagedSessionView>::new();
        let mut retry_after = BTreeMap::<String, std::time::Instant>::new();
        let mut skips = QuotaSkips::default();
        let mut jobs = JoinSet::new();
        let mut seed_jobs = JoinSet::new();
        let mut seed_after = BTreeMap::<String, std::time::Instant>::new();
        let mut assessed = BTreeMap::<String, u64>::new();
        let mut rechecks = BTreeSet::<String>::new();
        let mut action_recheck = BTreeMap::<String, std::time::Instant>::new();

        let mut generation = 0_u64;
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        // The one point that decides why this service stops. Shutdown is
        // checked first, so an input that closes once shutdown is requested
        // still ends as a shutdown.
        let end = loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break FeedEnd::Shutdown,
                update = async {
                    loop {
                        tokio::select! {
                            update = input.recv() => return update,
                            id = async {
                                match rechecks.pop_first() {
                                    Some(id) => id,
                                    None => std::future::pending().await,
                                }
                            } => {
                                if let Some(view) = latest.get(&id) {
                                    return Some(SessionManagerUpdate { session_id: id, view: view.clone() });
                                }
                            }
                        }
                    }
                } => {
                    let Some(update) = update else { break FeedEnd::SessionManagerStopped };
                    let id = update.session_id;
                    let view = update.view;
                    latest.insert(id.clone(), view.clone());
                    if let Some(snapshot) = view.snapshot.as_ref().filter(|s| unified(s) && s.operational.assessment_context.is_none())
                        && view.connected && seed_after.get(&id).is_none_or(|at| *at <= std::time::Instant::now())
                        && let Ok(seed_work) = gate.enter_unless_draining("seed Jev authorization") {
                        seed_after.insert(id.clone(), std::time::Instant::now() + Duration::from_secs(60));
                        let snapshot = snapshot.clone();
                        let control = environment.control.clone();
                        let session = id.clone();
                        seed_jobs.spawn(async move {
                            let result = async {
                                let _work = seed_work;
                                let expected = RelayCursor { ordinal: snapshot.operational.latest_ordinal, digest: snapshot.operational.latest_digest.clone() };
                                let evidence = tokio::task::spawn_blocking(move || {
                                    crate::database::load_continuation_evidence(&snapshot.materialized.session_id, snapshot.materialized.applied_event_ordinal, &snapshot.materialized.applied_event_digest)
                                }).await.context("collect retained authorization")??;
                                let context = mj_core::assessment::ContextHistory { messages: evidence.messages, authorization_complete: true, assistant_history_omitted: evidence.assistant_history_omitted, open_assistant_id: None, final_reply_omitted: false };
                                let handle = control.wait_for_session(&session, Duration::from_secs(5)).await?;
                                handle.submit(format!("assessment-seed-{}", expected.ordinal), RelayCommand::SeedAssessmentContext { expected, context: Box::new(context) }).await?;
                                handle.sync_now().await?;
                                Ok::<_, anyhow::Error>(())
                            }.await;
                            (session, result)
                        });
                    }
                    let completed = view
                        .snapshot
                        .as_ref()
                        .and_then(ended_turn)
                        .map(|t| t.key.to_owned());
                    let self_started = view
                        .snapshot
                        .as_ref()
                        .and_then(ended_turn)
                        .is_some_and(|t| t.self_started());
                    let previous = seen.insert(id.clone(), completed.clone());
                    if deferred.get(&id).is_some_and(|key| Some(key) != completed.as_ref()) {
                        deferred.remove(&id);
                    }
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
                    let worker_assessment = view.snapshot.as_ref().filter(|s| unified(s)).and_then(|s| s.operational.assessment.as_ref()).filter(|a| a.current() && a.verdict.is_some());
                    let new_completion = if let Some(a) = worker_assessment {
                        assessed.insert(id.clone(), a.revision) != Some(a.revision)
                    } else { previous.is_some_and(|old| old != completed) && completed.is_some() };
                    // Whatever held a deferred continuation has stopped: ask
                    // again, since the conversation may have moved on.
                    let released = !new_completion && deferred.contains_key(&id) && eligible(&view);
                    let admission_retry = admission_deferred.remove(&id).is_some_and(|key| key == completed);
                    if (new_completion || released || admission_retry) && check_eligible(&view) && (environment.allowed)(&id) {
                        let Ok(upgrade_work) = gate.enter_unless_draining("automatic continuation") else {
                            admission_deferred.insert(id.clone(), completed);
                            publish(&tx, &environment, id, view, false);
                            continue;
                        };
                        deferred.remove(&id);
                        generation = generation.wrapping_add(1);
                        let epoch = generation;
                        let session = id.clone();
                        let snapshot = view.snapshot.as_ref().expect("eligible snapshot").clone();
                        let diagnostic = environment.log.as_ref().filter(|_| !unified(&snapshot)).map(|log| log.start(&id, "continuation",
                            "Is the turn blocked by subscription quota, or does authorized unfinished work remain?",
                            "All real user instructions since context reset and whole recent assistant messages. Tool history is excluded; assistant_history_omitted reports older omitted assistant context."));
                        if let Some(diagnostic) = &diagnostic {
                            diagnostic.update(None, serde_json::json!({
                                "turn": if self_started { "self-started" } else { "prompted" },
                                "trigger": if released { "driver-stopped" } else { "turn-ended" },
                            }));
                        }
                        let request_diagnostic = diagnostic.clone();
                        let classify = classifier.clone();
                        let task_work = upgrade_work.clone();
                        let abort = jobs.spawn(async move {
                            let _task_work = task_work;
                            let result = async {
                                if unified(&snapshot) {
                                    let a = snapshot.operational.assessment.as_ref().context("missing worker assessment")?;
                                    return Ok(mj_core::continuation::ContinuationVerdict {
                                        quota_limit: f64::from(a.action == Some(mj_core::assessment::Action::RecoverQuota)),
                                        unfinished: f64::from(a.action == Some(mj_core::assessment::Action::Continue)),
                                        no_input_needed: f64::from(a.action == Some(mj_core::assessment::Action::Continue)),
                                    });
                                }
                                let evidence_work = _task_work.clone();
                                let evidence = tokio::task::spawn_blocking(move || {
                                    let _evidence_work = evidence_work;
                                    let quota_message = quota::message(&snapshot);
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
                result = seed_jobs.join_next(), if !seed_jobs.is_empty() => {
                    match result {
                        Some(Ok((session, Err(error)))) => tracing::warn!(%session, %error, "Jev authorization seed unavailable"),
                        Some(Err(error)) if !error.is_cancelled() => tracing::error!(%error, "Jev authorization seed task failed"),
                        _ => {}
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
                                        Ok(recovery) if (environment.allowed)(&id) && quota_schedulable(&p.view) => {
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
                                && quota_schedulable(&p.view) && (environment.allowed)(&id) {
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
                                            ended_turn(snapshot)
                                                .context("missing ended turn")?
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
                            } else if result.as_ref().is_ok_and(|v| v.should_continue())
                                && continuation_allowed(&p.view)
                                && driven(&p.view)
                                && (environment.allowed)(&id)
                            {
                                if let Some(diagnostic) = &p.diagnostic {
                                    diagnostic.update(None, serde_json::json!({"deferred_by": driver_name(&p.view)}));
                                    diagnostic.finish("deferred", "Something else is still moving the session on. Mj will check again once it stops.");
                                }
                                deferred.insert(id.clone(), p.completed.clone());
                                publish(&tx, &environment, id, p.view, false);
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
                    if !gate.is_draining() {
                        for id in admission_deferred.keys() {
                            rechecks.insert(id.clone());
                        }
                    }
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
                        if !clear && recovery.retry_at_ms.is_none_or(|t| t > mj_core::clock::epoch_millis()) { continue; }
                        // Due from here on: every refusal to submit is named, once a minute.
                        let mut refusal = if clear { Vec::new() } else { quota_resume_blockers(view) };
                        if clear && !view.connected { refusal.push("session is not connected"); }
                        if !refusal.is_empty() {
                            skips.note(environment.log.as_ref(), id, recovery, &refusal);
                            continue;
                        }
                        generation = generation.wrapping_add(1);
                        let epoch = generation;
                        let Ok(upgrade_work) = gate.enter_unless_draining("quota continuation") else {
                            skips.note(environment.log.as_ref(), id, recovery, &["the daemon is draining for an upgrade"]);
                            continue
                        };
                        skips.forget(id);
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
                    for (id, view) in &latest {
                        let seed_due = view.snapshot.as_ref().is_some_and(|s| s.operational.assessment_context.is_none())
                            && seed_after.get(id).is_none_or(|at| *at <= std::time::Instant::now());
                        if view.snapshot.as_ref().is_some_and(unified) && (check_eligible(view) || seed_due) && !pending.contains_key(id)
                            && action_recheck.get(id).is_none_or(|at| *at <= std::time::Instant::now()) {
                            action_recheck.insert(id.clone(), std::time::Instant::now() + Duration::from_secs(30));
                            assessed.remove(id);
                            rechecks.insert(id.clone());
                        }
                    }
                    let live = (environment.live)();
                    seen.retain(|id, _| live.contains(id));
                    deferred.retain(|id, _| live.contains(id));
                    admission_deferred.retain(|id, _| live.contains(id));
                    rechecks.retain(|id| live.contains(id));
                    latest.retain(|id, _| live.contains(id));
                    retry_after.retain(|id, _| live.contains(id));
                    skips.retain(&live);
                    assessed.retain(|id, _| live.contains(id));
                    action_recheck.retain(|id, _| live.contains(id));
                    seed_after.retain(|id, _| live.contains(id));
                }
            }
        };
        // Timed apart from the loop's own exit, so a slow join says whether
        // the loop or its checks held the daemon's shutdown.
        let stopping = std::time::Instant::now();
        let running = (seed_jobs.len(), jobs.len());
        seed_jobs.shutdown().await;
        jobs.shutdown().await;
        let took = stopping.elapsed();
        if took >= Duration::from_millis(250) {
            tracing::info!(
                seed_jobs = running.0,
                jobs = running.1,
                duration_ms = took.as_millis(),
                "continuation checks stopped"
            );
        }

        Ok(end)
    });
    (rx, task)
}

#[cfg(test)]
mod tests;

mod quota;
