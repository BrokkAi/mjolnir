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
            // A pull rewrites the canonical file, so one profile is never
            // reconciled twice at once.
            let mut busy = BTreeSet::<String>::new();
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
    reconcile_profile_guarded(
        &SessionRelays::Direct,
        targets,
        triggered_by,
        None,
        None,
        unknown_slopcop_login,
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
        // The first pass takes sessions in the order their credentials are
        // ready; the second sees every result.
        let mut ready = (0..targets.len())
            .filter(|index| aws_credentials[*index].is_some())
            .collect::<std::collections::VecDeque<_>>();
        loop {
            let index = match ready.pop_front() {
                Some(index) => index,
                None => {
                    let Some((index, minted)) = futures::StreamExt::next(&mut minting).await else {
                        break;
                    };
                    aws_credentials[index] = Some(
                        match minted
                            .context("AWS role assumption task stopped")
                            .and_then(|result| result)
                        {
                            Ok(credentials) => AwsSync::Install(credentials),
                            Err(error) => AwsSync::Unavailable {
                                error: format!("{error:#}"),
                                scoped_required: true,
                                // Scoped credentials the controller itself
                                // issued and the worker may still hold.
                                scoped_held: targets[index].aws_credentials.as_ref().and_then(
                                    |source| {
                                        crate::aws_assume_role::ScopedAwsCredentials::shared()
                                            .cached(&targets[index].session_id, source)
                                    },
                                ),
                            },
                        },
                    );
                    index
                }
            };
            let target = &targets[index];
            let aws = aws_credentials[index]
                .as_ref()
                .expect("credentials arrived");
            reconcile_target(&pass_context, target, aws, pass, &mut outcomes, &mut pulled).await;
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

/// What reconciling one session did: the actions taken, and the failure of
/// its AWS step, which does not stop the other steps.
pub(super) struct SessionSync {
    pub(super) actions: Vec<CredentialSyncAction>,
    pub(super) aws_error: Option<String>,
}

/// What one reconcile pass shares between its sessions.
struct PassContext<'a> {
    relays: &'a SessionRelays,
    gate: Option<&'a Arc<crate::recovery_gate::RecoveryGate>>,
    runtime: Option<&'a Arc<crate::daemon::RuntimeState>>,
    skills: &'a Arc<CanonicalSkills>,
    slopcop_login: &'a HostSlopcopLogin,
    triggered_by: Option<&'a str>,
}

/// One session's reconciliation in one pass, recorded in `outcomes`.
async fn reconcile_target(
    context: &PassContext<'_>,
    target: &CredentialSyncTarget,
    aws_credentials: &AwsSync,
    pass: usize,
    outcomes: &mut BTreeMap<String, CredentialSyncOutcome>,
    pulled: &mut bool,
) {
    let PassContext {
        relays,
        gate,
        runtime,
        skills,
        slopcop_login,
        triggered_by,
    } = context;
    let result = match *gate {
        Some(gate) => gate
            .run_background(&target.session_id, async {
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
                let github_token = if target.sync_github_token {
                    crate::controller::github_token_for_session(
                        target.session_id.clone(),
                        target.github.clone(),
                    )
                    .await?
                } else {
                    None
                };
                reconcile_session(
                    relays,
                    target,
                    skills,
                    github_token.as_deref(),
                    aws_credentials,
                    slopcop_login,
                )
                .await
            })
            .await
            .unwrap_or_else(|| Ok(None)),
        None => {
            let github_token = if target.sync_github_token {
                crate::controller::github_token_for_session(
                    target.session_id.clone(),
                    target.github.clone(),
                )
                .await
            } else {
                Ok(None)
            };
            match github_token {
                Ok(github_token) => {
                    reconcile_session(
                        relays,
                        target,
                        skills,
                        github_token.as_deref(),
                        aws_credentials,
                        slopcop_login,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
        }
    };
    match result {
        // Deferral is not a successful credential check: in particular
        // it must not mark an authentication-triggered login refused.
        Ok(None) => {}
        Ok(Some(SessionSync {
            actions,
            aws_error: Some(error),
        })) => {
            // The AWS step failed; the session's other steps ran.
            *pulled |= actions.contains(&CredentialSyncAction::Pulled);
            tracing::warn!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                pass = pass + 1,
                error = %error,
                "AWS credential synchronization failed for relay session"
            );
            outcomes.insert(
                target.session_id.clone(),
                CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Err(error),
                },
            );
        }
        Ok(Some(SessionSync { actions, .. }))
            if actions.is_empty()
                && (*triggered_by != Some(target.session_id.as_str())
                    || outcomes.contains_key(&target.session_id)) => {}
        Ok(Some(SessionSync { actions, .. })) => {
            *pulled |= actions.contains(&CredentialSyncAction::Pulled);
            outcomes.insert(
                target.session_id.clone(),
                CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Ok(actions),
                },
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id = %target.session_id,
                profile_id = %target.profile_id,
                pass = pass + 1,
                error = %error,
                "credential synchronization failed for relay session"
            );
            outcomes.insert(
                target.session_id.clone(),
                CredentialSyncOutcome {
                    session_id: target.session_id.clone(),
                    outcome: Err(format!("{error:#}")),
                },
            );
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

/// What one session is reconciled against, read on the controller before its
/// worker is asked anything.
pub(super) struct SessionCanonical {
    pub(super) credential_path: std::path::PathBuf,
    pub(super) credential: CredentialSnapshot,
    pub(super) credential_bytes: Vec<u8>,
    pub(super) skills: Arc<CanonicalSkills>,
    pub(super) github_token: Option<String>,
    pub(super) aws: AwsSync,
    pub(super) slopcop_login: HostSlopcopLogin,
}

impl SessionCanonical {
    pub(super) fn read(
        target: &CredentialSyncTarget,
        skills: Arc<CanonicalSkills>,
        github_token: Option<&str>,
    ) -> Result<Self> {
        let credential_path = harness_authentication_marker(target.harness, &target.profile_home);
        // A login database is read where the harness keeps it; the profile
        // home holds no copy of it.
        let (credential, credential_bytes) = if target.harness.keeps_logins_in_database() {
            match crate::opencode_logins::home_logins(&target.profile_home)? {
                Some(bytes) => (CredentialSnapshot::of(target.harness, &bytes), bytes),
                None => (CredentialSnapshot::absent(), Vec::new()),
            }
        } else {
            read_credential_file(target.harness, &credential_path)?
        };
        Ok(Self {
            credential_path,
            credential,
            credential_bytes,
            skills,
            github_token: github_token.map(ToOwned::to_owned),
            aws: AwsSync::Unmanaged,
            slopcop_login: HostSlopcopLogin::Unknown,
        })
    }
}

/// Reconcile one session. Returns every action taken, where an empty list
/// means the copies already agree, or `None` when the session could not be
/// reached this cycle because its actor had no connection to lend.
pub(super) async fn reconcile_session(
    relays: &SessionRelays,
    target: &CredentialSyncTarget,
    skills: &Arc<CanonicalSkills>,
    github_token: Option<&str>,
    aws: &AwsSync,
    slopcop_login: &HostSlopcopLogin,
) -> Result<Option<SessionSync>> {
    let mut canonical = SessionCanonical::read(target, skills.clone(), github_token)?;
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
pub(super) async fn reconcile_on(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical: &SessionCanonical,
) -> Result<SessionSync> {
    let mut actions = Vec::new();
    // The AWS step reports its own failure and never stops the steps below:
    // the GitHub token, the skills and the slopcop login do not depend on STS.
    let aws_error = match reconcile_aws(client, &canonical.aws).await {
        Ok(action) => {
            actions.extend(action);
            match &canonical.aws {
                AwsSync::Unavailable { error, .. } => Some(error.clone()),
                _ => None,
            }
        }
        Err(error) => Some(match &canonical.aws {
            AwsSync::Unavailable {
                error: unavailable, ..
            } => {
                format!(
                    "{unavailable}; and the session's AWS credentials could not be withdrawn: {error:#}"
                )
            }
            _ => format!("{error:#}"),
        }),
    };
    let skills = canonical
        .skills
        .for_format(client.skills_archive_format(), target.skills_scope)?;
    actions.extend(
        reconcile_connected(
            client,
            target,
            &canonical.credential_path,
            &canonical.credential,
            &canonical.credential_bytes,
            skills,
            canonical.github_token.as_deref(),
        )
        .await?,
    );
    // Last, so a slopcop failure cannot hold back the harness's own login.
    if let Some(action) = reconcile_slopcop_login(client, target, &canonical.slopcop_login).await? {
        actions.push(action);
    }
    Ok(SessionSync { actions, aws_error })
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
            if !held.present {
                return Ok(None);
            }
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
            let removed = client.remove_aws_credentials().await?;
            anyhow::ensure!(
                !removed.present,
                "the session kept its AWS credentials after removal"
            );
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

pub(super) async fn reconcile_connected(
    client: &mut RelayClient,
    target: &CredentialSyncTarget,
    canonical_path: &Path,
    canonical: &CredentialSnapshot,
    canonical_bytes: &[u8],
    canonical_skills: &mj_core::skills::SkillsArchive,
    github_token: Option<&str>,
) -> Result<Vec<CredentialSyncAction>> {
    let mut actions = Vec::new();
    // Profiles without a harness login file already receive credentials
    // through their environment or target credential chain.
    if !target.skips_login_file_sync {
        reconcile_credentials(
            client,
            target,
            canonical_path,
            canonical,
            canonical_bytes,
            &mut actions,
        )
        .await?;
    }
    if reconcile_skills(client, target, canonical_skills).await? {
        actions.push(CredentialSyncAction::SkillsPushed);
    }
    if target.sync_github_token
        && let Some(action) = reconcile_github_token(client, target, github_token).await?
    {
        actions.push(action);
    }
    Ok(actions)
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
