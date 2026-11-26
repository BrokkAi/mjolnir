//! Which sessions the daemon keeps supplied with credentials.

use std::collections::{BTreeMap, BTreeSet};

use mj_core::credentials::CredentialSyncTarget;
use mj_core::state::SessionState;

use crate::session_manager::session_target_is_pollable;
use mj_core::config::Config;
use mj_core::state::State;
use mj_worker_launch::{local_profile_homes, worker_launch};

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

pub fn staged_credential_sessions(config: &Config, state: &State) -> BTreeSet<String> {
    staged_credential_sessions_with_cache(config, state, None)
}

/// Observe remote homes in parallel on the daemon's background worker. A
/// caller with an attachment owner can reuse the observation until reconnect.
pub fn staged_credential_sessions_with_cache(
    config: &Config,
    state: &State,
    wakes: Option<&super::CredentialWakes>,
) -> BTreeSet<String> {
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
        .filter(|session| local_profile_homes::session_has_a_staged_home_of_its_own(session))
        .map(|session| session.id.clone())
        .collect::<BTreeSet<_>>();
    std::thread::scope(|scope| {
        let tasks = remote
            .into_iter()
            .map(|session| {
                scope.spawn(move || {
                    let owned = match wakes {
                        Some(wakes) => wakes.observe_profile(session, config),
                        None => local_profile_homes::session_has_a_staged_home_on_target(
                            session,
                            config,
                            &mj_core::targets::ProcessExecutor,
                        ),
                    };
                    owned.then(|| session.id.clone())
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            match task.join() {
                Ok(Some(id)) => {
                    staged.insert(id);
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
    staged: &BTreeSet<String>,
) -> Vec<CredentialSyncTarget> {
    state.sessions.values()
        .filter(|session| staged.contains(&session.id))
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
