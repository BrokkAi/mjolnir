use super::*;

pub struct CredentialSyncCoordinator {
    pub(super) handle: CredentialSyncHandle,
    pub(super) results: mpsc::UnboundedReceiver<CredentialSyncResult>,
}

impl CredentialSyncCoordinator {
    pub fn spawn() -> Self {
        let (targets_tx, mut targets_rx) = watch::channel(Vec::new());
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
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
                            enqueue(&mut queue, SyncTrigger { profile_id, cause: None });
                        }
                    }
                    changed = targets_rx.changed() => {
                        if changed.is_err() { break; }
                        for profile_id in profiles_with_targets(&targets_rx.borrow()) {
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
                    tokio::spawn(async move {
                        let joined =
                            tokio::spawn(async move { reconcile_profile(&targets).await }).await;
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
                targets: Arc::new(targets_tx),
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

/// Reconcile one profile with every live session that runs it.
///
/// A pull makes every other session's copy stale by definition, so the pass
/// runs again once with the new canonical bytes. Two passes are enough: the
/// second cannot pull anything the first did not already see unless a harness
/// refreshed mid-cycle, and that lands in the next cycle.
pub(super) async fn reconcile_profile(
    targets: &[CredentialSyncTarget],
) -> Vec<CredentialSyncOutcome> {
    // The token lookup may run `gh auth token`, a synchronous child process,
    // so it goes to the blocking pool rather than stalling a scheduler thread.
    let github_token = match targets.iter().any(|target| target.sync_github_token) {
        true => tokio::task::spawn_blocking(crate::controller::controller_github_token)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!("github token lookup task stopped: {error}");
                None
            }),
        false => None,
    };
    let mut outcomes = BTreeMap::<String, CredentialSyncOutcome>::new();
    for pass in 0..2 {
        let mut pulled = false;
        for target in targets {
            match reconcile_session(target, github_token.as_deref()).await {
                Ok(actions) if actions.is_empty() => {}
                Ok(actions) => {
                    pulled |= actions.contains(&CredentialSyncAction::Pulled);
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
        if !pulled || pass == 1 {
            break;
        }
    }
    outcomes.into_values().collect()
}

/// The skills tree this session should converge to: the profile's own skills
/// plus Mjolnir's managed skills, exactly as launch staging writes them into
/// the session's staged home.
pub(super) fn canonical_session_skills(
    target: &CredentialSyncTarget,
) -> Result<mj_core::skills::SkillsArchive> {
    mj_core::skills::session_skills(target.harness, &target.profile_home).with_context(|| {
        format!(
            "collect canonical skills for profile {} from {}",
            target.profile_id,
            target.profile_home.display()
        )
    })
}

/// Returns every action taken; an empty list means the copies already agree.
pub(super) async fn reconcile_session(
    target: &CredentialSyncTarget,
    github_token: Option<&str>,
) -> Result<Vec<CredentialSyncAction>> {
    let canonical_path = harness_authentication_marker(target.harness, &target.profile_home);
    let (canonical, canonical_bytes) = read_credential_file(target.harness, &canonical_path)?;
    let canonical_skills = canonical_session_skills(target)?;
    let mut client = RelayClient::connect(&target.spec, &target.session_id).await?;
    let result = reconcile_connected(
        &mut client,
        target,
        &canonical_path,
        &canonical,
        &canonical_bytes,
        &canonical_skills,
        github_token,
    )
    .await;
    // Detach even when the exchange failed; the worker and harness keep
    // running either way. A failed detach only leaks a short-lived proxy, so it
    // is reported rather than turned into a sync failure.
    if let Err(error) = client.detach().await {
        tracing::warn!(
            session_id = %target.session_id,
            "could not close the credential sync connection: {error:#}"
        );
    }
    result
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
    // An API-key profile keeps its key in the profile environment, which the
    // worker already receives in the launch environment. There is no
    // credential file on either side to compare or copy.
    if !target.authenticates_with_api_key {
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

/// Converge the session's synced skills trees onto the canonical archive.
/// Returns true when a push happened. Workers old enough to predate skills
/// sync answer the unknown method with `InvalidRequest`; those sessions are
/// skipped quietly until their target is re-provisioned.
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
        .install_skills(&canonical.encode(mj_core::skills::SkillsArchiveFormat::Gzip))
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
