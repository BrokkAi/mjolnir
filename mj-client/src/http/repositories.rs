use super::HttpDaemonClient;
use mj_core::local_git::LocalRemoteRepair;
use mj_core::state::{
    ArchiveSpacePreview, ProjectSourceIdentity, RecoveryScan, ResumeRepositorySourcePreflight,
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Serialize, Deserialize)]
pub struct NewSessionRemoteRepairsRequest {
    pub bundle_id: String,
    pub target_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepairRepositoryRemotesRequest {
    pub bundle_id: String,
    pub repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionProjectSourceRequest {
    pub session_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScanRecoveryRequest {
    pub all_instances: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DestroyRecoveryRequest {
    pub session_id: String,
    pub target_id: String,
    pub confirmation: String,
    pub all_instances: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeRepositoryPreflightRequest {
    pub session_id: String,
    pub target_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReplaceResumeRepositoryOriginRequest {
    pub session_id: String,
    pub repository_id: String,
    pub replacement: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveSpacePreviewRequest {
    pub older_than_days: Option<u32>,
}

impl HttpDaemonClient {
    pub async fn new_session_remote_repairs(
        &self,
        bundle_id: String,
        target_id: String,
    ) -> anyhow::Result<Vec<LocalRemoteRepair>> {
        self.post_json(
            "/api/v1/projects/remotes/preflight",
            &NewSessionRemoteRepairsRequest {
                bundle_id,
                target_id,
            },
        )
        .await
    }

    pub async fn repair_repository_remotes(
        &self,
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    ) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/projects/remotes/repair",
            &RepairRepositoryRemotesRequest { bundle_id, repairs },
        )
        .await
    }

    pub async fn session_project_source(
        &self,
        session_id: String,
    ) -> anyhow::Result<ProjectSourceIdentity> {
        self.post_json(
            "/api/v1/projects/session-source",
            &SessionProjectSourceRequest { session_id },
        )
        .await
    }

    pub async fn scan_recovery(&self, all_instances: bool) -> anyhow::Result<RecoveryScan> {
        self.post_json(
            "/api/v1/recovery/scan",
            &ScanRecoveryRequest { all_instances },
        )
        .await
    }

    pub async fn destroy_recovery(
        &self,
        session_id: String,
        target_id: String,
        confirmation: String,
        all_instances: bool,
    ) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/recovery/destroy",
            &DestroyRecoveryRequest {
                session_id,
                target_id,
                confirmation,
                all_instances,
            },
        )
        .await
    }

    pub async fn resume_repository_preflight(
        &self,
        session_id: String,
        target_id: String,
    ) -> anyhow::Result<ResumeRepositorySourcePreflight> {
        self.post_json(
            "/api/v1/projects/resume/preflight",
            &ResumeRepositoryPreflightRequest {
                session_id,
                target_id,
            },
        )
        .await
    }

    pub async fn replace_resume_repository_origin(
        &self,
        session_id: String,
        repository_id: String,
        replacement: String,
    ) -> anyhow::Result<ResumeRepositorySourcePreflight> {
        self.post_json(
            "/api/v1/projects/resume/replace-origin",
            &ReplaceResumeRepositoryOriginRequest {
                session_id,
                repository_id,
                replacement,
            },
        )
        .await
    }

    pub async fn archive_space_preview(
        &self,
        older_than_days: Option<u32>,
    ) -> anyhow::Result<ArchiveSpacePreview> {
        self.post_json(
            "/api/v1/archives/space-preview",
            &ArchiveSpacePreviewRequest { older_than_days },
        )
        .await
    }
}
