//! CPU use of one worker's process tree, normalized to its machine.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCpuUsage {
    /// Machine CPU share over the newest interval, in tenths of a percent.
    pub recent_permille: u16,
    /// Time-weighted share with a one-hour decay, corrected for startup.
    pub hourly_permille: u16,
    /// Seconds covered by the average, capped at one hour.
    pub hourly_covered_secs: u32,
    pub online_cpus: u32,
}
