use super::HttpDaemonClient;
use crate::daemon::FirstRunSetupReport;
use crate::web::{WebListenerProcess, WebViewerAccess};
use mj_core::config::Config;
use mj_core::subagent::SubagentOptions;
use mj_core::targets::HostCapacityRow;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Serialize, Deserialize)]
pub struct RunFirstRunSetupRequest {
    pub project_directory: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveSetupRequest {
    pub original: String,
    pub updated: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecoverWebViewerRequest {
    pub action: crate::web::WebViewerRecovery,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveSpinnerStyleRequest {
    pub style: mj_core::config::SpinnerStyle,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetQuotaPreferencesRequest {
    pub prefer: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeQuotaProfilesRequest {
    pub profile_ids: Vec<String>,
    pub forced: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubagentOptionsRequest {
    pub profile: String,
    pub model: Option<String>,
    pub config: Option<Config>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WarmProfileCapabilitiesRequest {
    pub config: Config,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenameProfileRequest {
    pub old_id: String,
    pub new_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenameTargetRequest {
    pub old_id: String,
    pub new_id: String,
}

impl HttpDaemonClient {
    pub async fn config(&self) -> anyhow::Result<Config> {
        self.get_json("/api/v1/setup/configuration").await
    }

    pub async fn first_run_setup_due(&self) -> anyhow::Result<bool> {
        self.get_json("/api/v1/setup/first-run/due").await
    }

    pub async fn run_first_run_setup(
        &self,
        project_directory: Option<PathBuf>,
    ) -> anyhow::Result<Option<FirstRunSetupReport>> {
        self.post_json(
            "/api/v1/setup/first-run",
            &RunFirstRunSetupRequest { project_directory },
        )
        .await
    }

    pub async fn run_doctor(&self) -> anyhow::Result<Vec<String>> {
        self.post_empty("/api/v1/setup/doctor").await
    }

    pub async fn save_setup(&self, original: String, updated: String) -> anyhow::Result<Config> {
        self.post_json(
            "/api/v1/setup/save",
            &SaveSetupRequest { original, updated },
        )
        .await
    }

    pub async fn api_credential(&self) -> anyhow::Result<String> {
        self.get_json("/api/v1/terminal/api-credential").await
    }

    pub async fn web_access(&self) -> anyhow::Result<WebViewerAccess> {
        self.get_json("/api/v1/terminal/web-viewer/access").await
    }

    pub async fn recover_web_viewer(
        &self,
        action: crate::web::WebViewerRecovery,
    ) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/terminal/web-viewer/recover",
            &RecoverWebViewerRequest { action },
        )
        .await
    }

    pub async fn inspect_web_listener(&self) -> anyhow::Result<Vec<WebListenerProcess>> {
        self.get_json("/api/v1/terminal/web-viewer/listeners").await
    }

    pub async fn save_spinner_style(
        &self,
        style: mj_core::config::SpinnerStyle,
    ) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/terminal/spinner-style",
            &SaveSpinnerStyleRequest { style },
        )
        .await
    }

    pub async fn set_quota_preferences(&self, prefer: Vec<String>) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/terminal/quota/preferences",
            &SetQuotaPreferencesRequest { prefer },
        )
        .await
    }

    pub async fn check_availability(&self) -> anyhow::Result<()> {
        self.post_empty("/api/v1/terminal/availability/check").await
    }

    pub async fn host_capacity(&self) -> anyhow::Result<Vec<HostCapacityRow>> {
        self.get_json("/api/v1/terminal/capacity").await
    }

    pub async fn refresh_capacity(&self) -> anyhow::Result<()> {
        self.post_empty("/api/v1/terminal/capacity/refresh").await
    }

    pub async fn refresh_quota(&self) -> anyhow::Result<()> {
        self.post_empty("/api/v1/terminal/quota/refresh").await
    }

    pub async fn probe_quota_profiles(
        &self,
        profile_ids: Vec<String>,
        forced: bool,
    ) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/terminal/quota/probe",
            &ProbeQuotaProfilesRequest {
                profile_ids,
                forced,
            },
        )
        .await
    }

    pub async fn subagent_options(
        &self,
        profile: String,
        model: Option<String>,
        config: Option<Config>,
    ) -> anyhow::Result<SubagentOptions> {
        self.post_json(
            "/api/v1/terminal/subagent-options",
            &SubagentOptionsRequest {
                profile,
                model,
                config,
            },
        )
        .await
    }

    pub async fn warm_profile_capabilities(&self, config: Config) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/terminal/profile-capabilities/warm",
            &WarmProfileCapabilitiesRequest { config },
        )
        .await
    }

    pub async fn rename_profile(&self, old_id: String, new_id: String) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/setup/profiles/rename",
            &RenameProfileRequest { old_id, new_id },
        )
        .await
    }

    pub async fn rename_target(&self, old_id: String, new_id: String) -> anyhow::Result<()> {
        self.post_json(
            "/api/v1/setup/targets/rename",
            &RenameTargetRequest { old_id, new_id },
        )
        .await
    }
}
