//! Daemon-computed environment readiness; profile environments retain sources only.
use crate::settings::RejectedRuntime;
use mj_core::config::{HarnessProfile, Machine, TargetTemplate};
use std::collections::BTreeMap;
/// What probing one machine found.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MachineRuntimeProbe {
    pub machine_id: String,
    /// The machine as it was probed.
    pub machine: Machine,
    pub finding: MachineFinding,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum MachineFinding {
    /// The machine answered.
    Answered {
        /// The runtimes it can run, under the ids they would like.
        runtimes: Vec<(String, TargetTemplate)>,
        /// The runtimes it cannot run, with why.
        rejected: Vec<RejectedRuntime>,
    },
    /// The machine did not answer, so what it runs is unknown.
    Unreachable { detail: String, remediation: String },
}

/// One of this computer's container engines, as a check found it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LocalEngine {
    /// Why it cannot run sessions, or `None` when it can.
    pub missing: Option<String>,
    /// Whether its command is on this computer at all. A standard runtime
    /// for an engine that was never installed is left out without a word;
    /// one that is installed but not working is named.
    pub installed: bool,
}

/// What one availability check found and changed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AvailabilityChecked {
    /// Every configured profile as it was checked, and why it is missing.
    pub profiles: BTreeMap<String, (HarnessProfile, Option<String>)>,
    /// This computer's engines, by runtime kind (`podman`, `docker`,
    /// `apple-container`).
    pub local: BTreeMap<String, LocalEngine>,
    /// What each SSH and EC2 machine can run.
    pub machines: Vec<MachineRuntimeProbe>,
    pub added_profiles: Vec<String>,
    pub added_runtimes: BTreeMap<String, Vec<String>>,
}
