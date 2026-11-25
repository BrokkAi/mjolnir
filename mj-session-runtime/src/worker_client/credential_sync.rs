use super::*;
use crate::session_manager::{RelayConnectionJob, RelayJobDeferred, SessionManagerControl};
use futures::StreamExt;
use mj_core::config::harness_authentication_marker;
use mj_core::credentials::{
    LoginDeliveryState, RejectedLogins, credential_copy_for_harness, credential_holds_refresh_token,
};

type DeliveryRegistry = Arc<std::sync::Mutex<RejectedLogins>>;

/// How a reconciliation reaches each session's worker.
#[derive(Clone)]
pub enum SessionRelays {
    /// The session actor's own connection. A sync never opens a connection
    /// of its own to a live worker.
    Actors(SessionManagerControl),
    /// A connection of its own per session, for tests against fixture workers.
    #[doc(hidden)]
    Direct,
}

/// Reads the host's slopcop login. The daemon reads its own environment and
/// slopcop's configuration; tests pass their own reader, so a test never reads
/// the developer's login.
pub type SlopcopLoginSource = fn() -> Result<Option<mj_core::slopcop::SlopcopLogin>>;

/// What one reconciliation knows about the host's slopcop login.
#[derive(Debug, Clone)]
pub enum HostSlopcopLogin {
    /// It could not be read. Session copies stay as they are, because an
    /// unreadable login is not a signed-out one.
    Unknown,
    SignedOut,
    SignedIn(mj_core::slopcop::SlopcopLogin),
}

/// What credential sync asks the daemon. The daemon owns its session records
/// and configuration, so sync reads these facts from it when it acts and
/// keeps no copy. Tests run sync without a daemon.
pub trait CredentialSyncRuntime: Send + Sync {
    fn pending_retired_logins(
        &self,
        session_id: &str,
    ) -> Result<Vec<mj_core::credentials::PendingRetiredLogin>>;
    fn clear_pending_retired_login(&self, session_id: &str, operation_id: &str) -> Result<()>;
    fn retired_login_move_active(&self, session_id: &str, operation_id: &str) -> Result<bool>;
    fn credential_profile(&self, profile_id: &str) -> Option<mj_core::config::HarnessProfile>;
    /// The published list of sessions to keep supplied.
    fn credential_targets(&self) -> Arc<watch::Sender<Vec<CredentialSyncTarget>>>;
    fn credential_session_ids(
        &self,
    ) -> mj_core::snapshot_map::SnapshotMap<String, mj_core::state::SessionRecord>;
    fn credential_session_ended(&self, session_id: &str) -> bool;
    fn credential_profile_source_is_current(&self, target: &CredentialSyncTarget) -> bool;
    fn credential_target_is_current(&self, target: &CredentialSyncTarget) -> Result<bool>;
    /// The sessions a sync of `profile_id` leaves out, each with the reason.
    fn credential_sync_skips(
        &self,
        profile_id: &str,
        fanout: bool,
        caused_by: Option<&str>,
        present: &dyn Fn(&str) -> bool,
    ) -> Vec<(String, &'static str)>;
}

/// A test reader: the host's login is never known, so no session copy changes.
pub(super) fn unknown_slopcop_login() -> Result<Option<mj_core::slopcop::SlopcopLogin>> {
    bail!("tests do not read the host's slopcop login")
}

/// What a session's job owns from the moment it is queued on the session's
/// actor until it has run or been abandoned: for AWS renewal, its
/// install lock. The job holds it, not the task that queued it, so stopping
/// that task (its target changed, the session went away) cannot hand the
/// session to a newer renewal while the older job is running.
///
/// One state machine decides who may release it. While the job is only
/// queued, the task awaiting it can abandon it (its future was dropped), and
/// abandoning releases the lock at once. Once the actor starts the job it runs
/// to the end under the lock, because it is then using the session's
/// connection. The actor finds an abandoned job over and drops it unrun.
#[derive(Clone)]
pub struct JobHold(Arc<HoldState>);

struct HoldState {
    phase: std::sync::Mutex<HoldPhase>,
    runtime: Option<Arc<dyn CredentialSyncRuntime>>,
}

enum HoldPhase {
    Queued(Option<SessionTurn>),
    Running { _installing: Option<SessionTurn> },
    Over,
}

impl JobHold {
    pub fn new(
        installing: Option<SessionTurn>,
        runtime: Option<Arc<dyn CredentialSyncRuntime>>,
    ) -> Self {
        Self(Arc::new(HoldState {
            phase: std::sync::Mutex::new(HoldPhase::Queued(installing)),
            runtime,
        }))
    }

    fn phase(&self) -> std::sync::MutexGuard<'_, HoldPhase> {
        self.0
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The actor's turn to run the job. `false` when the job was abandoned
    /// (its lock is already released).
    fn start(&self) -> bool {
        // The lock is dropped after the mutex is released, so a release never
        // runs another lock's code under this one.
        let mut released = HoldPhase::Over;
        let started = {
            let mut phase = self.phase();
            match std::mem::replace(&mut *phase, HoldPhase::Over) {
                HoldPhase::Queued(installing) => {
                    *phase = HoldPhase::Running {
                        _installing: installing,
                    };
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

    /// The job is done: release the lock.
    fn finish(&self) {
        let released = std::mem::replace(&mut *self.phase(), HoldPhase::Over);
        drop(released);
    }

    /// The task awaiting the job no longer wants it. Releases the lock if
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

/// Held by the task awaiting a queued job: if that task stops before the
/// actor has started the job, the job's lock is released right then.
pub struct QueuedJob(JobHold);

impl Drop for QueuedJob {
    fn drop(&mut self) {
        self.0.abandon_if_queued();
    }
}

/// The sessions whose credentials are owed a sync because their actor refused
/// a job while its connection was leased or reconnecting. The actor is the one
/// owner of that fact: when its connection is usable again it wakes the
/// coordinator, which reconciles the session from the host's current login.
/// The wake carries no job, so a long lease cannot hold a profile's pass open.
#[derive(Debug)]
pub struct CredentialWakes(watch::Sender<BTreeSet<String>>);

impl Default for CredentialWakes {
    fn default() -> Self {
        Self(watch::channel(BTreeSet::new()).0)
    }
}

impl CredentialWakes {
    pub fn wake(&self, session_id: &str) {
        self.0
            .send_if_modified(|sessions| sessions.insert(session_id.to_owned()));
    }

    pub fn subscribe(&self) -> watch::Receiver<BTreeSet<String>> {
        let mut wakes = self.0.subscribe();
        wakes.mark_changed();
        wakes
    }

    pub fn take(&self) -> BTreeSet<String> {
        let mut ready = BTreeSet::new();
        self.0.send_if_modified(|sessions| {
            ready = std::mem::take(sessions);
            false
        });
        ready
    }
}

/// One holder at a time per session, for a kind of step that must not overlap
/// itself (a session's delivery results, its AWS renewals). A step
/// waits only for the same session's other step, never another session's. The
/// lock is released on every exit path.
///
/// The map is keyed by session id and only that session's entry is touched by
/// `enter` and by a release, so a batch of sessions costs one lookup each.
#[derive(Clone, Default)]
pub struct SessionSteps(Arc<std::sync::Mutex<SessionLocks>>);

type SessionLocks = BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>;

/// A session's turn. Dropping it releases the session's lock and, when nobody
/// else holds or waits for that lock, forgets the session's entry.
pub struct SessionTurn {
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
    pub fn tracked(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub async fn enter(&self, session_id: &str) -> SessionTurn {
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
pub struct SessionAccess<'a> {
    pub relays: &'a SessionRelays,
    pub runtime: Option<&'a Arc<dyn CredentialSyncRuntime>>,
}

pub enum PassEvent {
    Pulled(String),
    Finished(CredentialSyncResult),
}

pub struct CredentialSyncCoordinator {
    pub handle: CredentialSyncHandle,
    pub results: mpsc::UnboundedReceiver<CredentialSyncResult>,
}

impl CredentialSyncCoordinator {
    #[doc(hidden)]
    pub fn spawn() -> Self {
        Self::spawn_inner(
            SessionRelays::Direct,
            None,
            unknown_slopcop_login,
            SYNC_INTERVAL,
        )
    }

    /// A coordinator that retries a session's AWS renewal after
    /// `retry` instead of after the sync interval.
    #[cfg(test)]
    pub fn spawn_with_renewal_retry(retry: Duration) -> Self {
        Self::spawn_inner(SessionRelays::Direct, None, unknown_slopcop_login, retry)
    }

    pub fn spawn_guarded(
        manager: SessionManagerControl,
        runtime: Arc<dyn CredentialSyncRuntime>,
        rejected: DeliveryRegistry,
    ) -> Self {
        Self::spawn_with_registry(
            SessionRelays::Actors(manager),
            Some(runtime),
            mj_core::slopcop::host_login,
            SYNC_INTERVAL,
            rejected,
        )
    }

    fn spawn_inner(
        relays: SessionRelays,
        runtime: Option<Arc<dyn CredentialSyncRuntime>>,
        slopcop_login: SlopcopLoginSource,
        renewal_retry: Duration,
    ) -> Self {
        Self::spawn_with_registry(
            relays,
            runtime,
            slopcop_login,
            renewal_retry,
            Default::default(),
        )
    }

    pub fn spawn_with_registry(
        relays: SessionRelays,
        runtime: Option<Arc<dyn CredentialSyncRuntime>>,
        slopcop_login: SlopcopLoginSource,
        renewal_retry: Duration,
        rejected: DeliveryRegistry,
    ) -> Self {
        let targets_tx = runtime
            .as_ref()
            .map(|runtime| runtime.credential_targets())
            .unwrap_or_else(|| Arc::new(watch::channel(Vec::new()).0));
        let mut targets_rx = targets_tx.subscribe();
        let handle_targets = targets_tx.clone();
        let (triggers_tx, mut triggers_rx) = mpsc::unbounded_channel::<SyncTrigger>();
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<PassEvent>();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        mj_core::runtime::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + SYNC_INTERVAL,
                SYNC_INTERVAL,
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut profiles = BTreeMap::<String, Arc<ProfileLogin>>::new();
            let mut queue = VecDeque::<SyncTrigger>::new();
            let mut renewal_logs = BTreeSet::new();
            let mut scoped = BTreeMap::<String, BTreeSet<String>>::new();
            // Role-scoped AWS credentials are renewed by a task per session,
            // never by the profile's pass: a pass that waits on one stalled
            // session must not hold back another session's renewal.
            let renewals_env = RenewalEnv {
                relays: relays.clone(),
                runtime: runtime.clone(),
                installs: SessionSteps::default(),
                results: results_tx.clone(),
                retry: renewal_retry,
            };
            let mut renewals = RenewalTasks::default();
            let mut previous_targets = BTreeMap::<String, CredentialSyncTarget>::new();
            // Sessions whose actor refused a job while leased or reconnecting
            // say so here when they can take one again.
            let wakes = match &relays {
                SessionRelays::Actors(manager) => Some(manager.credential_wakes.clone()),
                SessionRelays::Direct => None,
            };
            let mut owed = wakes.as_ref().map(|wakes| wakes.subscribe());
            loop {
                tokio::select! {
                    () = async {
                        match owed.as_mut() {
                            Some(owed) => { let _ = owed.changed().await; }
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Some(wakes) = &wakes {
                            let sessions = wakes.take();
                            for target in targets_rx.borrow().iter()
                                .filter(|target| sessions.contains(&target.session_id))
                            {
                                queue_session(&mut queue, &mut scoped, target);
                            }
                        }
                    }
                    _ = tick.tick() => {
                        // Whether or not any session is left to sync, the
                        // cache of issued credentials drops what has expired.
                        crate::aws_assume_role::AwsCredentialCache::shared().sweep_expired();
                        renewals.sync(&targets_rx.borrow(), &renewals_env);
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
                            scoped.remove(&profile_id);
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    changed = targets_rx.changed() => {
                        if changed.is_err() { break; }
                        // Compare the publication we consume, not its order.
                        // Removed sessions have nothing left to reconcile.
                        // Copy out of the borrow: the loop below takes the
                        // runtime owner lock, and the owner publishes to this
                        // channel while holding it, so a held borrow deadlocks.
                        let targets = targets_rx.borrow_and_update().clone();
                        renewals.sync(&targets, &renewals_env);
                        let next: BTreeMap<_, _> = targets.iter()
                            .map(|target| (target.session_id.clone(), target.clone()))
                            .collect();
                        for (id, old) in &previous_targets {
                            if next.get(id) == Some(old) { continue; }
                            let conclusion = if next.contains_key(id) {
                                Some("credential target changed")
                            } else if runtime.as_ref().is_none_or(|runtime| runtime.credential_session_ended(id)) {
                                Some("session ended")
                            } else {
                                None
                            };
                            if let Some(reason) = conclusion {
                                rejected.lock().expect("rejected logins poisoned")
                                    .end_delivery(old, None, reason.into());
                            }
                        }
                        for target in targets.iter()
                            .filter(|target| previous_targets.get(&target.session_id) != Some(target))
                        {
                            queue_session(&mut queue, &mut scoped, target);
                        }
                        previous_targets = next;
                        if let Some(runtime) = &runtime {
                            let owner = runtime.credential_session_ids();
                            rejected.lock().expect("rejected logins poisoned")
                                .retain_deliveries(|id| owner.contains_key(id));
                        } else {
                            rejected.lock().expect("rejected logins poisoned")
                                .retain_deliveries(|id| previous_targets.contains_key(id));
                        }
                        // Keep the same profile owner across a temporary loss of
                        // its last target while an earlier pass can still adopt.
                        profiles.retain(|profile_id, profile| Arc::strong_count(profile) > 1
                            || targets.iter().any(|target| target.profile_id == *profile_id));
                    }
                    trigger = triggers_rx.recv() => {
                        let Some(trigger) = trigger else { break };
                        // Explicit requests without a cause come from quota/login renewal.
                        if trigger.cause.is_none() {
                            scoped.remove(&trigger.profile_id);
                            renewal_logs.insert(trigger.profile_id.clone());
                        }
                        enqueue(&mut queue, trigger);
                    }
                    completed = completed_rx.recv() => {
                        let Some(event) = completed else { break };
                        match event {
                            PassEvent::Pulled(profile_id) => {
                                scoped.remove(&profile_id);
                                renewal_logs.insert(profile_id.clone());
                                enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
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

                while let Some(trigger) = queue.pop_front() {
                    let sessions = if trigger.cause.is_none() {
                        scoped.remove(&trigger.profile_id)
                    } else {
                        None
                    };
                    let targets: Vec<_> = targets_rx
                        .borrow()
                        .iter()
                        .filter(|target| {
                            target.profile_id == trigger.profile_id
                                && sessions
                                    .as_ref()
                                    .is_none_or(|ids| ids.contains(&target.session_id))
                        })
                        .cloned()
                        .collect();
                    let full_fanout = sessions.is_none();
                    let mut log = SyncLog::new(
                        trigger.cause.as_ref(),
                        renewal_logs.remove(&trigger.profile_id),
                    );
                    log.targeted = !full_fanout;
                    if targets.is_empty() {
                        log_missing_targets(runtime.as_ref(), &trigger.profile_id, &targets, log);
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
                    let profile = profiles
                        .entry(trigger.profile_id.clone())
                        .or_insert_with(|| {
                            Arc::new(ProfileLogin::new(
                                Some(completed_tx.clone()),
                                rejected.clone(),
                            ))
                        })
                        .clone();
                    let completed_tx = completed_tx.clone();
                    // The join is awaited so a panicked reconcile is reported.
                    // The reconcile
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
                    let relays = relays.clone();
                    let runtime = runtime.clone();
                    let log = log.into_owned();
                    mj_core::runtime::spawn(async move {
                        let (focus_tx, focus_rx) = if triggered_by.is_some() {
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            (Some(tx), Some(rx))
                        } else {
                            (None, None)
                        };
                        let reconcile = mj_core::runtime::spawn(async move {
                            reconcile_profile_observed(
                                &SessionAccess {
                                    relays: &relays,
                                    runtime: runtime.as_ref(),
                                },
                                &targets,
                                triggered_by.as_deref(),
                                slopcop_login,
                                log.as_ref(),
                                &profile,
                                focus_tx,
                            )
                            .await
                        });
                        let mut focus = trigger.cause;
                        // The authentication verdict only awaits its session
                        // and that session's AWS outcome. Peers report routine
                        // successes or failures when their own pass finishes.
                        if let Some(answer) = focus_rx
                            && let Ok(outcome) = answer.await
                        {
                            let mut outcomes = outcome.into_iter().collect();
                            if let Some(report) = aws_report {
                                let report = report.await.unwrap_or_else(|_| {
                                    unknown_renewal_outcome(
                                        focus.as_ref().map_or("", |cause| &cause.session_id),
                                    )
                                });
                                merge_report(&mut outcomes, report);
                            }
                            finish_pass(
                                &completed_tx,
                                CredentialSyncResult {
                                    profile_id: trigger.profile_id.clone(),
                                    trigger: focus.take(),
                                    failure: None,
                                    outcomes,
                                    partial: false,
                                },
                            );
                        }
                        let (failure, outcomes) = match reconcile.await {
                            Ok(outcomes) => (None, outcomes),
                            Err(error) => (Some(format!("sync task stopped: {error}")), Vec::new()),
                        };
                        finish_pass(
                            &completed_tx,
                            CredentialSyncResult {
                                profile_id: trigger.profile_id,
                                trigger: focus,
                                failure,
                                outcomes,
                                partial: false,
                            },
                        );
                    });
                }
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
/// cycle for each session with managed AWS credentials.
#[cfg(all(test, unix))]
pub async fn reconcile_profile(
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
) -> Vec<CredentialSyncOutcome> {
    let access = SessionAccess {
        relays: &SessionRelays::Direct,
        runtime: None,
    };
    let login = reconcile_profile_guarded(&access, targets, triggered_by, unknown_slopcop_login);
    let cycles = futures::future::join_all(
        targets
            .iter()
            .filter(|target| target.aws_credentials.is_some())
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
/// AWS credentials are handled independently of this pass: each
/// session has a renewal task of its own (see `RenewalTasks`), so a session
/// that does not answer stalls this pass and its own renewal, and nothing else.
///
/// A session that already agreed is left out of the outcomes, except
/// `triggered_by`, the session whose failure asked for this sync: that it was
/// reached and had nothing to change is what shows the profile's own login
/// is the one the provider refused.
fn finish_pass(completed: &mpsc::UnboundedSender<PassEvent>, result: CredentialSyncResult) {
    let profile_id = result.profile_id.clone();
    if completed.send(PassEvent::Finished(result)).is_err() {
        tracing::debug!(
            %profile_id,
            "credential sync coordinator stopped before receiving completion"
        );
    }
}

// A deferral or change wakes only the session that became available.
fn queue_session(
    queue: &mut VecDeque<SyncTrigger>,
    scoped: &mut BTreeMap<String, BTreeSet<String>>,
    target: &CredentialSyncTarget,
) {
    if queue
        .iter()
        .any(|request| request.profile_id == target.profile_id && request.cause.is_none())
        && !scoped.contains_key(&target.profile_id)
    {
        return; // An already-queued profile-wide pass covers this session.
    }
    scoped
        .entry(target.profile_id.clone())
        .or_default()
        .insert(target.session_id.clone());
    if !queue
        .iter()
        .any(|request| request.profile_id == target.profile_id && request.cause.is_none())
    {
        queue.push_back(SyncTrigger {
            profile_id: target.profile_id.clone(),
            cause: None,
        });
    }
}

#[cfg(test)]
pub async fn reconcile_profile_guarded(
    access: &SessionAccess<'_>,
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
    slopcop_login: SlopcopLoginSource,
) -> Vec<CredentialSyncOutcome> {
    reconcile_profile_observed(
        access,
        targets,
        triggered_by,
        slopcop_login,
        SyncLog::default(),
        &Arc::new(ProfileLogin::new(None, Default::default())),
        None,
    )
    .await
}

#[derive(Clone, Copy, Default)]
struct SyncLog<'a> {
    cause: Option<&'a mj_core::credentials::CredentialSyncCause>,
    fanout: bool,
    targeted: bool,
}

impl<'a> SyncLog<'a> {
    fn new(cause: Option<&'a mj_core::credentials::CredentialSyncCause>, fanout: bool) -> Self {
        Self {
            cause: cause.filter(|cause| {
                cause.reason == mj_core::credentials::CredentialSyncReason::AuthenticationFailure
            }),
            fanout,
            targeted: false,
        }
    }

    fn into_owned(self) -> OwnedSyncLog {
        OwnedSyncLog {
            cause: self.cause.cloned(),
            fanout: self.fanout,
            targeted: self.targeted,
        }
    }

    fn record(self, profile_id: &str, session_id: &str, reason: &str) {
        tracing::info!(
            %profile_id,
            %session_id,
            cause_ordinal = self.cause.map(|cause| cause.ordinal),
            reason,
            "credential sync session disposition"
        );
    }
}

struct OwnedSyncLog {
    cause: Option<mj_core::credentials::CredentialSyncCause>,
    fanout: bool,
    targeted: bool,
}

impl OwnedSyncLog {
    fn as_ref(&self) -> SyncLog<'_> {
        SyncLog {
            cause: self.cause.as_ref(),
            fanout: self.fanout,
            targeted: self.targeted,
        }
    }
}

fn log_missing_targets(
    runtime: Option<&Arc<dyn CredentialSyncRuntime>>,
    profile_id: &str,
    targets: &[CredentialSyncTarget],
    log: SyncLog<'_>,
) {
    let present = |id: &str| targets.iter().any(|target| target.session_id == id);
    if let Some(runtime) = runtime {
        let skipped = runtime.credential_sync_skips(
            profile_id,
            log.fanout && !log.targeted,
            log.cause.map(|cause| cause.session_id.as_str()),
            &present,
        );
        for (session_id, reason) in skipped {
            log.record(profile_id, &session_id, reason);
        }
    } else if let Some(cause) = log.cause
        && !present(&cause.session_id)
    {
        log.record(
            profile_id,
            &cause.session_id,
            "not an eligible target: no published credential sync target",
        );
    }
}

async fn reconcile_profile_observed(
    access: &SessionAccess<'_>,
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
    slopcop_login: SlopcopLoginSource,
    mut log: SyncLog<'_>,
    profile: &Arc<ProfileLogin>,
    mut focus: Option<tokio::sync::oneshot::Sender<Option<CredentialSyncOutcome>>>,
) -> Vec<CredentialSyncOutcome> {
    let SessionAccess { relays, runtime } = *access;
    let Some(first) = targets.first().cloned() else {
        return Vec::new();
    };
    // The reading this pass took, which an authentication failure's session
    // supplies the lock evidence for the failed session's verdict (see
    // `SessionCanonical::pinned`).
    let mut own_login = None;
    if let Some(target) = targets.iter().find(|target| !target.skips_login_file_sync) {
        let (changed, published) = profile.refresh(runtime, target, log.cause.is_some()).await;
        own_login = published.filter(|_| log.cause.is_some());
        if changed && log.targeted {
            profile.send(PassEvent::Pulled(target.profile_id.clone()));
        }
    }
    for target in targets
        .iter()
        .filter(|target| !target.skips_login_file_sync)
    {
        profile.begin_delivery(runtime, target);
    }
    // Read once per pass, and only when a session needs a copy: a raw
    // localhost session reads the host's own login.
    let slopcop_login = if targets.iter().any(syncs_slopcop_login) {
        match mj_core::runtime::spawn_blocking(slopcop_login)
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
    // Targets share a profile but can differ in host CLI access. Collect off
    // scheduler threads once per wire format, then derive both scoped trees.
    let skills_target = first.clone();
    let skills = Arc::new(
        mj_core::runtime::spawn_blocking(move || CanonicalSkills::collect(&skills_target))
            .await
            .unwrap_or_else(|error| {
                CanonicalSkills::failed(&format!("skills collection task stopped: {error}"))
            }),
    );
    if let Some(session_id) = triggered_by
        && !targets.iter().any(|target| target.session_id == session_id)
        && let Some(focus) = focus.take()
    {
        let _ = focus.send(None);
    }
    let login = PassLogin::Shared(profile.clone());
    let pass_context = PassContext {
        relays,
        runtime,
        skills: &skills,
        slopcop_login: &slopcop_login,
        login: &login,
        profile,
        triggered_by,
        own_login: own_login.as_ref(),
    };
    let login_pass = async {
        let mut outcomes = BTreeMap::<String, CredentialSyncOutcome>::new();
        let mut skipped = Vec::new();
        for pass in 0..2 {
            let mut pulled = false;
            let mut reconciles: futures::stream::FuturesUnordered<_> = targets
                .iter()
                .map(|target| {
                    let reported = outcomes.contains_key(&target.session_id);
                    let pass_context = &pass_context;
                    async move {
                        (
                            target,
                            Box::pin(reconcile_target(pass_context, target, pass, reported)).await,
                        )
                    }
                })
                .collect();
            while let Some((target, result)) = reconciles.next().await {
                pulled |= result.pulled;
                if log.cause.is_some() || (log.fanout && result.login_skipped) {
                    log.record(&target.profile_id, &target.session_id, &result.reason);
                } else if result.login_skipped {
                    skipped.push((target.session_id.clone(), result.reason));
                }
                if triggered_by == Some(target.session_id.as_str())
                    && let Some(focus) = focus.take()
                {
                    let _ = focus.send(result.outcome);
                    continue;
                }
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
            log.fanout |= pulled
                || outcomes.values().any(|outcome| {
                    outcome
                        .outcome
                        .as_ref()
                        .is_ok_and(|actions| actions.contains(&CredentialSyncAction::Pushed))
                });
            if log.fanout && log.cause.is_none() {
                for (session_id, reason) in skipped.drain(..) {
                    log.record(&first.profile_id, &session_id, &reason);
                }
            }
            if !pulled || pass == 1 || profile.events.is_some() {
                break;
            }
        }
        outcomes
    };
    let outcomes = login_pass.await;
    log_missing_targets(runtime, &first.profile_id, targets, log);
    outcomes.into_values().collect()
}

/// Fold one more step's result into a session's outcome. A session whose
/// whole sync failed keeps that failure (as `SyncStep::Session`, which reports
/// without a step), so the other step's actions are recorded beside it.
pub fn merge_step_outcome(
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
            unchanged_login: None,
            current_access_token: None,
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
        unchanged_login: None,
        current_access_token: None,
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
        // The profile's pass checked the login, skills, and tokens.
        outcome
            .step_failures
            .retain(|failure| failure.step == SyncStep::Aws);
    }
}

/// What a sync pass does about one session's AWS credentials.
#[derive(Clone)]
pub enum AwsSync {
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

/// The controller's GitHub token for a session, or why it could not be read.
#[derive(Clone)]
pub enum GithubSync {
    Known(Option<String>),
    /// The lookup failed. The session's copy stays as it is, because an
    /// unreadable token is not an absent one.
    Unavailable(String),
}

/// What reconciling one session did: the actions its steps took, and the
/// failure of each step that failed. Every step runs whether or not another
/// failed.
pub struct SessionSync {
    pub unchanged_login: Option<CredentialSnapshot>,
    /// Access-token hash of the profile's login as this sync read it.
    pub current_access_token: Option<String>,
    pub actions: Vec<CredentialSyncAction>,
    pub failures: Vec<StepFailure>,
    /// The login install response arrives before the daemon can publish the
    /// worker's operational observation. New workers acknowledge delivery
    /// through that observation so a child cannot be held on a stale view.
    reports_held_login: bool,
}

/// The role, role session and session policy credentials were issued for.
#[derive(Clone)]
pub struct ScopeExpectation {
    pub role_arn: String,
    pub role_session_name: String,
    /// Fingerprint of the effective session policy (inline and managed): a
    /// narrower policy than the one the credentials were issued under makes
    /// them broader than configured, so they do not match.
    pub session_policy_fingerprint: String,
    /// The `SourceIdentity` the credentials must have been issued with: the
    /// role session name when `source_identity` is on, nothing when it is off.
    pub source_identity: Option<String>,
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
    runtime: Option<&'a Arc<dyn CredentialSyncRuntime>>,
    skills: &'a Arc<CanonicalSkills>,
    slopcop_login: &'a HostSlopcopLogin,
    login: &'a PassLogin,
    profile: &'a Arc<ProfileLogin>,
    triggered_by: Option<&'a str>,
    /// The login this pass read, set only for an authentication-triggered pass.
    own_login: Option<&'a Arc<CanonicalLogin>>,
}

/// What reconciling one session in one pass found.
struct TargetResult {
    reason: String,
    login_skipped: bool,
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

// Without a daemon to say which sessions are current, a test run has every
// session current on its thread, unless it sets this to false (then the
// session records on disk decide).
thread_local! {
    pub(super) static SESSIONS_ARE_CURRENT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Whether the session is still the one this target describes. A lifecycle
/// may have completed since the dashboard published its target list.
async fn target_is_current(
    runtime: Option<&Arc<dyn CredentialSyncRuntime>>,
    target: &CredentialSyncTarget,
) -> Result<bool> {
    if let Some(runtime) = runtime {
        return runtime.credential_target_is_current(target);
    }
    if (cfg!(test) || mj_core::test_hooks::test_fixtures_enabled())
        && SESSIONS_ARE_CURRENT.with(std::cell::Cell::get)
    {
        return Ok(true);
    }
    let candidate = target.clone();
    mj_core::runtime::spawn_blocking(move || {
        // Read only: cancellation may leave this blocking read finishing
        // after admission has been released.
        let config = mj_core::config::Config::load()?;
        let state = crate::database::load_state()?;
        Ok::<_, anyhow::Error>(super::credential_sync_target_is_current(
            &config, state, &candidate,
        ))
    })
    .await
    .context("reload credential sync target")?
}

/// One session's reconciliation in one pass. `reported` says an earlier pass
/// already recorded an outcome for it.
async fn reconcile_target(
    context: &PassContext<'_>,
    target: &CredentialSyncTarget,
    pass: usize,
    reported: bool,
) -> TargetResult {
    let PassContext {
        relays,
        runtime,
        skills,
        slopcop_login,
        login,
        profile,
        triggered_by,
        own_login,
    } = context;
    // Keep the delivery result ordered with the next attempt on this session;
    // the actor itself releases the AWS/relay turn before handing us its result.
    let _delivery = profile.deliveries.enter(&target.session_id).await;
    let requested = profile.fingerprint(target);
    let result = async {
        // A failed GitHub token lookup is that step's failure, not the
        // session's: skills and logins are independent of it.
        let github = if target.sync_github_token {
            match crate::github_app::github_token_for_session(
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
            HostLogins {
                slopcop: slopcop_login,
                harness: login,
                // The session whose failure asked for this pass is delivered,
                // and judged against, the reading the pass took.
                pinned: own_login
                    .filter(|_| *triggered_by == Some(target.session_id.as_str()))
                    .cloned(),
            },
            JobHold::new(None, runtime.cloned()),
        )
        .await
    }
    .await;
    let deferred = matches!(&result, Err(error) if RelayJobDeferred::marks(error));
    let awaits_worker_observation = runtime.is_some()
        && matches!(&result, Ok(Some(sync)) if sync.reports_held_login
            && (sync.unchanged_login.is_some() || sync.actions.contains(&CredentialSyncAction::Pushed))
            && !sync.failures.iter().any(|failure| failure.step == SyncStep::HarnessLogin));
    if !deferred
        && !awaits_worker_observation
        && !target.skips_login_file_sync
        && let Some(requested) = requested.as_deref()
        && profile.fingerprint(target).as_deref() == Some(requested)
    {
        let reason = match &result {
            Ok(Some(sync)) => login_disposition(
                target,
                sync.unchanged_login.as_ref(),
                &sync.actions,
                &sync.failures,
            ),
            Err(error) => format!("{error:#}"),
            Ok(None) => "credential sync did not reach the session".into(),
        };
        profile
            .rejected
            .lock()
            .expect("rejected logins poisoned")
            .end_delivery(target, Some(requested), reason);
    }
    match result {
        // Deferral is not a successful credential check: in particular
        // it must not mark an authentication-triggered login refused.
        Ok(None) => unreachable!("a deferred login sync carries a reason"),
        Err(error) if RelayJobDeferred::marks(&error) => TargetResult {
            reason: format!("{error:#}"),
            login_skipped: true,
            outcome: None,
            pulled: false,
            clean: false,
        },
        Ok(Some(SessionSync {
            actions,
            failures,
            unchanged_login,
            ..
        })) if actions.is_empty()
            && failures.is_empty()
            && (*triggered_by != Some(target.session_id.as_str()) || reported) =>
        {
            TargetResult {
                reason: login_disposition(target, unchanged_login.as_ref(), &actions, &failures),
                login_skipped: true,
                outcome: None,
                pulled: false,
                clean: true,
            }
        }
        Ok(Some(SessionSync {
            actions,
            failures,
            unchanged_login,
            current_access_token,
            ..
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
                reason: login_disposition(target, unchanged_login.as_ref(), &actions, &failures),
                login_skipped: !actions.iter().any(|action| {
                    matches!(
                        action,
                        CredentialSyncAction::Pushed | CredentialSyncAction::Pulled
                    )
                }),
                pulled: actions.contains(&CredentialSyncAction::Pulled),
                clean: failures.is_empty(),
                outcome: Some(CredentialSyncOutcome {
                    unchanged_login,
                    current_access_token,
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
                reason: format!("{error:#}"),
                login_skipped: true,
                pulled: false,
                clean: false,
                outcome: Some(CredentialSyncOutcome {
                    unchanged_login: None,
                    current_access_token: None,
                    session_id: target.session_id.clone(),
                    outcome: Err(format!("{error:#}")),
                    step_failures: Vec::new(),
                }),
            }
        }
    }
}

fn login_disposition(
    target: &CredentialSyncTarget,
    unchanged_login: Option<&CredentialSnapshot>,
    actions: &[CredentialSyncAction],
    failures: &[StepFailure],
) -> String {
    if let Some(failure) = failures
        .iter()
        .find(|failure| failure.step == SyncStep::HarnessLogin)
    {
        format!("login sync failed: {}", failure.detail)
    } else if actions.contains(&CredentialSyncAction::Pulled) {
        "pulled the session's fresher login".into()
    } else if actions.contains(&CredentialSyncAction::Pushed) {
        "pushed the current profile login".into()
    } else if target.skips_login_file_sync {
        "not an eligible login target: shares the profile login or uses another authentication scheme".into()
    } else if unchanged_login.is_some() {
        "session already holds the current login".into()
    } else {
        "no eligible credential transfer for the reported copies".into()
    }
}

/// One session's AWS cycle: mint or export (or take from the cache), then
/// install on the session's own connection, or withdraw what must not stay.
/// What distinguishes one AWS cycle from another.
pub struct AwsCycleOptions<'a> {
    /// The renewal's install lock, held by the install job.
    pub installing: Option<SessionTurn>,
    /// Drop the cached credentials first: the session's were perhaps refused.
    pub force_new: bool,
    /// Where to report, before the cycle ends, that credentials were withdrawn
    /// while the replacement is pending.
    pub interim: Option<&'a mpsc::UnboundedSender<CredentialSyncResult>>,
    pub static_refresh_interval: Duration,
}

impl Default for AwsCycleOptions<'_> {
    fn default() -> Self {
        Self {
            installing: None,
            force_new: false,
            interim: None,
            static_refresh_interval: SYNC_INTERVAL,
        }
    }
}

pub async fn aws_cycle(
    access: &SessionAccess<'_>,
    target: &CredentialSyncTarget,
    options: AwsCycleOptions<'_>,
) -> (String, Result<Option<SessionSync>>) {
    let AwsCycleOptions {
        installing,
        force_new,
        interim,
        static_refresh_interval,
    } = options;
    let session_id = target.session_id.clone();
    // A session that is gone, or replaced, is not given credentials. This is
    // checked before the mint: an AssumeRole call can take seconds.
    match target_is_current(access.runtime, target).await {
        Ok(true) => {}
        Ok(false) => return (session_id, Ok(None)),
        Err(error) => return (session_id, Err(error)),
    }
    let Some(source) = target.aws_credentials.as_ref() else {
        return (session_id, Ok(None));
    };
    let mut withdrawn = Vec::new();
    if source.role.is_some() {
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
        if !source.mints {
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
                            unchanged_login: None,
                            current_access_token: None,
                            session_id: session_id.clone(),
                            outcome: Ok(held.actions.clone()),
                            step_failures: failures,
                        }],
                    });
                }
                None => withdrawn = held.actions.clone(),
            }
        }
    }
    let (mint, _stops_with_the_task) =
        start_aws_mint(target, source, force_new, static_refresh_interval);
    let aws = match mint
        .await
        .context("AWS credential task stopped")
        .and_then(|result| result)
    {
        Ok(credentials) => AwsSync::Install(credentials),
        Err(error) => AwsSync::Unavailable {
            error: format!("{error:#}"),
            scoped_required: source.role.is_some(),
            expected_scope: ScopeExpectation::of(target),
        },
    };
    let mut result = aws_job(access, target, installing, SyncWork::Aws(Box::new(aws))).await;
    if let Ok(Some(sync)) = &mut result {
        sync.actions.splice(0..0, withdrawn);
    }
    (session_id, result)
}

/// One AWS job on the session.
async fn aws_job(
    access: &SessionAccess<'_>,
    target: &CredentialSyncTarget,
    installing: Option<SessionTurn>,
    work: SyncWork,
) -> Result<Option<SessionSync>> {
    let hold = JobHold::new(installing, access.runtime.cloned());
    match run_on_session(access.relays, target, work, hold).await {
        Err(error) if RelayJobDeferred::marks(&error) => Ok(None),
        result => result,
    }
}

/// What a session's renewal task needs, shared by all of them.
#[derive(Clone)]
pub struct RenewalEnv {
    pub relays: SessionRelays,
    pub runtime: Option<Arc<dyn CredentialSyncRuntime>>,
    /// Orders one session's renewals, so a replaced task's cycle finishes
    /// before its successor's begins.
    pub installs: SessionSteps,
    pub results: mpsc::UnboundedSender<CredentialSyncResult>,
    /// How long after a cycle that did not complete cleanly the next is tried.
    pub retry: Duration,
}

impl RenewalEnv {
    pub fn access(&self) -> SessionAccess<'_> {
        SessionAccess {
            relays: &self.relays,
            runtime: self.runtime.as_ref(),
        }
    }
}

/// A request to renew a session's credentials now, with new ones, and to
/// answer with the cycle's outcome.
pub struct RenewalWake {
    pub reply: tokio::sync::oneshot::Sender<CredentialSyncOutcome>,
}

struct RenewalTask {
    target: CredentialSyncTarget,
    handle: tokio::task::JoinHandle<()>,
    wakes: mpsc::UnboundedSender<RenewalWake>,
}

/// One renewal task per session with managed AWS credentials, started as the
/// published targets change. A session's AWS credentials are renewed on their
/// own schedule, at half their lifetime, by a task that is not part of any
/// profile pass: a stalled session holds its own renewal and nothing else.
#[derive(Default)]
pub struct RenewalTasks(BTreeMap<String, RenewalTask>);

impl RenewalTasks {
    /// A task standing in for the session's renewal, as one that had been
    /// started for `target`.
    #[cfg(all(test, unix))]
    pub fn with_task(
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
    #[cfg(all(test, unix))]
    pub fn is_running(&self, session_id: &str) -> bool {
        self.0
            .get(session_id)
            .is_some_and(|task| !task.handle.is_finished())
    }

    pub fn sync(&mut self, targets: &[CredentialSyncTarget], env: &RenewalEnv) {
        let wanted = targets
            .iter()
            .filter(|target| target.aws_credentials.is_some())
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
                    unchanged_login: None,
                    current_access_token: None,
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
                let handle = mj_core::runtime::spawn(renew_session_aws(
                    env.clone(),
                    target.clone(),
                    wake_rx,
                ));
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
    pub fn wake(
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
pub async fn renew_session_aws(
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
    let source = target.aws_credentials.as_ref();
    let retry_limit = source
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
                static_refresh_interval: env.retry,
            },
        )
        .await;
        let completed = matches!(&result, Ok(Some(sync)) if sync.failures.is_empty());
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
                source.and_then(|source| {
                    crate::aws_assume_role::AwsCredentialCache::shared()
                        .remaining_validity(&target.session_id, source)
                }),
            );
        }
        let mut outcomes = BTreeMap::new();
        merge_aws_cycle(&mut outcomes, &target.session_id, result);
        let mut report = outcomes.remove(&target.session_id).or_else(|| {
            checked.then(|| CredentialSyncOutcome {
                unchanged_login: None,
                current_access_token: None,
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
            && let Some(source) = source
            && let Some(until) = crate::aws_assume_role::AwsCredentialCache::shared()
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

/// How long to wait before the next attempt after the `failures`-th
/// consecutive failure (counting from zero): the backoff, and while the
/// current credentials are still valid no more than half the time they have
/// left (but never less than `base`). Attempts then come closer as the expiry
/// nears, so the last one lands before it; the limit is the expiry itself.
/// Once the credentials have expired (`remaining` is `None`) the backoff
/// stands: an authentication failure, which wakes the renewal, is how a session
/// that has recovered renews promptly.
pub fn retry_delay(
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

/// The mint is owned by the task that awaits it: the returned guard, dropped
/// with that task, terminates the AWS CLI's process group through the shared
/// executor's cancellation, and only then does the cache entry's lock (held
/// across the subprocess) come free. A superseded task's stuck helper therefore
/// cannot keep its replacement waiting.
fn start_aws_mint(
    target: &CredentialSyncTarget,
    source: &mj_core::aws_credentials::AwsCredentialSource,
    force_new: bool,
    static_refresh_interval: Duration,
) -> (
    tokio::task::JoinHandle<Result<mj_core::aws_credentials::AwsCredentials>>,
    mj_core::targets::ProcessCancellationGuard,
) {
    let source = source.clone();
    let session_id = target.session_id.clone();
    let executor = crate::targets::CancellableProcessExecutor::new(Arc::default());
    let stops_with_the_task = executor.cancel_on_drop();
    let mint = mj_core::runtime::spawn_blocking(move || {
        let cache = crate::aws_assume_role::AwsCredentialCache::shared();
        if force_new {
            cache.forget(&session_id, &source);
        }
        cache.credentials_for(&session_id, &source, &executor, static_refresh_interval)
    });
    (mint, stops_with_the_task)
}

/// The skills tree this session should converge to: the profile's own skills
/// plus Mjolnir's managed skills, exactly as launch staging writes them into
/// the session's staged home, collected for the archive format the session's
/// worker reads.
pub fn canonical_session_skills(
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
pub struct CanonicalSkills {
    plain: [std::result::Result<mj_core::skills::SkillsArchive, String>; 2],
    gzip: [std::result::Result<mj_core::skills::SkillsArchive, String>; 2],
}

impl CanonicalSkills {
    pub fn collect(target: &CredentialSyncTarget) -> Self {
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
#[derive(Clone)]
pub struct CanonicalLogin {
    profile_home: std::path::PathBuf,
    harness: mj_core::config::HarnessKind,
    /// How the controller reads and writes it.
    pub access: LoginAccess,
    pub credential: CredentialSnapshot,
    pub bytes: Vec<u8>,
    /// Read while holding the native login lock, so no renewal was in
    /// progress: only such a reading can say whether a rejected token was the
    /// profile's current one.
    read_under_lock: bool,
}

/// A login as read from where its store keeps it.
#[cfg(all(unix, test))]
type LoginRead = Result<(CredentialSnapshot, Vec<u8>), String>;

/// How the controller reads and writes a profile's login: its store
/// ([`mj_core::config::HarnessKind::login_store`]) and the bounded `security`
/// tool a macOS login needs.
#[derive(Clone)]
pub struct LoginAccess {
    store: mj_core::config::HarnessLoginStore,
    security: Arc<dyn crate::targets::CommandExecutor + Send + Sync>,
    /// The credential file whose directory carries the locks a Keychain write
    /// takes, when not Claude Code's own `~/.claude` (tests).
    lock_marker: Option<std::path::PathBuf>,
}

impl LoginAccess {
    pub fn new(
        store: mj_core::config::HarnessLoginStore,
        security: Arc<dyn crate::targets::CommandExecutor + Send + Sync>,
    ) -> Self {
        Self {
            store,
            security,
            lock_marker: None,
        }
    }

    #[cfg(all(test, unix))]
    pub fn with_lock_marker(mut self, marker: std::path::PathBuf) -> Self {
        self.lock_marker = Some(marker);
        self
    }

    /// The daemon's `security` lookup, bounded because it can wait on a
    /// Keychain prompt.
    fn bounded(store: mj_core::config::HarnessLoginStore) -> Self {
        Self::new(
            store,
            Arc::new(crate::targets::BoundedProcessExecutor::new(
                crate::harness_auth::KEYCHAIN_READ_TIMEOUT,
            )),
        )
    }

    fn file(marker: std::path::PathBuf) -> Self {
        Self::bounded(mj_core::config::HarnessLoginStore::File { marker })
    }

    /// The login as it is now: a file, or the Keychain item. Blocking.
    fn read(&self, kind: mj_core::config::HarnessKind) -> Result<(CredentialSnapshot, Vec<u8>)> {
        match &self.store {
            mj_core::config::HarnessLoginStore::File { marker } => {
                read_credential_file(kind, marker)
            }
            mj_core::config::HarnessLoginStore::MacKeychain => {
                // A locked Keychain is an error here: treating it as "no
                // login" would let the sync act on a login that merely could
                // not be read.
                match crate::harness_auth::read_claude_keychain_login(&*self.security)? {
                    crate::harness_auth::KeychainLogin::Present(bytes) => {
                        Ok((CredentialSnapshot::of(kind, &bytes), bytes))
                    }
                    crate::harness_auth::KeychainLogin::Missing => {
                        Ok((CredentialSnapshot::absent(), Vec::new()))
                    }
                    crate::harness_auth::KeychainLogin::Unreadable { status } => Err(anyhow!(
                        "the macOS Keychain could not be read (security exited with status {status})"
                    )),
                }
            }
        }
    }

    /// [`LoginAccess::read`] off the scheduler thread.
    async fn read_blocking(
        &self,
        kind: mj_core::config::HarnessKind,
    ) -> Result<(CredentialSnapshot, Vec<u8>)> {
        let access = self.clone();
        mj_core::runtime::spawn_blocking(move || access.read(kind))
            .await
            .context("read profile credentials task stopped")?
    }

    /// Adopt a session's fresher login as the profile's: into its file, or on
    /// a Mac into the Keychain. See
    /// [`mj_core::credentials::install_credentials_if_newer`], and
    /// [`crate::claude_login::install_keychain_login_if_newer`] for the Keychain.
    async fn install_if_newer(
        &self,
        kind: mj_core::config::HarnessKind,
        bytes: &[u8],
    ) -> Result<mj_core::credentials::CredentialInstall> {
        match &self.store {
            mj_core::config::HarnessLoginStore::File { marker } => {
                mj_core::credentials::install_credentials_if_newer(kind, marker, bytes).await
            }
            mj_core::config::HarnessLoginStore::MacKeychain => {
                let lock_marker = match &self.lock_marker {
                    Some(marker) => marker.clone(),
                    None => self.store.lock_marker()?,
                };
                crate::claude_login::install_keychain_login_if_newer(
                    &lock_marker,
                    self.security.clone(),
                    bytes,
                )
                .await
            }
        }
    }
}

/// The host's logins a pass reads once for all of a profile's sessions.
pub struct HostLogins<'a> {
    pub slopcop: &'a HostSlopcopLogin,
    pub harness: &'a PassLogin,
    /// See [`SessionCanonical::pinned`].
    pub pinned: Option<Arc<CanonicalLogin>>,
}

/// Where a pass finds the profile's login. Decided once per pass, from
/// [`mj_core::config::HarnessKind::login_store`], so every session of the
/// profile agrees on it.
pub enum PassLogin {
    Shared(Arc<ProfileLogin>),
    PerSession,
    /// The macOS Keychain, read once for the whole pass: every session of the
    /// profile reconciles against the same item, and each read spawns
    /// `security`. A read that is slightly stale by the time a late session
    /// is reached is harmless: a pull rereads under the locks and installs
    /// only a strictly newer login, and a push of an older login is refused by
    /// the session for being older.
    #[cfg(all(unix, test))]
    Keychain {
        read: Arc<LoginRead>,
        access: LoginAccess,
    },
}

/// Hash of the access token in the profile's Claude login as its store holds
/// it now, or `None` when the profile is not a Claude login or it cannot be
/// read. This is the answer to "is the token that failed still current?" for
/// every consumer that is not itself a sync pass (#1547).
///
/// A plain read, without the native login lock: only a sync pass, which reads
/// under the lock, creates a refusal. This only re-checks whether the login
/// has changed since, so during a renewal it can lag but never invent one.
pub async fn current_claude_access_token(
    profile: &mj_core::config::HarnessProfile,
) -> Option<String> {
    if profile.kind != mj_core::config::HarnessKind::Claude
        || !profile.auth_scheme().uses_native_login_file()
    {
        return None;
    }
    let store = profile
        .kind
        .login_store(mj_core::config::HarnessHost::current(), &profile.home);
    let (_, bytes) = LoginAccess::bounded(store)
        .read_blocking(profile.kind)
        .await
        .ok()?;
    mj_core::credentials::claude_access_token_fingerprint(&bytes)
}

/// Serializes reads and adoption of the single profile login. Session relay
/// calls never hold this lock: an unreachable worker cannot hold up a new
/// canonical decision or delivery to a different worker.
pub struct ProfileLogin {
    decide: tokio::sync::Mutex<()>,
    current: watch::Sender<Option<Result<Arc<CanonicalLogin>, String>>>,
    deliveries: SessionSteps,
    rejected: DeliveryRegistry,
    events: Option<mpsc::UnboundedSender<PassEvent>>,
}

impl ProfileLogin {
    pub fn new(
        events: Option<mpsc::UnboundedSender<PassEvent>>,
        rejected: DeliveryRegistry,
    ) -> Self {
        Self {
            decide: Default::default(),
            current: watch::Sender::new(None),
            deliveries: Default::default(),
            rejected,
            events,
        }
    }

    /// Read the profile's login and publish it. Returns whether it changed,
    /// and the reading this call published, so the pass that took it can keep
    /// using that very reading however many other passes publish after it.
    pub async fn refresh(
        &self,
        runtime: Option<&Arc<dyn CredentialSyncRuntime>>,
        target: &CredentialSyncTarget,
        under_lock: bool,
    ) -> (bool, Option<Arc<CanonicalLogin>>) {
        let _decision = self.decide.lock().await;
        let before = self.for_target(target).ok();
        let login = self.read(target, under_lock).await;
        if runtime.is_some_and(|runtime| !runtime.credential_profile_source_is_current(target)) {
            return (false, None);
        }
        let changed = before.as_ref().map(|before| &before.credential)
            != login.as_ref().ok().map(|after| &after.credential);
        let published = login.as_ref().ok().cloned();
        self.current.send_replace(Some(login));
        (changed && before.is_some(), published)
    }

    /// The profile's login for this pass, read once: the pass delivers it and
    /// judges an authentication failure against it, so the two cannot see
    /// different logins. A pass that follows an authentication failure
    /// (`under_lock`) reads a Claude login under the native lock, which waits
    /// for a renewal in progress. If the lock cannot be taken in time the
    /// login is read plainly (a worker keeps the newer login, so delivering it
    /// is safe) and the reading is not marked as taken under the lock.
    async fn read(
        &self,
        target: &CredentialSyncTarget,
        under_lock: bool,
    ) -> Result<Arc<CanonicalLogin>, String> {
        let plain = |target: CredentialSyncTarget| async move {
            mj_core::runtime::spawn_blocking(move || {
                read_canonical_login(&target, &PassLogin::PerSession)
            })
            .await
            .context("profile login reader stopped")
            .and_then(|result| result)
            .map_err(|error| format!("{error:#}"))
        };
        if under_lock
            && target.harness == mj_core::config::HarnessKind::Claude
            && !target.skips_login_file_sync
        {
            let store = target.harness.login_store(
                mj_core::config::HarnessHost::current(),
                &target.profile_home,
            );
            let locked_target = target.clone();
            if let Some(login) = mj_core::credentials::read_login_under_native_lock(
                target.harness,
                &store,
                mj_core::credentials::CLAUDE_REFRESH_LOCK_WAIT,
                move || read_canonical_login(&locked_target, &PassLogin::PerSession),
            )
            .await
            {
                return login
                    .map(|login| {
                        let mut login =
                            Arc::try_unwrap(login).unwrap_or_else(|shared| shared.as_ref().clone());
                        login.read_under_lock = true;
                        Arc::new(login)
                    })
                    .map_err(|error| format!("{error:#}"));
            }
        }
        plain(target.clone()).await
    }

    fn for_target(&self, target: &CredentialSyncTarget) -> Result<Arc<CanonicalLogin>> {
        self.current
            .borrow()
            .as_ref()
            .cloned()
            .context("profile login has not been read")?
            .map_err(|error| anyhow!("{error}"))
            .and_then(|login| {
                anyhow::ensure!(
                    login.profile_home == target.profile_home && login.harness == target.harness,
                    "profile login location changed"
                );
                Ok(login)
            })
    }

    fn fingerprint(&self, target: &CredentialSyncTarget) -> Option<String> {
        let login = self.for_target(target).ok()?;
        if target.harness == mj_core::config::HarnessKind::Claude {
            mj_core::credentials::claude_access_token_fingerprint(&login.bytes)
        } else {
            login
                .credential
                .present
                .then(|| login.credential.fingerprint.clone())
        }
    }

    fn begin_delivery(
        &self,
        runtime: Option<&Arc<dyn CredentialSyncRuntime>>,
        target: &CredentialSyncTarget,
    ) {
        if runtime.is_some_and(|runtime| runtime.credential_session_ended(&target.session_id)) {
            return;
        }
        let Some(fingerprint) = self.fingerprint(target) else {
            self.rejected
                .lock()
                .expect("rejected logins poisoned")
                .end_delivery(target, None, "profile has no current readable login".into());
            return;
        };
        self.rejected
            .lock()
            .expect("rejected logins poisoned")
            .start_delivery(target, &fingerprint);
    }

    fn send(&self, event: PassEvent) {
        if let Some(events) = &self.events
            && events.send(event).is_err()
        {
            tracing::debug!("credential sync coordinator stopped before receiving delivery state");
        }
    }

    async fn adopt(
        &self,
        target: &CredentialSyncTarget,
        bytes: &[u8],
    ) -> Result<mj_core::credentials::CredentialInstall> {
        let _decision = self.decide.lock().await;
        let login = self.for_target(target)?;
        let installed = login.access.install_if_newer(target.harness, bytes).await?;
        if installed.installed || installed.snapshot != login.credential {
            // A pull and the host's own concurrent renewal both invalidate the
            // deliveries selected against the previous generation.
            self.current
                .send_replace(Some(self.read(target, false).await));
            self.send(PassEvent::Pulled(target.profile_id.clone()));
        }
        Ok(installed)
    }
}

fn read_canonical_login(
    target: &CredentialSyncTarget,
    login: &PassLogin,
) -> Result<Arc<CanonicalLogin>> {
    let kind = target.harness;
    let (access, credential, bytes) = match login {
        PassLogin::Shared(owner) => return owner.for_target(target),
        #[cfg(all(unix, test))]
        PassLogin::Keychain { read, access } => {
            let (credential, bytes) = read.as_ref().clone().map_err(|error| anyhow!("{error}"))?;
            (access.clone(), credential, bytes)
        }
        PassLogin::PerSession if kind.keeps_logins_in_database() => {
            let access =
                LoginAccess::file(harness_authentication_marker(kind, &target.profile_home));
            let (credential, bytes) =
                match crate::opencode_logins::home_logins(&target.profile_home)? {
                    Some(bytes) => (CredentialSnapshot::of(kind, &bytes), bytes),
                    None => (CredentialSnapshot::absent(), Vec::new()),
                };
            (access, credential, bytes)
        }
        PassLogin::PerSession => {
            let access = LoginAccess::bounded(kind.login_store(
                mj_core::config::HarnessHost::current(),
                &target.profile_home,
            ));
            let (credential, bytes) = access.read(kind)?;
            (access, credential, bytes)
        }
    };
    Ok(Arc::new(CanonicalLogin {
        access,
        credential,
        bytes,
        profile_home: target.profile_home.clone(),
        harness: kind,
        read_under_lock: false,
    }))
}

/// What one session is reconciled against, read on the controller before its
/// worker is asked anything. Nothing here fails the session as a whole: each
/// part that could not be read is the failure of the one step that needs it.
pub struct SessionCanonical {
    profile: Option<Arc<ProfileLogin>>,
    pub login: Result<Arc<CanonicalLogin>, String>,
    /// The reading an authentication-triggered pass took at its start, for
    /// the session that failed. Only its evidence is used (that it was read
    /// under the native lock, after this failure); the session is delivered
    /// the newest login. See `reconcile_on`.
    pub pinned: Option<Arc<CanonicalLogin>>,
    pub skills: Arc<CanonicalSkills>,
    pub github: GithubSync,
    pub slopcop_login: HostSlopcopLogin,
}

impl SessionCanonical {
    pub fn read(
        target: &CredentialSyncTarget,
        skills: Arc<CanonicalSkills>,
        github: GithubSync,
        login: &PassLogin,
    ) -> Self {
        Self {
            // A profile without a login file has nothing to read.
            login: if target.skips_login_file_sync {
                Err("not synchronized".to_owned())
            } else {
                read_canonical_login(target, login).map_err(|error| format!("{error:#}"))
            },
            pinned: None,
            profile: match login {
                PassLogin::Shared(owner) => Some(owner.clone()),
                _ => None,
            },
            skills,
            github,
            slopcop_login: HostSlopcopLogin::Unknown,
        }
    }
}

/// What a session's worker connection is asked to do.
pub enum SyncWork {
    /// Every sync step of the profile's pass.
    Full(Box<SessionCanonical>),
    /// Only the session's AWS credentials.
    Aws(Box<AwsSync>),
}

/// Reconcile one session. Returns every action taken, where an empty list
/// means the copies already agree, or `None` when the session could not be
/// reached this cycle because its actor had no connection to lend.
pub async fn reconcile_session(
    relays: &SessionRelays,
    target: &CredentialSyncTarget,
    skills: &Arc<CanonicalSkills>,
    github: &GithubSync,
    host: HostLogins<'_>,
    hold: JobHold,
) -> Result<Option<SessionSync>> {
    let mut canonical =
        SessionCanonical::read(target, skills.clone(), github.clone(), host.harness);
    canonical.pinned = host.pinned.clone();
    canonical.slopcop_login = host.slopcop.clone();
    run_on_session(relays, target, SyncWork::Full(Box::new(canonical)), hold).await
}

/// Run `work` on the session's worker connection.
pub async fn run_on_session(
    relays: &SessionRelays,
    target: &CredentialSyncTarget,
    work: SyncWork,
    hold: JobHold,
) -> Result<Option<SessionSync>> {
    match relays {
        SessionRelays::Actors(manager) => {
            let Some(handle) = manager.find_session(target.session_id.clone()).await? else {
                return Err(RelayJobDeferred(
                    "credential sync waits for the session's relay actor",
                )
                .into());
            };
            let (job, response, _queued) = CredentialSyncJob::queue(target.clone(), work, hold);
            handle.run_on_connection(Box::new(job)).await;
            match response
                .await
                .context("the session actor dropped the credential sync")?
            {
                Ok(sync) => Ok(Some(sync)),
                Err(error) => Err(error),
            }
        }
        SessionRelays::Direct => {
            let _queued = QueuedJob(hold);
            let mut client = RelayClient::connect(&target.spec, &target.session_id).await?;
            let result = run_work(&mut client, target, work, _queued.0.0.runtime.as_ref()).await;
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
    runtime: Option<&Arc<dyn CredentialSyncRuntime>>,
) -> Result<SessionSync> {
    let (adopted_profiles, result) = if let Some(runtime) = runtime {
        match runtime.pending_retired_logins(&target.session_id) {
            Ok(pending) => {
                drain_retired_logins(
                    client,
                    pending,
                    |id| runtime.credential_profile(id),
                    |id| runtime.clear_pending_retired_login(&target.session_id, id),
                    |id| Ok(!runtime.retired_login_move_active(&target.session_id, id)?),
                )
                .await
            }
            Err(error) => (Default::default(), Err(error)),
        }
    } else {
        (Default::default(), Ok(()))
    };
    if let Err(error) = result {
        tracing::warn!(session_id = %target.session_id, %error, "could not adopt retired logins; retained for retry");
    }
    match work {
        SyncWork::Full(mut canonical) => {
            if adopted_profiles.contains(&target.profile_id) {
                if let Some(owner) = &canonical.profile {
                    owner.refresh(runtime, target, false).await;
                    canonical.login = owner
                        .for_target(target)
                        .map_err(|error| format!("{error:#}"));
                } else {
                    canonical.login = mj_core::runtime::spawn_blocking({
                        let target = target.clone();
                        move || read_canonical_login(&target, &PassLogin::PerSession)
                    })
                    .await
                    .context("canonical login reader stopped")?
                    .map_err(|error| format!("{error:#}"));
                }
            }
            reconcile_on(client, target, &canonical).await
        }
        SyncWork::Aws(aws) => {
            let mut sync = SessionSync {
                unchanged_login: None,
                current_access_token: None,
                actions: Vec::new(),
                failures: Vec::new(),
                reports_held_login: false,
            };
            reconcile_aws_step(client, &aws, &mut sync).await;
            Ok(sync)
        }
    }
}

/// Reconcile one session over a connection to its worker.
///
/// Every step runs whether or not another failed, and each failure is
/// recorded on its own. A failing GitHub lookup must not hold back the
/// harness login, skills, or SlopCop login.
pub async fn reconcile_on(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: &SessionCanonical,
) -> Result<SessionSync> {
    let mut sync = SessionSync {
        unchanged_login: None,
        current_access_token: None,
        actions: Vec::new(),
        failures: Vec::new(),
        reports_held_login: client.protocol_version() >= mj_core::relay::RELAY_HELD_LOGIN_PROTOCOL,
    };
    let SessionSync {
        actions,
        failures,
        unchanged_login,
        current_access_token,
        ..
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
        let fresh = canonical.profile.as_ref().map(|owner| {
            owner
                .for_target(target)
                .map_err(|error| format!("{error:#}"))
        });
        match fresh.as_ref().unwrap_or(&canonical.login) {
            Ok(login) => {
                // The session whose failure asked for this pass is delivered the
                // newest login, like every session. Whether that login is the
                // rejected one is only known from a reading under the native
                // lock (a renewal in progress would otherwise make the old
                // login look refused): either the newest reading itself was
                // read under it, or this pass's own pinned reading was and
                // holds the same bytes. The pinned reading was taken after
                // this failure, so evidence from an earlier failure's pass
                // can never count. Otherwise the verdict is unknown.
                *current_access_token = canonical
                    .pinned
                    .as_ref()
                    .filter(|pinned| {
                        login.read_under_lock
                            || (pinned.read_under_lock && pinned.bytes == login.bytes)
                    })
                    .and_then(|_| {
                        mj_core::credentials::claude_access_token_fingerprint(&login.bytes)
                    });
                match reconcile_credentials(
                    client,
                    target,
                    &login.access,
                    &login.credential,
                    &login.bytes,
                    actions,
                    canonical.profile.as_deref(),
                )
                .await
                {
                    Ok(snapshot) => {
                        *unchanged_login = snapshot;
                        // A pull made the session's copy the profile's, so the
                        // login read above is no longer the profile's: the
                        // verdict is unknown, never "renewed" or "refused".
                        if actions.contains(&CredentialSyncAction::Pulled) {
                            *current_access_token = None;
                        }
                    }
                    Err(error) => note(
                        SyncStep::HarnessLogin,
                        "harness login",
                        format!("{error:#}"),
                    ),
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
            step: SyncStep::Aws,
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
            Ok(Some(CredentialSyncAction::AwsCredentialsPushed))
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
pub struct CredentialSyncJob {
    pub target: CredentialSyncTarget,
    pub work: SyncWork,
    pub reply: tokio::sync::oneshot::Sender<Result<SessionSync>>,
    pub hold: JobHold,
}

impl CredentialSyncJob {
    /// The job, the receiver of its answer, and the guard that abandons the
    /// job if the awaiting task stops first.
    pub fn queue(
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
    fn wake_when_usable(&self) -> bool {
        true
    }

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
            // waited (its target changed, the session went away), which
            // already released the hold, and the work is dropped unrun.
            if !hold.start() {
                tracing::debug!(
                    session_id = %target.session_id,
                    "credential sync dropped: no longer wanted"
                );
                // Deferred, not failed: the awaiting side, if it is still
                // there, tries again in the next cycle.
                let _ = reply.send(Err(anyhow::Error::new(RelayJobDeferred(
                    "the credential sync was abandoned before it started",
                ))));
                return;
            }
            // A lifecycle may have completed since the target was published.
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
            let result = run_work(client, &target, work, hold.0.runtime.as_ref()).await;
            // Released before the answer, so whoever awaits it can take the
            // session's install lock at once.
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
        tracing::debug!(
            session_id = %self.target.session_id,
            reason = %format!("{error:#}"),
            "credential sync job refused by the session actor"
        );
        if self.reply.send(Err(error)).is_err() {
            tracing::debug!(
                session_id = %self.target.session_id,
                "credential sync refusal receiver was already closed"
            );
        }
    }
}

/// Canonical adoption uses the same locked file/Keychain path as ordinary sync.
pub async fn adopt_copied_login(
    profile: &mj_core::config::HarnessProfile,
    bytes: &[u8],
) -> Result<bool> {
    anyhow::ensure!(
        profile.auth_scheme().uses_native_login_file(),
        "profile no longer uses a native login"
    );
    anyhow::ensure!(
        mj_core::credentials::harvestable_login_bytes(profile.kind, bytes)?,
        "login is not a harvestable copied grant"
    );
    let access = LoginAccess::bounded(
        profile
            .kind
            .login_store(mj_core::config::HarnessHost::current(), &profile.home),
    );
    let installed = access.install_if_newer(profile.kind, bytes).await?;
    let incoming = CredentialSnapshot::of(profile.kind, bytes);
    anyhow::ensure!(
        installed.installed
            || installed.snapshot.fingerprint == incoming.fingerprint
            || reconcile(&installed.snapshot, &incoming) == SyncAction::Push,
        "canonical login cannot be ordered against the retired grant; retaining the source copy"
    );
    Ok(installed.installed)
}

/// Match receipts against daemon-owned Move authority, adopt under the canonical
/// lock, then acknowledge the exact fingerprint before clearing authority.
/// Installed profile IDs survive partial failures so callers can refresh cached
/// canonical readings. Only complete, non-deferred scans may prune absent receipts.
/// No secret enters an event or a checkpoint.
pub async fn drain_retired_logins(
    client: &mut RelayClient,
    pending: Vec<mj_core::credentials::PendingRetiredLogin>,
    profile: impl Fn(&str) -> Option<mj_core::config::HarnessProfile>,
    clear: impl Fn(&str) -> Result<()>,
    may_prune: impl Fn(&str) -> Result<bool>,
) -> (BTreeSet<String>, Result<()>) {
    let mut adopted = BTreeSet::new();
    if pending.is_empty() {
        return (adopted, Ok(()));
    }
    let mut failure = None;
    let mut rpc_failed = false;
    let mut deferred_scan = false;
    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let (logins, has_more, deferred) =
            match client.read_retired_logins_after(cursor.as_deref()).await {
                Ok(page) => page,
                Err(error) => return (adopted, Err(error)),
            };
        // Pre-93 workers cannot prove absence: they have no receipt RPC.
        deferred_scan |= deferred
            || client.protocol_version()
                < mj_core::relay::RELAY_RETIRED_LOGIN_AND_CONTINUITY_PROTOCOL;
        if has_more
            && logins
                .last()
                .is_none_or(|last| cursor.as_ref().is_some_and(|cursor| last.id <= *cursor))
        {
            return (
                adopted,
                Err(anyhow::anyhow!("retired login cursor did not advance")),
            );
        }
        cursor = logins.last().map(|login| login.id.clone());
        for login in logins {
            seen.insert(login.id.clone());
            let Some(auth) = pending.iter().find(|auth| {
                auth.operation_id == login.id
                    && auth.source_profile_id == login.profile_id
                    && auth.harness_kind == login.harness
                    && auth.store_id == login.store_id
            }) else {
                tracing::warn!(operation_id = %login.id, "refusing unauthorized retired login");
                failure = Some(anyhow::anyhow!("unauthorized retired login"));
                continue;
            };
            let result = async {
                if let Some(profile) = profile(&auth.source_profile_id) {
                    anyhow::ensure!(profile.kind == auth.harness_kind, "retired login harness differs from source profile");
                    if adopt_copied_login(&profile, &login.bytes).await? {
                        adopted.insert(auth.source_profile_id.clone());
                    }
                } else {
                    tracing::info!(profile_id = %auth.source_profile_id, "retired login source profile no longer exists");
                }
                if let Err(error) = client.ack_retired_login(&login.id,
                    &mj_core::credentials::credential_fingerprint(&login.bytes)).await {
                    rpc_failed = true;
                    return Err(error);
                }
                clear(&login.id)
            }.await;
            if let Err(error) = result {
                tracing::warn!(profile_id = %auth.source_profile_id, operation_id = %login.id, %error, "retired login retained for retry");
                failure = Some(error);
            }
        }
        if !has_more {
            break;
        }
    }
    if !deferred_scan && !rpc_failed {
        for auth in &pending {
            if !seen.contains(&auth.operation_id) {
                let result = (|| {
                    if may_prune(&auth.operation_id)? {
                        clear(&auth.operation_id)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    failure = Some(error);
                }
            }
        }
    }
    (adopted, failure.map_or(Ok(()), Err))
}

pub async fn reconcile_credentials(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    access: &LoginAccess,
    canonical: &CredentialSnapshot,
    canonical_bytes: &[u8],
    actions: &mut Vec<CredentialSyncAction>,
    owner: Option<&ProfileLogin>,
) -> Result<Option<CredentialSnapshot>> {
    let mut session = client.credential_state().await?;
    if let Some(owner) = owner
        && let Some(fingerprint) = owner.fingerprint(target)
        && owner
            .rejected
            .lock()
            .expect("rejected logins poisoned")
            .delivery_mismatch(target, &fingerprint, &session)
    {
        bail!("worker did not retain the installed profile login");
    }
    if target.harness.keeps_logins_in_database() {
        if let Some(bytes) = stored_logins_push(canonical, canonical_bytes, &session) {
            let installed = client
                .install_credentials(bytes)
                .await
                .context("push profile login")?;
            // An empty export is still a present staged file on the worker.
            if installed.present
                && installed.fingerprint == mj_core::credentials::credential_fingerprint(bytes)
            {
                actions.push(CredentialSyncAction::Pushed);
            } else {
                return Err(login_install_mismatch(
                    owner,
                    target,
                    canonical_bytes,
                    installed,
                ));
            }
        }
        return Ok(None);
    }
    let kind = target.harness;
    let mut current = canonical.clone();
    let mut bytes = canonical_bytes.to_owned();
    let mut unchanged = true;
    loop {
        // What the session should hold: the profile's login, as a harness may
        // hold it. For Claude that omits the refresh token, so the copy can
        // never spend the grant the daemon alone refreshes (#1490).
        let desired_bytes = if kind.copies_omit_refresh_token() {
            credential_copy_for_harness(kind, &bytes).into_owned()
        } else {
            bytes.clone()
        };
        let desired = if current.present {
            CredentialSnapshot::of(kind, &desired_bytes)
        } else {
            CredentialSnapshot::absent()
        };
        match session_copy_action(kind, &current, &desired, &session) {
            SyncAction::None => {
                if current.present
                    && session.present
                    && current.fingerprint != session.fingerprint
                    && current.freshness_epoch_ms.is_none()
                    && session.freshness_epoch_ms.is_none()
                {
                    tracing::warn!(
                        session_id = %target.session_id,
                        profile_id = %target.profile_id,
                        "credential copies differ but neither reports a refresh time; leaving both alone"
                    );
                }
                // Only this original, identical snapshot can identify the login
                // the failing session held. A concurrent refresh is not evidence
                // that the newly observed login was refused. A copy without the
                // refresh token holds the same login.
                return Ok((unchanged
                    && current.present
                    && session.present
                    && session.fingerprint == desired.fingerprint)
                    .then_some(current));
            }
            SyncAction::Push => {
                let installed = client
                    .install_credentials(&desired_bytes)
                    .await
                    .context("push profile login")?;
                if installed.present && installed.fingerprint == desired.fingerprint {
                    log_credential_transfer("pushed", target, &session, &installed);
                    actions.push(CredentialSyncAction::Pushed);
                    return Ok(None);
                }
                if session_copy_action(kind, &current, &desired, &installed) != SyncAction::Pull {
                    return Err(login_install_mismatch(owner, target, &bytes, installed));
                }
                session = installed;
                continue;
            }
            SyncAction::Pull => {
                let received = client.read_credentials().await?;
                validate_credential_payload(kind, &received).with_context(|| {
                    format!(
                        "session {} returned an unusable credential file",
                        target.session_id
                    )
                })?;
                let incoming = CredentialSnapshot::of(kind, &received);
                // A copy without the refresh token cannot become the
                // profile's login, however late it expires, nor bring back a
                // login the host no longer has (a logout).
                if kind.copies_omit_refresh_token()
                    && !credential_holds_refresh_token(kind, &received)
                {
                    tracing::debug!(
                        session_id = %target.session_id,
                        profile_id = %target.profile_id,
                        "the session's credential copy expires later but holds no refresh token; leaving both alone"
                    );
                    return Ok(None);
                }
                // Into where the profile's login is kept: its file, or on a
                // Mac the Keychain (#1491 review: a session that refreshed
                // the shared grant holds the only live refresh token).
                let installed = match owner {
                    Some(owner) => owner.adopt(target, &received).await,
                    None => access.install_if_newer(kind, &received).await,
                }
                .with_context(|| {
                    format!(
                        "install fresher credentials from session {} for profile {}",
                        target.session_id, target.profile_id
                    )
                })?;
                if installed.installed {
                    log_credential_transfer("pulled", target, &current, &installed.snapshot);
                    actions.push(CredentialSyncAction::Pulled);
                    if !kind.copies_omit_refresh_token() {
                        return Ok(None);
                    }
                    // The adopted login still sits in the session with its
                    // refresh token: fall through to reread the profile and
                    // push the copy without it back.
                } else {
                    anyhow::ensure!(
                        installed.snapshot != current || incoming != session,
                        "profile {} retained its credentials without accepting the newer session login or observing a changed snapshot",
                        target.profile_id
                    );
                    session = incoming;
                }
            }
        }
        // A destination retained a different generation. Reconcile that actual
        // state now, rather than turning a stale decision into a clean check.
        // Every repeat follows a changed snapshot; no lock spans relay I/O.
        unchanged = false;
        (current, bytes) = match owner {
            Some(owner) => {
                let login = owner.for_target(target)?;
                (login.credential.clone(), login.bytes.clone())
            }
            None => access.read_blocking(kind).await?,
        };
    }
}

fn login_install_mismatch(
    owner: Option<&ProfileLogin>,
    target: &CredentialSyncTarget,
    requested: &[u8],
    observed: CredentialSnapshot,
) -> anyhow::Error {
    let reason = "worker did not retain the installed profile login";
    if let Some(owner) = owner
        && let Some(fingerprint) = owner.fingerprint(target)
        && (if target.harness == mj_core::config::HarnessKind::Claude {
            mj_core::credentials::claude_access_token_fingerprint(requested)
        } else {
            Some(mj_core::credentials::credential_fingerprint(requested))
        })
        .as_deref()
            == Some(&fingerprint)
    {
        owner
            .rejected
            .lock()
            .expect("rejected logins poisoned")
            .record_delivery(
                target,
                &fingerprint,
                LoginDeliveryState::Concluded {
                    reason: reason.into(),
                },
                Some(observed),
            );
    }
    anyhow!("{reason}")
}

/// Which way a session's copy and the profile's login converge. `desired` is
/// the profile's login as the session may hold it.
///
/// A harness whose copies omit the refresh token never lets a copy replace the
/// profile unless it is strictly newer (it carries its own login): a copy that
/// merely expires later has no refresh token to adopt. Otherwise the session
/// is pushed whenever it differs from `desired`, so a copy staged before that
/// rule, which still holds the refresh token, converges at the same expiry.
fn session_copy_action(
    kind: mj_core::config::HarnessKind,
    canonical: &CredentialSnapshot,
    desired: &CredentialSnapshot,
    session: &CredentialSnapshot,
) -> SyncAction {
    let action = reconcile(canonical, session);
    if !kind.copies_omit_refresh_token() || action == SyncAction::Pull {
        return action;
    }
    if !canonical.present {
        return action;
    }
    if !session.present {
        return SyncAction::Push;
    }
    if session.fingerprint == desired.fingerprint
        || (canonical.freshness_epoch_ms.is_none() && session.freshness_epoch_ms.is_none())
    {
        return SyncAction::None;
    }
    SyncAction::Push
}

/// Record which way a login moved, never its bytes.
fn log_credential_transfer(
    direction: &str,
    target: &CredentialSyncTarget,
    old: &CredentialSnapshot,
    new: &CredentialSnapshot,
) {
    tracing::info!(
        direction,
        profile_id = %target.profile_id,
        session_id = %target.session_id,
        old_expires_at_ms = ?old.freshness_epoch_ms,
        new_expires_at_ms = ?new.freshness_epoch_ms,
        "synchronized a harness login"
    );
}

/// The export of a profile with no logins.
const NO_STORED_LOGINS: &[u8] = b"[]";

/// What to push to a session whose staged logins are `session`, when the
/// profile's login database exports `canonical` (`bytes`); `None` when the
/// session already holds them. The database is the profile's only copy, so a
/// session's copy is never pulled, and a session staged while the profile had
/// no logins holds no export, which is the same as an empty one.
pub fn stored_logins_push<'a>(
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

pub async fn reconcile_github_token(
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
/// localhost session's home is staged without one and reads the host's
/// own login.
pub async fn reconcile_slopcop_login(
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
pub async fn reconcile_skills(
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

pub fn sync_method_unsupported(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| rejected.0.code == RelayErrorCode::InvalidRequest)
}

#[cfg(all(test, unix))]
mod retired_tests {
    use super::*;
    use mj_core::config::{HarnessKind, HarnessProfile};

    fn authorize(
        login: &mj_core::credentials::RetiredLoginSnapshot,
    ) -> mj_core::credentials::PendingRetiredLogin {
        mj_core::credentials::PendingRetiredLogin {
            operation_id: login.id.clone(),
            source_profile_id: login.profile_id.clone(),
            harness_kind: login.harness,
            store_id: login.store_id.clone(),
        }
    }

    // Recorded relay peer: exercise the wire client and canonical store/locks.
    const PEER: &str = r#"
import json, sys, os
path, protocol = sys.argv[1], int(sys.argv[2])
for line in sys.stdin:
    req = json.loads(line)
    method = req['request']['method']
    if method == 'hello':
        payload = {'type':'hello','data':{'negotiated':protocol,'relay_version':'retired-login-fixture','session_id':'retired-test'}}
    elif method == 'read_retired_logins':
        with open(path) as f: logins = json.load(f)
        after = req['request'].get('params', {}).get('after')
        logins = sorted([login for login in logins if after is None or login['id'] > after], key=lambda login:login['id'])
        page, charged = [], 0
        for login in logins:
            size = len(__import__('base64').b64decode(login['bytes']))
            if page and charged + size > 2 * 1024 * 1024: break
            page.append(login)
            charged += size
        payload = {'type':'retired_logins','data':{'logins':page,'has_more':len(page) < len(logins),'deferred':os.path.exists(path + '.deferred')}}
    elif method == 'credential_state':
        worker = path + '.worker'
        data = open(worker, 'rb').read() if os.path.exists(worker) else None
        payload = {'type':'credential_state','data':{'present':data is not None,'fingerprint':__import__('hashlib').sha256(data).hexdigest() if data else '', 'freshness_epoch_ms':None}}
    elif method == 'install_credentials':
        data = __import__('base64').b64decode(req['request']['params']['data'])
        with open(path + '.worker', 'wb') as f: f.write(data)
        payload = {'type':'credential_state','data':{'present':True,'fingerprint':__import__('hashlib').sha256(data).hexdigest(),'freshness_epoch_ms':None}}
    elif method == 'ack_retired_login':
        with open(path) as f: logins = json.load(f)
        import hashlib, base64
        replacement = path + '.replace-on-ack'
        if os.path.exists(replacement):
            with open(replacement) as f: logins = json.load(f)
            with open(path, 'w') as f: json.dump(logins, f)
            os.remove(replacement)
        params = req['request']['params']
        for login in logins:
            if login['id'] == params['id'] and hashlib.sha256(base64.b64decode(login['bytes'])).hexdigest() != params['fingerprint']:
                print(json.dumps({'request_id':req['request_id'],'protocol_version':protocol,'result':'error','error':{'code':'invalid_state','message':'retired login receipt changed before acknowledgement'}}), flush=True)
                break
        else:
            logins = [login for login in logins if login['id'] != params['id']]
            with open(path, 'w') as f: json.dump(logins, f)
            print(json.dumps({'request_id':req['request_id'],'protocol_version':protocol,'result':'ok','payload':{'type':'retired_login_acknowledged'}}), flush=True)
        continue
        with open(path, 'w') as f: json.dump(logins, f)
        payload = {'type':'retired_login_acknowledged'}
    else:
        raise AssertionError(method)
    response = json.dumps({'request_id':req['request_id'],'protocol_version':protocol,'result':'ok','payload':payload})
    assert len(response.encode()) < 8 * 1024 * 1024
    print(response, flush=True)
"#;

    struct Authority {
        profile: HarnessProfile,
        pending: std::sync::Mutex<BTreeMap<String, mj_core::credentials::PendingRetiredLogin>>,
        active: bool,
    }

    impl CredentialSyncRuntime for Authority {
        fn credential_profile(&self, _: &str) -> Option<HarnessProfile> {
            Some(self.profile.clone())
        }
        fn pending_retired_logins(
            &self,
            _: &str,
        ) -> Result<Vec<mj_core::credentials::PendingRetiredLogin>> {
            Ok(self.pending.lock().unwrap().values().cloned().collect())
        }
        fn clear_pending_retired_login(&self, _: &str, id: &str) -> Result<()> {
            self.pending.lock().unwrap().remove(id);
            Ok(())
        }
        fn retired_login_move_active(&self, _: &str, _: &str) -> Result<bool> {
            Ok(self.active)
        }
        fn credential_targets(&self) -> Arc<watch::Sender<Vec<CredentialSyncTarget>>> {
            unreachable!()
        }
        fn credential_session_ids(
            &self,
        ) -> mj_core::snapshot_map::SnapshotMap<String, mj_core::state::SessionRecord> {
            unreachable!()
        }
        fn credential_session_ended(&self, _: &str) -> bool {
            unreachable!()
        }
        fn credential_profile_source_is_current(&self, _: &CredentialSyncTarget) -> bool {
            true
        }
        fn credential_target_is_current(&self, _: &CredentialSyncTarget) -> Result<bool> {
            unreachable!()
        }
        fn credential_sync_skips(
            &self,
            _: &str,
            _: bool,
            _: Option<&str>,
            _: &dyn Fn(&str) -> bool,
        ) -> Vec<(String, &'static str)> {
            unreachable!()
        }
    }

    async fn drain_authorized(
        client: &mut RelayClient,
        authority: &Authority,
    ) -> (BTreeSet<String>, Result<()>) {
        drain_retired_logins(
            client,
            authority.pending_retired_logins("session").unwrap(),
            |id| authority.credential_profile(id),
            |id| authority.clear_pending_retired_login("session", id),
            |id| Ok(!authority.retired_login_move_active("session", id)?),
        )
        .await
    }

    fn receipt() -> mj_core::credentials::RetiredLoginSnapshot {
        mj_core::credentials::RetiredLoginSnapshot {
            id: "move".into(),
            profile_id: "source".into(),
            harness: HarnessKind::Codex,
            store_id: "store".into(),
            login_relative_path: "auth.json".into(),
            bytes: br#"{"last_refresh":"2026-11-25T00:00:00Z"}"#.to_vec(),
        }
    }

    fn peer(path: &std::path::Path) -> CommandSpec {
        CommandSpec::new(
            "python3",
            [
                "-c".to_owned(),
                PEER.to_owned(),
                path.to_string_lossy().into_owned(),
                "93".into(),
            ],
        )
    }

    #[tokio::test]
    async fn credential_sync_authorizes_every_receipt_identity_field_before_adoption() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("canonical");
        std::fs::create_dir(&home).unwrap();
        let old = br#"{"last_refresh":"2026-11-24T00:00:00Z"}"#;
        let path = root.path().join("receipts.json");
        let login = receipt();
        let authority = Authority {
            profile: HarnessProfile::new(HarnessKind::Codex, home.clone()),
            pending: std::sync::Mutex::new(BTreeMap::from([(login.id.clone(), authorize(&login))])),
            active: false,
        };
        let mut client = RelayClient::connect(&peer(&path), "retired-test")
            .await
            .unwrap();
        for field in ["profile", "operation", "harness", "store"] {
            std::fs::write(home.join("auth.json"), old).unwrap();
            let mut forged = login.clone();
            match field {
                "profile" => forged.profile_id = "victim-profile".into(),
                "operation" => forged.id = "unrelated-operation".into(),
                "harness" => forged.harness = HarnessKind::Kimi,
                "store" => forged.store_id = "other-store".into(),
                _ => unreachable!(),
            }
            // Keep the legitimate Move active: no absent-record pruning.
            std::fs::write(&path, serde_json::to_vec(&vec![forged.clone()]).unwrap()).unwrap();
            let (_, result) = drain_retired_logins(
                &mut client,
                vec![authorize(&login)],
                |_| panic!("forged receipt reached canonical store"),
                |_| panic!("forged receipt acknowledged or active authority pruned"),
                |_| Ok(false),
            )
            .await;
            assert!(result.is_err(), "{field}");
            assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), old);
            assert_eq!(
                client.read_retired_logins_after(None).await.unwrap().0,
                vec![forged]
            );
        }
        std::fs::write(&path, serde_json::to_vec(&vec![login.clone()]).unwrap()).unwrap();
        let (adopted, result) = drain_authorized(&mut client, &authority).await;
        result.unwrap();
        assert_eq!(adopted, BTreeSet::from(["source".into()]));
        assert!(authority.pending.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), login.bytes);
        // An empty authority skips the RPC even when the worker cannot list.
        std::fs::remove_file(&path).unwrap();
        drain_authorized(&mut client, &authority).await.1.unwrap();
        client.detach().await.unwrap();
    }

    #[tokio::test]
    async fn credential_sync_deferred_and_active_moves_retain_absent_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("receipts.json");
        std::fs::write(&path, "[]").unwrap();
        let login = receipt();
        let mut authority = Authority {
            profile: HarnessProfile::new(HarnessKind::Codex, root.path()),
            pending: std::sync::Mutex::new(BTreeMap::from([(login.id.clone(), authorize(&login))])),
            active: true,
        };
        let mut client = RelayClient::connect(&peer(&path), "retired-test")
            .await
            .unwrap();
        let deferred = path.with_extension("json.deferred");
        std::fs::write(&deferred, "").unwrap();
        drain_authorized(&mut client, &authority).await.1.unwrap();
        assert_eq!(authority.pending.lock().unwrap().len(), 1);
        authority.active = false;
        drain_authorized(&mut client, &authority).await.1.unwrap();
        assert_eq!(
            authority.pending.lock().unwrap().len(),
            1,
            "even settled Moves retain authority during deferred scans"
        );
        std::fs::remove_file(&deferred).unwrap();
        authority.active = true;
        drain_authorized(&mut client, &authority).await.1.unwrap();
        assert_eq!(authority.pending.lock().unwrap().len(), 1);
        authority.active = false;
        drain_authorized(&mut client, &authority).await.1.unwrap();
        assert!(authority.pending.lock().unwrap().is_empty());
        client.detach().await.unwrap();
    }

    #[tokio::test]
    async fn credential_sync_stale_ack_retains_replaced_receipt_and_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("receipts.json");
        let original = receipt();
        let mut login = original.clone();
        login.bytes = br#"{"last_refresh":"2026-11-26T00:00:00Z"}"#.to_vec();
        std::fs::write(&path, serde_json::to_vec(&vec![original]).unwrap()).unwrap();
        std::fs::write(
            path.with_extension("json.replace-on-ack"),
            serde_json::to_vec(&vec![login.clone()]).unwrap(),
        )
        .unwrap();
        let authority = Authority {
            profile: HarnessProfile::new(HarnessKind::Codex, root.path()),
            pending: std::sync::Mutex::new(BTreeMap::from([(login.id.clone(), authorize(&login))])),
            active: false,
        };
        let mut client = RelayClient::connect(&peer(&path), "retired-test")
            .await
            .unwrap();
        let (adopted, result) = drain_authorized(&mut client, &authority).await;
        assert!(result.is_err());
        assert!(
            adopted.contains("source"),
            "adoption survives a later ack error"
        );
        assert_eq!(
            client.read_retired_logins_after(None).await.unwrap().0,
            vec![login]
        );
        assert_eq!(authority.pending.lock().unwrap().len(), 1);
        drain_authorized(&mut client, &authority).await.1.unwrap();
        assert!(authority.pending.lock().unwrap().is_empty());
        client.detach().await.unwrap();
    }

    #[tokio::test]
    async fn credential_sync_full_refreshes_owner_after_retired_adoption_before_push() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("canonical");
        std::fs::create_dir(&home).unwrap();
        let old = br#"{"last_refresh":"2026-11-24T00:00:00Z"}"#;
        std::fs::write(home.join("auth.json"), old).unwrap();
        let path = root.path().join("receipts.json");
        let login = receipt();
        std::fs::write(&path, serde_json::to_vec(&vec![login.clone()]).unwrap()).unwrap();
        let authority: Arc<dyn CredentialSyncRuntime> = Arc::new(Authority {
            profile: HarnessProfile::new(HarnessKind::Codex, home.clone()),
            pending: std::sync::Mutex::new(BTreeMap::from([(login.id.clone(), authorize(&login))])),
            active: false,
        });
        let mut target = test_fixtures::skills_sync_target(&home);
        target.harness = HarnessKind::Codex;
        target.profile_id = "source".into();
        let owner = Arc::new(ProfileLogin::new(None, Default::default()));
        owner.refresh(None, &target, false).await;
        let canonical = SessionCanonical::read(
            &target,
            Arc::new(CanonicalSkills::failed("no skills in this fixture")),
            GithubSync::Known(None),
            &PassLogin::Shared(owner.clone()),
        );
        assert_eq!(canonical.login.as_ref().unwrap().bytes.as_slice(), old);
        let mut client = RelayClient::connect(&peer(&path), "retired-test")
            .await
            .unwrap();
        let sync = run_work(
            &mut client,
            &target,
            SyncWork::Full(Box::new(canonical)),
            Some(&authority),
        )
        .await
        .unwrap();
        assert!(
            sync.actions.contains(&CredentialSyncAction::Pushed),
            "{:?}",
            sync.failures
        );
        assert_eq!(
            std::fs::read(path.with_extension("json.worker")).unwrap(),
            login.bytes
        );
        assert_eq!(
            owner.for_target(&target).unwrap().bytes.as_slice(),
            login.bytes
        );
        client.detach().await.unwrap();
    }

    #[tokio::test]
    async fn credential_sync_retired_backlog_adopts_later_pages_despite_an_earlier_failure() {
        use mj_core::credentials::RetiredLoginSnapshot;
        let root = tempfile::tempdir().unwrap();
        let bad_home = root.path().join("bad");
        let good_home = root.path().join("good");
        std::fs::create_dir_all(bad_home.join("auth.json")).unwrap();
        std::fs::create_dir_all(&good_home).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({"last_refresh":"2026-11-25T00:00:00Z","padding":"x".repeat(800000)})).unwrap();
        let logins: Vec<_> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|id| RetiredLoginSnapshot {
                id: id.into(),
                profile_id: if id == "a" { "bad" } else { "good" }.into(),
                harness: HarnessKind::Codex,
                store_id: "store".into(),
                login_relative_path: "auth.json".into(),
                bytes: bytes.clone(),
            })
            .collect();
        let receipts = root.path().join("receipts.json");
        std::fs::write(&receipts, serde_json::to_vec(&logins).unwrap()).unwrap();
        let spec = CommandSpec::new(
            "python3",
            [
                "-c".to_owned(),
                PEER.to_owned(),
                receipts.to_string_lossy().into_owned(),
                "93".into(),
            ],
        );
        let mut client = RelayClient::connect(&spec, "retired-test").await.unwrap();
        let (first, more, _) = client.read_retired_logins_after(None).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(more);
        let (second, more, _) = client.read_retired_logins_after(Some("b")).await.unwrap();
        assert_eq!(second.len(), 2);
        assert!(!more);
        assert!(
            drain_retired_logins(
                &mut client,
                logins.iter().map(authorize).collect(),
                |id| Some(HarnessProfile::new(
                    HarnessKind::Codex,
                    if id == "bad" {
                        bad_home.clone()
                    } else {
                        good_home.clone()
                    }
                )),
                |_| Ok(()),
                |_| Ok(true)
            )
            .await
            .1
            .is_err()
        );
        assert_eq!(std::fs::read(good_home.join("auth.json")).unwrap(), bytes);
        assert_eq!(
            client.read_retired_logins_after(None).await.unwrap().0,
            vec![logins[0].clone()]
        );
        client.detach().await.unwrap();
    }

    #[tokio::test]
    async fn credential_sync_retired_grants_retry_failed_adoption_and_ack_only_after_install() {
        let root = tempfile::tempdir().unwrap();
        for kind in [
            HarnessKind::Codex,
            HarnessKind::Kimi,
            HarnessKind::Grok,
            HarnessKind::Muse,
            HarnessKind::Antigravity,
            HarnessKind::Claude,
        ] {
            if kind == HarnessKind::Claude && cfg!(target_os = "macos") {
                continue;
            }
            let home = root.path().join(kind.id());
            std::fs::create_dir_all(&home).unwrap();
            let marker = harness_authentication_marker(kind, &home);
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            // A directory at the canonical file makes adoption fail visibly.
            std::fs::create_dir(&marker).unwrap();
            let mut json = match kind {
                HarnessKind::Codex => serde_json::json!({"last_refresh":"2026-11-25T00:00:00Z"}),
                HarnessKind::Kimi => serde_json::json!({"expires_at":1795564800_i64}),
                HarnessKind::Grok => {
                    serde_json::json!({"issuer::id":{"expires_at":"2026-11-25T00:00:00Z"}})
                }
                HarnessKind::Muse => {
                    serde_json::json!({"providers":{"meta":{"expires_at":1795564800_i64}}})
                }
                HarnessKind::Antigravity => {
                    serde_json::json!({"token":{"expiry":"2026-11-25T00:00:00Z"}})
                }
                HarnessKind::Claude => {
                    serde_json::json!({"claudeAiOauth":{"expiresAt":1795564800000_i64,"refreshToken":"grant"}})
                }
                _ => unreachable!(),
            };
            json["padding"] = serde_json::Value::String("x".repeat(70000));
            let bytes = serde_json::to_vec(&json).unwrap();
            let login = mj_core::credentials::RetiredLoginSnapshot {
                id: "move".into(),
                profile_id: "source-profile".into(),
                harness: kind,
                store_id: "store".into(),
                login_relative_path: kind.credential_file_name().into(),
                bytes: bytes.clone(),
            };
            let receipts = home.join("receipts.json");
            std::fs::write(&receipts, serde_json::to_vec(&vec![login.clone()]).unwrap()).unwrap();
            let spec = CommandSpec::new(
                "python3",
                [
                    "-c".to_owned(),
                    PEER.to_owned(),
                    receipts.to_string_lossy().into_owned(),
                    "93".into(),
                ],
            );
            let mut client = RelayClient::connect(&spec, "retired-test").await.unwrap();
            let profile = HarnessProfile::new(kind, home.clone());
            assert!(
                drain_retired_logins(
                    &mut client,
                    vec![authorize(&login)],
                    |_| Some(profile.clone()),
                    |_| Ok(()),
                    |_| Ok(true)
                )
                .await
                .1
                .is_err()
            );
            assert_eq!(
                client.read_retired_logins_after(None).await.unwrap().0,
                vec![login.clone()]
            );
            std::fs::remove_dir(&marker).unwrap();
            drain_retired_logins(
                &mut client,
                vec![authorize(&login)],
                |_| Some(profile.clone()),
                |_| Ok(()),
                |_| Ok(true),
            )
            .await
            .1
            .unwrap();
            assert_eq!(std::fs::read(&marker).unwrap(), bytes);
            assert!(
                client
                    .read_retired_logins_after(None)
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
            // A lost ack can replay installation without changing canonical bytes.
            std::fs::write(&receipts, serde_json::to_vec(&vec![login.clone()]).unwrap()).unwrap();
            drain_retired_logins(
                &mut client,
                vec![authorize(&login)],
                |_| Some(profile.clone()),
                |_| Ok(()),
                |_| Ok(true),
            )
            .await
            .1
            .unwrap();
            assert_eq!(std::fs::read(&marker).unwrap(), bytes);
            // Equal timestamps do not order distinct rotating grants either.
            let mut tied = login.clone();
            let mut rotated = json.clone();
            match kind {
                HarnessKind::Codex => {
                    rotated["tokens"] = serde_json::json!({"refresh_token":"rotated"})
                }
                HarnessKind::Kimi => rotated["refresh_token"] = "rotated".into(),
                HarnessKind::Grok => rotated["issuer::id"]["refresh_token"] = "rotated".into(),
                HarnessKind::Muse => {
                    rotated["providers"]["meta"]["refresh_token"] = "rotated".into()
                }
                HarnessKind::Antigravity => rotated["token"]["refresh_token"] = "rotated".into(),
                HarnessKind::Claude => rotated["claudeAiOauth"]["refreshToken"] = "rotated".into(),
                _ => unreachable!(),
            }
            tied.bytes = serde_json::to_vec(&rotated).unwrap();
            assert_eq!(
                CredentialSnapshot::of(kind, &tied.bytes).freshness_epoch_ms,
                CredentialSnapshot::of(kind, &bytes).freshness_epoch_ms
            );
            std::fs::write(&receipts, serde_json::to_vec(&vec![tied.clone()]).unwrap()).unwrap();
            assert!(
                drain_retired_logins(
                    &mut client,
                    vec![authorize(&login)],
                    |_| Some(profile.clone()),
                    |_| Ok(()),
                    |_| Ok(true)
                )
                .await
                .1
                .is_err()
            );
            assert_eq!(
                client.read_retired_logins_after(None).await.unwrap().0,
                vec![tied]
            );
            assert_eq!(std::fs::read(&marker).unwrap(), bytes);
            // Unknown, unequal grants have no safe discard ordering.
            let mut ambiguous = login.clone();
            ambiguous.bytes = if kind == HarnessKind::Claude {
                br#"{"claudeAiOauth":{"refreshToken":"unknown-new"}}"#.to_vec()
            } else {
                br#"{"refresh_token":"unknown-new"}"#.to_vec()
            };
            let unknown_canonical = if kind == HarnessKind::Claude {
                br#"{"claudeAiOauth":{"refreshToken":"unknown-old"}}"#.as_slice()
            } else {
                br#"{"refresh_token":"unknown-old"}"#.as_slice()
            };
            std::fs::write(&marker, unknown_canonical).unwrap();
            std::fs::write(
                &receipts,
                serde_json::to_vec(&vec![ambiguous.clone()]).unwrap(),
            )
            .unwrap();
            assert!(
                drain_retired_logins(
                    &mut client,
                    vec![authorize(&login)],
                    |_| Some(profile.clone()),
                    |_| Ok(()),
                    |_| Ok(true)
                )
                .await
                .1
                .is_err()
            );
            assert_eq!(
                client.read_retired_logins_after(None).await.unwrap().0,
                vec![ambiguous]
            );
            assert_eq!(std::fs::read(&marker).unwrap(), unknown_canonical);
            std::fs::write(&marker, &bytes).unwrap();
            std::fs::write(&receipts, serde_json::to_vec(&vec![login.clone()]).unwrap()).unwrap();
            drain_retired_logins(
                &mut client,
                vec![authorize(&login)],
                |_| None,
                |_| Ok(()),
                |_| Ok(true),
            )
            .await
            .1
            .unwrap();
            assert!(
                client
                    .read_retired_logins_after(None)
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
            assert_eq!(std::fs::read(&marker).unwrap(), bytes);
            client.detach().await.unwrap();
            // Pre-93 workers must never receive an unknown retired-login RPC.
            let old = CommandSpec::new(
                "python3",
                [
                    "-c".to_owned(),
                    PEER.to_owned(),
                    receipts.to_string_lossy().into_owned(),
                    "92".into(),
                ],
            );
            let mut client = RelayClient::connect(&old, "retired-test").await.unwrap();
            drain_retired_logins(
                &mut client,
                vec![authorize(&login)],
                |_| panic!("old worker has no snapshots"),
                |_| panic!("old worker cannot prove absence"),
                |_| Ok(true),
            )
            .await
            .1
            .unwrap();
            client.detach().await.unwrap();
        }
    }
}
