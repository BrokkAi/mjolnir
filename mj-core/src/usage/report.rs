//! Read-only session accounting report shared by the daemon and terminal.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePeriod {
    #[default]
    Hourly,
    Daily,
    Weekly,
    Monthly,
}

impl UsagePeriod {
    pub const ALL: [Self; 4] = [Self::Hourly, Self::Daily, Self::Weekly, Self::Monthly];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Hourly => "Hourly",
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Monthly => "Monthly",
        }
    }

    pub const fn duration_ms(self) -> i64 {
        match self {
            Self::Hourly => 3_600_000,
            Self::Daily => 86_400_000,
            Self::Weekly => 7 * 86_400_000,
            Self::Monthly => 30 * 86_400_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageQuery {
    /// Parent's last profile; None includes every parent and its full tree.
    pub profile: Option<String>,
    #[serde(default)]
    pub period: UsagePeriod,
    #[serde(default)]
    pub include_suspended: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsageEstimate {
    pub model: Option<String>,
    pub totals: BTreeMap<String, crate::storage::UsageCounterTotal>,
    /// Known token cost, in 10^-10 USD. None means no known price.
    pub cost_usd_ticks: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageEstimate {
    pub session_id: String,
    pub title: String,
    pub profile: String,
    pub suspended: bool,
    pub subagent_count: usize,
    pub models: Vec<ModelUsageEstimate>,
    pub coverage: crate::storage::UsageCoverage,
    /// Sum of priced models; unpriced models remain in `models`.
    pub cost_usd_ticks: u64,
}

impl SessionUsageEstimate {
    pub fn is_partial(&self) -> bool {
        self.models
            .iter()
            .any(|model| model.cost_usd_ticks.is_none())
            || self.coverage.full_turn_reports < self.coverage.recorded_turns
            || self.coverage.unfinished_turns > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageReport {
    pub since_ms: i64,
    pub until_ms: i64,
    /// Includes retained parent profile names even if no longer configured.
    pub profiles: Vec<String>,
    pub sessions: Vec<SessionUsageEstimate>,
    pub cost_usd_ticks: u64,
}
