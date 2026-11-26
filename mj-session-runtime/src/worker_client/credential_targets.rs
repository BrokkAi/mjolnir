//! Which sessions the daemon keeps supplied with credentials.

use std::collections::BTreeMap;

use mj_core::credentials::CredentialSyncTarget;
use mj_core::state::SessionState;

use crate::session_manager::session_target_is_pollable;
use mj_core::config::Config;
use mj_core::state::State;
pub use mj_worker_launch::local_profile_homes::StagedProfileOwnership;
use mj_worker_launch::{local_profile_homes, worker_launch};
pub type StagedCredentialSessions = BTreeMap<String, StagedProfileOwnership>;

/// Sessions whose worker can answer credential requests right now. Sessions
/// still provisioning or already disconnected would only produce connection
/// errors, so they stay out.
///
/// Every session runs from a staged home of its own, on this machine as on any
/// other target, so each one is reconciled the same way. The one exception is a
/// local session an earlier release started from the profile home itself. Its
/// worker still reads that home, so a push would replace the person's own
/// skills tree; it is left out until it is next staged (see
/// [`mj_worker_launch::local_profile_homes`]). A local session whose login
/// file links to the profile's own keeps its skills sync but skips the login
/// file, which is already the profile's.
pub fn credential_sync_targets(config: &Config, state: &State) -> Vec<CredentialSyncTarget> {
    let schemes = config
        .profiles
        .iter()
        .map(|(id, profile)| (id.clone(), !profile.auth_scheme().syncs_login()))
        .collect();
    credential_sync_targets_with_auth(config, state, &schemes)
}

pub fn credential_sync_targets_with_auth(
    config: &Config,
    state: &State,
    schemes: &BTreeMap<String, bool>,
) -> Vec<CredentialSyncTarget> {
    credential_sync_targets_from_sources(
        config,
        state,
        schemes,
        &staged_credential_sessions(config, state),
    )
}

pub fn staged_credential_sessions(config: &Config, state: &State) -> StagedCredentialSessions {
    staged_credential_sessions_with_cache(config, state, None, None, false)
}

/// Return owned homes. With an attachment owner, remote probes run
/// independently and their cached observations are reused until reconnect.
pub fn staged_credential_sessions_with_cache(
    config: &Config,
    state: &State,
    wakes: Option<&std::sync::Arc<super::CredentialWakes>>,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    reprobe_non_owned: bool,
) -> StagedCredentialSessions {
    let sessions = state
        .sessions
        .values()
        .filter(|session| {
            session_target_is_pollable(session) && session.state != SessionState::Disconnected
        })
        .collect::<Vec<_>>();
    let (remote, local): (Vec<_>, Vec<_>) = sessions.into_iter().partition(|session| {
        matches!(
            session.target.as_deref(),
            Some(mj_core::state::TargetLocator::SshHostBare { .. })
        ) && !session.harness_kind.nested_home()
    });
    let mut staged = local
        .into_iter()
        .filter_map(|session| {
            local_profile_homes::probe_staged_home_on_target(
                session,
                config,
                &mj_core::targets::ProcessExecutor,
            )
            .filter(|ownership| ownership.owns_home())
            .map(|ownership| (session.id.clone(), ownership))
        })
        .collect::<StagedCredentialSessions>();
    if let Some(wakes) = wakes {
        for session in remote {
            if let Some(ownership) = wakes
                .cached_or_spawn_profile_probe(session, config, cancellation, reprobe_non_owned)
                .filter(|ownership| ownership.owns_home())
            {
                staged.insert(session.id.clone(), ownership);
            }
        }
        return staged;
    }
    std::thread::scope(|scope| {
        let tasks = remote
            .into_iter()
            .map(|session| {
                scope.spawn(move || {
                    let ownership = local_profile_homes::probe_staged_home_on_target(
                        session,
                        config,
                        &mj_core::targets::ProcessExecutor,
                    );
                    ownership
                        .filter(|ownership| ownership.owns_home())
                        .map(|ownership| (session.id.clone(), ownership))
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            match task.join() {
                Ok(Some((id, ownership))) => {
                    staged.insert(id, ownership);
                }
                Ok(None) => {}
                Err(_) => tracing::error!("credential profile observation panicked"),
            }
        }
    });
    staged
}

pub fn credential_sync_targets_from_sources(
    config: &Config,
    state: &State,
    schemes: &BTreeMap<String, bool>,
    staged: &StagedCredentialSessions,
) -> Vec<CredentialSyncTarget> {
    state.sessions.values()
        .filter(|session| staged.get(&session.id).is_some_and(|ownership| ownership.owns_home()))
        .filter_map(|session| {
            let profile = config.profiles.get(&session.last_profile)?;
            let spec = match worker_launch::reconnect_command(config, state, &session.id) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build credential sync target: {error:#}");
                    return None;
                }
            };
            let aws_credentials = match worker_launch::aws_credential_sync_source(config, state, &session.id) {
                Ok(source) => source,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not select AWS credential sync source: {error:#}");
                    return None;
                }
            };
            Some(CredentialSyncTarget {
                session_id: session.id.clone(),
                profile_id: session.last_profile.clone(),
                harness: profile.kind,
                profile_home: profile.home.clone(),
                aws_credentials,
                skips_login_file_sync: *schemes.get(&session.last_profile)?
                    || staged.get(&session.id) == Some(&StagedProfileOwnership::OwnedSharedLogin)
                    || local_profile_homes::session_shares_profile_login(
                        session,
                        profile.kind,
                    ),
                github: config.github.clone(),
                sync_github_token: !session.no_github_credentials
                    && target_syncs_github_token(session.target.as_deref()),
                skills_scope: session.target.as_ref()?.skills_scope(),
                spec,
            })
        })
        .collect()
}

pub fn target_syncs_github_token(target: Option<&mj_core::state::TargetLocator>) -> bool {
    target.is_some()
}

/// Revalidate a copied target while holding background admission: a lifecycle
/// may have completed since the dashboard published its target list.
pub fn credential_sync_target_is_current(
    config: &Config,
    mut state: State,
    candidate: &CredentialSyncTarget,
) -> bool {
    state.sessions.retain(|id, _| id == &candidate.session_id);
    credential_sync_targets(config, &state).contains(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    // An SSH host-bare login file can still symlink to a personal home.
    #[test]
    fn host_bare_shared_login_skips_login_but_keeps_other_syncs() {
        use mj_core::config::{HarnessKind, HarnessProfile, TargetTemplate};
        use mj_core::state::{TargetConnection, TargetLocator, TargetRuntimeSettings};
        let id = "0123456789abcdef0123456789abcdef";
        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        config.profiles.insert(
            "codex".into(),
            HarnessProfile::new(HarnessKind::Codex, "/profile"),
        );
        let mut session = crate::database::test_fixtures::checkpoint_test_session(id);
        session.target_template_id = "localhost".into();
        session.project_directory = Some("/workspace".into());
        session.target = Some(Box::new(TargetLocator::SshHostBare {
            host: "source".into(),
            data_root: "/remote".into(),
        }));
        let mut runtime = TargetRuntimeSettings::from(&TargetTemplate::LocalBare);
        runtime.kind = "ssh-host-bare".into();
        runtime.connection = TargetConnection::Ssh {
            ssh: mj_core::config::SshConnection {
                host: "source".into(),
                user: None,
                identity_file: None,
                extra_args: vec![],
            },
        };
        session.target_runtime = Some(runtime.into());
        let mut state = State::default();
        state.sessions.insert(id.into(), session);
        let schemes = BTreeMap::from([("codex".into(), false)]);
        for (ownership, skip) in [
            (StagedProfileOwnership::Owned, false),
            (StagedProfileOwnership::OwnedSharedLogin, true),
        ] {
            let staged = BTreeMap::from([(id.into(), ownership)]);
            let targets = credential_sync_targets_from_sources(&config, &state, &schemes, &staged);
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].skips_login_file_sync, skip);
            assert!(targets[0].sync_github_token);
            assert_eq!(
                targets[0].skills_scope,
                mj_core::skills::SkillsScope::Isolated
            );
        }
        let shared = BTreeMap::from([(id.into(), StagedProfileOwnership::SharedHome)]);
        assert!(
            credential_sync_targets_from_sources(&config, &state, &schemes, &shared).is_empty()
        );
    }
}
