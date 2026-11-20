//! Worker binary acquisition, profile staging, and worker installation.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::targets::{
    self, CommandExecutor, CommandPlan, CommandSpec, ProvisionStage, ProvisionStageGuard, SshTarget,
};
use mj_core::config::{HarnessKind, HarnessProfile, ProjectBundle, atomic_write, data_dir};
#[cfg(any(test, feature = "test-support"))]
use mj_core::harness_runtime::{CLAUDE_ACP_VERSION, CODEX_ACP_VERSION};
use mj_core::project_memory::ProjectMemoryIdentity;
use mj_core::worker_build::{verify_worker_build, worker_build_id};
use mj_core::worker_launch::{
    HarnessRuntimePolicy, ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery, WorkerLaunchConfig,
};
use mj_session_types::{
    DeferredWorkerBinaryRefresh, WorkerBinaryRefresh, WorkerLaunchRefreshPlan, WorkerWorkspace,
};

use crate::targets::{backend_locator, target_profile_home};
use mj_targets::command_checks::{command_error_detail, execute_checked};

/// Run a `reqwest::blocking` request on a dedicated OS thread and return its
/// result.
///
/// A `reqwest::blocking::Client` owns a private Tokio runtime and drops it when
/// the client is dropped. Dropping a runtime while the current thread has a
/// Tokio `block_on` context entered panics with "Cannot drop a runtime in a
/// context where blocking is not allowed". The session-move lifecycle drives
/// this otherwise synchronous staging code under `Handle::block_on` (see
/// `daemon::session_move`), so the parent thread does have such a context
/// entered. A freshly spawned OS thread has entered no runtime, so the client's
/// runtime is created and dropped there without tripping that check. Every
/// caller of these HTTP helpers is protected, not just the move path.
fn on_dedicated_thread<T: Send>(work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|scope| {
        scope.spawn(work).join().unwrap_or_else(|panic| {
            Err(anyhow::anyhow!(
                "blocking HTTP thread panicked: {}",
                targets::command_thread_panic_message(panic.as_ref())
            ))
        })
    })
}

pub mod launch;
pub use launch::{LaunchHistory, WorkerFilePreparation};
mod staging;
pub use staging::*;
mod binary_source;
pub use binary_source::*;
mod binary_select;
pub use binary_select::ssh_worker_binary_prerequisite;
pub use binary_select::worker_source_problem;
pub use binary_select::*;
mod harness;
pub use harness::*;
mod catalog;
pub use catalog::*;
mod image_worker;
pub use image_worker::image_worker_label;
pub use image_worker::*;
mod install;
mod prewarm;
pub use install::*;
pub use prewarm::{prewarm_profile_content, shared_content_ssh};
mod store;
use store::*;
pub use store::{install_session_executable, install_worker_binary, store_root};
mod microvm;
pub use microvm::ensure_microvm_engine;
mod upgrade;
pub use upgrade::*;
mod session_token;
pub use session_token::*;
mod project_memory;
pub use project_memory::*;
mod process;
pub use process::*;
pub use process::{ControlWorker, recorded_exit_reason, run_control_command};

#[cfg(any(test, feature = "test-support"))]
pub mod test_fixtures;
#[cfg(test)]
mod tests;
