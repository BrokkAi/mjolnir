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
    /// Counts only turns recorded on this profile, anywhere in a parent's
    /// sub-agent tree; None counts every turn. Rows remain whole parents.
    pub profile: Option<String>,
    #[serde(default)]
    pub period: UsagePeriod,
    #[serde(default)]
    pub include_suspended: bool,
}

/// Recorded tokens that cannot be priced: the model has no known rate, or
/// its counters do not say how many tokens of each kind it used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnknownCost;

/// A token cost in 10^-10 USD, or why it cannot be known.
pub type CostUsdTicks = Result<u64, UnknownCost>;

/// Add `cost` to `total`; an unknown part makes the whole sum unknown.
pub fn add_cost(total: CostUsdTicks, cost: CostUsdTicks) -> anyhow::Result<CostUsdTicks> {
    Ok(match (total, cost) {
        (Ok(a), Ok(b)) => Ok(a
            .checked_add(b)
            .ok_or_else(|| anyhow::anyhow!("cost overflow"))?),
        _ => Err(UnknownCost),
    })
}

/// Orders costs from highest to lowest, with unknown costs last.
pub fn descending_cost(a: &CostUsdTicks, b: &CostUsdTicks) -> std::cmp::Ordering {
    match (a, b) {
        (Ok(a), Ok(b)) => b.cmp(a),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => std::cmp::Ordering::Equal,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsageEstimate {
    pub model: Option<String>,
    pub totals: BTreeMap<String, crate::storage::UsageCounterTotal>,
    pub cost_usd_ticks: CostUsdTicks,
}

impl Default for ModelUsageEstimate {
    fn default() -> Self {
        Self {
            model: None,
            totals: BTreeMap::new(),
            cost_usd_ticks: Ok(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageEstimate {
    pub session_id: String,
    pub title: String,
    pub profile: String,
    pub suspended: bool,
    pub subagent_count: usize,
    pub models: Vec<ModelUsageEstimate>,
    pub coverage: crate::storage::UsageCoverage,
    /// Sum of `models`; unknown when any model's cost is unknown.
    pub cost_usd_ticks: CostUsdTicks,
}

impl Default for SessionUsageEstimate {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            title: String::new(),
            profile: String::new(),
            suspended: false,
            subagent_count: 0,
            models: Vec::new(),
            coverage: Default::default(),
            cost_usd_ticks: Ok(0),
        }
    }
}

impl SessionUsageEstimate {
    /// Some turns in the period are still running or reported no full usage,
    /// so a known cost is a lower bound.
    pub fn is_partial(&self) -> bool {
        self.coverage.full_turn_reports < self.coverage.recorded_turns
            || self.coverage.unfinished_turns > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageReport {
    pub since_ms: i64,
    pub until_ms: i64,
    /// Includes retained parent and turn profile names even if no longer
    /// configured.
    pub profiles: Vec<String>,
    pub sessions: Vec<SessionUsageEstimate>,
    /// Sum of `sessions`; unknown when any session's cost is unknown.
    pub cost_usd_ticks: CostUsdTicks,
}

impl Default for SessionUsageReport {
    fn default() -> Self {
        Self {
            since_ms: 0,
            until_ms: 0,
            profiles: Vec::new(),
            sessions: Vec::new(),
            cost_usd_ticks: Ok(0),
        }
    }
}
