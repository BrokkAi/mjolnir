use super::{HttpDaemonClient, encode_segment};
use crate::daemon::WorkspaceDrafts;
use anyhow::Result;
use mj_core::storage::DetachedSessionDraft;
use mj_core::workspace::WorkspaceRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalDraftRequest {
    pub client_id: String,
    pub owner_pid: u32,
    pub revision: u64,
    pub draft: DetachedSessionDraft,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadReceiptRequest {
    pub client_id: String,
    pub workspace_id: String,
    pub through: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardDraftsRequest {
    pub draft_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredDraft {
    pub session_id: String,
    pub text: String,
    pub revision: Option<i64>,
}

fn workspace_path(id: &str, suffix: &str) -> Result<String> {
    Ok(format!(
        "/api/v1/workspaces/{}{suffix}",
        encode_segment(id)?
    ))
}

impl HttpDaemonClient {
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        #[derive(Deserialize)]
        struct Listing {
            workspaces: Vec<WorkspaceRecord>,
        }
        let listing: Listing = self.get_json("/api/v1/workspaces").await?;
        Ok(listing.workspaces)
    }

    pub async fn list_workspace_drafts(&self) -> Result<Vec<WorkspaceDrafts>> {
        self.get_json("/api/v1/workspaces/drafts").await
    }

    pub async fn create_workspace(&self, name: String) -> Result<WorkspaceRecord> {
        #[derive(Deserialize)]
        struct Created {
            workspace: WorkspaceRecord,
        }
        let created: Created = self
            .post_json("/api/v1/workspaces", &serde_json::json!({ "name": name }))
            .await?;
        Ok(created.workspace)
    }

    pub async fn rename_workspace(&self, workspace_id: String, name: String) -> Result<()> {
        self.patch_json_no_content(
            &workspace_path(&workspace_id, "")?,
            &serde_json::json!({ "name": name }),
        )
        .await
    }

    pub async fn touch_workspace(&self, workspace_id: String) -> Result<()> {
        self.post_empty_no_content(&workspace_path(&workspace_id, "/touch")?)
            .await
    }

    pub async fn close_workspace(&self, workspace_id: String) -> Result<()> {
        self.post_empty_no_content(&workspace_path(&workspace_id, "/close")?)
            .await
    }

    pub async fn cancel_workspace_close(&self, workspace_id: String) -> Result<()> {
        self.delete_no_content(&workspace_path(&workspace_id, "/close")?)
            .await
    }

    /// Read the source. The destination terminal save retires it atomically.
    pub async fn recover_draft(&self, draft_id: String) -> Result<RecoveredDraft> {
        self.get_json(&format!("/api/v1/drafts/{}", encode_segment(&draft_id)?))
            .await
    }

    pub async fn discard_drafts(
        &self,
        workspace_id: String,
        draft_id: Option<String>,
    ) -> Result<usize> {
        #[derive(Deserialize)]
        struct Discarded {
            count: usize,
        }
        let discarded: Discarded = self
            .post_json(
                &workspace_path(&workspace_id, "/drafts/discard")?,
                &DiscardDraftsRequest { draft_id },
            )
            .await?;
        Ok(discarded.count)
    }

    pub async fn persist_terminal_draft(
        &self,
        client_id: String,
        session_id: String,
        owner_pid: u32,
        revision: u64,
        draft: DetachedSessionDraft,
    ) -> Result<()> {
        self.put_json_no_content(
            &super::sessions::session_path(&session_id, "/terminal-draft")?,
            &TerminalDraftRequest {
                client_id,
                owner_pid,
                revision,
                draft,
            },
        )
        .await
    }

    pub async fn persist_read_receipt(
        &self,
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
    ) -> Result<u64> {
        #[derive(Deserialize)]
        struct Receipt {
            ordinal: u64,
        }
        let receipt: Receipt = self
            .post_json(
                &super::sessions::session_path(&session_id, "/read-receipt")?,
                &ReadReceiptRequest {
                    client_id,
                    workspace_id,
                    through,
                },
            )
            .await?;
        Ok(receipt.ordinal)
    }
}
