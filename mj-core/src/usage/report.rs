//! Read-only session accounting report shared by the daemon and terminal.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HOUR_MS: i64 = 3_600_000;
/// The graph shows this many whole clock hours, ending with the current one.
pub const USAGE_GRAPH_HOURS: i64 = 24;

/// Start of the clock hour (UTC) that contains `ms`.
pub const fn hour_start_ms(ms: i64) -> i64 {
    // An API caller can send any i64; the extremes saturate.
    ms.div_euclid(HOUR_MS).saturating_mul(HOUR_MS)
}

/// Start of the oldest hour the graph shows when `now_ms` is the current time.
pub const fn graph_start_ms(now_ms: i64) -> i64 {
    hour_start_ms(now_ms) - (USAGE_GRAPH_HOURS - 1) * HOUR_MS
}

/// Unknown fields are ignored: a client from before the hour graph still sends
/// `period`, and the report it gets back is the graph's 24 hours.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageQuery {
    /// Counts only turns recorded on this profile, anywhere in a parent's
    /// sub-agent tree; None counts every turn. Rows remain whole parents.
    pub profile: Option<String>,
    /// Start of one clock hour (see `hour_start_ms`) whose turns the rows
    /// cover. None covers every hour the graph shows.
    #[serde(default)]
    pub hour_start_ms: Option<i64>,
    #[serde(default)]
    pub include_suspended: bool,
}

/// One bar of the usage graph: the turns that completed in one clock hour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HourlyUsage {
    pub start_ms: i64,
    /// Sum of the costs that are known.
    pub known_cost_usd_ticks: u64,
    /// Some turn in the hour used a model that cannot be priced, so the
    /// known sum is a lower bound.
    pub has_unknown_cost: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAgentRole {
    Primary,
    Orchestrator,
    Subtle,
    Straightforward,
    Oracle,
    Reviewer,
    Subagent,
    UnclassifiedSubagent,
}

impl UsageAgentRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => "Primary agents",
            Self::Orchestrator => "Orchestrators",
            Self::Subtle => "Subtle subagents",
            Self::Straightforward => "Straightforward subagents",
            Self::Oracle => "Oracles",
            Self::Reviewer => "Reviewers",
            Self::Subagent => "Other subagents",
            Self::UnclassifiedSubagent => "Unclassified subagents",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUsageEstimate {
    pub role: UsageAgentRole,
    pub models: Vec<ModelUsageEstimate>,
    pub cost_usd_ticks: CostUsdTicks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageReport {
    /// The span `sessions` and `agent_usage` cover: the selected hour, or
    /// every hour the graph shows.
    pub since_ms: i64,
    pub until_ms: i64,
    /// One bar per hour the graph shows, oldest first, for the same profile
    /// and suspended filters but regardless of the selected hour.
    #[serde(default)]
    pub hours: Vec<HourlyUsage>,
    /// Includes retained parent and turn profile names even if no longer
    /// configured.
    pub profiles: Vec<String>,
    pub sessions: Vec<SessionUsageEstimate>,
    /// Filtered model totals grouped by each session's retained role. Children
    /// destroyed before kind retention was introduced remain unclassified.
    #[serde(default)]
    pub agent_usage: Vec<AgentUsageEstimate>,
    /// Sum of `sessions`; unknown when any session's cost is unknown.
    pub cost_usd_ticks: CostUsdTicks,
}

impl Default for SessionUsageReport {
    fn default() -> Self {
        Self {
            since_ms: 0,
            until_ms: 0,
            hours: Vec::new(),
            profiles: Vec::new(),
            sessions: Vec::new(),
            agent_usage: Vec::new(),
            cost_usd_ticks: Ok(0),
        }
    }
}
