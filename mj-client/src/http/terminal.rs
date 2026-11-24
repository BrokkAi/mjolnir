use super::{
    HttpApiError, HttpDaemonClient, encode_segment,
    sessions::{optional, session_path},
    workspaces::workspace_path,
};
use crate::daemon::StoredTranscriptItem;
use crate::daemon::{
    DashboardStartup, ResumeCandidates, SessionRecordWithVerdict, SessionTextSearchPage,
    StoredSessionMetadata, WorkspaceLayouts,
};
use crate::runtime_feed::{RuntimeCursor, SessionTailReply};
use anyhow::Result;
use mj_core::attention::NotificationDismissal;
use mj_core::state::{ContainerUpgradePreview, SessionResourceAllocation};
use mj_core::state::{MaterializedSession, MaterializedSessionSummary, SessionRecord};
use mj_core::targets::AdditionalMount;
use mj_core::workspace::{ConversationLayout, PaneSize, PaneSizes};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSettingsRequest {
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub mounts: Vec<AdditionalMount>,
    pub mount_history: Vec<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerSettingsResponse {
    pub preview: Option<ContainerUpgradePreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfileSelectionRequest {
    pub provider: mj_core::config::QuotaProvider,
    pub profile_id: Option<String>,
    pub target_template_id: Option<String>,
}

impl HttpDaemonClient {
    pub async fn select_provider_profile(
        &self,
        request: ProviderProfileSelectionRequest,
    ) -> Result<String> {
        self.post_json("/api/v1/profiles/select", &request).await
    }

    pub async fn dashboard_startup(
        &self,
        client_id: String,
        workspace_id: String,
    ) -> Result<DashboardStartup> {
        self.post_json(
            "/api/v1/dashboard/startup",
            &serde_json::json!({ "client_id": client_id, "workspace_id": workspace_id }),
        )
        .await
    }

    pub async fn workspace_layouts(&self, workspace_ids: Vec<String>) -> Result<WorkspaceLayouts> {
        self.post_json(
            "/api/v1/workspaces/layouts",
            &serde_json::json!({ "workspace_ids": workspace_ids }),
        )
        .await
    }

    pub async fn stored_session_metadata(&self) -> Result<StoredSessionMetadata> {
        self.get_json("/api/v1/sessions/stored-metadata").await
    }

    pub async fn stored_projection_tail(
        &self,
        session_id: String,
        items: usize,
    ) -> Result<Option<MaterializedSession>> {
        self.get_json_query(
            &session_path(&session_id, "/stored-projection-tail")?,
            &[("items", items)],
        )
        .await
    }

    pub async fn stored_transcript_item(
        &self,
        session_id: String,
        stable_id: String,
    ) -> Result<StoredTranscriptItem> {
        optional(
            self.get_json(&session_path(
                &session_id,
                &format!("/stored-items/{}", encode_segment(&stable_id)?),
            )?)
            .await,
        )
        .map(|item| match item {
            Some(item) => StoredTranscriptItem::Found(item),
            None => StoredTranscriptItem::Missing,
        })
    }

    pub async fn stored_agent_message_through(
        &self,
        session_id: String,
        through: u64,
    ) -> Result<StoredTranscriptItem> {
        optional(
            self.get_json_query(
                &session_path(&session_id, "/stored-agent-message")?,
                &[("through", through)],
            )
            .await,
        )
        .map(|item| match item {
            Some(item) => StoredTranscriptItem::Found(item),
            None => StoredTranscriptItem::Missing,
        })
    }

    pub async fn stored_session_summary(
        &self,
        session_id: String,
    ) -> Result<Option<MaterializedSessionSummary>> {
        self.get_json(&session_path(&session_id, "/stored-summary")?)
            .await
    }

    pub async fn session_tail(
        &self,
        session_id: String,
        cursor: RuntimeCursor,
    ) -> Result<SessionTailReply> {
        self.post_json(
            &session_path(&session_id, "/tail")?,
            &serde_json::json!({ "cursor": cursor }),
        )
        .await
    }

    pub async fn resume_candidates(&self) -> Result<ResumeCandidates> {
        self.get_json("/api/v1/sessions/resume-candidates").await
    }

    pub async fn session_record_with_verdict(
        &self,
        session_id: String,
    ) -> Result<Option<SessionRecordWithVerdict>> {
        optional(self.get_json(&session_path(&session_id, "/record")?).await)
    }

    pub async fn go_startup_session(
        &self,
        workspace_id: String,
        last_session_id: Option<String>,
    ) -> Result<Option<SessionRecord>> {
        let path = workspace_path(&workspace_id, "/go-startup")?;
        match last_session_id {
            Some(id) => self.get_json_query(&path, &[("last_session_id", id)]).await,
            None => self.get_json(&path).await,
        }
    }

    pub async fn session_text_search(&self, query: String) -> Result<SessionTextSearchPage> {
        self.get_json_query("/api/v1/sessions/text-search", &[("q", query)])
            .await
    }

    pub async fn dismiss_notification(
        &self,
        session_id: String,
        target: NotificationDismissal,
    ) -> Result<u64> {
        self.post_json(
            &session_path(&session_id, "/notifications/dismiss")?,
            &serde_json::json!({ "target": target }),
        )
        .await
    }

    pub async fn record_operator_activity(&self) -> Result<()> {
        self.post_empty_no_content("/api/v1/operator/activity")
            .await
    }

    pub async fn set_operator_away(&self, away: bool) -> Result<()> {
        self.put_json_no_content(
            "/api/v1/operator/away",
            &serde_json::json!({ "away": away }),
        )
        .await
    }

    pub async fn attach(&self, client_id: String) -> Result<()> {
        self.post_empty_no_content(&format!(
            "/api/v1/terminal-clients/{}/attach",
            encode_segment(&client_id)?
        ))
        .await
    }

    /// Renew this terminal's lease once. A draining daemon returns false so
    /// the next keep-alive can check whether a new build has taken over.
    pub async fn try_attach(&self, client_id: String) -> Result<bool> {
        let path = format!(
            "/api/v1/terminal-clients/{}/attach",
            encode_segment(&client_id)?
        );
        match self
            .send(Method::POST, &path, |request| request, false)
            .await
        {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<HttpApiError>()
                    .is_some_and(HttpApiError::is_handoff) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn detach(&self, client_id: String) -> Result<()> {
        let path = format!(
            "/api/v1/terminal-clients/{}/detach",
            encode_segment(&client_id)?
        );
        self.send(Method::POST, &path, |request| request, false)
            .await?;
        Ok(())
    }

    pub async fn save_sidebar_size(&self, size: PaneSize) -> Result<()> {
        self.put_json_no_content(
            "/api/v1/ui/sidebar-size",
            &serde_json::json!({ "size": size }),
        )
        .await
    }

    pub async fn save_workspace_pane_sizes(
        &self,
        workspace_id: String,
        sizes: PaneSizes,
    ) -> Result<()> {
        self.put_json_no_content(
            &workspace_path(&workspace_id, "/pane-sizes")?,
            &serde_json::json!({ "sizes": sizes }),
        )
        .await
    }

    pub async fn save_workspace_layout(
        &self,
        workspace_id: String,
        layout: ConversationLayout,
    ) -> Result<()> {
        self.put_json_no_content(
            &workspace_path(&workspace_id, "/layout")?,
            &serde_json::json!({ "layout": layout }),
        )
        .await
    }

    pub async fn set_session_title(&self, session_id: String, title: String) -> Result<String> {
        self.put_json(
            &session_path(&session_id, "/title")?,
            &serde_json::json!({ "title": title }),
        )
        .await
    }

    pub async fn set_session_notifications_ignored(
        &self,
        session_id: String,
        ignored: bool,
    ) -> Result<()> {
        self.put_json_no_content(
            &session_path(&session_id, "/notifications-ignored")?,
            &serde_json::json!({ "ignored": ignored }),
        )
        .await
    }

    pub async fn set_session_workspace(
        &self,
        session_id: String,
        workspace_id: String,
    ) -> Result<()> {
        self.put_json_no_content(
            &session_path(&session_id, "/workspace")?,
            &serde_json::json!({ "workspace_id": workspace_id }),
        )
        .await
    }

    pub async fn set_session_container_settings(
        &self,
        session_id: String,
        resource_allocation: Option<SessionResourceAllocation>,
        mounts: Vec<AdditionalMount>,
        mount_history: Vec<PathBuf>,
    ) -> Result<Option<mj_core::state::ContainerUpgradePreview>> {
        let response: ContainerSettingsResponse = self
            .put_json(
                &session_path(&session_id, "/container-settings")?,
                &ContainerSettingsRequest {
                    resource_allocation,
                    mounts,
                    mount_history,
                },
            )
            .await?;
        Ok(response.preview)
    }

    pub async fn session_record(&self, session_id: String) -> Result<Option<SessionRecord>> {
        Ok(self
            .session_record_with_verdict(session_id)
            .await?
            .map(|session| session.record))
    }

    pub async fn native_agent_history(
        &self,
        owner: String,
        child: String,
        before: Option<(u64, String)>,
    ) -> Result<mj_core::native_agent::NativeAgentHistoryPage> {
        let mut query = vec![("full", "true".to_owned())];
        if let Some((position, id)) = before {
            query.push(("before_position", position.to_string()));
            query.push(("before_id", id));
        }
        self.get_json_query(
            &session_path(
                &owner,
                &format!("/native-agents/{}/history", encode_segment(&child)?),
            )?,
            &query,
        )
        .await
    }
}
