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
/// actor until it has run or been dropped: the session's background admission,
/// its turn among the session's steps, and, for a role-scoped renewal, its
/// install lock. The job holds them, not the task that queued it, so stopping
/// that task (its target changed, the session went away) cannot hand the
/// session to a newer job while the older one is still queued or running.
/// Released together when the last holder drops it, admission first.
#[derive(Clone)]
pub(super) struct JobHold(Arc<HeldSlots>);

struct HeldSlots {
    admission: Option<crate::recovery_gate::RecoveryAttempt>,
    runtime: Option<Arc<crate::daemon::RuntimeState>>,
    _turn: Option<tokio::sync::OwnedMutexGuard<()>>,
    _installing: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl JobHold {
    pub(super) fn new(
        admission: Option<crate::recovery_gate::RecoveryAttempt>,
        turn: Option<tokio::sync::OwnedMutexGuard<()>>,
        installing: Option<tokio::sync::OwnedMutexGuard<()>>,
        runtime: Option<Arc<crate::daemon::RuntimeState>>,
    ) -> Self {
        Self(Arc::new(HeldSlots {
            admission,
            runtime,
            _turn: turn,
            _installing: installing,
        }))
    }

    /// The job runs under the gate's admission, so the gate's lifecycle
    /// operations can cancel it and its target is checked as current.
    fn gated(&self) -> bool {
        self.0.admission.is_some()
    }

    /// A lifecycle operation asked the admitted work to stop.
    fn cancelled(&self) -> bool {
        self.0
            .admission
            .as_ref()
            .is_some_and(|admission| admission.load(std::sync::atomic::Ordering::Acquire))
    }
}

/// One holder at a time per session. Within a pass, a session's login step and
/// its AWS install both need the session's single background admission slot
/// (`RecoveryGate::try_start`), which turns a second taker away; taking turns
/// here first means the two never contend for it, so neither is skipped
/// because the other held it. A step waits only for the same session's other
/// step, never another session's. The lock is released on every exit path.
/// A second instance orders a session's role-scoped renewals themselves.
#[derive(Clone, Default)]
pub(super) struct SessionSteps(
    Arc<std::sync::Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
);

impl SessionSteps {
    pub(super) async fn enter(&self, session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Finished sessions leave no entry behind.
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(session_id).and_then(std::sync::Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(session_id.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }
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
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<CredentialSyncResult>();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + SYNC_INTERVAL,
                SYNC_INTERVAL,
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // A pull rewrites the canonical file, so one profile is never
            // reconciled twice at once.
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
                        let Some(result) = completed else { break };
                        busy.remove(&result.profile_id);
                        if result.trigger.is_some()
                            || result.failure.is_some()
                            || !result.outcomes.is_empty()
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
                            let mut outcomes = reconcile_profile_guarded(
                                &relays,
                                &targets,
                                triggered_by.as_deref(),
                                gate.as_ref(),
                                runtime.as_ref(),
                                slopcop_login,
                                &steps,
                            )
                            .await;
                            if let Some(report) = aws_report {
                                // The renewal task answers once its cycle ends;
                                // none answers when the task was stopped, and
                                // its replacement reports for itself.
                                if let Ok(Some(report)) = report.await {
                                    merge_report(&mut outcomes, report);
                                }
                            }
                            outcomes
                        })
                        .await;
                        let (failure, outcomes) = match joined {
                            Ok(outcomes) => (None, outcomes),
                            Err(error) => (Some(format!("sync task stopped: {error}")), Vec::new()),
                        };
                        let profile_id = trigger.profile_id.clone();
                        if completed_tx
                            .send(CredentialSyncResult {
                                profile_id: trigger.profile_id,
                                trigger: trigger.cause,
                                failure,
                                outcomes,
                                partial: false,
                            })
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
    let login = reconcile_profile_guarded(
        &SessionRelays::Direct,
        targets,
        triggered_by,
        None,
        None,
        unknown_slopcop_login,
        &steps,
    );
    let cycles = futures::future::join_all(
        targets
            .iter()
            .filter(|target| {
                target
                    .aws_credentials
                    .as_ref()
                    .is_some_and(|source| source.role.is_some())
            })
            .map(|target| {
                aws_cycle(
                    &SessionRelays::Direct,
                    None,
                    None,
                    target,
                    &steps,
                    None,
                    false,
                )
            }),
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
    relays: &SessionRelays,
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
    gate: Option<&Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&Arc<crate::daemon::RuntimeState>>,
    slopcop_login: SlopcopLoginSource,
    steps: &SessionSteps,
) -> Vec<CredentialSyncOutcome> {
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
    // operation above.
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
/// whole login step failed keeps that failure as a step failure (under the
/// login pass's steps), so the other step's actions are recorded beside it.
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
        Err(detail) => {
            outcome.step_failures.insert(
                0,
                StepFailure {
                    step: SyncStep::HarnessLogin,
                    detail,
                },
            );
            outcome.outcome = Ok(actions);
        }
    }
    outcome.step_failures.extend(failures);
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
        Ok(Some(SessionSync { actions, failures })) => {
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
            } => SyncStep::Aws,
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
async fn admitted<T, F: std::future::Future<Output = Result<Option<T>>>>(
    gate: Option<&Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&Arc<crate::daemon::RuntimeState>>,
    target: &CredentialSyncTarget,
    turn: tokio::sync::OwnedMutexGuard<()>,
    installing: Option<tokio::sync::OwnedMutexGuard<()>>,
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
        Ok(Some(SessionSync { actions, failures }))
            if actions.is_empty()
                && failures.is_empty()
                && (*triggered_by != Some(target.session_id.as_str()) || reported) =>
        {
            TargetResult {
                outcome: None,
                pulled: false,
                clean: true,
            }
        }
        Ok(Some(SessionSync { actions, failures })) => {
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
pub(super) async fn aws_cycle(
    relays: &SessionRelays,
    gate: Option<&Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&Arc<crate::daemon::RuntimeState>>,
    target: &CredentialSyncTarget,
    steps: &SessionSteps,
    installing: Option<tokio::sync::OwnedMutexGuard<()>>,
    force_new: bool,
) -> (String, Result<Option<SessionSync>>) {
    let session_id = target.session_id.clone();
    // A session that is gone, or replaced, is not given credentials. This is
    // checked before the mint, which stays outside admission: an AssumeRole
    // call can take seconds and must not hold the session's slot.
    // (Without a gate, as in tests of the direct path, nothing says which
    // sessions are current, here or under admission.)
    if gate.is_some() {
        match target_is_current(runtime, target).await {
            Ok(true) => {}
            Ok(false) => return (session_id, Ok(None)),
            Err(error) => return (session_id, Err(error)),
        }
    }
    let aws = match start_scoped_mint(target, force_new) {
        Some(mint) => match mint
            .await
            .context("AWS role assumption task stopped")
            .and_then(|result| result)
        {
            Ok(credentials) => AwsSync::Install(credentials),
            Err(error) => AwsSync::Unavailable {
                error: format!("{error:#}"),
                scoped_required: true,
                expected_scope: ScopeExpectation::of(target),
            },
        },
        None => AwsSync::Unmanaged,
    };
    // Takes its turn with the same session's login step instead of competing
    // with it for the admission slot, where a refusal would skip the install.
    let turn = steps.enter(&target.session_id).await;
    let result = admitted(gate, runtime, target, turn, installing, |hold| async move {
        run_on_session(relays, target, SyncWork::Aws(Box::new(aws)), hold).await
    })
    .await;
    (session_id, result)
}

/// What a session's renewal task needs, shared by all of them.
#[derive(Clone)]
struct RenewalEnv {
    relays: SessionRelays,
    gate: Option<Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<Arc<crate::daemon::RuntimeState>>,
    steps: SessionSteps,
    /// Orders one session's renewals, so a replaced task's cycle finishes
    /// before its successor's begins.
    installs: SessionSteps,
    results: mpsc::UnboundedSender<CredentialSyncResult>,
    /// How long after a cycle that did not complete cleanly the next is tried.
    retry: Duration,
}

/// A request to renew a session's credentials now, with new ones, and to
/// answer with the cycle's outcome.
struct RenewalWake {
    reply: tokio::sync::oneshot::Sender<Option<CredentialSyncOutcome>>,
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
struct RenewalTasks(BTreeMap<String, RenewalTask>);

impl RenewalTasks {
    fn sync(&mut self, targets: &[CredentialSyncTarget], env: &RenewalEnv) {
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
    fn wake(
        &self,
        session_id: &str,
    ) -> Option<tokio::sync::oneshot::Receiver<Option<CredentialSyncOutcome>>> {
        let task = self.0.get(session_id)?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        task.wakes.send(RenewalWake { reply }).ok()?;
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
async fn renew_session_aws(
    env: RenewalEnv,
    target: CredentialSyncTarget,
    mut wakes: mpsc::UnboundedReceiver<RenewalWake>,
) {
    let mut failed_before = false;
    let mut wake = None::<RenewalWake>;
    loop {
        let mut delay = env.retry;
        let installing = env.installs.enter(&target.session_id).await;
        let (_, result) = aws_cycle(
            &env.relays,
            env.gate.as_ref(),
            env.runtime.as_ref(),
            &target,
            &env.steps,
            Some(installing),
            wake.is_some(),
        )
        .await;
        let completed = matches!(&result, Ok(Some(sync)) if sync.failures.is_empty());
        let mut outcomes = BTreeMap::new();
        merge_aws_cycle(&mut outcomes, &target.session_id, result);
        let failed = outcomes
            .values()
            .any(|outcome| !outcome.step_failures.is_empty() || outcome.outcome.is_err());
        let ended = failed_before && completed;
        let mut report = outcomes.remove(&target.session_id);
        // A trigger that woke this cycle carries its outcome, and the task
        // then says only that an earlier failure ended. Said when there is
        // something to say: work done, a failure, or the end of a failure, so
        // a persistent one is not repeated.
        if let Some(wake) = wake.take() {
            report = match wake.reply.send(report) {
                Ok(()) => None,
                Err(undelivered) => undelivered,
            };
        }
        if let Some(outcome) = report.or_else(|| {
            ended.then(|| CredentialSyncOutcome {
                session_id: target.session_id.clone(),
                outcome: Ok(Vec::new()),
                step_failures: Vec::new(),
            })
        }) {
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
        failed_before = failed;
        if completed
            && let Some(source) = &target.aws_credentials
            && let Some(until) = crate::aws_assume_role::ScopedAwsCredentials::shared()
                .renew_in(&target.session_id, source)
        {
            delay = until;
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            next = wakes.recv() => match next {
                Some(next) => wake = Some(next),
                None => return,
            },
        }
    }
}

/// Start the AWS role assumption for a session whose profile names a role,
/// from the shared cache or a new `AssumeRole`; it runs on a blocking thread
/// from the moment this returns. `None` for a session without a role, which
/// shares the profile's unscoped export. A failure is this session's failed
/// outcome: the worker keeps the credentials it has, and no path falls back to
/// the unscoped export. `force_new` drops the cached credentials first, for a
/// session whose credentials may have been refused.
fn start_scoped_mint(
    target: &CredentialSyncTarget,
    force_new: bool,
) -> Option<tokio::task::JoinHandle<Result<mj_core::aws_credentials::AwsCredentials>>> {
    let source = target
        .aws_credentials
        .clone()
        .filter(|source| source.role.is_some())?;
    let (profile_id, session_id) = (target.profile_id.clone(), target.session_id.clone());
    Some(tokio::task::spawn_blocking(move || {
        let cache = crate::aws_assume_role::ScopedAwsCredentials::shared();
        if force_new {
            cache.forget(&session_id, &source);
        }
        cache.credentials_for(
            &profile_id,
            &session_id,
            &source,
            &crate::targets::BoundedProcessExecutor::new(Duration::from_secs(30)),
        )
    }))
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
            let (reply, response) = tokio::sync::oneshot::channel();
            handle
                .run_on_connection(Box::new(CredentialSyncJob {
                    target: target.clone(),
                    work,
                    reply,
                    hold,
                }))
                .await;
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
            };
            reconcile_aws_step(client, &aws, &mut sync).await;
            Ok(sync)
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
    };
    reconcile_aws_step(client, &canonical.aws, &mut sync).await;
    let SessionSync { actions, failures } = &mut sync;
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
    let failed = |detail: String| StepFailure {
        step: aws.step(),
        detail,
    };
    match reconcile_aws(client, aws).await {
        Ok(action) => sync.actions.extend(action),
        Err(error) => sync.failures.push(failed(format!("{step}: {error:#}"))),
    }
    if let AwsSync::Unavailable { error, .. } = aws {
        sync.failures
            .push(failed(format!("AWS credentials: {error}")));
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
        } => {
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
                // The record is the session's to write; only one the
                // controller made, for this very secret, counts.
                && crate::aws_assume_role::scope_is_authentic(&recorded, &held.fingerprint)
            {
                return Ok(None);
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
            // lifecycle operation may have cancelled its admission. The work
            // is then dropped, and the hold with it.
            if reply.is_closed() || hold.cancelled() {
                tracing::debug!(
                    session_id = %target.session_id,
                    "credential sync dropped: no longer wanted"
                );
                return;
            }
            if hold.gated() {
                match target_is_current(hold.0.runtime.as_ref(), &target).await {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = reply.send(Err(anyhow::Error::new(RelayJobDeferred(
                            "the session changed before the credential sync ran",
                        ))));
                        return;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                }
            }
            let result = run_work(client, &target, work).await;
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
