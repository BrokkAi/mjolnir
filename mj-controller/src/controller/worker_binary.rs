//! Worker binary acquisition, profile staging, and worker installation.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::session_manager::{
    DeferredWorkerBinaryRefresh, ProjectMemorySyncTarget, WorkerBinaryRefresh,
    WorkerLaunchRefreshPlan, WorkerRecoveryPlan, WorkerWorkspace,
};
use crate::targets::{self, CommandExecutor, CommandPlan, CommandSpec, ProvisionStage, SshTarget};
use mj_core::config::{
    HarnessKind, HarnessProfile, ProjectBundle, ProjectRepository, atomic_write, data_dir,
};
use mj_core::harness_runtime::{CLAUDE_ACP_VERSION, CODEX_ACP_PACKAGE, CODEX_ACP_VERSION};
use mj_core::project_memory::{ProjectMemoryIdentity, RepositoryMemoryIdentity};
use mj_core::worker_build::{BUILD_ID, verify_worker_build};
use mj_core::worker_launch::{
    HarnessRuntimePolicy, ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery, WorkerLaunchConfig,
    WorkerOwnership,
};

use super::backend::backend_locator;
use super::readiness::{
    WORKER_EXIT_RECORD_MARKER, WORKER_PROCESS_MARKER, WORKER_STARTUP_RECORD_MARKER,
};
use super::{Controller, execute_checked, target_profile_home};

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

mod launch;
mod staging;
pub(super) use staging::*;
mod project_memory;
use project_memory::*;
mod binary_source;
pub use binary_source::*;
mod binary_select;
pub(super) use binary_select::*;
mod harness;
pub(super) use harness::*;
mod catalog;
pub(super) use catalog::*;
mod install;
pub(super) use install::*;
mod upgrade;
pub(crate) use upgrade::*;
mod process;
pub(super) use process::*;

#[cfg(test)]
mod tests;
