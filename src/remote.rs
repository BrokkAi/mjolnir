//! Compatibility re-exports for remote-control support.

pub use mj_remote::*;

use anyhow::Context;

#[derive(Debug)]
pub struct ServerOptions {
    pub hostname: Option<String>,
    pub tailscale_detect: bool,
    pub port: u16,
    pub history_days: u32,
    pub session_ttl_days: u32,
    pub logout_all: bool,
    pub cwd: std::path::PathBuf,
    pub additional_directories: Vec<std::path::PathBuf>,
    pub snapshot_exclusions: Vec<std::path::PathBuf>,
    pub fs_max_text_bytes: u64,
    pub termination: tokio_util::sync::CancellationToken,
}

pub async fn run_server(options: ServerOptions) -> anyhow::Result<()> {
    let config_path = crate::config::default_config_path();
    let mut cfg = crate::config::Config::load(&config_path)
        .with_context(|| format!("load {}", config_path.display()))?;
    cfg.apply_default_team();
    // A machine with no launchable model still gets a serving viewer: the
    // web UI walks the user through sign-in and team selection, and the
    // session manager re-resolves until a roster binds. Every other
    // resolution failure stays fatal.
    let resolved = match crate::roster::resolve(&cfg, &options.cwd).await {
        Ok(roster) => Ok(roster),
        Err(error) => match error.downcast_ref::<mj_core::roster::NothingLaunchable>() {
            Some(nothing) => {
                tracing::warn!("starting setup-pending: {}", nothing.message);
                Err(nothing.message.clone())
            }
            None => return Err(error),
        },
    };
    let config_hash = crate::remote_host::config_file_hash(&config_path);
    let session_manager: std::sync::Arc<crate::remote_host::RootServerSessionManager> =
        std::sync::Arc::new(match &resolved {
            Ok(roster) => crate::remote_host::RootServerSessionManager::new_roster(
                roster.clone(),
                config_hash,
                options.cwd.clone(),
                options.additional_directories.clone(),
                options.snapshot_exclusions.clone(),
                options.fs_max_text_bytes,
            ),
            Err(reason) => crate::remote_host::RootServerSessionManager::new_unresolved(
                reason.clone(),
                config_hash,
                options.cwd.clone(),
                options.additional_directories.clone(),
                options.snapshot_exclusions.clone(),
                options.fs_max_text_bytes,
            ),
        });
    run_server_runtime(RuntimeServerOptions {
        config: cfg,
        roster: resolved.map_err(SetupPending),
        hostname: options.hostname,
        tailscale_detect: options.tailscale_detect,
        port: options.port,
        history_days: options.history_days,
        session_ttl_days: options.session_ttl_days,
        logout_all: options.logout_all,
        cwd: options.cwd,
        additional_directories: options.additional_directories,
        snapshot_exclusions: options.snapshot_exclusions,
        fs_max_text_bytes: options.fs_max_text_bytes,
        session_manager,
        termination: options.termination,
    })
    .await
}
