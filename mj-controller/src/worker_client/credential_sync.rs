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

/// Which sessions a reconciliation is working on, and the one thing sessions
/// of a profile must not do at once.
///
/// A session is reconciled by at most one pass at a time, so a session whose
/// actor does not answer holds only itself: the next pass skips it and serves
/// the profile's other sessions on schedule. What truly must not overlap is
/// rewriting the profile's canonical login, so that has a lock of its own per
/// profile, held only around the compare-and-write, never across a call to a
/// worker.
#[derive(Clone, Default)]
pub(super) struct SessionsInFlight(Arc<InFlightState>);

#[derive(Default)]
struct InFlightState {
    sessions: std::sync::Mutex<BTreeSet<String>>,
    pulls: std::sync::Mutex<BTreeMap<String, Arc<std::sync::Mutex<()>>>>,
    released: tokio::sync::Notify,
}

/// A session's reconciliation in progress. Dropping it, on any exit path,
/// frees the session.
pub(super) struct SessionClaim {
    state: Arc<InFlightState>,
    session_id: String,
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        self.state
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
        self.state.released.notify_one();
    }
}

impl SessionsInFlight {
    /// The session, unless a pass is already reconciling it.
    pub(super) fn claim(&self, session_id: &str) -> Option<SessionClaim> {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_owned())
            .then(|| SessionClaim {
                state: Arc::clone(&self.0),
                session_id: session_id.to_owned(),
            })
    }

    pub(super) fn is_claimed(&self, session_id: &str) -> bool {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
    }

    /// The lock around rewriting this profile's canonical login.
    pub(super) fn login_pull_lock(&self, profile_id: &str) -> Arc<std::sync::Mutex<()>> {
        Arc::clone(
            self.0
                .pulls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(profile_id.to_owned())
                .or_default(),
        )
    }

    /// Completes when some session has been freed since the last time.
    async fn released(&self) {
        self.0.released.notified().await;
    }
}

pub struct CredentialSyncCoordinator {
    pub(super) handle: CredentialSyncHandle,
    pub(super) results: mpsc::UnboundedReceiver<CredentialSyncResult>,
}

impl CredentialSyncCoordinator {
    #[cfg(test)]
    pub fn spawn() -> Self {
        Self::spawn_inner(SessionRelays::Direct, None, None, unknown_slopcop_login)
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
        )
    }

    fn spawn_inner(
        relays: SessionRelays,
        gate: Option<Arc<crate::recovery_gate::RecoveryGate>>,
        runtime: Option<Arc<crate::daemon::RuntimeState>>,
        slopcop_login: SlopcopLoginSource,
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
            // Sessions are reconciled one pass at a time each; a profile's
            // other sessions are not held up by one that does not answer.
            let in_flight = SessionsInFlight::default();
            let mut queue = VecDeque::<SyncTrigger>::new();
            let mut previous_targets = BTreeMap::<String, CredentialSyncTarget>::new();
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    changed = targets_rx.changed() => {
                        if changed.is_err() { break; }
                        // Compare the publication we consume, not its order.
                        // Removed sessions have nothing left to reconcile.
                        let targets = targets_rx.borrow_and_update();
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
                    // A session finished: a trigger waiting for it can run.
                    _ = in_flight.released() => {}
                    completed = completed_rx.recv() => {
                        let Some(result) = completed else { break };
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
                    // The session whose failure asked for this sync is reached
                    // or the sync waits for it. With every session in flight
                    // there is nothing to serve; the next schedule retries.
                    if trigger
                        .cause
                        .as_ref()
                        .is_some_and(|cause| in_flight.is_claimed(&cause.session_id))
                        || targets
                            .iter()
                            .all(|target| in_flight.is_claimed(&target.session_id))
                    {
                        if trigger.cause.is_some() {
                            deferred.push_back(trigger);
                        }
                        continue;
                    }
                    let completed_tx = completed_tx.clone();
                    let in_flight = in_flight.clone();
                    // The join is awaited so a panicked reconcile is reported
                    // and its profile always leaves the busy set. The reconcile
                    // runs as an ordinary task, never as `Handle::block_on` on
                    // a blocking thread: the scheduler cancels tasks before it
                    // shuts the timer driver, whereas a blocking thread keeps
                    // polling its connect timeouts through runtime shutdown
                    // and panics with "A Tokio 1.x context was found, but it
                    // is being shutdown".
                    let triggered_by = trigger.cause.as_ref().map(|cause| cause.session_id.clone());
                    let gate = gate.clone();
                    let relays = relays.clone();
                    let runtime = runtime.clone();
                    tokio::spawn(async move {
                        let joined = tokio::spawn(async move {
                            reconcile_profile_guarded(
                                &relays,
                                &targets,
                                triggered_by.as_deref(),
                                gate.as_ref(),
                                runtime.as_ref(),
                                slopcop_login,
                                &in_flight,
                            )
                            .await
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

#[cfg(all(test, unix))]
pub(super) async fn reconcile_profile(
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
) -> Vec<CredentialSyncOutcome> {
    reconcile_profile_with(&SessionsInFlight::default(), targets, triggered_by).await
}

#[cfg(all(test, unix))]
pub(super) async fn reconcile_profile_with(
    in_flight: &SessionsInFlight,
    targets: &[CredentialSyncTarget],
    triggered_by: Option<&str>,
) -> Vec<CredentialSyncOutcome> {
    reconcile_profile_guarded(
        &SessionRelays::Direct,
        targets,
        triggered_by,
        None,
        None,
        unknown_slopcop_login,
        in_flight,
    )
    .await
}

/// Reconcile one profile with every live session that runs it.
///
/// A pull makes every other session's copy stale by definition, so the pass
/// runs again once with the new canonical bytes. Two passes are enough: the
/// second cannot pull anything the first did not already see unless a harness
/// refreshed mid-cycle, and that lands in the next cycle.
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
    in_flight: &SessionsInFlight,
) -> Vec<CredentialSyncOutcome> {
    // Sessions another pass is already working on are that pass's to finish.
    // The claim that decides is taken per session below; this only spares the
    // work of preparing for sessions that will be skipped.
    let targets = targets
        .iter()
        .filter(|target| !in_flight.is_claimed(&target.session_id))
        .cloned()
        .collect::<Vec<_>>();
    let targets = targets.as_slice();
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
    // Each role-scoped session's mint is an independent AWS call that can take
    // seconds. They all start now and run together, and each session is
    // reconciled as its own mint completes, so one slow or failing call never
    // delays another session's install. The cache also drops expired
    // credentials here, so a stopped session's secrets do not outlive them.
    crate::aws_assume_role::ScopedAwsCredentials::shared().sweep_expired();
    let mut minting = futures::stream::FuturesUnordered::new();
    for (index, target) in targets.iter().enumerate() {
        if let Some(mint) = start_scoped_mint(target) {
            minting.push(async move { (index, mint.await) });
        }
    }
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
        in_flight,
    };
    // What each session's AWS credentials turned out to be, as they arrive.
    let mut aws_credentials = targets
        .iter()
        .map(|target| match &target.aws_credentials {
            None => Some(AwsSync::Unmanaged),
            Some(source) if source.role.is_some() => None,
            Some(_) => Some(match &unscoped_aws_credentials {
                Ok(Some(credentials)) => AwsSync::Install(credentials.clone()),
                Ok(None) => AwsSync::Unmanaged,
                Err(error) => AwsSync::Unavailable {
                    error: format!("{error:#}"),
                    scoped_required: false,
                    scoped_held: None,
                },
            }),
        })
        .collect::<Vec<Option<AwsSync>>>();
    let mut outcomes = BTreeMap::<String, CredentialSyncOutcome>::new();
    for pass in 0..2 {
        let mut pulled = false;
        // Every session is reconciled as soon as its own AWS credentials are
        // known, and all of them together: a slow mint or a slow session actor
        // holds only itself. Results are taken as they finish.
        let mut running = futures::stream::FuturesUnordered::new();
        let dispatch = |index: usize, aws: AwsSync, reported: bool| {
            let target = &targets[index];
            let context = &pass_context;
            async move {
                (
                    index,
                    reconcile_target(context, target, aws, pass, reported).await,
                )
            }
        };
        for index in 0..targets.len() {
            if let Some(aws) = &aws_credentials[index] {
                let reported = outcomes.contains_key(&targets[index].session_id);
                running.push(dispatch(index, aws.clone(), reported));
            }
        }
        loop {
            tokio::select! {
                Some((index, minted)) = futures::StreamExt::next(&mut minting),
                    if !minting.is_empty() =>
                {
                    let aws = match minted
                        .context("AWS role assumption task stopped")
                        .and_then(|result| result)
                    {
                        Ok(credentials) => AwsSync::Install(credentials),
                        Err(error) => AwsSync::Unavailable {
                            error: format!("{error:#}"),
                            scoped_required: true,
                            // Scoped credentials the controller itself issued
                            // and the worker may still hold.
                            scoped_held: targets[index].aws_credentials.as_ref().and_then(
                                |source| {
                                    crate::aws_assume_role::ScopedAwsCredentials::shared()
                                        .cached(&targets[index].session_id, source)
                                },
                            ),
                        },
                    };
                    aws_credentials[index] = Some(aws.clone());
                    let reported = outcomes.contains_key(&targets[index].session_id);
                    running.push(dispatch(index, aws, reported));
                }
                Some((index, result)) = futures::StreamExt::next(&mut running),
                    if !running.is_empty() =>
                {
                    pulled |= result.pulled;
                    if let Some(outcome) = result.outcome {
                        outcomes.insert(targets[index].session_id.clone(), outcome);
                    }
                }
                else => break,
            }
        }
        if !pulled || pass == 1 {
            break;
        }
    }
    outcomes.into_values().collect()
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
        /// Unexpired scoped credentials the controller issued for this
        /// session, which the worker may still hold. Keeping them until they
        /// expire is safe: they are limited to the role.
        scoped_held: Option<mj_core::aws_credentials::AwsCredentials>,
    },
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
    pub(super) failures: Vec<String>,
}

/// What one reconcile pass shares between its sessions.
struct PassContext<'a> {
    relays: &'a SessionRelays,
    gate: Option<&'a Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&'a Arc<crate::daemon::RuntimeState>>,
    skills: &'a Arc<CanonicalSkills>,
    slopcop_login: &'a HostSlopcopLogin,
    triggered_by: Option<&'a str>,
    in_flight: &'a SessionsInFlight,
}

/// What reconciling one session in one pass found.
struct TargetResult {
    outcome: Option<CredentialSyncOutcome>,
    /// A fresher copy of the profile's login was pulled from the session.
    pulled: bool,
}

/// The failures of a session's sync steps, as one message. A lost connection
/// fails every step the same way, so a message is said once.
fn joined_failures(failures: &[String]) -> Option<String> {
    let mut unique = Vec::<&str>::new();
    for failure in failures {
        if !unique.contains(&failure.as_str()) {
            unique.push(failure);
        }
    }
    (!unique.is_empty()).then(|| unique.join("; "))
}

/// One session's reconciliation in one pass. `reported` says an earlier pass
/// already recorded an outcome for it.
async fn reconcile_target(
    context: &PassContext<'_>,
    target: &CredentialSyncTarget,
    aws_credentials: AwsSync,
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
        in_flight,
    } = context;
    // Held until this session's reconciliation ends, on any path.
    let Some(_claim) = in_flight.claim(&target.session_id) else {
        return TargetResult {
            outcome: None,
            pulled: false,
        };
    };
    // A failed GitHub token lookup is that step's failure, not the session's:
    // the AWS credentials, skills and logins do not depend on it.
    let github_token = |target: &CredentialSyncTarget| {
        let (session_id, github) = (target.session_id.clone(), target.github.clone());
        let wanted = target.sync_github_token;
        async move {
            if !wanted {
                return GithubSync::Known(None);
            }
            match crate::controller::github_token_for_session(session_id, github).await {
                Ok(token) => GithubSync::Known(token),
                Err(error) => GithubSync::Unavailable(format!("{error:#}")),
            }
        }
    };
    let result = match *gate {
        Some(gate) => {
            gate.run_background(&target.session_id, async {
                let current = if let Some(runtime) = *runtime {
                    runtime.credential_target_is_current(target)?
                } else {
                    let candidate = target.clone();
                    tokio::task::spawn_blocking(move || {
                        // Read only: cancellation may leave this blocking
                        // read finishing after admission has been released.
                        let controller = crate::controller::Controller {
                            config: mj_core::config::Config::load()?,
                            state: crate::database::load_state()?,
                        };
                        Ok::<_, anyhow::Error>(crate::pollers::credential_sync_target_is_current(
                            controller, &candidate,
                        ))
                    })
                    .await
                    .context("reload credential sync target")??
                };
                if !current {
                    return Ok(None);
                }
                let github = github_token(target).await;
                reconcile_session(
                    relays,
                    target,
                    skills,
                    &github,
                    &aws_credentials,
                    slopcop_login,
                    &in_flight.login_pull_lock(&target.profile_id),
                )
                .await
            })
            .await
            .unwrap_or_else(|| Ok(None))
        }
        None => {
            let github = github_token(target).await;
            reconcile_session(
                relays,
                target,
                skills,
                &github,
                &aws_credentials,
                slopcop_login,
                &in_flight.login_pull_lock(&target.profile_id),
            )
            .await
        }
    };
    match result {
        // Deferral is not a successful credential check: in particular
        // it must not mark an authentication-triggered login refused.
        Ok(None) => TargetResult {
            outcome: None,
            pulled: false,
        },
        Ok(Some(SessionSync { actions, failures }))
            if actions.is_empty()
                && failures.is_empty()
                && (*triggered_by != Some(target.session_id.as_str()) || reported) =>
        {
            TargetResult {
                outcome: None,
                pulled: false,
            }
        }
        Ok(Some(SessionSync { actions, failures })) => {
            let step_failure = joined_failures(&failures);
            if let Some(failure) = &step_failure {
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
                outcome: Some(CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Ok(actions),
                    step_failure,
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
                outcome: Some(CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Err(format!("{error:#}")),
                    step_failure: None,
                }),
            }
        }
    }
}

/// Start the AWS role assumption for a session whose profile names a role,
/// from the shared cache or a new `AssumeRole`; it runs on a blocking thread
/// from the moment this returns. `None` for a session without a role, which
/// shares the profile's unscoped export. A failure is this session's failed
/// outcome: the worker keeps the credentials it has, and no path falls back to
/// the unscoped export.
fn start_scoped_mint(
    target: &CredentialSyncTarget,
) -> Option<tokio::task::JoinHandle<Result<mj_core::aws_credentials::AwsCredentials>>> {
    let source = target
        .aws_credentials
        .clone()
        .filter(|source| source.role.is_some())?;
    let (profile_id, session_id) = (target.profile_id.clone(), target.session_id.clone());
    Some(tokio::task::spawn_blocking(move || {
        crate::aws_assume_role::ScopedAwsCredentials::shared().credentials_for(
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
    /// Held around rewriting the profile's canonical login.
    pub(super) login_pull: Arc<std::sync::Mutex<()>>,
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
            login_pull: Arc::default(),
        }
    }
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
    login_pull: &Arc<std::sync::Mutex<()>>,
) -> Result<Option<SessionSync>> {
    let mut canonical = SessionCanonical::read(target, skills.clone(), github.clone());
    canonical.login_pull = Arc::clone(login_pull);
    canonical.aws = aws.clone();
    canonical.slopcop_login = slopcop_login.clone();
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
                    canonical,
                    reply,
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
            let result = reconcile_on(&mut client, target, &canonical).await;
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
    let mut actions = Vec::new();
    let mut failures = Vec::new();
    let mut note = |step: &str, error: String| failures.push(format!("{step}: {error}"));

    match reconcile_aws(client, &canonical.aws).await {
        Ok(action) => actions.extend(action),
        Err(error) => note("AWS credentials withdrawal", format!("{error:#}")),
    }
    if let AwsSync::Unavailable { error, .. } = &canonical.aws {
        note("AWS credentials", error.clone());
    }
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
                    &canonical.login_pull,
                    &mut actions,
                )
                .await
                {
                    note("harness login", format!("{error:#}"));
                }
            }
            Err(error) => note("harness login", error.clone()),
        }
    }
    match canonical
        .skills
        .for_format(client.skills_archive_format(), target.skills_scope)
    {
        Ok(skills) => match reconcile_skills(client, target, skills).await {
            Ok(true) => actions.push(CredentialSyncAction::SkillsPushed),
            Ok(false) => {}
            Err(error) => note("skills", format!("{error:#}")),
        },
        Err(error) => note("skills", format!("{error:#}")),
    }
    if target.sync_github_token {
        match &canonical.github {
            GithubSync::Known(token) => {
                match reconcile_github_token(client, target, token.as_deref()).await {
                    Ok(action) => actions.extend(action),
                    Err(error) => note("GitHub token", format!("{error:#}")),
                }
            }
            GithubSync::Unavailable(error) => note("GitHub token", error.clone()),
        }
    }
    match reconcile_slopcop_login(client, target, &canonical.slopcop_login).await {
        Ok(action) => actions.extend(action),
        Err(error) => note("slopcop login", format!("{error:#}")),
    }
    Ok(SessionSync { actions, failures })
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
            if client.aws_credentials_state().await? == expected {
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
            scoped_held,
            scoped_required: true,
            ..
        } => {
            let held = client.aws_credentials_state().await?;
            // Credentials this controller issued for the role stay until they
            // expire. Anything else the worker holds may be the host's own,
            // installed before the role was configured, and static host keys
            // never expire.
            if let Some(scoped) = scoped_held
                && mj_core::aws_credentials::snapshot(&scoped.encode()?) == held
            {
                return Ok(None);
            }
            anyhow::ensure!(
                client.protocol_version() >= mj_core::relay::RELAY_AWS_CREDENTIALS_REMOVAL_PROTOCOL,
                "the session's worker (relay protocol {}) cannot withdraw AWS credentials; restart the session so a current worker replaces it",
                client.protocol_version()
            );
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
struct CredentialSyncJob {
    target: CredentialSyncTarget,
    canonical: SessionCanonical,
    reply: tokio::sync::oneshot::Sender<Result<SessionSync>>,
}

impl RelayConnectionJob for CredentialSyncJob {
    fn run<'a>(self: Box<Self>, client: &'a mut RelayClient) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let Self {
                target,
                canonical,
                reply,
            } = *self;
            let result = reconcile_on(client, &target, &canonical).await;
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
    login_pull: &std::sync::Mutex<()>,
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
            // Sessions of a profile are reconciled together, and only this
            // rewrite must not overlap: compare against the file as it is now,
            // since another session may have pulled something fresher while
            // this one's bytes were in flight, and write only if this is
            // still the fresher copy.
            let _rewriting = login_pull
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (now, _) = read_credential_file(target.harness, canonical_path)?;
            if matches!(reconcile(&now, &session), SyncAction::Pull) {
                write_credential_file(target.harness, canonical_path, &bytes).with_context(
                    || {
                        format!(
                            "install fresher credentials from session {} for profile {}",
                            target.session_id, target.profile_id
                        )
                    },
                )?;
                actions.push(CredentialSyncAction::Pulled);
            }
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
