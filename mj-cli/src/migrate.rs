//! Host migration commands and the source cutover owner.

use anyhow::Result;
#[cfg(not(unix))]
use anyhow::bail;

pub(crate) use mj_controller::migrate::cli::{MigrateArgs, MigratePinArgs, MigrateReceiveArgs};

pub(crate) fn run_pin(args: MigratePinArgs) -> Result<()> {
    #[cfg(unix)]
    {
        mj_controller::migrate::pin::run_pin_helper(
            &args.db,
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        bail!("host migration only supports Unix hosts")
    }
}

pub(crate) fn apply_receive_env(args: &MigrateReceiveArgs, instance: Option<&str>) -> Result<()> {
    // Called from main before starting any threads.
    unsafe {
        if let Some(path) = &args.data_dir {
            std::env::set_var("MJ_DATA_DIR", path);
        }
        if let Some(path) = &args.config_dir {
            std::env::set_var("MJ_CONFIG_DIR", path);
        }
    }
    mj_core::config::apply_instance_flag(instance)
}

pub(crate) fn run_receive(_args: MigrateReceiveArgs) -> Result<()> {
    #[cfg(not(unix))]
    bail!("host migration only supports Unix hosts");
    #[cfg(unix)]
    mj_controller::migrate::orchestrator::run_receive(Some(std::sync::Arc::new(|| {
        Box::pin(async {
            crate::daemon::connect_or_start_with_held_start_lock()
                .await
                .map(|_| ())
        })
    })))
}

#[cfg(not(unix))]
pub(crate) async fn run_migrate(_args: MigrateArgs) -> Result<()> {
    bail!("host migration only supports Unix hosts")
}

#[cfg(unix)]
pub(crate) async fn run_migrate(args: MigrateArgs) -> Result<()> {
    mj_controller::migrate::orchestrator::run(args.into(), CliDaemon).await
}

#[cfg(unix)]
struct CliDaemon;

#[cfg(unix)]
impl mj_controller::migrate::orchestrator::MigrationDaemonControl for CliDaemon {
    type StartGuard = crate::daemon::DaemonStartGuard;

    async fn daemon_configured_for_container(&self) -> bool {
        crate::daemon::daemon_configured_for_container().await
    }

    async fn acquire_start_guard(&self, path: std::path::PathBuf) -> Result<Self::StartGuard> {
        crate::daemon::acquire_start_guard(path).await
    }

    async fn stop_instance_holding(&self, guard: &Self::StartGuard, force: bool) -> Result<bool> {
        crate::daemon::stop_instance_holding(guard, force).await
    }

    async fn connect_or_start_holding(&self, guard: &Self::StartGuard) -> Result<()> {
        crate::daemon::connect_or_start_holding(guard)
            .await
            .map(|_| ())
    }
}
