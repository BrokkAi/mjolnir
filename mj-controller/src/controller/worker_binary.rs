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
    ProjectMemorySyncTarget, RemoteWorkerBinaryRefresh, WorkerBinaryRefresh,
    WorkerBinaryRefreshPlan, WorkerLaunchRefreshPlan, WorkerRecoveryPlan, WorkerWorkspace,
};
use crate::targets::{self, CommandExecutor, CommandPlan, CommandSpec, ProvisionStage, SshTarget};
use mj_core::config::{
    HarnessKind, HarnessProfile, ProjectBundle, ProjectRepository, atomic_write, data_dir,
};
use mj_core::harness_runtime::{CLAUDE_ACP_VERSION, CODEX_ACP_PACKAGE, CODEX_ACP_VERSION};
use mj_core::project_memory::{ProjectMemoryIdentity, RepositoryMemoryIdentity};
use mj_core::worker_launch::{
    HarnessRuntimePolicy, ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery, WorkerLaunchConfig,
    WorkerOwnership,
};

use super::backend::backend_locator;
use super::readiness::WORKER_EXIT_RECORD_MARKER;
use super::{Controller, execute_checked, target_profile_home};

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
