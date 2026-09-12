//! Quota data and display helpers shared by Mjolnir's control surfaces.

use serde::{Deserialize, Serialize};

use mj_core::config::HarnessKind;

/// Label used when a harness is billed by API usage rather than a subscription.
pub const API_LABEL: &str = "API";

/// A quota window reported by a harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaWindow {
    pub label: String,
    pub remaining_percent: Option<u8>,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    pub resets: Option<String>,
    #[serde(default)]
    pub resets_at_epoch_seconds: Option<i64>,
}

/// The quota report shown for one harness profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileQuota {
    pub profile_id: String,
    pub harness: HarnessKind,
    pub windows: Vec<QuotaWindow>,
    pub extra: Option<String>,
    pub error: Option<String>,
    pub refreshed_at_epoch_seconds: u64,
}

impl ProfileQuota {
    pub fn weekly_window(&self) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| is_weekly_quota_window(&window.label))
    }

    pub fn five_hour_window(&self) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| is_short_quota_window(&window.label))
    }

    /// Whether the report says the profile is usage-priced: an API-billed
    /// harness has no subscription window to fill, so it reports the API label
    /// in place of one rather than inventing a percentage.
    pub fn is_usage_priced(&self) -> bool {
        self.error.is_none() && self.windows.is_empty() && self.extra.as_deref() == Some(API_LABEL)
    }

    pub fn five_hour_projects_exhaustion(&self) -> bool {
        self.five_hour_window().is_some_and(|window| {
            projects_exhaustion_before_reset(window, self.refreshed_at_epoch_seconds)
        })
    }

    pub fn compact(&self) -> String {
        if let Some(error) = &self.error {
            return quota_error_label(error);
        }
        let mut seen_resets = std::collections::BTreeSet::new();
        let mut parts = self
            .windows
            .iter()
            .filter(|window| {
                !is_short_quota_window(&window.label)
                    || projects_exhaustion_before_reset(window, self.refreshed_at_epoch_seconds)
            })
            .map(|window| {
                let usage = match (window.remaining_percent, window.used, window.limit) {
                    (Some(remaining), _, _) => format!("{remaining}% left"),
                    (_, Some(used), Some(limit)) => format!("{used}/{limit}"),
                    _ => "available".to_string(),
                };
                match window
                    .resets
                    .as_ref()
                    .filter(|reset| seen_resets.insert((*reset).clone()))
                {
                    Some(reset) => format!("{} {usage}, resets {reset}", window.label),
                    None => format!("{} {usage}", window.label),
                }
            })
            .collect::<Vec<_>>();
        if let Some(extra) = &self.extra {
            parts.push(extra.clone());
        }
        if parts.is_empty() {
            "no quota windows reported".to_string()
        } else {
            parts.join(" · ")
        }
    }

    pub fn error_label(&self) -> Option<String> {
        self.error.as_deref().map(quota_error_label)
    }
}

fn quota_error_label(error: &str) -> String {
    // This is the stable user-facing marker emitted by the Claude usage
    // adapter. Keep the display contract independent of the controller crate.
    if error == "login expired" {
        error.to_string()
    } else if error.starts_with("rate limited") {
        // The provider is throttling the usage endpoint, which is not the same
        // as the quota being unknown for good.
        "rate limited".to_string()
    } else {
        "unavailable".to_string()
    }
}

/// The dashboard's long-window column. A harness billed monthly rather than
/// weekly belongs in the same column; the label itself names the real period.
fn is_weekly_quota_window(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "week" | "weekly" | "7d" | "month" | "monthly"
    )
}

fn is_short_quota_window(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "5h" | "5-hour" | "5 hour"
    )
}

/// Whether this window is on course to run out before it resets.
#[must_use]
pub fn projects_exhaustion(window: &QuotaWindow, now: u64) -> bool {
    projects_exhaustion_before_reset(window, now)
}

fn projects_exhaustion_before_reset(window: &QuotaWindow, now: u64) -> bool {
    const FIVE_HOURS_SECONDS: i64 = 5 * 60 * 60;
    let Some(reset) = window.resets_at_epoch_seconds else {
        return false;
    };
    let Ok(now) = i64::try_from(now) else {
        return false;
    };
    let remaining_time = reset - now;
    let elapsed = FIVE_HOURS_SECONDS - remaining_time;
    if remaining_time <= 0 || elapsed <= 0 || elapsed >= FIVE_HOURS_SECONDS {
        return false;
    }
    if let (Some(used), Some(limit)) = (window.used, window.limit)
        && limit > 0
    {
        return i128::from(used.clamp(0, limit)) * i128::from(FIVE_HOURS_SECONDS)
            > i128::from(limit) * i128::from(elapsed);
    }
    window
        .remaining_percent
        .is_some_and(|remaining| i64::from(100 - remaining) * FIVE_HOURS_SECONDS > 100 * elapsed)
}
