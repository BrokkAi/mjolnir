//! The migration command lines.
//!
//! The parsers live with the orchestration they feed so `mj-cli` stays a thin
//! adapter. They are optional: the daemon does not parse migration arguments.

use crate::migrate::orchestrator::MigrateOptions;
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Args)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct MigratePinArgs {
    #[arg(long)]
    pub db: PathBuf,
}

#[derive(Debug, Args)]
pub struct MigrateReceiveArgs {
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct MigrateArgs {
    /// Configured SSH machine ID or OpenSSH destination for the new host.
    destination: String,
    /// Stable SSH destination that the new host can use to reach this host.
    #[arg(long)]
    source_host: Option<String>,
    /// Machine ID to assign to this host in the migrated configuration.
    #[arg(long)]
    source_machine_id: Option<String>,
    /// OpenSSH argument used from the destination to reach this host (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    source_ssh_arg: Vec<String>,
    /// OpenSSH argument used to reach the destination (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    ssh_arg: Vec<String>,
    /// Destination instance name.
    #[arg(long)]
    dest_instance: Option<String>,
    /// Override the destination data directory.
    #[arg(long)]
    dest_data_dir: Option<PathBuf>,
    /// Override the destination configuration directory.
    #[arg(long)]
    dest_config_dir: Option<PathBuf>,
    /// Path to the mj executable on the destination.
    #[arg(long, default_value = "mj")]
    remote_command: String,
    /// Inspect and report the migration without changing either host.
    #[arg(long)]
    pub dry_run: bool,
    /// Copy logs, diagnostics, backups, and other optional data.
    #[arg(long)]
    include_logs: bool,
    /// Copy the SessionWiki index.
    #[arg(long)]
    include_sessionwiki: bool,
    /// Leave the destination SessionWiki index untouched.
    #[arg(long, conflicts_with = "include_sessionwiki")]
    skip_sessionwiki: bool,
    /// Replace an existing destination SessionWiki index.
    #[arg(long)]
    overwrite_sessionwiki: bool,
    /// Skip source-host connectivity checks when retiring this host.
    #[arg(long)]
    disconnect_local: bool,
    /// Report EC2 SSH ingress gaps without changing security groups.
    #[arg(long)]
    no_aws_modify: bool,
    /// Do not start the destination daemon after activation.
    #[arg(long)]
    no_start: bool,
}

impl From<MigrateArgs> for MigrateOptions {
    fn from(args: MigrateArgs) -> Self {
        Self {
            destination: args.destination,
            source_host: args.source_host,
            source_machine_id: args.source_machine_id,
            source_ssh_arg: args.source_ssh_arg,
            ssh_arg: args.ssh_arg,
            dest_instance: args.dest_instance,
            dest_data_dir: args.dest_data_dir,
            dest_config_dir: args.dest_config_dir,
            remote_command: args.remote_command,
            dry_run: args.dry_run,
            include_logs: args.include_logs,
            include_sessionwiki: args.include_sessionwiki,
            skip_sessionwiki: args.skip_sessionwiki,
            overwrite_sessionwiki: args.overwrite_sessionwiki,
            disconnect_local: args.disconnect_local,
            no_aws_modify: args.no_aws_modify,
            no_start: args.no_start,
        }
    }
}
