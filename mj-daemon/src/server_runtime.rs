//! The phone-oriented remote-control server: its HTTP surface, the controller
//! actions phones request, and the concurrency limits that keep them safe.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
pub(crate) mod api;
mod api_activity;
pub(crate) use mj_controller::profile_catalog;
pub(crate) mod publication;

use mj_core::config::{Config, PhoneConfig, is_bare_project_target};
use mj_core::refusal::Refusal;
use mj_core::remote_git::{display_network_url, display_network_urls};
use mj_core::state::{MaterializedSession, ProjectSourceIdentity, SessionRecord, State};

use crate::server::{
    ActionOutcome, BrowserTranscript, ControllerAction, ControllerRequest, ResumeQueueDisposition,
    ServerOptions, ViewerActivityDetails, ViewerActivityKind, ViewerBackgroundTask,
    ViewerMoveRecovery, ViewerQueuedPrompt, ViewerQuota, ViewerSnapshot, ViewerUserShell,
};
use mj_controller::controller::Controller;
use mj_controller::quota::ProfileQuota;
use mj_controller::session_manager::{SessionManagerControl, new_command_id};
use mj_controller::tailscale::TailscaleTls;
#[cfg(test)]
use mj_controller::targets::ProcessExecutor;
use mj_controller::targets::{CancellableProcessExecutor, CommandExecutor};
use mj_core::relay::RelayCommand;
use mj_core::workspace::WorkspaceRecord;

use crate::daemon::{CreateSessionRequest, LifecycleControl, ResumeSessionRequest, RuntimeState};
use crate::pollers::QUOTA_STALE_AFTER;
use mj_controller::controller::config_only_controller;

fn prompt_submission_error(error: anyhow::Error) -> anyhow::Error {
    use mj_controller::session_manager::SubmitRefusalCode;
    use mj_core::relay::RelayErrorCode;

    if error.is::<mj_client::session::Refused>()
        && error
            .downcast_ref::<SubmitRefusalCode>()
            .is_some_and(|code| code.0 == RelayErrorCode::InvalidState)
    {
        let message = error.root_cause().to_string();
        return error.context(Refusal::precondition(message));
    }
    error
}

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
pub(crate) use projection::ConversationProjectionKey;
use projection::*;
mod args;
use args::*;
mod phone_actions;
use phone_actions::*;
mod run;
pub(crate) use run::*;
mod support;
use support::*;
pub(crate) mod preflight;
use preflight::*;
mod actions;
use actions::*;
mod project_discovery;
mod project_sources;
pub(crate) use project_sources::ProjectSourceKey;
use project_sources::*;
pub(crate) mod snapshot;
pub(crate) use crate::server::phone_prompt_blocks;
use snapshot::*;

#[cfg(test)]
mod tests;
