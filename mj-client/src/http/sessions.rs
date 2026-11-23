use super::{HttpApiError, HttpDaemonClient};
use crate::daemon::{
    RegisteredSession, WikiHitTranscript, WikiRestoreRequest, WikiSearchPage, WikiSessionInfo,
    WikiSyncStatus,
};
use anyhow::{Context, Result, ensure};
use mj_core::attachment::AttachmentRef;
use mj_core::local_git::SessionGitStatus;
use mj_core::state::{ContainerUpgradePreview, ContainerUpgradeRequest, SessionResourceAllocation};
use mj_core::targets::AdditionalMount;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWorkingContext {
    pub cwd: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiRestoreBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub profile_id: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_mounts: Vec<AdditionalMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_allocation: Option<SessionResourceAllocation>,
    /// Direct daemon clients use the full registration to follow creation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_registration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiRestoreResponse {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered: Option<RegisteredSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopBackgroundTaskRequest {
    pub background_task_id: String,
}

fn encode_segment(segment: &str) -> Result<String> {
    // The URL parser resolves dot segments even when percent-encoded.
    ensure!(
        !matches!(segment, "" | "." | ".."),
        "invalid API path segment"
    );
    let encoded: String = url::form_urlencoded::byte_serialize(segment.as_bytes()).collect();
    Ok(encoded.replace('+', "%20"))
}

fn session_path(id: &str, suffix: &str) -> Result<String> {
    Ok(format!("/api/v1/sessions/{}{suffix}", encode_segment(id)?))
}

fn wiki_path(id: &str, suffix: &str) -> Result<String> {
    Ok(format!(
        "/api/v1/wiki/sessions/{}{suffix}",
        encode_segment(id)?
    ))
}

fn optional<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Err(error)
            if error
                .downcast_ref::<HttpApiError>()
                .is_some_and(|error| error.status() == StatusCode::NOT_FOUND) =>
        {
            Ok(None)
        }
        result => result.map(Some),
    }
}

impl HttpDaemonClient {
    pub async fn upload_attachment(
        &self,
        id: &str,
        bytes: Vec<u8>,
        mime_type: &str,
    ) -> Result<AttachmentRef> {
        #[derive(Deserialize)]
        struct Uploaded {
            attachment: AttachmentRef,
        }
        let uploaded: Uploaded = self
            .post_bytes(&session_path(id, "/attachments")?, bytes, mime_type, None)
            .await?;
        Ok(uploaded.attachment)
    }

    pub async fn start_turn_review(&self, id: &str) -> Result<()> {
        self.post_empty_no_content(&session_path(id, "/review")?)
            .await
    }

    pub async fn stop_background_task(&self, id: &str, background_task_id: String) -> Result<()> {
        self.post_json_no_content(
            &session_path(id, "/background-tasks/stop")?,
            &StopBackgroundTaskRequest { background_task_id },
        )
        .await
    }

    pub async fn preview_container_upgrade(&self, id: &str) -> Result<ContainerUpgradePreview> {
        self.get_json(&session_path(id, "/container-upgrade")?)
            .await
    }

    pub async fn upgrade_container(&self, request: &ContainerUpgradeRequest) -> Result<()> {
        self.post_json_no_content(
            &session_path(&request.selection.session_id, "/container-upgrade")?,
            request,
        )
        .await
    }

    pub async fn cancel_container_upgrade(&self, id: &str) -> Result<()> {
        self.delete_no_content(&session_path(id, "/container-upgrade")?)
            .await
    }

    pub async fn session_working_context(&self, id: &str) -> Result<SessionWorkingContext> {
        self.get_json(&session_path(id, "/working-context")?).await
    }

    pub async fn session_git_status(&self, id: &str) -> Result<SessionGitStatus> {
        self.get_json(&session_path(id, "/git-status")?).await
    }

    pub async fn wiki_sync_request(&self) -> Result<WikiSyncStatus> {
        self.post_empty("/api/v1/wiki/sync").await
    }

    pub async fn wiki_sync_watch(&self, epoch: &str, after: u64) -> Result<WikiSyncStatus> {
        self.get_json_query(
            "/api/v1/wiki/sync/watch",
            &[("epoch", epoch), ("after", &after.to_string())],
        )
        .await
    }

    pub async fn wiki_search(&self, query: &str, limit: usize) -> Result<WikiSearchPage> {
        self.get_json_query(
            "/api/v1/wiki/search",
            &[("q", query), ("limit", &limit.to_string())],
        )
        .await
    }

    pub async fn wiki_brief(&self, id: &str, max_chars: usize) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Brief {
            markdown: String,
        }
        let response = self
            .get_json_query::<Brief>(&wiki_path(id, "/brief")?, &[("max_chars", max_chars)])
            .await;
        Ok(optional(response)?.map(|brief| brief.markdown))
    }

    pub async fn wiki_hits(
        &self,
        id: &str,
        query: &str,
        context_messages: usize,
        per_message_chars: usize,
    ) -> Result<Option<WikiHitTranscript>> {
        optional(
            self.get_json_query(
                &wiki_path(id, "/hits")?,
                &[
                    ("q", query),
                    ("context_messages", &context_messages.to_string()),
                    ("per_message_chars", &per_message_chars.to_string()),
                ],
            )
            .await,
        )
    }

    pub async fn wiki_session(&self, id: &str) -> Result<Option<WikiSessionInfo>> {
        optional(self.get_json(&wiki_path(id, "")?).await)
    }

    pub async fn wiki_restore(&self, request: WikiRestoreRequest) -> Result<RegisteredSession> {
        let WikiRestoreRequest {
            wiki_id,
            workspace_id,
            profile_id,
            target_template_id,
            project_directory,
            additional_mounts,
            resource_allocation,
        } = request;
        let restored: WikiRestoreResponse = self
            .post_json(
                &wiki_path(&wiki_id, "/restore")?,
                &WikiRestoreBody {
                    workspace_id: Some(workspace_id),
                    profile_id,
                    target_id: target_template_id,
                    project_directory,
                    additional_mounts,
                    resource_allocation,
                    include_registration: true,
                    ..Default::default()
                },
            )
            .await?;
        restored
            .registered
            .context("wiki restore response omits the registered session")
    }
}
