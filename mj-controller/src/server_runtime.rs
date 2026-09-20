//! The phone-oriented remote-control server: its HTTP surface, the controller
//! actions phones request, and the concurrency limits that keep them safe.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
mod api;
mod api_activity;
mod profile_catalog;

use mj_core::config::{Config, HarnessProfile, PhoneConfig, is_bare_project_target};
use mj_core::refusal::Refusal;
use mj_core::remote_git::display_url;
use mj_core::state::{MaterializedSession, ProjectSourceIdentity, SessionRecord, State};

use crate::controller::Controller;
use crate::quota::ProfileQuota;
use crate::server::{
    ActionOutcome, BackgroundTaskStopFailure, BackgroundTaskStopRequest, BrowserTranscript,
    ControllerAction, ControllerRequest, MovePreparationRequest, PreflightFailure,
    ReadReceiptRequest, ResumeQueueDisposition, ServerOptions, ViewerActivityDetails,
    ViewerActivityKind, ViewerBackgroundTask, ViewerMoveRecovery, ViewerQueuedPrompt, ViewerQuota,
    ViewerSnapshot, ViewerUserShell,
};
use crate::session_manager::{SessionManagerChannels, SessionManagerControl, new_command_id};
use crate::tailscale::TailscaleTls;
#[cfg(test)]
use crate::targets::ProcessExecutor;
use crate::targets::{CancellableProcessExecutor, CommandExecutor};
use crate::worker_client::CredentialSyncCoordinator;
use mj_core::relay::RelayCommand;
use mj_core::workspace::WorkspaceRecord;

use crate::controller::config_only_controller;
use crate::daemon::{
    CreateSessionControl, CreateSessionRequest, ResumeSessionRequest, RuntimeState,
};
use crate::pollers::{
    CredentialSyncNotices, CredentialSyncSignalTracker, QUOTA_STALE_AFTER, QuotaRefreshBatch,
    QuotaUpdate, apply_worker_record_update, credential_sync_targets, dashboard_worker_targets,
    projected_queued_prompts, queued_prompt_projection, quota_refresh_profiles,
    schedule_due_credential_syncs, spawn_quota_refresher,
};

#[derive(Debug, Clone)]
pub struct ServerArgs {
    bind: String,
    tailscale_detect: bool,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
}

impl From<&PhoneConfig> for ServerArgs {
    fn from(config: &PhoneConfig) -> Self {
        Self {
            bind: config.bind.clone(),
            tailscale_detect: config.tailscale_detect,
            tls_cert: config.tls_cert.clone(),
            tls_key: config.tls_key.clone(),
        }
    }
}

const TAILSCALE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const TAILSCALE_RENEW_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

mod projection;
use projection::*;
mod args;
use args::*;
mod phone_actions;
use phone_actions::*;
mod run;
pub use run::*;
mod support;
use support::*;
mod preflight;
use preflight::*;
mod actions;
use actions::*;
mod project_sources;
use project_sources::*;
mod snapshot;
use snapshot::*;

#[cfg(test)]
mod tests;
