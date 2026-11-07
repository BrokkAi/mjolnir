//! The AWS profile the controller staged into each session.
//!
//! A session's AWS configuration sits in a home the session can write, so the
//! profile it names is not evidence of the selection: a session that renamed
//! its profile section would have the sync export, mint and install whatever
//! host profile it named. The controller keeps the selection itself, durably
//! (`session_aws_selections`), written by the one step that stages the
//! configuration (`prepare_worker_files`) and read by everything that mints or
//! exports for the session.

use anyhow::{Context, Result};
use mj_core::credentials::CredentialSyncTarget;

/// Where the credential sync learns which AWS profile a session was staged
/// with. Both methods block (they read the database, and the second reads the
/// profile's configuration); callers run them off the async threads.
pub(crate) trait SessionAwsSelections: Send + Sync {
    /// The profile recorded for the session when it was staged.
    fn recorded(&self, session_id: &str) -> Result<Option<String>>;

    /// For a session staged before selections were recorded: resolve the
    /// selection the way launch does, from the profile's configuration, and
    /// record it unless one exists. It returns what is recorded, which is
    /// fixed for the session's life from then on.
    fn backfill(&self, target: &CredentialSyncTarget) -> Result<String>;
}

/// The daemon's selections, in its database.
pub(crate) struct DatabaseSelections;

impl SessionAwsSelections for DatabaseSelections {
    fn recorded(&self, session_id: &str) -> Result<Option<String>> {
        crate::database::session_aws_profile(session_id)
    }

    fn backfill(&self, target: &CredentialSyncTarget) -> Result<String> {
        let config = mj_core::config::Config::load()?;
        let profile = config
            .profiles
            .get(&target.profile_id)
            .with_context(|| format!("profile {} is not configured", target.profile_id))?;
        let selected = crate::aws_credentials::selected_source(profile)?;
        let name = selected
            .environment
            .get("AWS_PROFILE")
            .cloned()
            .context("the profile selects no AWS profile")?;
        crate::database::backfill_session_aws_profile(&target.session_id, &name)
    }
}
