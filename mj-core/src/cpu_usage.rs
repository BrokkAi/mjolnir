//! CPU accounting scopes, normalized to their machine.
use serde::{Deserialize, Serialize};

/// The physical scope actually measured, including for legacy workers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuSampleScope {
    #[default]
    ProcessTree,
    Container,
}

/// Placement chosen by the controller; container samples share a physical scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuAccountingMode {
    #[default]
    ProcessTree,
    Container,
}

impl CpuAccountingMode {
    pub fn is_process_tree(&self) -> bool {
        matches!(self, Self::ProcessTree)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCpuUsage {
    #[serde(default)]
    pub scope: CpuSampleScope,
    /// Machine CPU share over the newest interval, in tenths of a percent.
    pub recent_permille: u16,
    /// Time-weighted share with a one-hour decay, corrected for startup.
    pub hourly_permille: u16,
    /// Seconds covered by the average, capped at one hour.
    pub hourly_covered_secs: u32,
    pub online_cpus: u32,
}
