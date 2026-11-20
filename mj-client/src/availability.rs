//! Daemon-computed environment readiness; profile environments retain sources only.
use crate::settings::RejectedRuntime;
use mj_core::config::{Machine, TargetTemplate};
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
    /// The SSH check failed, so what it runs is unknown.
    Unreachable {
        detail: String,
        remediation: String,
        /// True only when the host answered nothing at all.
        // Older daemons omit this fact; keep their failures actionable.
        #[serde(default)]
        answered_nothing: bool,
    },
}

/// A verdict applies to the entry's current configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AvailabilityVerdict {
    Available,
    Missing {
        reason: String,
    },
    #[default]
    Unknown,
}

impl AvailabilityVerdict {
    pub fn missing_reason(&self) -> Option<&str> {
        match self {
            Self::Missing { reason } => Some(reason),
            _ => None,
        }
    }
}

/// One line under a target picker. A machine that did not answer is muted
/// and can name several runtimes in one line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AvailabilityNote {
    pub text: String,
    pub full: String,
    pub routine: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeAvailability {
    pub profiles: BTreeMap<String, AvailabilityVerdict>,
    pub targets: BTreeMap<String, AvailabilityVerdict>,
    pub target_notes: Vec<AvailabilityNote>,
    pub machines_without_runtime: BTreeMap<String, String>,
    /// Explanations prepared for browser and API clients in the daemon.
    #[serde(skip)]
    pub browser: BrowserAvailabilityReasons,
    /// The local engine command is absent regardless of the last probe.
    pub uninstalled_engines: std::collections::BTreeSet<String>,
}

/// Public wording for the same missing verdicts. Only in-process projection
/// code needs both versions. The terminal feed carries its own wording.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserAvailabilityReasons {
    pub profiles: BTreeMap<String, String>,
    pub targets: BTreeMap<String, String>,
    pub target_notes: Vec<AvailabilityNote>,
}
