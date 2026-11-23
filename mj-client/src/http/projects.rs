use super::{HttpDaemonClient, encode_segment};
use crate::daemon::{GitIdentity, NewSessionPreflightRequest, PreflightNew};
use anyhow::{Context, Result};
use mj_core::path_completion::{CompletionHost, CompletionKind, PathCompletion};
use mj_core::project_catalog::{ProjectCatalogView, SavedProject};
use mj_core::project_picker::{ProjectDiscovery, ProjectDiscoveryRequest};
use mj_core::state::{MovePreparation, MoveSelection};
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

impl HttpDaemonClient {
    pub async fn preflight_new_session(
        &self,
        request: &NewSessionPreflightRequest,
    ) -> Result<PreflightNew> {
        self.post_json("/api/v1/preflight/new", request).await
    }

    pub async fn complete_path(
        &self,
        host: CompletionHost,
        prefix: String,
        kind: CompletionKind,
    ) -> Result<PathCompletion> {
        let mut body = json!({ "prefix": prefix, "kind": kind });
        match host {
            CompletionHost::Local => {}
            CompletionHost::Target(id) => body["target_id"] = json!(id),
            CompletionHost::Machine(machine) => body["machine"] = json!(machine),
        }
        self.post_json("/api/v1/paths/complete", &body).await
    }

    pub async fn discover_projects(
        &self,
        request: &ProjectDiscoveryRequest,
    ) -> Result<ProjectDiscovery> {
        self.post_json("/api/v1/projects/discover", request).await
    }

    pub async fn project_catalog(&self, refresh: bool, retry: bool) -> Result<ProjectCatalogView> {
        if refresh {
            Self::json(
                self.request(Method::POST, "/api/v1/projects")
                    .query(&[("retry", retry)]),
            )
            .await
        } else {
            self.get_json("/api/v1/projects").await
        }
    }

    pub async fn create_project(&self, sources: Vec<String>) -> Result<SavedProject> {
        #[derive(Deserialize)]
        struct Created {
            project: Option<SavedProject>,
        }
        let created: Created = self
            .post_json(
                "/api/v1/bundles",
                &json!({ "sources": sources, "include_project": true }),
            )
            .await?;
        created
            .project
            .context("bundle response omits the created project")
    }

    pub async fn remove_project(&self, bundle_id: String) -> Result<()> {
        self.delete_no_content(&format!("/api/v1/bundles/{}", encode_segment(&bundle_id)?))
            .await
    }

    pub async fn git_identity(&self) -> Result<GitIdentity> {
        self.get_json("/api/v1/git-identity").await
    }

    pub async fn set_git_identity(&self, name: String, email: String) -> Result<GitIdentity> {
        self.post_json(
            "/api/v1/git-identity",
            &json!({ "name": name, "email": email }),
        )
        .await
    }

    pub async fn prepare_move_session(&self, selection: MoveSelection) -> Result<MovePreparation> {
        self.post_json("/api/v1/moves/prepare", &selection).await
    }
}
