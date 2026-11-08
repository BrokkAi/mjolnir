use super::*;
use crate::session_manager::{RelayConnectionJob, RelayJobDeferred, SessionManagerControl};

/// How a reconciliation reaches each session's worker.
#[derive(Clone)]
pub(super) enum SessionRelays {
    /// The session actor's own connection. A sync never opens a connection
    /// of its own to a live worker.
    Actors(SessionManagerControl),
    /// A connection of its own per session, for tests against fixture workers.
    #[cfg(test)]
    Direct,
}

/// Reads the host's slopcop login. The daemon reads its own environment and
/// slopcop's configuration; tests pass their own reader, so a test never reads
/// the developer's login.
pub(super) type SlopcopLoginSource = fn() -> Result<Option<mj_core::slopcop::SlopcopLogin>>;

/// What one reconciliation knows about the host's slopcop login.
#[derive(Debug, Clone)]
pub(super) enum HostSlopcopLogin {
    /// It could not be read. Session copies stay as they are, because an
    /// unreadable login is not a signed-out one.
    Unknown,
    SignedOut,
    SignedIn(mj_core::slopcop::SlopcopLogin),
}

/// A test reader: the host's login is never known, so no session copy changes.
#[cfg(test)]
pub(super) fn unknown_slopcop_login() -> Result<Option<mj_core::slopcop::SlopcopLogin>> {
    bail!("tests do not read the host's slopcop login")
}

/// What a session's job owns from the moment it is queued on the session's
/// actor until it has run or been abandoned: the session's background
/// admission, its turn among the session's steps and, for a role-scoped
/// renewal, its install lock. The job holds them, not the task that queued it,
/// so stopping that task (its target changed, the session went away) cannot
/// hand the session to a newer job while the older one is running.
///
/// One state machine decides who may release them. While the job is only
/// queued, the task awaiting it can abandon it (its future was dropped, or the
/// gate cancelled its admission), and abandoning releases the slots at once:
/// a close, restart or move waiting for the admission, and a daemon handoff
/// behind it, must not wait for an actor busy with something else to reach a
/// job nobody wants. Once the actor starts the job it runs to the end under
/// the slots, because it is then using the session's connection. The actor
/// finds an abandoned job over and drops it unrun.
#[derive(Clone)]
pub(super) struct JobHold(Arc<HoldState>);

struct HoldState {
    phase: std::sync::Mutex<HoldPhase>,
    /// The admission's cancellation flag; `None` without a gate.
    cancellation: Option<Arc<std::sync::atomic::AtomicBool>>,
    runtime: Option<Arc<crate::daemon::RuntimeState>>,
}

enum HoldPhase {
    Queued(HeldSlots),
    Running { _slots: HeldSlots },
    Over,
}

/// Released in declaration order: the admission first, so whoever waits on the
/// session's turn then finds the admission already free.
struct HeldSlots {
    _admission: Option<crate::recovery_gate::RecoveryAttempt>,
    _turn: Option<SessionTurn>,
    _installing: Option<SessionTurn>,
}

impl JobHold {
    pub(super) fn new(
        admission: Option<crate::recovery_gate::RecoveryAttempt>,
        turn: Option<SessionTurn>,
        installing: Option<SessionTurn>,
        runtime: Option<Arc<crate::daemon::RuntimeState>>,
    ) -> Self {
        Self(Arc::new(HoldState {
            cancellation: admission.as_ref().map(|admission| admission.cancellation()),
            phase: std::sync::Mutex::new(HoldPhase::Queued(HeldSlots {
                _admission: admission,
                _turn: turn,
                _installing: installing,
            })),
            runtime,
        }))
    }

    /// The job runs under the gate's admission, so the gate's lifecycle
    /// operations can cancel it and its target is checked as current.
    fn gated(&self) -> bool {
        self.0.cancellation.is_some()
    }

    fn phase(&self) -> std::sync::MutexGuard<'_, HoldPhase> {
        self.0
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The actor's turn to run the job. `false` when the job was abandoned
    /// (its slots are already released), or a lifecycle operation cancelled
    /// the admission while it waited (they are released now).
    fn start(&self) -> bool {
        let cancelled = self
            .0
            .cancellation
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
        // Slots are dropped after the lock is released, so a release never
        // runs another lock's code under this one.
        let mut released = HoldPhase::Over;
        let started = {
            let mut phase = self.phase();
            match std::mem::replace(&mut *phase, HoldPhase::Over) {
                HoldPhase::Queued(slots) if !cancelled => {
                    *phase = HoldPhase::Running { _slots: slots };
                    true
                }
                other => {
                    released = other;
                    false
                }
            }
        };
        drop(released);
        started
    }

    /// The admission's cancellation flag, for the connection to observe while
    /// the job's calls are in flight.
    fn cancellation(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.0.cancellation.clone()
    }

    /// A lifecycle operation asked the admitted work to stop.
    fn cancelled(&self) -> bool {
        self.0
            .cancellation
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    }

    /// The job is done: release the slots.
    fn finish(&self) {
        let released = std::mem::replace(&mut *self.phase(), HoldPhase::Over);
        drop(released);
    }

    /// The task awaiting the job no longer wants it. Releases the slots if
    /// the actor has not started it; a job already running is not touched.
    fn abandon_if_queued(&self) {
        let released = {
            let mut phase = self.phase();
            if matches!(*phase, HoldPhase::Queued(_)) {
                std::mem::replace(&mut *phase, HoldPhase::Over)
            } else {
                HoldPhase::Over
            }
        };
        drop(released);
    }
}

/// Held by the task awaiting a queued job: if that task stops, or the gate
/// cancels its admission, before the actor has started the job, the job's
/// slots are released right then.
pub(super) struct QueuedJob(JobHold);

impl Drop for QueuedJob {
    fn drop(&mut self) {
        self.0.abandon_if_queued();
    }
}

/// One holder at a time per session. Within a pass, a session's login step and
/// its AWS install both need the session's single background admission slot
/// (`RecoveryGate::try_start`), which turns a second taker away; taking turns
/// here first means the two never contend for it, so neither is skipped
/// because the other held it. A step waits only for the same session's other
/// step, never another session's. The lock is released on every exit path.
/// A second instance orders a session's role-scoped renewals themselves.
///
/// The map is keyed by session id and only that session's entry is touched by
/// `enter` and by a release, so a batch of sessions costs one lookup each.
#[derive(Clone, Default)]
pub(super) struct SessionSteps(Arc<std::sync::Mutex<SessionLocks>>);

type SessionLocks = BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>;

/// A session's turn. Dropping it releases the session's lock and, when nobody
/// else holds or waits for that lock, forgets the session's entry.
pub(super) struct SessionTurn {
    // Both dropped first, which is what lets the entry's count reach zero.
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
    locks: Arc<std::sync::Mutex<SessionLocks>>,
    session_id: String,
}

impl Drop for SessionTurn {
    fn drop(&mut self) {
        drop(self.guard.take());
        drop(self.lock.take());
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A waiter holds a strong reference, taken under this mutex.
        if locks
            .get(&self.session_id)
            .is_some_and(|lock| lock.strong_count() == 0)
        {
            locks.remove(&self.session_id);
        }
    }
}

impl SessionSteps {
    /// How many sessions the lock map has an entry for.
    #[cfg(test)]
    pub(super) fn tracked(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub(super) async fn enter(&self, session_id: &str) -> SessionTurn {
        let lock = {
            let mut locks = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match locks.get(session_id).and_then(std::sync::Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(session_id.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        // Built before waiting, so a future dropped while waiting also
        // forgets an entry nobody is left to use.
        let mut turn = SessionTurn {
            guard: None,
            lock: Some(lock),
            locks: Arc::clone(&self.0),
            session_id: session_id.to_owned(),
        };
        if let Some(lock) = turn.lock.clone() {
            turn.guard = Some(lock.lock_owned().await);
        }
        turn
    }
}

/// What a reconciliation needs to reach sessions and to know which AWS profile
/// each was staged with.
#[derive(Clone, Copy)]
pub(super) struct SessionAccess<'a> {
    pub(super) relays: &'a SessionRelays,
    pub(super) gate: Option<&'a Arc<crate::recovery_gate::RecoveryGate>>,
    pub(super) runtime: Option<&'a Arc<crate::daemon::RuntimeState>>,
    pub(super) steps: &'a SessionSteps,
}

/// What a profile's spawned reconcile tells the coordinator loop, which alone
/// owns the profile's busy slot.
enum PassEvent {
    /// The reconcile ended: the profile can be synced again.
    Released(String),
    /// The trigger's answer, once everything it waits for has ended.
    Finished(CredentialSyncResult),
}

pub struct CredentialSyncCoordinator {
    pub(super) handle: CredentialSyncHandle,
    pub(super) results: mpsc::UnboundedReceiver<CredentialSyncResult>,
}

impl CredentialSyncCoordinator {
    #[cfg(test)]
    pub fn spawn() -> Self {
        Self::spawn_inner(
            SessionRelays::Direct,
            None,
            None,
            unknown_slopcop_login,
            SYNC_INTERVAL,
        )
    }

    /// A coordinator that retries a role-scoped session's AWS renewal after
    /// `retry` instead of after the sync interval.
    #[cfg(test)]
    pub fn spawn_with_renewal_retry(retry: Duration) -> Self {
        Self::spawn_inner(
            SessionRelays::Direct,
            None,
            None,
            unknown_slopcop_login,
            retry,
        )
    }

    pub fn spawn_guarded(
        manager: SessionManagerControl,
        gate: Arc<crate::recovery_gate::RecoveryGate>,
        runtime: Arc<crate::daemon::RuntimeState>,
    ) -> Self {
        Self::spawn_inner(
            SessionRelays::Actors(manager),
            Some(gate),
            Some(runtime),
            mj_core::slopcop::host_login,
            SYNC_INTERVAL,
        )
    }

    fn spawn_inner(
        relays: SessionRelays,
        gate: Option<Arc<crate::recovery_gate::RecoveryGate>>,
        runtime: Option<Arc<crate::daemon::RuntimeState>>,
        slopcop_login: SlopcopLoginSource,
        renewal_retry: Duration,
    ) -> Self {
        let targets_tx = runtime
            .as_ref()
            .map(|runtime| runtime.credential_targets.clone())
            .unwrap_or_else(|| Arc::new(watch::channel(Vec::new()).0));
        let mut targets_rx = targets_tx.subscribe();
        let handle_targets = targets_tx.clone();
        let (triggers_tx, mut triggers_rx) = mpsc::unbounded_channel::<SyncTrigger>();
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<PassEvent>();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + SYNC_INTERVAL,
                SYNC_INTERVAL,
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // A pull rewrites the canonical file, so one profile is never
            // reconciled twice at once. The loop alone inserts and removes:
            // each insert is matched by one `PassEvent::Released`, sent as soon
            // as the reconcile has ended (or panicked), and never later than
            // the trigger's answer, which can wait for a renewal.
            let mut busy = BTreeSet::<String>::new();
            let mut queue = VecDeque::<SyncTrigger>::new();
            // Role-scoped AWS credentials are renewed by a task per session,
            // never by the profile's pass: a pass that waits on one stalled
            // session must not hold back another session's renewal.
            let renewals_env = RenewalEnv {
                relays: relays.clone(),
                gate: gate.clone(),
                runtime: runtime.clone(),
                steps: SessionSteps::default(),
                installs: SessionSteps::default(),
                results: results_tx.clone(),
                retry: renewal_retry,
            };
            let steps = renewals_env.steps.clone();
            let mut renewals = RenewalTasks::default();
            let mut previous_targets = BTreeMap::<String, CredentialSyncTarget>::new();
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        // Whether or not any session is left to sync, the
                        // cache of issued credentials drops what has expired.
                        crate::aws_assume_role::ScopedAwsCredentials::shared().sweep_expired();
                        renewals.sync(&targets_rx.borrow(), &renewals_env);
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    changed = targets_rx.changed() => {
                        if changed.is_err() { break; }
                        // Compare the publication we consume, not its order.
                        // Removed sessions have nothing left to reconcile.
                        let targets = targets_rx.borrow_and_update();
                        renewals.sync(&targets, &renewals_env);
                        let changed_profiles: BTreeSet<_> = targets.iter()
                            .filter(|target| previous_targets.get(&target.session_id) != Some(target))
                            .map(|target| target.profile_id.clone())
                            .collect();
                        previous_targets = targets.iter()
                            .map(|target| (target.session_id.clone(), target.clone()))
                            .collect();
                        for profile_id in changed_profiles {
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    trigger = triggers_rx.recv() => {
                        let Some(trigger) = trigger else { break };
                        enqueue(&mut queue, trigger);
                    }
                    completed = completed_rx.recv() => {
                        let Some(event) = completed else { break };
                        match event {
                            // The queued triggers are looked at below, so one
                            // deferred by this slot starts at once.
                            PassEvent::Released(profile_id) => {
                                busy.remove(&profile_id);
                            }
                            PassEvent::Finished(result)
                                if result.trigger.is_some()
                                    || result.failure.is_some()
                                    || !result.outcomes.is_empty() =>
                            {
                                let profile_id = result.profile_id.clone();
                                if results_tx.send(result).is_err() {
                                    tracing::debug!(
                                        %profile_id,
                                        operation = "credential_sync_result",
                                        "credential sync result receiver was already closed"
                                    );
                                }
                            }
                            PassEvent::Finished(_) => {}
                        }
                    }
                }

                let mut deferred = VecDeque::new();
                while let Some(trigger) = queue.pop_front() {
                    if busy.contains(&trigger.profile_id) {
                        deferred.push_back(trigger);
                        continue;
                    }
                    let targets: Vec<_> = targets_rx
                        .borrow()
                        .iter()
                        .filter(|target| target.profile_id == trigger.profile_id)
                        .cloned()
                        .collect();
                    if targets.is_empty() {
                        if trigger.cause.is_some() {
                            let profile_id = trigger.profile_id.clone();
                            if results_tx
                                .send(CredentialSyncResult {
                                    profile_id: trigger.profile_id,
                                    trigger: trigger.cause,
                                    failure: None,
                                    outcomes: Vec::new(),
                                    partial: false,
                                })
                                .is_err()
                            {
                                tracing::debug!(
                                    %profile_id,
                                    operation = "credential_sync_result",
                                    "credential sync result receiver was already closed"
                                );
                            }
                        }
                        continue;
                    }
                    busy.insert(trigger.profile_id.clone());
                    let completed_tx = completed_tx.clone();
                    // The join is awaited so a panicked reconcile is reported
                    // and its profile always leaves the busy set. The reconcile
                    // runs as an ordinary task, never as `Handle::block_on` on
                    // a blocking thread: the scheduler cancels tasks before it
                    // shuts the timer driver, whereas a blocking thread keeps
                    // polling its connect timeouts through runtime shutdown
                    // and panics with "A Tokio 1.x context was found, but it
                    // is being shutdown".
                    let triggered_by = trigger.cause.as_ref().map(|cause| cause.session_id.clone());
                    // A session whose request was refused for authentication
                    // may hold credentials the provider no longer accepts: its
                    // renewal task mints new ones now, and its outcome is part
                    // of this trigger's result.
                    let aws_report = triggered_by
                        .as_deref()
                        .and_then(|session_id| renewals.wake(session_id));
                    let gate = gate.clone();
                    let relays = relays.clone();
                    let runtime = runtime.clone();
                    let steps = steps.clone();
                    tokio::spawn(async move {
                        let joined = tokio::spawn(async move {
                            reconcile_profile_guarded(
                                &SessionAccess {
                                    relays: &relays,
                                    gate: gate.as_ref(),
                                    runtime: runtime.as_ref(),
                                    steps: &steps,
                                },
                                &targets,
                                triggered_by.as_deref(),
                                slopcop_login,
                            )
                            .await
                        })
                        .await;
                        let profile_id = trigger.profile_id.clone();
                        // The profile's slot covers the reconcile only. It is
                        // released here, on every path (a panicked reconcile
                        // included), before anything waits for a renewal:
                        // a renewal stalled in STS must not hold up the
                        // profile's login, token and skills syncs.
                        if completed_tx
                            .send(PassEvent::Released(profile_id.clone()))
                            .is_err()
                        {
                            tracing::debug!(
                                %profile_id,
                                operation = "credential_sync_completion",
                                "credential sync coordinator stopped before receiving completion"
                            );
                        }
                        let (failure, mut outcomes) = match joined {
                            Ok(outcomes) => (None, outcomes),
                            Err(error) => (Some(format!("sync task stopped: {error}")), Vec::new()),
                        };
                        if let Some(report) = aws_report {
                            // The trigger's answer includes the renewal's
                            // outcome, so it is sent when both have ended.
                            // A renewal task stopped before it answered
                            // (its session closed or its target changed)
                            // leaves the outcome unknown, which the answer
                            // says; its replacement reports for itself.
                            let report = report.await.unwrap_or_else(|_| {
                                unknown_renewal_outcome(
                                    trigger.cause.as_ref().map_or("", |c| &c.session_id),
                                )
                            });
                            merge_report(&mut outcomes, report);
                        }
                        if completed_tx
                            .send(PassEvent::Finished(CredentialSyncResult {
                                profile_id: trigger.profile_id,
                                trigger: trigger.cause,
                                failure,
                                outcomes,
                                partial: false,
                            }))
                            .is_err()
                        {
                            tracing::debug!(
                                %profile_id,
                                operation = "credential_sync_completion",
                                "credential sync coordinator stopped before receiving completion"
                            );
                        }
                    });
                }
                queue = deferred;
            }
        });
        Self {
            handle: CredentialSyncHandle {
                targets: handle_targets,
                triggers: triggers_tx,
            },
            results: results_rx,
        }
    }

    pub fn handle(&self) -> CredentialSyncHandle {
        self.handle.clone()
    }

    pub fn try_result(&mut self) -> Option<CredentialSyncResult> {
        self.results.try_recv().ok()
    }

    /// Waits for the next finished sync.
    ///
    /// Event-driven loops select on this instead of polling; `None` means the
    /// coordinator task has stopped. Cancel-safe, so a lost `select!` race
    /// keeps the result queued.
    pub async fn result(&mut self) -> Option<CredentialSyncResult> {
        self.results.recv().await
    }
}

/// What the coordinator does over time, once: the profile's pass, and one AWS
/// cycle for each role-scoped session.
#[cfg(all(test, unix))]
pub(super) async fn reconcile_profile(
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
) -> Vec<CredentialSyncOutcome> {
    let steps = SessionSteps::default();
    let access = SessionAccess {
        relays: &SessionRelays::Direct,
        gate: None,
        runtime: None,
        steps: &steps,
    };
    let login = reconcile_profile_guarded(&access, targets, triggered_by, unknown_slopcop_login);
    let cycles = futures::future::join_all(
        targets
            .iter()
            .filter(|target| {
                target
                    .aws_credentials
                    .as_ref()
                    .is_some_and(|source| source.role.is_some())
            })
            .map(|target| aws_cycle(&access, target, AwsCycleOptions::default())),
    );
    let (outcomes, cycles) = tokio::join!(Box::pin(login), Box::pin(cycles));
    let mut outcomes = outcomes
        .into_iter()
        .map(|outcome| (outcome.session_id.clone(), outcome))
        .collect::<BTreeMap<_, _>>();
    for (session_id, result) in cycles {
        merge_aws_cycle(&mut outcomes, &session_id, result);
    }
    outcomes.into_values().collect()
}

/// Reconcile one profile with every live session that runs it.
///
/// The profile's login, skills, GitHub token and slopcop login are
/// reconciled one session after another: the canonical login is one fact for
/// the whole profile, and a push or pull decided against it must not overlap
/// another. A pull makes every other session's copy stale by definition, so
/// the pass runs again once with the new canonical bytes. Two passes are
/// enough: the second cannot pull anything the first did not already see
/// unless a harness refreshed mid-cycle, and that lands in the next cycle.
///
/// The role-scoped AWS credentials are not part of this pass at all: each
/// session has a renewal task of its own (see `RenewalTasks`), so a session
/// that does not answer stalls this pass and its own renewal, and nothing else.
///
/// A session that already agreed is left out of the outcomes, except
/// `triggered_by`, the session whose failure asked for this sync: that it was
/// reached and had nothing to change is what shows the profile's own login
/// is the one the provider refused.
pub(super) async fn reconcile_profile_guarded(
    access: &SessionAccess<'_>,
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
    slopcop_login: SlopcopLoginSource,
) -> Vec<CredentialSyncOutcome> {
    let SessionAccess {
        relays,
        gate,
        runtime,
        steps,
    } = *access;
    let Some(first) = targets.first().cloned() else {
        return Vec::new();
    };
    // Read once per pass, and only when a session needs a copy: a raw
    // localhost session reads the host's own login.
    let slopcop_login = if targets.iter().any(syncs_slopcop_login) {
        match tokio::task::spawn_blocking(slopcop_login)
            .await
            .context("slopcop login task stopped")
            .and_then(|result| result)
        {
            Ok(Some(login)) => HostSlopcopLogin::SignedIn(login),
            Ok(None) => HostSlopcopLogin::SignedOut,
            Err(error) => {
                tracing::warn!(
                    profile_id = %first.profile_id,
                    "could not read the host's slopcop login; session copies stay as they are: {error:#}"
                );
                HostSlopcopLogin::Unknown
            }
        }
    } else {
        HostSlopcopLogin::Unknown
    };
    // Without a role, every session of the profile receives the same export,
    // so it is taken once per pass.
    let unscoped_aws_credentials = match targets.iter().find_map(|target| {
        target
            .aws_credentials
            .clone()
            .filter(|source| source.role.is_none())
    }) {
        Some(source) => {
            let exported = tokio::task::spawn_blocking(move || {
                crate::aws_credentials::export(
                    &source,
                    &crate::targets::BoundedProcessExecutor::new(Duration::from_secs(30)),
                )
            })
            .await
            .context("AWS credential export task stopped")
            .and_then(|result| result);
            exported.map(Some)
        }
        None => Ok(None),
    };
    // Targets share a profile but can differ in host CLI access. Collect off
    // scheduler threads once per wire format, then derive both scoped trees.
    let skills = Arc::new(
        tokio::task::spawn_blocking(move || CanonicalSkills::collect(&first))
            .await
            .unwrap_or_else(|error| {
                CanonicalSkills::failed(&format!("skills collection task stopped: {error}"))
            }),
    );
    let pass_context = PassContext {
        relays,
        gate,
        runtime,
        skills: &skills,
        slopcop_login: &slopcop_login,
        triggered_by,
        steps,
    };
    // The AWS step of the sequential pass: the profile's one export. A
    // role-scoped session's AWS credentials are installed by its own
    // operation, the renewal task's.
    let aws_for_login = targets
        .iter()
        .map(|target| match &target.aws_credentials {
            Some(source) if source.role.is_none() => match &unscoped_aws_credentials {
                Ok(Some(credentials)) => AwsSync::Install(credentials.clone()),
                Ok(None) => AwsSync::Unmanaged,
                Err(error) => AwsSync::Unavailable {
                    error: format!("{error:#}"),
                    scoped_required: false,
                    expected_scope: None,
                },
            },
            _ => AwsSync::Unmanaged,
        })
        .collect::<Vec<_>>();
    let login_pass = async {
        let mut outcomes = BTreeMap::<String, CredentialSyncOutcome>::new();
        for pass in 0..2 {
            let mut pulled = false;
            for (index, target) in targets.iter().enumerate() {
                let reported = outcomes.contains_key(&target.session_id);
                let result = Box::pin(reconcile_target(
                    &pass_context,
                    target,
                    &aws_for_login[index],
                    pass,
                    reported,
                ))
                .await;
                pulled |= result.pulled;
                match result.outcome {
                    Some(outcome) => {
                        outcomes.insert(target.session_id.clone(), outcome);
                    }
                    // A later pass that reached the session cleanly replaces
                    // the failure an earlier pass reported for it.
                    None if result.clean => clear_failure(&mut outcomes, &target.session_id),
                    None => {}
                }
            }
            if !pulled || pass == 1 {
                break;
            }
        }
        outcomes
    };
    let outcomes = login_pass.await;
    outcomes.into_values().collect()
}

/// Fold one more step's result into a session's outcome. A session whose
/// whole sync failed keeps that failure (as `SyncStep::Session`, which reports
/// without a step), so the other step's actions are recorded beside it.
pub(super) fn merge_step_outcome(
    outcomes: &mut BTreeMap<String, CredentialSyncOutcome>,
    session_id: &str,
    actions: Vec<CredentialSyncAction>,
    failures: Vec<StepFailure>,
) {
    if actions.is_empty() && failures.is_empty() {
        return;
    }
    let outcome = outcomes
        .entry(session_id.to_owned())
        .or_insert_with(|| CredentialSyncOutcome {
            session_id: session_id.to_owned(),
            outcome: Ok(Vec::new()),
            step_failures: Vec::new(),
        });
    match std::mem::replace(&mut outcome.outcome, Ok(Vec::new())) {
        Ok(mut existing) => {
            existing.extend(actions);
            outcome.outcome = Ok(existing);
        }
        // The session's whole sync failed (its connection): that is no step's
        // failure, and is kept as such beside the AWS result.
        Err(detail) => {
            outcome.step_failures.insert(
                0,
                StepFailure {
                    step: SyncStep::Session,
                    detail,
                },
            );
            outcome.outcome = Ok(actions);
        }
    }
    outcome.step_failures.extend(failures);
}

/// The outcome of a woken renewal that never answered.
fn unknown_renewal_outcome(session_id: &str) -> CredentialSyncOutcome {
    CredentialSyncOutcome {
        session_id: session_id.to_owned(),
        outcome: Ok(Vec::new()),
        step_failures: vec![StepFailure {
            step: SyncStep::Aws,
            detail: "AWS credentials: the session's renewal stopped before it answered, so whether its credentials are valid is unknown".into(),
        }],
    }
}

/// Fold the outcome of a session's own AWS cycle into the profile pass's
/// outcomes.
fn merge_report(outcomes: &mut Vec<CredentialSyncOutcome>, report: CredentialSyncOutcome) {
    let mut by_session = std::mem::take(outcomes)
        .into_iter()
        .map(|outcome| (outcome.session_id.clone(), outcome))
        .collect::<BTreeMap<_, _>>();
    merge_step_outcome(
        &mut by_session,
        &report.session_id,
        report.outcome.unwrap_or_default(),
        report.step_failures,
    );
    *outcomes = by_session.into_values().collect();
}

/// Fold one AWS cycle's result into a session's outcome.
fn merge_aws_cycle(
    outcomes: &mut BTreeMap<String, CredentialSyncOutcome>,
    session_id: &str,
    result: Result<Option<SessionSync>>,
) {
    match result {
        Ok(Some(SessionSync {
            actions, failures, ..
        })) => {
            if let Some(failure) = joined_failures(&failures) {
                tracing::warn!(
                    session_id,
                    error = %failure,
                    "AWS credential synchronization step failed for relay session"
                );
            }
            merge_step_outcome(outcomes, session_id, actions, failures);
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                session_id,
                error = %error,
                "AWS credential synchronization failed for relay session"
            );
            merge_step_outcome(
                outcomes,
                session_id,
                Vec::new(),
                vec![StepFailure {
                    step: SyncStep::Aws,
                    detail: format!("AWS credentials: {error:#}"),
                }],
            );
        }
    }
}

/// A pass that reached the session and found nothing wrong ends the failure an
/// earlier pass reported for it; what that pass did stays.
fn clear_failure(outcomes: &mut BTreeMap<String, CredentialSyncOutcome>, session_id: &str) {
    if let Some(outcome) = outcomes.get_mut(session_id) {
        if outcome.outcome.is_err() {
            outcome.outcome = Ok(Vec::new());
        }
        // The profile's pass checked every step but the role-scoped AWS one,
        // which its own task owns.
        outcome
            .step_failures
            .retain(|failure| failure.step == SyncStep::Aws);
    }
}

/// What a sync pass does about one session's AWS credentials.
#[derive(Clone)]
pub(super) enum AwsSync {
    /// The session's AWS credentials are not managed by the controller.
    Unmanaged,
    /// The session should hold exactly these.
    Install(mj_core::aws_credentials::AwsCredentials),
    /// New credentials are still being obtained. Before they arrive, what the
    /// session holds is reconciled against `expected_scope`, exactly as for
    /// `Unavailable`, but nothing is reported as failed: matching credentials
    /// stay, anything else is withdrawn.
    Pending {
        expected_scope: Option<ScopeExpectation>,
    },
    /// They could not be obtained. Nothing is installed, and this step's
    /// failure is reported on its own: the session's other sync steps still
    /// run.
    Unavailable {
        error: String,
        /// The profile requires role-scoped credentials. A worker may still
        /// hold the host's own credentials, installed before the role was
        /// configured; unless it provably holds `scoped_held`, they are
        /// withdrawn rather than left in place. Static host keys never expire.
        scoped_required: bool,
        /// What credentials the worker holds must have been issued for to stay
        /// (until they expire): the worker records that beside the secret, so
        /// this holds across a daemon restart. Anything else it holds is
        /// withdrawn.
        expected_scope: Option<ScopeExpectation>,
    },
}

impl AwsSync {
    /// The step a failure of this AWS sync belongs to: the session's own
    /// role-scoped credentials, or the profile's one export.
    fn step(&self) -> SyncStep {
        match self {
            AwsSync::Install(credentials) if credentials.scope().is_some() => SyncStep::Aws,
            AwsSync::Unavailable {
                scoped_required: true,
                ..
            }
            | AwsSync::Pending { .. } => SyncStep::Aws,
            _ => SyncStep::AwsExport,
        }
    }
}

/// The controller's GitHub token for a session, or why it could not be read.
#[derive(Clone)]
pub(super) enum GithubSync {
    Known(Option<String>),
    /// The lookup failed. The session's copy stays as it is, because an
    /// unreadable token is not an absent one.
    Unavailable(String),
}

/// What reconciling one session did: the actions its steps took, and the
/// failure of each step that failed. Every step runs whether or not another
/// failed.
pub(super) struct SessionSync {
    pub(super) actions: Vec<CredentialSyncAction>,
    pub(super) failures: Vec<StepFailure>,
    /// The AWS profile the session's worker was staged with, when the work
    /// asked for it (`SyncWork::StagedProfile`).
    pub(super) staged_profile: Option<String>,
}

/// The role, role session and session policy credentials were issued for.
#[derive(Clone)]
pub(super) struct ScopeExpectation {
    pub(super) role_arn: String,
    pub(super) role_session_name: String,
    /// Fingerprint of the effective session policy (inline and managed): a
    /// narrower policy than the one the credentials were issued under makes
    /// them broader than configured, so they do not match.
    pub(super) session_policy_fingerprint: String,
    /// The `SourceIdentity` the credentials must have been issued with: the
    /// role session name when `source_identity` is on, nothing when it is off.
    pub(super) source_identity: Option<String>,
}

impl ScopeExpectation {
    fn of(target: &CredentialSyncTarget) -> Option<Self> {
        let role = target.aws_credentials.as_ref()?.role.as_ref()?;
        let role_session_name = crate::aws_assume_role::session_name(&target.session_id);
        Some(Self {
            role_arn: role.role_arn.clone(),
            session_policy_fingerprint: role.session_policy_fingerprint(),
            source_identity: role.source_identity.then(|| role_session_name.clone()),
            role_session_name,
        })
    }

    fn matches(&self, recorded: &mj_core::aws_credentials::AwsCredentialScope) -> bool {
        recorded.role_arn == self.role_arn
            && recorded.role_session_name == self.role_session_name
            && recorded.session_policy_fingerprint == self.session_policy_fingerprint
            && recorded.source_identity == self.source_identity
            && recorded.expires_at_epoch_millis > mj_core::clock::epoch_millis()
    }
}

/// What one reconcile pass shares between its sessions.
struct PassContext<'a> {
    relays: &'a SessionRelays,
    gate: Option<&'a Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&'a Arc<crate::daemon::RuntimeState>>,
    skills: &'a Arc<CanonicalSkills>,
    slopcop_login: &'a HostSlopcopLogin,
    triggered_by: Option<&'a str>,
    steps: &'a SessionSteps,
}

/// What reconciling one session in one pass found.
struct TargetResult {
    outcome: Option<CredentialSyncOutcome>,
    /// A fresher copy of the profile's login was pulled from the session.
    pulled: bool,
    /// The session was reached and every step succeeded, whether or not it
    /// has anything to report.
    clean: bool,
}

/// The failures of a session's sync steps, as one message for a log. A lost
/// connection fails every step the same way, so a message is said once.
fn joined_failures(failures: &[StepFailure]) -> Option<String> {
    let mut unique = Vec::<&str>::new();
    for failure in failures {
        if !unique.contains(&failure.detail.as_str()) {
            unique.push(&failure.detail);
        }
    }
    (!unique.is_empty()).then(|| unique.join("; "))
}

// Lets a test run the gated path without a daemon to say which sessions are
// current: every session is, on the thread that sets this.
#[cfg(test)]
thread_local! {
    pub(super) static SESSIONS_ARE_CURRENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the session is still the one this target describes. A lifecycle
/// may have completed since the dashboard published its target list.
async fn target_is_current(
    runtime: Option<&Arc<crate::daemon::RuntimeState>>,
    target: &CredentialSyncTarget,
) -> Result<bool> {
    #[cfg(test)]
    if SESSIONS_ARE_CURRENT.with(std::cell::Cell::get) {
        return Ok(true);
    }
    if let Some(runtime) = runtime {
        return runtime.credential_target_is_current(target);
    }
    let candidate = target.clone();
    tokio::task::spawn_blocking(move || {
        // Read only: cancellation may leave this blocking read finishing
        // after admission has been released.
        let controller = crate::controller::Controller {
            config: mj_core::config::Config::load()?,
            state: crate::database::load_state()?,
        };
        Ok::<_, anyhow::Error>(crate::pollers::credential_sync_target_is_current(
            controller, &candidate,
        ))
    })
    .await
    .context("reload credential sync target")?
}

/// Run `work` for the session under background admission, when the session is
/// still current. `None` means the work was deferred. `work` receives the hold
/// (the admission, the session's turn and its install lock) to pass on to
/// whatever outlives this future.
pub(super) async fn admitted<T, F: std::future::Future<Output = Result<Option<T>>>>(
    gate: Option<&Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&Arc<crate::daemon::RuntimeState>>,
    target: &CredentialSyncTarget,
    turn: SessionTurn,
    installing: Option<SessionTurn>,
    work: impl FnOnce(JobHold) -> F,
) -> Result<Option<T>> {
    match gate {
        Some(gate) => gate
            .run_background_owned(&target.session_id, |admission| async move {
                if !target_is_current(runtime, target).await? {
                    return Ok(None);
                }
                work(JobHold::new(
                    Some(admission),
                    Some(turn),
                    installing,
                    runtime.cloned(),
                ))
                .await
            })
            .await
            .unwrap_or_else(|| Ok(None)),
        None => work(JobHold::new(None, Some(turn), installing, runtime.cloned())).await,
    }
}

/// One session's reconciliation in one pass. `reported` says an earlier pass
/// already recorded an outcome for it.
async fn reconcile_target(
    context: &PassContext<'_>,
    target: &CredentialSyncTarget,
    aws_credentials: &AwsSync,
    pass: usize,
    reported: bool,
) -> TargetResult {
    let PassContext {
        relays,
        gate,
        runtime,
        skills,
        slopcop_login,
        triggered_by,
        steps,
    } = context;
    // After the same session's AWS install, if it is running, never racing it
    // for the session's admission slot.
    let turn = steps.enter(&target.session_id).await;
    let result = admitted(*gate, *runtime, target, turn, None, |hold| async move {
        // A failed GitHub token lookup is that step's failure, not the
        // session's: the AWS credentials, skills and logins do not depend on it.
        let github = if target.sync_github_token {
            match crate::controller::github_token_for_session(
                target.session_id.clone(),
                target.github.clone(),
            )
            .await
            {
                Ok(token) => GithubSync::Known(token),
                Err(error) => GithubSync::Unavailable(format!("{error:#}")),
            }
        } else {
            GithubSync::Known(None)
        };
        reconcile_session(
            relays,
            target,
            skills,
            &github,
            aws_credentials,
            slopcop_login,
            hold,
        )
        .await
    })
    .await;
    match result {
        // Deferral is not a successful credential check: in particular
        // it must not mark an authentication-triggered login refused.
        Ok(None) => TargetResult {
            outcome: None,
            pulled: false,
            clean: false,
        },
        Ok(Some(SessionSync {
            actions, failures, ..
        })) if actions.is_empty()
            && failures.is_empty()
            && (*triggered_by != Some(target.session_id.as_str()) || reported) =>
        {
            TargetResult {
                outcome: None,
                pulled: false,
                clean: true,
            }
        }
        Ok(Some(SessionSync {
            actions, failures, ..
        })) => {
            if let Some(failure) = joined_failures(&failures) {
                tracing::warn!(
                    session_id = %target.session_id,
                    profile_id = %target.profile_id,
                    pass = pass + 1,
                    error = %failure,
                    "credential synchronization step failed for relay session"
                );
            }
            TargetResult {
                pulled: actions.contains(&CredentialSyncAction::Pulled),
                clean: failures.is_empty(),
                outcome: Some(CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Ok(actions),
                    step_failures: failures,
                }),
            }
        }
        Err(error) => {
            tracing::warn!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                pass = pass + 1,
                error = %error,
                "credential synchronization failed for relay session"
            );
            TargetResult {
                pulled: false,
                clean: false,
                outcome: Some(CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Err(format!("{error:#}")),
                    step_failures: Vec::new(),
                }),
            }
        }
    }
}

/// One role-scoped session's AWS cycle: mint (or take from the cache), then
/// install on the session's own connection, or withdraw what must not stay.
/// What distinguishes one AWS cycle from another.
#[derive(Default)]
pub(super) struct AwsCycleOptions<'a> {
    /// The renewal's install lock, held by the install job.
    pub(super) installing: Option<SessionTurn>,
    /// Drop the cached credentials first: the session's were perhaps refused.
    pub(super) force_new: bool,
    /// Where to report, before the cycle ends, that credentials were withdrawn
    /// while the replacement is pending.
    pub(super) interim: Option<&'a mpsc::UnboundedSender<CredentialSyncResult>>,
}

pub(super) async fn aws_cycle(
    access: &SessionAccess<'_>,
    target: &CredentialSyncTarget,
    options: AwsCycleOptions<'_>,
) -> (String, Result<Option<SessionSync>>) {
    let AwsCycleOptions {
        installing,
        force_new,
        interim,
    } = options;
    let session_id = target.session_id.clone();
    // A session that is gone, or replaced, is not given credentials. This is
    // checked before the mint, which stays outside admission: an AssumeRole
    // call can take seconds and must not hold the session's slot.
    // (Without a gate, as in tests of the direct path, nothing says which
    // sessions are current, here or under admission.)
    if access.gate.is_some() {
        match target_is_current(access.runtime, target).await {
            Ok(true) => {}
            Ok(false) => return (session_id, Ok(None)),
            Err(error) => return (session_id, Err(error)),
        }
    }
    let Some(base) = target
        .aws_credentials
        .as_ref()
        .filter(|source| source.role.is_some())
    else {
        // Not a role-scoped session: nothing for a renewal to do.
        return (session_id, Ok(None));
    };
    // A role is configured for the profile, so nothing the session holds may
    // outlast it unless it matches the role: unscoped credentials, or ones for
    // another role or policy, are withdrawn. That depends on the role, the
    // policy and the installed secret alone, never on the profile name or on
    // whether the profile still authenticates with the AWS credential chain
    // (which decides only whether credentials are minted), so it comes first
    // and nothing later in the cycle can stop it. Matching credentials stay
    // until the replacement arrives; a session with obsolete credentials goes
    // without until then, which is the fail-closed choice.
    let reconcile = AwsSync::Pending {
        expected_scope: ScopeExpectation::of(target),
    };
    if !base.mints {
        let result = aws_job(
            access,
            target,
            installing,
            SyncWork::Aws(Box::new(reconcile)),
        )
        .await;
        return (session_id, result);
    }
    let held = match aws_job(access, target, None, SyncWork::Aws(Box::new(reconcile))).await {
        Ok(Some(sync)) => sync,
        // Not reached this cycle: the next one tries again.
        Ok(None) => return (session_id, Ok(None)),
        Err(error) => return (session_id, Err(error)),
    };
    // What the withdrawal did is said now, not when the cycle ends: the mint
    // may take as long as it takes. Said once: with a sink the final result
    // does not carry it again.
    let mut withdrawn = Vec::new();
    if !held.actions.is_empty() || !held.failures.is_empty() {
        let mut failures = held.failures.clone();
        if !held.actions.is_empty() {
            failures.push(StepFailure {
                step: SyncStep::Aws,
                detail: "AWS credentials: the session's previous credentials did not match this profile's role and were withdrawn; it has none until new role-scoped ones are installed".into(),
            });
        }
        match interim {
            Some(sink) => {
                let _ = sink.send(CredentialSyncResult {
                    profile_id: target.profile_id.clone(),
                    trigger: None,
                    failure: None,
                    partial: true,
                    outcomes: vec![CredentialSyncOutcome {
                        session_id: session_id.clone(),
                        outcome: Ok(held.actions.clone()),
                        step_failures: failures,
                    }],
                });
            }
            None => withdrawn = held.actions.clone(),
        }
    }
    // The name the credentials are installed under is the one the worker's
    // staged configuration has (the session's environment selects it). A
    // worker that cannot answer now defers the cycle; nothing changes. A
    // configuration that cannot be read is the cycle's failure, said with what
    // the person can do; the withdrawal above has already happened.
    let staged = match aws_job(access, target, None, SyncWork::StagedProfile).await {
        Ok(Some(sync)) => match sync.staged_profile {
            Some(staged) => staged,
            None => return (session_id, Ok(None)),
        },
        Ok(None) => return (session_id, Ok(None)),
        Err(error) => {
            let mut details = held
                .failures
                .iter()
                .map(|failure| failure.detail.clone())
                .collect::<Vec<_>>();
            details.push(format!("AWS credentials: {error:#}"));
            return (
                session_id,
                Ok(Some(SessionSync {
                    actions: withdrawn,
                    failures: vec![StepFailure {
                        step: SyncStep::Aws,
                        detail: details.join("; "),
                    }],
                    staged_profile: None,
                })),
            );
        }
    };
    // The host profile the mint runs as is the host's selection as it is now,
    // read now. A configuration that cannot be read fails the cycle closed: the
    // session stays managed, nothing is minted, credentials the controller
    // issued are kept until they expire, anything else is withdrawn, and the
    // reason is reported.
    let selection = base.clone();
    let source = match tokio::task::spawn_blocking(move || {
        crate::aws_credentials::resolve(&selection)
    })
    .await
    {
        Ok(Ok(source)) => source,
        failed => {
            let error = match failed {
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => error.to_string(),
                Ok(Ok(_)) => unreachable!(),
            };
            let unavailable = AwsSync::Unavailable {
                error: format!(
                    "the host's AWS profile could not be determined, so no credentials were renewed: {error}"
                ),
                scoped_required: true,
                expected_scope: ScopeExpectation::of(target),
            };
            let result = aws_job(
                access,
                target,
                installing,
                SyncWork::Aws(Box::new(unavailable)),
            )
            .await;
            return (session_id, result);
        }
    };
    let aws = match start_scoped_mint(target, &source, &staged, force_new) {
        Some((mint, _stops_with_the_task)) => match mint
            .await
            .context("AWS role assumption task stopped")
            .and_then(|result| result)
        {
            Ok(credentials) => AwsSync::Install(credentials),
            // A failed mint, including a harness configuration that could not
            // be read this time: the session stays managed, keeps what the
            // controller issued until it expires, loses anything else, and
            // the failure is reported.
            Err(error) => AwsSync::Unavailable {
                error: format!("{error:#}"),
                scoped_required: true,
                expected_scope: ScopeExpectation::of(target),
            },
        },
        None => AwsSync::Unmanaged,
    };
    let mut result = aws_job(access, target, installing, SyncWork::Aws(Box::new(aws))).await;
    if let Ok(Some(sync)) = &mut result {
        sync.actions.splice(0..0, withdrawn);
    }
    (session_id, result)
}

/// One AWS job on the session: it takes its turn with the same session's login
/// step instead of competing with it for the admission slot, where a refusal
/// would skip it.
async fn aws_job(
    access: &SessionAccess<'_>,
    target: &CredentialSyncTarget,
    installing: Option<SessionTurn>,
    work: SyncWork,
) -> Result<Option<SessionSync>> {
    let turn = access.steps.enter(&target.session_id).await;
    admitted(
        access.gate,
        access.runtime,
        target,
        turn,
        installing,
        |hold| async move { run_on_session(access.relays, target, work, hold).await },
    )
    .await
}

/// What a session's renewal task needs, shared by all of them.
#[derive(Clone)]
pub(super) struct RenewalEnv {
    pub(super) relays: SessionRelays,
    pub(super) gate: Option<Arc<crate::recovery_gate::RecoveryGate>>,
    pub(super) runtime: Option<Arc<crate::daemon::RuntimeState>>,
    pub(super) steps: SessionSteps,
    /// Orders one session's renewals, so a replaced task's cycle finishes
    /// before its successor's begins.
    pub(super) installs: SessionSteps,
    pub(super) results: mpsc::UnboundedSender<CredentialSyncResult>,
    /// How long after a cycle that did not complete cleanly the next is tried.
    pub(super) retry: Duration,
}

impl RenewalEnv {
    pub(super) fn access(&self) -> SessionAccess<'_> {
        SessionAccess {
            relays: &self.relays,
            gate: self.gate.as_ref(),
            runtime: self.runtime.as_ref(),
            steps: &self.steps,
        }
    }
}

/// A request to renew a session's credentials now, with new ones, and to
/// answer with the cycle's outcome.
pub(super) struct RenewalWake {
    pub(super) reply: tokio::sync::oneshot::Sender<CredentialSyncOutcome>,
}

struct RenewalTask {
    target: CredentialSyncTarget,
    handle: tokio::task::JoinHandle<()>,
    wakes: mpsc::UnboundedSender<RenewalWake>,
}

/// One renewal task per role-scoped session, started and stopped as the
/// published targets change. A session's AWS credentials are renewed on their
/// own schedule, at half their lifetime, by a task that is not part of any
/// profile pass: a stalled session holds its own renewal and nothing else.
#[derive(Default)]
pub(super) struct RenewalTasks(BTreeMap<String, RenewalTask>);

impl RenewalTasks {
    /// A task standing in for the session's renewal, as one that had been
    /// started for `target`.
    #[cfg(test)]
    pub(super) fn with_task(
        target: &CredentialSyncTarget,
        handle: tokio::task::JoinHandle<()>,
        wakes: mpsc::UnboundedSender<RenewalWake>,
    ) -> Self {
        Self(BTreeMap::from([(
            target.session_id.clone(),
            RenewalTask {
                target: target.clone(),
                handle,
                wakes,
            },
        )]))
    }

    /// Whether the session has a renewal task that has not ended.
    #[cfg(test)]
    pub(super) fn is_running(&self, session_id: &str) -> bool {
        self.0
            .get(session_id)
            .is_some_and(|task| !task.handle.is_finished())
    }

    pub(super) fn sync(&mut self, targets: &[CredentialSyncTarget], env: &RenewalEnv) {
        let wanted = targets
            .iter()
            .filter(|target| {
                target
                    .aws_credentials
                    .as_ref()
                    .is_some_and(|source| source.role.is_some())
            })
            .map(|target| (target.session_id.as_str(), target))
            .collect::<BTreeMap<_, _>>();
        // A session that went away, or whose target changed (a role edit, a
        // new worker), has its task replaced; the rest are left alone.
        self.0.retain(|session_id, task| {
            let keep = wanted
                .get(session_id.as_str())
                .is_some_and(|target| **target == task.target);
            if !keep {
                task.handle.abort();
            }
            keep
        });
        // A task that has ended on its own (it panicked, or it stopped) is
        // noticed at every sync, reported as the session's failure, and
        // started again below; the sync interval paces the restarts.
        let ended = self
            .0
            .iter()
            .filter(|(_, task)| task.handle.is_finished())
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in ended {
            let Some(task) = self.0.remove(&session_id) else {
                continue;
            };
            // It has finished, so joining it does not wait.
            let how = match futures::FutureExt::now_or_never(task.handle) {
                Some(Err(error)) => error.to_string(),
                _ => "it stopped".to_owned(),
            };
            tracing::warn!(session_id, "a credential renewal task ended: {how}");
            let sent = env.results.send(CredentialSyncResult {
                profile_id: task.target.profile_id.clone(),
                trigger: None,
                failure: None,
                partial: true,
                outcomes: vec![CredentialSyncOutcome {
                    session_id: session_id.clone(),
                    outcome: Ok(Vec::new()),
                    step_failures: vec![StepFailure {
                        step: SyncStep::Aws,
                        detail: format!(
                            "AWS credentials: the session's renewal task stopped ({how}) and is being restarted"
                        ),
                    }],
                }],
            });
            if sent.is_err() {
                tracing::debug!(
                    session_id,
                    "credential sync result receiver was already closed"
                );
            }
        }
        for (session_id, target) in wanted {
            if !self.0.contains_key(session_id) {
                let (wakes, wake_rx) = mpsc::unbounded_channel();
                let handle = tokio::spawn(renew_session_aws(env.clone(), target.clone(), wake_rx));
                self.0.insert(
                    session_id.to_owned(),
                    RenewalTask {
                        target: target.clone(),
                        handle,
                        wakes,
                    },
                );
            }
        }
    }

    /// Ask the session's renewal task to renew now with new credentials. The
    /// answer is its cycle's outcome; `None` when the session has no task.
    pub(super) fn wake(
        &self,
        session_id: &str,
    ) -> Option<tokio::sync::oneshot::Receiver<CredentialSyncOutcome>> {
        let task = self.0.get(session_id)?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        // A task that has ended no longer reads its wakes. The message, and the
        // reply in it, is dropped, so the answer reports the renewal's outcome
        // as unknown, and the next sync restarts the task.
        if task.wakes.send(RenewalWake { reply }).is_err() {
            tracing::warn!(session_id, "a credential renewal task is not listening");
        }
        Some(answer)
    }
}

impl Drop for RenewalTasks {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.handle.abort();
        }
    }
}

/// Keep one session's AWS credentials renewed until the task is aborted. Each
/// cycle takes the session's install lock first, which orders it after any
/// cycle of a replaced task still queued or running, and ends on any path by
/// releasing it. A wake renews now, with new credentials, and answers with the
/// cycle's outcome.
pub(super) async fn renew_session_aws(
    env: RenewalEnv,
    target: CredentialSyncTarget,
    mut wakes: mpsc::UnboundedReceiver<RenewalWake>,
) {
    // Every trigger waiting for a cycle's outcome: a second wake does not
    // replace the first one's reply.
    let mut pending = Vec::<tokio::sync::oneshot::Sender<CredentialSyncOutcome>>::new();
    // Cycles in a row that reached the session and failed. A deferred cycle
    // (the actor was busy) is neither a failure nor a success.
    let mut failures_in_a_row = 0u32;
    let retry_limit = target
        .aws_credentials
        .as_ref()
        .and_then(|source| source.role.as_ref())
        .map_or(env.retry, |role| {
            crate::aws_assume_role::credential_half_life(role).max(env.retry)
        });
    loop {
        let mut delay = env.retry;
        // Triggers that arrived while the last cycle ran are answered by this
        // one, which is forced.
        while let Ok(wake) = wakes.try_recv() {
            pending.push(wake.reply);
        }
        let installing = env.installs.enter(&target.session_id).await;
        let (_, result) = aws_cycle(
            &env.access(),
            &target,
            AwsCycleOptions {
                installing: Some(installing),
                force_new: !pending.is_empty(),
                interim: Some(&env.results),
            },
        )
        .await;
        let completed = matches!(&result, Ok(Some(sync)) if sync.failures.is_empty());
        // The source as the cycle ran it (the host's selection now), which
        // keys the cached credentials the schedule looks for.
        let ran_as = scheduling_source(&target).await;
        // A cycle that reached the session has checked its AWS credentials,
        // whatever it found: clean or failed, its outcome is reported, and
        // the notices decide what that ends. A deferred cycle checked
        // nothing and says nothing.
        let checked = !matches!(&result, Ok(None));
        if completed {
            failures_in_a_row = 0;
        } else if checked {
            failures_in_a_row = failures_in_a_row.saturating_add(1);
            delay = retry_delay(
                env.retry,
                failures_in_a_row - 1,
                retry_limit,
                ran_as.as_ref().and_then(|source| {
                    crate::aws_assume_role::ScopedAwsCredentials::shared()
                        .remaining_validity(&target.session_id, source)
                }),
            );
        }
        let mut outcomes = BTreeMap::new();
        merge_aws_cycle(&mut outcomes, &target.session_id, result);
        let mut report = outcomes.remove(&target.session_id).or_else(|| {
            checked.then(|| CredentialSyncOutcome {
                session_id: target.session_id.clone(),
                outcome: Ok(Vec::new()),
                step_failures: Vec::new(),
            })
        });
        // The triggers that woke this cycle each receive its full outcome. The
        // task still reports what the cycle checked, without the actions the
        // triggers' results now carry. A deferred cycle (the actor was busy)
        // answers none of them: the next cycle, still forced, does.
        if checked && let Some(full) = report.clone() {
            let mut answered = false;
            for reply in pending.drain(..) {
                answered |= reply.send(full.clone()).is_ok();
            }
            if answered {
                report = report.map(|outcome| CredentialSyncOutcome {
                    outcome: Ok(Vec::new()),
                    ..outcome
                });
            }
        }
        if let Some(outcome) = report {
            let sent = env.results.send(CredentialSyncResult {
                profile_id: target.profile_id.clone(),
                trigger: None,
                failure: None,
                partial: true,
                outcomes: vec![outcome],
            });
            if sent.is_err() {
                return;
            }
        }
        if completed
            && let Some(source) = &ran_as
            && let Some(until) = crate::aws_assume_role::ScopedAwsCredentials::shared()
                .renew_in(&target.session_id, source)
        {
            delay = until;
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            next = wakes.recv() => match next {
                Some(next) => {
                    // A trigger asks for a fresh attempt now, and starts the
                    // delays over.
                    failures_in_a_row = 0;
                    pending.push(next.reply);
                }
                None => return,
            },
        }
    }
}

/// The source as the cycle just minted for it, to find its cached credentials:
/// the cache is keyed by the environment the mint ran in, which includes the
/// AWS profile the host's configuration selects. Read now, off the async
/// threads; `None` when it cannot be read (nothing is cached for it, and the
/// schedule falls back to the retry delay).
async fn scheduling_source(
    target: &CredentialSyncTarget,
) -> Option<mj_core::aws_credentials::AwsCredentialSource> {
    let source = target.aws_credentials.clone()?;
    tokio::task::spawn_blocking(move || crate::aws_credentials::resolve(&source).ok())
        .await
        .ok()
        .flatten()
}

/// How long to wait before the next attempt after the `failures`-th
/// consecutive failure (counting from zero): the backoff, and while the
/// current credentials are still valid no more than half the time they have
/// left (but never less than `base`). Attempts then come closer as the expiry
/// nears, so the last one lands before it; the limit is the expiry itself.
/// Once the credentials have expired (`remaining` is `None`) the backoff
/// stands: an authentication failure, which wakes the renewal, is how a session
/// that has recovered renews promptly.
pub(super) fn retry_delay(
    base: Duration,
    failures: u32,
    limit: Duration,
    remaining: Option<Duration>,
) -> Duration {
    let delay = backoff(base, failures, limit);
    match remaining {
        Some(remaining) => delay.min((remaining / 2).max(base)),
        None => delay,
    }
}

/// How long to wait after the `failures`-th consecutive failure (counting from
/// zero): `base` doubled each time, up to `limit`.
fn backoff(base: Duration, failures: u32, limit: Duration) -> Duration {
    base.saturating_mul(2u32.saturating_pow(failures.min(31)))
        .min(limit.max(base))
}

/// Start the AWS role assumption for a session whose profile names a role,
/// from the shared cache or a new `AssumeRole`; it runs on a blocking thread
/// from the moment this returns. `None` for a session without a role, which
/// shares the profile's unscoped export. A failure is this session's failed
/// outcome: the worker keeps the credentials it has, and no path falls back to
/// the unscoped export. `force_new` drops the cached credentials first, for a
/// session whose credentials may have been refused.
///
/// The mint is owned by the task that awaits it: the returned guard, dropped
/// with that task, terminates the AWS CLI's process group through the shared
/// executor's cancellation, and only then does the cache entry's lock (held
/// across the subprocess) come free. A superseded task's stuck helper therefore
/// cannot keep its replacement waiting.
fn start_scoped_mint(
    target: &CredentialSyncTarget,
    source: &mj_core::aws_credentials::AwsCredentialSource,
    worker_profile: &str,
    force_new: bool,
) -> Option<(
    tokio::task::JoinHandle<Result<mj_core::aws_credentials::AwsCredentials>>,
    mj_core::targets::ProcessCancellationGuard,
)> {
    source.role.as_ref()?;
    let source = source.clone();
    let session_id = target.session_id.clone();
    let worker_profile = worker_profile.to_owned();
    let executor = crate::targets::CancellableProcessExecutor::new(Arc::default());
    let stops_with_the_task = executor.cancel_on_drop();
    let mint = tokio::task::spawn_blocking(move || {
        let cache = crate::aws_assume_role::ScopedAwsCredentials::shared();
        if force_new {
            cache.forget(&session_id, &source);
        }
        let credentials = cache.credentials_for(
            &session_id,
            &source,
            // No deadline of our own; cancelled only with the task.
            &executor,
        )?;
        // Installed under the name the worker's configuration already has,
        // which may differ from the host profile the mint ran as.
        crate::aws_assume_role::for_worker_profile(credentials, &worker_profile)
    });
    Some((mint, stops_with_the_task))
}

/// The skills tree this session should converge to: the profile's own skills
/// plus Mjolnir's managed skills, exactly as launch staging writes them into
/// the session's staged home, collected for the archive format the session's
/// worker reads.
pub(super) fn canonical_session_skills(
    target: &CredentialSyncTarget,
    format: mj_core::skills::SkillsArchiveFormat,
) -> Result<mj_core::skills::SkillsArchive> {
    mj_core::skills::session_skills(
        target.harness,
        &target.profile_home,
        format,
        target.skills_scope,
    )
    .with_context(|| {
        format!(
            "collect canonical skills for profile {} from {}",
            target.profile_id,
            target.profile_home.display()
        )
    })
}

/// A profile's canonical skills tree in each archive format a worker may read.
/// Which one a session needs is known only from its worker's hello.
pub(super) struct CanonicalSkills {
    plain: [std::result::Result<mj_core::skills::SkillsArchive, String>; 2],
    gzip: [std::result::Result<mj_core::skills::SkillsArchive, String>; 2],
}

impl CanonicalSkills {
    pub(super) fn collect(target: &CredentialSyncTarget) -> Self {
        let mut base = target.clone();
        base.skills_scope = mj_core::skills::SkillsScope::Isolated;
        let collect = |format| {
            let isolated =
                canonical_session_skills(&base, format).map_err(|error| format!("{error:#}"));
            let localhost = isolated.clone().and_then(|archive| {
                archive
                    .for_session(
                        target.harness,
                        mj_core::skills::SkillsScope::Localhost,
                        format,
                    )
                    .map_err(|error| format!("{error:#}"))
            });
            [localhost, isolated]
        };
        Self {
            plain: collect(mj_core::skills::SkillsArchiveFormat::Plain),
            gzip: collect(mj_core::skills::SkillsArchiveFormat::Gzip),
        }
    }

    fn failed(reason: &str) -> Self {
        Self {
            plain: [Err(reason.to_owned()), Err(reason.to_owned())],
            gzip: [Err(reason.to_owned()), Err(reason.to_owned())],
        }
    }

    /// A tree that cannot be collected fails the whole reconciliation,
    /// credentials included.
    fn for_format(
        &self,
        format: mj_core::skills::SkillsArchiveFormat,
        scope: mj_core::skills::SkillsScope,
    ) -> Result<&mj_core::skills::SkillsArchive> {
        let collected = match format {
            mj_core::skills::SkillsArchiveFormat::Plain => &self.plain,
            mj_core::skills::SkillsArchiveFormat::Gzip => &self.gzip,
        };
        let index = match scope {
            mj_core::skills::SkillsScope::Localhost => 0,
            mj_core::skills::SkillsScope::Isolated => 1,
        };
        collected[index]
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }
}

/// The profile's login as the controller holds it.
pub(super) struct CanonicalLogin {
    pub(super) path: std::path::PathBuf,
    pub(super) credential: CredentialSnapshot,
    pub(super) bytes: Vec<u8>,
}

/// What one session is reconciled against, read on the controller before its
/// worker is asked anything. Nothing here fails the session as a whole: each
/// part that could not be read is the failure of the one step that needs it.
pub(super) struct SessionCanonical {
    pub(super) login: Result<CanonicalLogin, String>,
    pub(super) skills: Arc<CanonicalSkills>,
    pub(super) github: GithubSync,
    pub(super) aws: AwsSync,
    pub(super) slopcop_login: HostSlopcopLogin,
}

impl SessionCanonical {
    pub(super) fn read(
        target: &CredentialSyncTarget,
        skills: Arc<CanonicalSkills>,
        github: GithubSync,
    ) -> Self {
        let read_login = || -> Result<CanonicalLogin> {
            let path = harness_authentication_marker(target.harness, &target.profile_home);
            // A login database is read where the harness keeps it; the
            // profile home holds no copy of it.
            let (credential, bytes) = if target.harness.keeps_logins_in_database() {
                match crate::opencode_logins::home_logins(&target.profile_home)? {
                    Some(bytes) => (CredentialSnapshot::of(target.harness, &bytes), bytes),
                    None => (CredentialSnapshot::absent(), Vec::new()),
                }
            } else {
                read_credential_file(target.harness, &path)?
            };
            Ok(CanonicalLogin {
                path,
                credential,
                bytes,
            })
        };
        Self {
            // A profile without a login file has nothing to read.
            login: if target.skips_login_file_sync {
                Err("not synchronized".to_owned())
            } else {
                read_login().map_err(|error| format!("{error:#}"))
            },
            skills,
            github,
            aws: AwsSync::Unmanaged,
            slopcop_login: HostSlopcopLogin::Unknown,
        }
    }
}

/// What a session's worker connection is asked to do.
pub(super) enum SyncWork {
    /// Every sync step of the profile's pass.
    Full(Box<SessionCanonical>),
    /// Only the session's AWS credentials.
    Aws(Box<AwsSync>),
    /// Only the AWS profile the session's worker was staged with, for a
    /// role-scoped session.
    StagedProfile,
}

/// Reconcile one session. Returns every action taken, where an empty list
/// means the copies already agree, or `None` when the session could not be
/// reached this cycle because its actor had no connection to lend.
pub(super) async fn reconcile_session(
    relays: &SessionRelays,
    target: &CredentialSyncTarget,
    skills: &Arc<CanonicalSkills>,
    github: &GithubSync,
    aws: &AwsSync,
    slopcop_login: &HostSlopcopLogin,
    hold: JobHold,
) -> Result<Option<SessionSync>> {
    let mut canonical = SessionCanonical::read(target, skills.clone(), github.clone());
    canonical.aws = aws.clone();
    canonical.slopcop_login = slopcop_login.clone();
    run_on_session(relays, target, SyncWork::Full(Box::new(canonical)), hold).await
}

/// Run `work` on the session's worker connection.
pub(super) async fn run_on_session(
    relays: &SessionRelays,
    target: &CredentialSyncTarget,
    work: SyncWork,
    hold: JobHold,
) -> Result<Option<SessionSync>> {
    match relays {
        SessionRelays::Actors(manager) => {
            let Some(handle) = manager.find_session(target.session_id.clone()).await? else {
                tracing::debug!(
                    session_id = %target.session_id,
                    "credential sync waits for the session's relay actor"
                );
                return Ok(None);
            };
            let (job, response, _queued) = CredentialSyncJob::queue(target.clone(), work, hold);
            handle.run_on_connection(Box::new(job)).await;
            match response
                .await
                .context("the session actor dropped the credential sync")?
            {
                Ok(sync) => Ok(Some(sync)),
                Err(error) if RelayJobDeferred::marks(&error) => {
                    tracing::debug!(
                        session_id = %target.session_id,
                        reason = %error,
                        "credential sync deferred to the next cycle"
                    );
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        }
        #[cfg(test)]
        SessionRelays::Direct => {
            let _queued = QueuedJob(hold);
            let mut client = RelayClient::connect(&target.spec, &target.session_id).await?;
            let result = run_work(&mut client, target, work).await;
            if let Err(error) = client.detach().await {
                tracing::warn!(
                    session_id = %target.session_id,
                    "could not close the credential sync connection: {error:#}"
                );
            }
            result.map(Some)
        }
    }
}

async fn run_work(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    work: SyncWork,
) -> Result<SessionSync> {
    match work {
        SyncWork::Full(canonical) => reconcile_on(client, target, &canonical).await,
        SyncWork::Aws(aws) => {
            let mut sync = SessionSync {
                actions: Vec::new(),
                failures: Vec::new(),
                staged_profile: None,
            };
            reconcile_aws_step(client, &aws, &mut sync).await;
            Ok(sync)
        }
        SyncWork::StagedProfile => {
            // The name role-scoped credentials are installed under is the one
            // the worker's staged configuration already has, so the session's
            // environment keeps selecting it. The session can edit that file;
            // that is safe only because what is installed under the name is
            // role-scoped, so a changed name changes where those credentials
            // are written, not what they allow. The name is checked.
            anyhow::ensure!(
                client.protocol_version() >= mj_core::relay::RELAY_AWS_CREDENTIALS_REMOVAL_PROTOCOL,
                "the session's worker (relay protocol {}) cannot report the AWS profile it was staged with, so its credentials cannot be renewed. Mjolnir is replacing that worker automatically as soon as the session is idle; nothing needs to be done",
                client.protocol_version()
            );
            let (_, profile) = client.aws_credentials_staging().await?;
            let profile = profile.context(
                "the session has no valid staged AWS configuration, so there is no profile to renew credentials for. Restart the session to stage its AWS configuration again",
            )?;
            anyhow::ensure!(
                mj_core::aws_credentials::valid_profile_name(&profile),
                "the worker reported an AWS profile name that is not valid. Restart the session to stage its AWS configuration again"
            );
            Ok(SessionSync {
                actions: Vec::new(),
                failures: Vec::new(),
                staged_profile: Some(profile),
            })
        }
    }
}

/// Reconcile one session over a connection to its worker.
///
/// Every step runs whether or not another failed, and each failure is
/// recorded on its own: the AWS credentials, the harness login, the skills,
/// the GitHub token and the slopcop login do not depend on one another, so a
/// failing STS call or GitHub lookup must not hold back the rest.
pub(super) async fn reconcile_on(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: &SessionCanonical,
) -> Result<SessionSync> {
    let mut sync = SessionSync {
        actions: Vec::new(),
        failures: Vec::new(),
        staged_profile: None,
    };
    reconcile_aws_step(client, &canonical.aws, &mut sync).await;
    let SessionSync {
        actions, failures, ..
    } = &mut sync;
    let mut note = |step: SyncStep, label: &str, error: String| {
        failures.push(StepFailure {
            step,
            detail: format!("{label}: {error}"),
        })
    };
    // Profiles without a harness login file already receive credentials
    // through their environment or target credential chain.
    if !target.skips_login_file_sync {
        match &canonical.login {
            Ok(login) => {
                if let Err(error) = reconcile_credentials(
                    client,
                    target,
                    &login.path,
                    &login.credential,
                    &login.bytes,
                    actions,
                )
                .await
                {
                    note(
                        SyncStep::HarnessLogin,
                        "harness login",
                        format!("{error:#}"),
                    );
                }
            }
            Err(error) => note(SyncStep::HarnessLogin, "harness login", error.clone()),
        }
    }
    match canonical
        .skills
        .for_format(client.skills_archive_format(), target.skills_scope)
    {
        Ok(skills) => match reconcile_skills(client, target, skills).await {
            Ok(true) => actions.push(CredentialSyncAction::SkillsPushed),
            Ok(false) => {}
            Err(error) => note(SyncStep::Skills, "skills", format!("{error:#}")),
        },
        Err(error) => note(SyncStep::Skills, "skills", format!("{error:#}")),
    }
    if target.sync_github_token {
        match &canonical.github {
            GithubSync::Known(token) => {
                match reconcile_github_token(client, target, token.as_deref()).await {
                    Ok(action) => actions.extend(action),
                    Err(error) => note(SyncStep::GithubToken, "GitHub token", format!("{error:#}")),
                }
            }
            GithubSync::Unavailable(error) => {
                note(SyncStep::GithubToken, "GitHub token", error.clone())
            }
        }
    }
    match reconcile_slopcop_login(client, target, &canonical.slopcop_login).await {
        Ok(action) => actions.extend(action),
        Err(error) => note(SyncStep::Slopcop, "slopcop login", format!("{error:#}")),
    }
    Ok(sync)
}

/// The AWS step of a session's sync, recorded on `sync`. It fails on its own:
/// the other steps do not depend on STS.
async fn reconcile_aws_step(client: &mut RelayClient, aws: &AwsSync, sync: &mut SessionSync) {
    // Named for the branch that failed: installing credentials, or
    // withdrawing the ones that must not stay.
    let step = match aws {
        AwsSync::Install(_) => "AWS credentials install",
        _ => "AWS credentials withdrawal",
    };
    // One failure for the step, saying everything that went wrong: a refused
    // mint and a withdrawal that failed are both the reader's to know, and the
    // withdrawal first, because it says unscoped credentials may remain.
    let mut messages = Vec::new();
    match reconcile_aws(client, aws).await {
        Ok(action) => sync.actions.extend(action),
        Err(error) => messages.push(format!("{step}: {error:#}")),
    }
    if let AwsSync::Unavailable { error, .. } = aws {
        messages.push(format!("AWS credentials: {error}"));
    }
    if !messages.is_empty() {
        sync.failures.push(StepFailure {
            step: aws.step(),
            detail: messages.join("; "),
        });
    }
}

/// Converge the session's AWS credentials, or withdraw the host's own when
/// the profile requires role-scoped credentials and none can be issued.
async fn reconcile_aws(
    client: &mut RelayClient,
    aws: &AwsSync,
) -> Result<Option<CredentialSyncAction>> {
    match aws {
        AwsSync::Unmanaged => Ok(None),
        AwsSync::Install(credentials) => {
            let expected = mj_core::aws_credentials::snapshot(&credentials.encode()?);
            // The secret alone does not say the worker holds what Mjolnir
            // issued: a worker replaced by an upgrade keeps the secret it was
            // given before it could record a scope. Install again until the
            // worker's record matches, so a later outage or restart keeps
            // these credentials instead of withdrawing them.
            let scope_current = credentials.scope().is_none()
                || client.protocol_version()
                    < mj_core::relay::RELAY_AWS_CREDENTIALS_REMOVAL_PROTOCOL
                || client.aws_credentials_scope().await?.as_ref() == credentials.scope();
            if scope_current && client.aws_credentials_state().await? == expected {
                return Ok(None);
            }
            let installed = client.install_aws_credentials(credentials).await?;
            anyhow::ensure!(
                installed == expected,
                "session AWS credentials do not match the controller after installation"
            );
            Ok(Some(CredentialSyncAction::Pushed))
        }
        AwsSync::Unavailable {
            scoped_required: false,
            ..
        } => Ok(None),
        AwsSync::Unavailable {
            expected_scope,
            scoped_required: true,
            ..
        }
        | AwsSync::Pending { expected_scope } => {
            // A worker before relay protocol 55 can neither withdraw nor say
            // what it holds. It is not a dead end: the worker upgrade
            // coordinator replaces a worker whose build differs from this
            // controller's as soon as its session is idle (a busy one waits
            // for the turn), keeping its home, and the next pass then
            // withdraws on the new worker.
            anyhow::ensure!(
                client.protocol_version() >= mj_core::relay::RELAY_AWS_CREDENTIALS_REMOVAL_PROTOCOL,
                "the session's worker (relay protocol {}) predates withdrawing AWS credentials, so the host's own may still be installed. Mjolnir is replacing that worker automatically as soon as the session is idle and will then withdraw them; nothing needs to be done",
                client.protocol_version()
            );
            let held = client.aws_credentials_state().await?;
            // What the worker recorded when credentials were installed: kept
            // while unexpired and issued for this session's role and session.
            // Anything else, including credentials with no record, may be the
            // host's own from before the role was configured, and static host
            // keys never expire. The record is the worker's, so this holds
            // across a daemon restart.
            if held.present
                && let Some(expected) = expected_scope
                && let Some(recorded) = client.aws_credentials_scope().await?
                && expected.matches(&recorded)
            {
                // The record is the session's to write; only one the
                // controller made, for this very secret, counts. If the
                // controller cannot tell (its signing key is unreadable), the
                // session keeps what it has and the step reports why: an error
                // is not evidence that the record was forged.
                if crate::aws_assume_role::scope_is_authentic(&recorded, &held.fingerprint)
                    .await
                    .context("could not verify the session's AWS scope record; its credentials were left as they are")?
                {
                    return Ok(None);
                }
            }
            // Always asked, whatever the reported state: the state is "absent"
            // when a provider file is missing or differs, though the secret
            // file may still exist. Removal is idempotent and deletes the
            // secret file itself.
            let removed = client.remove_aws_credentials().await?;
            anyhow::ensure!(
                !removed.present,
                "the session kept its AWS credentials after removal"
            );
            if !held.present {
                return Ok(None);
            }
            Ok(Some(CredentialSyncAction::AwsCredentialsRemoved))
        }
    }
}

/// One session's reconciliation, run by its actor on the actor's connection.
/// It owns its `hold` until it has run or been dropped.
pub(super) struct CredentialSyncJob {
    pub(super) target: CredentialSyncTarget,
    pub(super) work: SyncWork,
    pub(super) reply: tokio::sync::oneshot::Sender<Result<SessionSync>>,
    pub(super) hold: JobHold,
}

impl CredentialSyncJob {
    /// The job, the receiver of its answer, and the guard that abandons the
    /// job if the awaiting task stops first.
    pub(super) fn queue(
        target: CredentialSyncTarget,
        work: SyncWork,
        hold: JobHold,
    ) -> (
        Self,
        tokio::sync::oneshot::Receiver<Result<SessionSync>>,
        QueuedJob,
    ) {
        let (reply, response) = tokio::sync::oneshot::channel();
        let queued = QueuedJob(hold.clone());
        (
            Self {
                target,
                work,
                reply,
                hold,
            },
            response,
            queued,
        )
    }
}

impl RelayConnectionJob for CredentialSyncJob {
    fn run<'a>(self: Box<Self>, client: &'a mut RelayClient) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let Self {
                target,
                work,
                reply,
                hold,
            } = *self;
            // The actor decides, when it is this job's turn, whether the job is
            // still wanted: the task that queued it may have stopped while it
            // waited (its target changed, the session went away), or a
            // lifecycle operation may have cancelled its admission. Either
            // already released the hold, and the work is dropped unrun.
            if !hold.start() {
                tracing::debug!(
                    session_id = %target.session_id,
                    "credential sync dropped: no longer wanted"
                );
                // Deferred, not failed: the awaiting side, if it is still
                // there, tries again in the next cycle.
                let _ = reply.send(Err(anyhow::Error::new(RelayJobDeferred(
                    "the credential sync was cancelled before it started",
                ))));
                return;
            }
            if hold.gated() {
                match target_is_current(hold.0.runtime.as_ref(), &target).await {
                    Ok(true) => {}
                    Ok(false) => {
                        hold.finish();
                        let _ = reply.send(Err(anyhow::Error::new(RelayJobDeferred(
                            "the session changed before the credential sync ran",
                        ))));
                        return;
                    }
                    Err(error) => {
                        hold.finish();
                        let _ = reply.send(Err(error));
                        return;
                    }
                }
            }
            // A lifecycle operation that cancels the admission cancels the
            // job's calls too: one in flight is abandoned (with its
            // connection, as a timeout would), and the next cycle reconciles
            // from what the worker reports.
            client.set_cancellation(hold.cancellation());
            let result = run_work(client, &target, work).await;
            client.set_cancellation(None);
            let result = if hold.cancelled() {
                Err(anyhow::Error::new(RelayJobDeferred(
                    "the credential sync was cancelled by a lifecycle operation",
                )))
            } else {
                result
            };
            // Released before the answer, so whoever awaits it can take the
            // session's slot at once.
            hold.finish();
            if reply.send(result).is_err() {
                tracing::debug!(
                    session_id = %target.session_id,
                    "credential sync result receiver was already closed"
                );
            }
        })
    }

    fn refuse(self: Box<Self>, error: anyhow::Error) {
        if self.reply.send(Err(error)).is_err() {
            tracing::debug!(
                session_id = %self.target.session_id,
                "credential sync refusal receiver was already closed"
            );
        }
    }
}

pub(super) async fn reconcile_credentials(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical_path: &Path,
    canonical: &CredentialSnapshot,
    canonical_bytes: &[u8],
    actions: &mut Vec<CredentialSyncAction>,
) -> Result<()> {
    let session = client.credential_state().await?;
    if target.harness.keeps_logins_in_database() {
        if let Some(bytes) = stored_logins_push(canonical, canonical_bytes, &session) {
            client.install_credentials(bytes).await?;
            actions.push(CredentialSyncAction::Pushed);
        }
        return Ok(());
    }
    match reconcile(canonical, &session) {
        SyncAction::None => {
            if canonical.present
                && session.present
                && canonical.fingerprint != session.fingerprint
                && canonical.freshness_epoch_ms.is_none()
                && session.freshness_epoch_ms.is_none()
            {
                tracing::warn!(
                    session_id = %target.session_id,
                    profile_id = %target.profile_id,
                    "credential copies differ but neither reports a refresh time; leaving both alone"
                );
            }
        }
        SyncAction::Push => {
            client.install_credentials(canonical_bytes).await?;
            actions.push(CredentialSyncAction::Pushed);
        }
        SyncAction::Pull => {
            let bytes = client.read_credentials().await?;
            validate_credential_payload(target.harness, &bytes).with_context(|| {
                format!(
                    "session {} returned an unusable credential file",
                    target.session_id
                )
            })?;
            write_credential_file(target.harness, canonical_path, &bytes).with_context(|| {
                format!(
                    "install fresher credentials from session {} for profile {}",
                    target.session_id, target.profile_id
                )
            })?;
            actions.push(CredentialSyncAction::Pulled);
        }
    }
    Ok(())
}

/// The export of a profile with no logins.
const NO_STORED_LOGINS: &[u8] = b"[]";

/// What to push to a session whose staged logins are `session`, when the
/// profile's login database exports `canonical` (`bytes`); `None` when the
/// session already holds them. The database is the profile's only copy, so a
/// session's copy is never pulled, and a session staged while the profile had
/// no logins holds no export, which is the same as an empty one.
pub(crate) fn stored_logins_push<'a>(
    canonical: &CredentialSnapshot,
    bytes: &'a [u8],
    session: &CredentialSnapshot,
) -> Option<&'a [u8]> {
    let wanted: &'a [u8] = if canonical.present {
        bytes
    } else {
        NO_STORED_LOGINS
    };
    let current = if session.present {
        session.fingerprint == mj_core::credentials::credential_fingerprint(wanted)
    } else {
        !canonical.present
    };
    (!current).then_some(wanted)
}

pub(super) async fn reconcile_github_token(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: Option<&str>,
) -> Result<Option<CredentialSyncAction>> {
    let session = match client.github_token_state().await {
        Ok(state) => state,
        Err(error) if sync_method_unsupported(&error) => {
            tracing::debug!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                "worker predates GitHub token sync; skipping until the target is re-provisioned"
            );
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    match canonical {
        Some(token) => {
            let canonical = mj_core::credentials::GithubTokenSnapshot::of(token);
            if session == canonical {
                return Ok(None);
            }
            let installed = client.install_github_token(token).await?;
            if installed != canonical {
                bail!(
                    "session {} GitHub token fingerprint does not match the controller after install",
                    target.session_id
                );
            }
            Ok(Some(CredentialSyncAction::GithubTokenPushed))
        }
        None if session.present => {
            let removed = client.remove_github_token().await?;
            if removed.present {
                bail!(
                    "session {} retained its GitHub token after removal",
                    target.session_id
                );
            }
            Ok(Some(CredentialSyncAction::GithubTokenRemoved))
        }
        None => Ok(None),
    }
}

/// Whether a session gets a copy of the host's slopcop login: only one that
/// cannot read the host's own, decided by its target's skills scope.
fn syncs_slopcop_login(target: &CredentialSyncTarget) -> bool {
    target.skills_scope == mj_core::skills::SkillsScope::Isolated
}

/// Converge an isolated session's copy of the slopcop login onto the host's.
/// Sync is the copy's only writer, and it writes only isolated sessions; a
/// raw localhost session's home is staged without one and reads the host's
/// own login.
pub(super) async fn reconcile_slopcop_login(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    host: &HostSlopcopLogin,
) -> Result<Option<CredentialSyncAction>> {
    let wanted = match host {
        _ if !syncs_slopcop_login(target) => return Ok(None),
        HostSlopcopLogin::Unknown => return Ok(None),
        HostSlopcopLogin::SignedOut => None,
        HostSlopcopLogin::SignedIn(login) => Some(login),
    };
    if !client.supports_slopcop_login_sync() {
        tracing::debug!(
            session_id = %target.session_id,
            protocol = client.protocol_version(),
            "worker predates slopcop login sync; it gets the login once the current worker replaces it"
        );
        return Ok(None);
    }
    let session = client.slopcop_login_state().await?;
    match wanted {
        Some(login) => {
            let wanted = login.snapshot();
            if session == wanted {
                return Ok(None);
            }
            let installed = client.install_slopcop_login(login).await?;
            anyhow::ensure!(
                installed == wanted,
                "session {} slopcop login does not match the host's after install",
                target.session_id
            );
            Ok(Some(CredentialSyncAction::SlopcopLoginPushed))
        }
        None if session.present => {
            let removed = client.remove_slopcop_login().await?;
            anyhow::ensure!(
                !removed.present,
                "session {} kept its slopcop login after removal",
                target.session_id
            );
            Ok(Some(CredentialSyncAction::SlopcopLoginRemoved))
        }
        None => Ok(None),
    }
}

/// Converge the session's synced skills trees onto the canonical archive,
/// which was collected for the archive format `client`'s worker reads and is
/// sent in that format. Returns true when a push happened. Workers old enough
/// to predate skills sync answer the unknown method with `InvalidRequest`;
/// those sessions are skipped quietly until their target is re-provisioned.
pub(super) async fn reconcile_skills(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: &mj_core::skills::SkillsArchive,
) -> Result<bool> {
    let canonical_state = canonical.state();
    let session = match client.skills_state().await {
        Ok(state) => state,
        Err(error) if sync_method_unsupported(&error) => {
            tracing::debug!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                "worker predates skills sync; skipping until the target is re-provisioned"
            );
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if session == canonical_state {
        return Ok(false);
    }
    let installed = client
        .install_skills(&canonical.encode(client.skills_archive_format()))
        .await?;
    if installed != canonical_state {
        bail!(
            "session {} skills fingerprint {} does not match the canonical {} after install",
            target.session_id,
            installed.fingerprint,
            canonical_state.fingerprint
        );
    }
    Ok(true)
}

pub(super) fn sync_method_unsupported(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| rejected.0.code == RelayErrorCode::InvalidRequest)
}
