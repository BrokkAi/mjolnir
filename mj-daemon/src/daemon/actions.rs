use super::*;

pub(super) async fn handle_action(
    action: DaemonAction,
    metadata: &DaemonMetadata,
    state: &Arc<RuntimeState>,
    cancellation: &CancellationToken,
) -> Result<DaemonReply> {
    match action {
        DaemonAction::PrepareUpgrade => {
            if mj_controller::upgrade::gate().try_close() {
                cancellation.cancel();
                Ok(DaemonReply::Done)
            } else {
                Ok(DaemonReply::UpgradePending)
            }
        }
        DaemonAction::ProveIdentity { .. } => {
            unreachable!("identity proof is served before authentication")
        }
        DaemonAction::Ping => Ok(DaemonReply::Pong),
        // Each blocker carries its count and age, so the waiting client can say
        // what the upgrade waits for and for how long.
        DaemonAction::UpgradeBlockers => Ok(DaemonReply::UpgradeBlockers(
            mj_controller::upgrade::blockers()
                .iter()
                .map(ToString::to_string)
                .collect(),
        )),
        DaemonAction::Identity => Ok(DaemonReply::Identity(DaemonIdentity {
            instance_id: metadata.instance_id.clone(),
            build_version: metadata.build_version.clone(),
            placement: metadata.placement.clone(),
        })),
        DaemonAction::Status => {
            state.prune_dead_clients();
            Ok(DaemonReply::Status(DaemonStatus {
                pid: metadata.pid,
                started_at: metadata.started_at.clone(),
                build_version: metadata.build_version.clone(),
                attached_clients: state.attachments().len(),
                phone_status: state.phone_status(),
            }))
        }
        DaemonAction::Stop => {
            if cancellation.is_cancelled() {
                return Ok(DaemonReply::Done);
            }
            mj_controller::upgrade::gate().begin_draining();
            // A destroy in flight was acknowledged as accepted and nothing
            // about it is durable, so the stop waits for it, up to a bound
            // (#1191). The reply does not wait.
            if mj_controller::upgrade::gate().destroys_hold_the_stop() {
                let cancellation = cancellation.clone();
                mj_core::runtime::spawn(async move {
                    while mj_controller::upgrade::gate().destroys_hold_the_stop() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    cancellation.cancel();
                });
            } else {
                cancellation.cancel();
            }
            Ok(DaemonReply::Done)
        }
    }
}

pub(super) fn is_upgrade_handoff_refusal(error: &anyhow::Error) -> bool {
    Refusal::of(error)
        .is_some_and(|refusal| refusal.code() == Some(mj_controller::upgrade::HANDOFF_REFUSAL_CODE))
}

/// Credential selection lives on the machine owning the profiles.
pub(super) fn transcription_auth(
    config: &mj_core::config::Config,
    preferred: &str,
) -> Option<PathBuf> {
    available_auth(auth_paths(config, preferred))
}

/// Find candidate Codex subscription auth files, preferring the session's
/// current profile and then sorting all remaining profile IDs.
fn auth_paths(config: &mj_core::config::Config, preferred: &str) -> Vec<PathBuf> {
    let mut profiles = config
        .profiles
        .iter()
        .filter(|(_, profile)| profile.kind == mj_core::config::HarnessKind::Codex)
        .collect::<Vec<_>>();
    profiles.sort_by_key(|(id, _)| (*id != preferred, *id));
    profiles
        .into_iter()
        .map(|(_, profile)| profile.home.join(profile.kind.credential_file_name()))
        .collect()
}

/// Return the first auth file containing complete ChatGPT-subscription OAuth
/// tokens. API-key auth files are deliberately skipped.
fn available_auth(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.into_iter().find(
        |path| match anvil_client::codex_auth::read_auth_dot_json_at(path) {
            Ok(Some(auth)) => auth.tokens.is_some_and(|tokens| {
                !tokens.access_token.trim().is_empty()
                    && !tokens.refresh_token.trim().is_empty()
                    && !tokens.account_id.trim().is_empty()
            }),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "could not inspect Codex dictation credentials");
                false
            }
        },
    )
}

impl RuntimeState {
    pub(crate) async fn dismiss_notification(
        &self,
        session_id: String,
        target: mj_core::attention::NotificationDismissal,
    ) -> Result<u64> {
        use mj_core::attention::{NotificationDismissal, NotificationKind};

        let runtime = self.runtime_publication()?;
        if matches!(
            target,
            NotificationDismissal::Question(_)
                | NotificationDismissal::Failure(_)
                | NotificationDismissal::Stack { .. }
        ) && runtime.metadata.notification_dismissals.get(&session_id) != Some(&target)
        {
            return Err(mj_core::refusal::Refusal::precondition(
                "notification is no longer current",
            )
            .into());
        }
        let (through, command_ordinal) = match target {
            NotificationDismissal::Failure(failure) => {
                blocking(move || {
                    mj_controller::database::dismiss_recorded_failure(&session_id, failure)
                })
                .await?;
                return Ok(0);
            }
            NotificationDismissal::Question(question) => {
                let through = question.completed_ordinal;
                let ordinal = self
                    .session_manager
                    .session(&session_id)
                    .await?
                    .submit(
                        format!("dismiss-question-{through}"),
                        RelayCommand::DismissQuestion { question },
                    )
                    .await?;
                (Some(through), Some(ordinal))
            }
            NotificationDismissal::ReadThrough(through) => (Some(through), None),
            NotificationDismissal::Stack { revision } => {
                let stack = &runtime
                    .sessions
                    .get(&session_id)
                    .and_then(|view| view.operational.as_deref())
                    .context("notification stack session missing")?
                    .notification_stack;
                let through = stack
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == NotificationKind::Important)
                    .map(|entry| entry.completed_ordinal)
                    .max();
                let ordinal = self
                    .session_manager
                    .session(&session_id)
                    .await?
                    .submit(
                        format!("clear-notifications-{revision}"),
                        RelayCommand::ClearNotifications { revision },
                    )
                    .await?;
                (through, Some(ordinal))
            }
        };
        let Some(through) = through else {
            return Ok(command_ordinal.unwrap_or_default());
        };
        let workspace_id = runtime
            .records
            .get(&session_id)
            .context("session missing")?
            .workspace_id
            .clone();
        let frontier = blocking(move || {
            mj_controller::database::persist_read_receipt(
                "notification-dismissal",
                &workspace_id,
                &session_id,
                through,
            )
        })
        .await?;
        Ok(command_ordinal.unwrap_or(frontier))
    }
}
