//! The controller's GitHub credentials, read from its configuration and
//! records by `crate::github_app`.

use anyhow::Result;

use super::Controller;
use crate::github_app::{self, GithubBundleSelectionError};

impl Controller {
    /// Reject a new bundle before admitting its session when its repositories
    /// resolve to more than one App installation.
    pub async fn validate_github_bundle_installations(
        &self,
        bundle_id: &str,
    ) -> std::result::Result<(), GithubBundleSelectionError> {
        github_app::validate_bundle_installations(&self.config, bundle_id).await
    }

    /// Resolve this session's GitHub credential, using the App installation
    /// selected by its accepted repository bundle.
    pub async fn github_token_for_session(&self, session_id: &str) -> Result<Option<String>> {
        github_app::token_for_session(&self.config, &self.state, session_id).await
    }

    pub(crate) async fn github_token_for_repository_preflight(
        &self,
        session_id: &str,
    ) -> Result<Option<String>> {
        github_app::token_for_repository_preflight(&self.config, &self.state, session_id).await
    }
}
