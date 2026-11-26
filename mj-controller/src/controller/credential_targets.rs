//! Which sessions the daemon keeps supplied with credentials, asked of a
//! `Controller`. The answers come from `crate::worker_client`, which reads the
//! same configuration and records.

use std::collections::BTreeMap;

use mj_core::credentials::CredentialSyncTarget;

use super::Controller;
use crate::worker_client;

pub use crate::worker_client::target_syncs_github_token;

pub fn credential_sync_targets(controller: &Controller) -> Vec<CredentialSyncTarget> {
    worker_client::credential_sync_targets(&controller.config, &controller.state)
}

pub fn credential_sync_targets_with_auth(
    controller: &Controller,
    schemes: &BTreeMap<String, bool>,
) -> Vec<CredentialSyncTarget> {
    worker_client::credential_sync_targets_with_auth(&controller.config, &controller.state, schemes)
}

pub fn staged_credential_sessions(
    controller: &Controller,
) -> worker_client::StagedCredentialSessions {
    worker_client::staged_credential_sessions(&controller.config, &controller.state)
}

pub fn staged_credential_sessions_with_cache(
    controller: &Controller,
    wakes: &worker_client::CredentialWakes,
) -> worker_client::StagedCredentialSessions {
    worker_client::staged_credential_sessions_with_cache(
        &controller.config,
        &controller.state,
        Some(wakes),
    )
}

pub fn credential_sync_targets_from_sources(
    controller: &Controller,
    schemes: &BTreeMap<String, bool>,
    staged: &worker_client::StagedCredentialSessions,
) -> Vec<CredentialSyncTarget> {
    worker_client::credential_sync_targets_from_sources(
        &controller.config,
        &controller.state,
        schemes,
        staged,
    )
}

/// Revalidate a copied target while holding background admission: a lifecycle
/// may have completed since the dashboard published its target list.
pub fn credential_sync_target_is_current(
    controller: Controller,
    candidate: &CredentialSyncTarget,
) -> bool {
    worker_client::credential_sync_target_is_current(
        &controller.config,
        controller.state,
        candidate,
    )
}
