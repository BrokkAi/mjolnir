//! Which sessions the daemon keeps supplied with credentials.

use std::collections::{BTreeMap, BTreeSet};

use mj_core::credentials::CredentialSyncTarget;
use mj_core::state::SessionState;

use crate::controller::Controller;
use crate::session_manager::session_target_is_pollable;

/// Sessions whose worker can answer credential requests right now. Sessions
/// still provisioning or already disconnected would only produce connection
/// errors, so they stay out.
///
/// Every session runs from a staged home of its own, on this machine as on any
/// other target, so each one is reconciled the same way. The one exception is a
/// local session an earlier release started from the profile home itself. Its
/// worker still reads that home, so a push would replace the person's own
/// skills tree; it is left out until it is next staged (see
/// [`crate::controller::local_profile_homes`]). A local session whose login
/// file links to the profile's own keeps its skills sync but skips the login
/// file, which is already the profile's.
pub fn credential_sync_targets(controller: &Controller) -> Vec<CredentialSyncTarget> {
    let schemes = controller
        .config
        .profiles
        .iter()
        .map(|(id, profile)| (id.clone(), !profile.auth_scheme().syncs_login()))
        .collect();
    credential_sync_targets_with_auth(controller, &schemes)
}

pub(crate) fn credential_sync_targets_with_auth(
    controller: &Controller,
    schemes: &BTreeMap<String, bool>,
) -> Vec<CredentialSyncTarget> {
    credential_sync_targets_from_sources(
        controller,
        schemes,
        &staged_credential_sessions(controller),
    )
}

pub fn staged_credential_sessions(controller: &Controller) -> BTreeSet<String> {
    controller
        .state
        .sessions
        .values()
        // Every session the daemon holds a worker connection for, whatever
        // its lifecycle state, so a login renewed while one is checkpointing
        // or closing reaches it too. A disconnected session would only produce
        // connection errors. A session that is starting, resuming or parked
        // has no connection yet: its start stages the current login, and a
        // failure on a renewed one is handled the moment it is reported.
        .filter(|session| {
            session_target_is_pollable(session) && session.state != SessionState::Disconnected
        })
        .filter(|session| {
            crate::controller::local_profile_homes::session_has_a_staged_home_of_its_own(session)
        })
        .map(|session| session.id.clone())
        .collect()
}

pub fn credential_sync_targets_from_sources(
    controller: &Controller,
    schemes: &BTreeMap<String, bool>,
    staged: &BTreeSet<String>,
) -> Vec<CredentialSyncTarget> {
    controller.state.sessions.values()
        .filter(|session| staged.contains(&session.id))
        .filter_map(|session| {
            let profile = controller.config.profiles.get(&session.last_profile)?;
            let spec = match controller.reconnect_command(&session.id) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build credential sync target: {error:#}");
                    return None;
                }
            };
            let aws_credentials = match controller.aws_credential_sync_source(&session.id) {
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
                    || crate::controller::local_profile_homes::session_shares_profile_login(
                        session,
                        profile.kind,
                    ),
                github: controller.config.github.clone(),
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
    mut controller: crate::controller::Controller,
    candidate: &CredentialSyncTarget,
) -> bool {
    controller
        .state
        .sessions
        .retain(|id, _| id == &candidate.session_id);
    credential_sync_targets(&controller).contains(candidate)
}
