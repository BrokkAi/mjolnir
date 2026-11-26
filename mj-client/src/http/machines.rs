use super::HttpDaemonClient;
use crate::daemon::PreflightNew;
use crate::discovery::SetupDetection;
use mj_core::credentials::LoginStatus;
use mj_core::state::BuildCachePreview;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
pub type ArchiveSizes = BTreeMap<String, Result<Option<u64>, String>>;
pub type ProfileLoginStatuses = BTreeMap<String, LoginStatus>;
pub type BuildCacheAvailability = (Option<BuildCachePreview>, bool);
pub type MountValidation = (PathBuf, Option<String>);
pub type SessionMountValidation = Option<(String, String)>;
use reqwest::Method;
#[derive(Debug, Serialize, Deserialize)]
pub struct PreviewBuildCacheRequest {
    pub machine: mj_core::config::Machine,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallMbxRequest {
    pub machine: mj_core::config::Machine,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateProjectDirectoryRequest {
    pub target_id: String,
    pub directory: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckpointArchiveSizesRequest {
    pub paths: std::collections::BTreeMap<String, PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TestTargetRequest {
    pub target_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoverSetupRequest {
    pub scope: crate::discovery::DetectScope,
    pub machines: Vec<(mj_core::config::MachineId, mj_core::config::Machine)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckProjectDirectoriesRequest {
    pub paths: Vec<std::path::PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResolveContainerPathRequest {
    pub target_id: String,
    pub source: std::path::PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResolveSetupPathRequest {
    pub machine: mj_core::config::Machine,
    pub source: std::path::PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateMountSourceRequest {
    pub target_id: String,
    pub source: std::path::PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateSessionMountsRequest {
    pub target_id: String,
    pub mounts: Vec<mj_core::targets::AdditionalMount>,
}

impl HttpDaemonClient {
    pub async fn preview_build_cache(
        &self,
        machine: mj_core::config::Machine,
    ) -> anyhow::Result<BuildCacheAvailability> {
        self.post_json(
            "/api/v1/machines/build-cache/preview",
            &PreviewBuildCacheRequest { machine },
        )
        .await
    }

    pub async fn install_mbx(&self, machine: mj_core::config::Machine) -> anyhow::Result<String> {
        self.json(
            Method::POST,
            "/api/v1/machines/build-cache/install",
            |request| request.json(&InstallMbxRequest { machine }),
            false,
        )
        .await
    }

    pub async fn validate_project_directory(
        &self,
        target_id: String,
        directory: PathBuf,
    ) -> anyhow::Result<PreflightNew> {
        self.post_json(
            "/api/v1/projects/directory/validate",
            &ValidateProjectDirectoryRequest {
                target_id,
                directory,
            },
        )
        .await
    }

    pub async fn checkpoint_archive_sizes(
        &self,
        paths: std::collections::BTreeMap<String, PathBuf>,
    ) -> anyhow::Result<ArchiveSizes> {
        self.post_json(
            "/api/v1/archives/sizes",
            &CheckpointArchiveSizesRequest { paths },
        )
        .await
    }

    pub async fn test_target(&self, target_id: String) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/machines/targets/test",
            &TestTargetRequest { target_id },
        )
        .await
    }

    pub async fn profile_login_statuses(&self) -> anyhow::Result<ProfileLoginStatuses> {
        self.post_empty("/api/v1/profiles/login-statuses").await
    }

    pub async fn discover_setup(
        &self,
        scope: crate::discovery::DetectScope,
        machines: Vec<(mj_core::config::MachineId, mj_core::config::Machine)>,
    ) -> anyhow::Result<SetupDetection> {
        self.post_json(
            "/api/v1/setup/discover",
            &DiscoverSetupRequest { scope, machines },
        )
        .await
    }

    pub async fn check_project_directories(
        &self,
        paths: Vec<std::path::PathBuf>,
    ) -> anyhow::Result<Vec<(PathBuf, bool)>> {
        self.post_json(
            "/api/v1/projects/directories/check",
            &CheckProjectDirectoriesRequest { paths },
        )
        .await
    }

    pub async fn resolve_container_path(
        &self,
        target_id: String,
        source: std::path::PathBuf,
    ) -> anyhow::Result<PathBuf> {
        self.post_json(
            "/api/v1/machines/container-path/resolve",
            &ResolveContainerPathRequest { target_id, source },
        )
        .await
    }

    pub async fn resolve_setup_path(
        &self,
        machine: mj_core::config::Machine,
        source: std::path::PathBuf,
    ) -> anyhow::Result<PathBuf> {
        self.post_json(
            "/api/v1/setup/path/resolve",
            &ResolveSetupPathRequest { machine, source },
        )
        .await
    }

    pub async fn validate_mount_source(
        &self,
        target_id: String,
        source: std::path::PathBuf,
    ) -> anyhow::Result<MountValidation> {
        self.post_json(
            "/api/v1/mounts/source/validate",
            &ValidateMountSourceRequest { target_id, source },
        )
        .await
    }

    pub async fn validate_session_mounts(
        &self,
        target_id: String,
        mounts: Vec<mj_core::targets::AdditionalMount>,
    ) -> anyhow::Result<SessionMountValidation> {
        self.post_json(
            "/api/v1/mounts/session/validate",
            &ValidateSessionMountsRequest { target_id, mounts },
        )
        .await
    }
}
