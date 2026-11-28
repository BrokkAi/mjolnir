//! Read-only session accounting report shared by the daemon and terminal.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};

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
    /// Selected clock hours; when present, overrides `hour_start_ms`.
    /// An empty set selects no hours.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_hours",
        deserialize_with = "deserialize_hours"
    )]
    pub hour_starts_ms: Option<BTreeSet<i64>>,
    #[serde(default)]
    pub include_suspended: bool,
}

fn serialize_hours<S: Serializer>(
    hours: &Option<BTreeSet<i64>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match hours {
        Some(hours) => serializer.serialize_str(
            &hours
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
        None => serializer.serialize_none(),
    }
}

fn deserialize_hours<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BTreeSet<i64>>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            if value.is_empty() {
                return Ok(BTreeSet::new());
            }
            value
                .split(',')
                .map(|part| part.parse().map_err(serde::de::Error::custom))
                .collect()
        })
        .transpose()
}

impl SessionUsageQuery {
    /// Inclusive selected intervals, clipped to now. Adjacent clock hours are
    /// merged so their shared boundary is not treated as missing coverage.
    pub fn selected_intervals(&self, now_ms: i64) -> Vec<(i64, i64)> {
        let starts = match &self.hour_starts_ms {
            Some(hours) => hours
                .iter()
                .copied()
                .map(hour_start_ms)
                .collect::<BTreeSet<_>>(),
            None => match self.hour_start_ms {
                Some(hour) => BTreeSet::from([hour_start_ms(hour)]),
                None => return vec![(graph_start_ms(now_ms), now_ms)],
            },
        };
        let mut intervals: Vec<(i64, i64)> = Vec::new();
        for start in starts {
            if start > now_ms {
                break;
            }
            let end = now_ms.min(start.saturating_add(HOUR_MS - 1));
            if let Some(last) = intervals.last_mut()
                && start <= last.1.saturating_add(1)
            {
                last.1 = last.1.max(end);
            } else {
                intervals.push((start, end));
            }
        }
        intervals
    }
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
    /// Completed turns on this model whose usage was missing, unspecified, or
    /// last-request-only. Their cost is unknown, so `cost_usd_ticks` is a lower
    /// bound: the known sum of full-turn reports, never poisoned to
    /// `Err(UnknownCost)` by them.
    #[serde(default)]
    pub unreported_turns: u64,
    pub cost_usd_ticks: CostUsdTicks,
}

impl Default for ModelUsageEstimate {
    fn default() -> Self {
        Self {
            model: None,
            totals: BTreeMap::new(),
            unreported_turns: 0,
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
        self.coverage.is_partial()
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

/// Usage of the turns recorded on one profile, whichever parent or sub-agent
/// ran them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileUsageEstimate {
    /// The profile each turn recorded when it started; None for turns that
    /// recorded none.
    pub profile: Option<String>,
    /// Counted exactly as a session's coverage, over this profile's turns, so
    /// `coverage.is_partial()` says whether the cost is a lower bound.
    pub coverage: crate::storage::UsageCoverage,
    /// Sum of the turns' model costs; unknown when any is unknown.
    pub cost_usd_ticks: CostUsdTicks,
}

impl Default for ProfileUsageEstimate {
    fn default() -> Self {
        Self {
            profile: None,
            coverage: Default::default(),
            cost_usd_ticks: Ok(0),
        }
    }
}

/// How much of one profile's unscoped weekly quota was consumed during the
/// report's span, as the daemon read it from the profile's quota history.
/// Not a dollar value: it never converts API cost to quota.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileQuotaUsage {
    pub profile: String,
    /// 10,000 is all of one weekly allowance. An estimate from the probes the
    /// history holds. None when no part of the span could be measured.
    pub consumed_basis_points: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_seconds: Option<u64>,
    /// The probes did not cover the whole span (no reading before the start or
    /// after the end, a reset whose earlier use was lost, or an interval that
    /// could not be read), so the figure covers only part of it.
    #[serde(default)]
    pub partial: bool,
    /// The profile's plan price, which weights profiles of one provider.
    pub monthly_price_usd: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileQuotaProjection {
    pub profile_id: String,
    pub window_label: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_short_window: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_weekly_window: Option<bool>,
    pub remaining_percent: u8,
    pub resets_at_epoch_seconds: Option<i64>,
    pub observed_pct_per_hour: Option<f64>,
    pub sustainable_pct_per_hour: Option<f64>,
    pub pace_ratio: Option<f64>,
    pub projected_exhaustion_epoch_seconds: Option<i64>,
}

impl PartialEq for ProfileQuotaProjection {
    fn eq(&self, other: &Self) -> bool {
        self.profile_id == other.profile_id
            && self.window_label == other.window_label
            && self.is_short_window == other.is_short_window
            && self.is_weekly_window == other.is_weekly_window
            && self.remaining_percent == other.remaining_percent
            && self.resets_at_epoch_seconds == other.resets_at_epoch_seconds
            && self.observed_pct_per_hour.map(f64::to_bits)
                == other.observed_pct_per_hour.map(f64::to_bits)
            && self.sustainable_pct_per_hour.map(f64::to_bits)
                == other.sustainable_pct_per_hour.map(f64::to_bits)
            && self.pace_ratio.map(f64::to_bits) == other.pace_ratio.map(f64::to_bits)
            && self.projected_exhaustion_epoch_seconds == other.projected_exhaustion_epoch_seconds
    }
}
impl Eq for ProfileQuotaProjection {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRoleUsageEstimate {
    pub profile: Option<String>,
    pub role: UsageAgentRole,
    pub models: Vec<ModelUsageEstimate>,
    pub cost_usd_ticks: CostUsdTicks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleAssignmentChange {
    pub id: i64,
    pub changed_at_ms: i64,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageReport {
    /// Enclosing bounds of the selected hours; gaps do not contribute usage.
    pub since_ms: i64,
    pub until_ms: i64,
    /// One bar per hour the graph shows, oldest first, for the same profile
    /// and suspended filters but regardless of the selected hour.
    #[serde(default)]
    pub hours: Vec<HourlyUsage>,
    /// Includes retained parent and turn profile names even if no longer
    /// configured.
    pub profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profile_providers: BTreeMap<String, String>,
    pub sessions: Vec<SessionUsageEstimate>,
    /// Filtered model totals grouped by each session's retained role. Children
    /// destroyed before kind retention was introduced remain unclassified.
    #[serde(default)]
    pub agent_usage: Vec<AgentUsageEstimate>,
    /// The same turns grouped by the profile each was recorded on, which can
    /// differ from its parent session's profile.
    #[serde(default)]
    pub profile_usage: Vec<ProfileUsageEstimate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile_role_usage: Vec<ProfileRoleUsageEstimate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub role_changes: Vec<RoleAssignmentChange>,
    /// Weekly quota consumed in the selected intervals, filled by the daemon.
    #[serde(default)]
    pub quota_usage: Vec<ProfileQuotaUsage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_projections: Vec<ProfileQuotaProjection>,
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
            profile_providers: BTreeMap::new(),
            sessions: Vec::new(),
            agent_usage: Vec::new(),
            profile_usage: Vec::new(),
            profile_role_usage: Vec::new(),
            role_changes: Vec::new(),
            quota_usage: Vec::new(),
            quota_projections: Vec::new(),
            cost_usd_ticks: Ok(0),
        }
    }
}
