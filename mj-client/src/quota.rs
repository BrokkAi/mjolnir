//! Quota data and display helpers shared by Mjolnir's control surfaces.

use std::collections::{BTreeMap, BTreeSet};

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

/// What the daemon publishes about quota. The daemon is the only process that
/// asks a provider for quota; every surface reads this instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaSnapshot {
    /// The latest report for each enabled profile.
    pub reports: BTreeMap<String, ProfileQuota>,
    /// Profiles the daemon is asking a provider about right now.
    pub probing: BTreeSet<String>,
    /// Probe cycles the daemon has finished. A surface that asked for a
    /// refresh remembers the value it saw and knows the refresh is over when
    /// this grows.
    pub cycles: u64,
}

/// The quota report shown for one harness profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileQuota {
    /// Unused provider-granted resets, absent when the provider cannot report them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banked_resets: Option<u64>,
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

/// Which existing TUI countdown convention applies to a quota window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCountdownStyle {
    #[default]
    Long,
    FiveHour,
}

impl QuotaWindow {
    pub fn reset_countdown_style(&self) -> ResetCountdownStyle {
        if is_short_quota_window(&self.label) {
            ResetCountdownStyle::FiveHour
        } else {
            ResetCountdownStyle::Long
        }
    }

    pub fn reset_display(&self, now: u64, banked_resets: Option<u64>) -> String {
        format_quota_reset(
            now,
            self.resets_at_epoch_seconds,
            self.resets.as_deref(),
            self.reset_countdown_style(),
            banked_resets,
        )
    }
}

impl ProfileQuota {
    /// Only the long quota window carries the account's banked reset balance.
    pub fn banked_resets_for_window(&self, window: &QuotaWindow) -> Option<u64> {
        self.banked_resets.filter(|_| {
            self.weekly_window()
                .is_some_and(|weekly| weekly.label == window.label)
        })
    }
}

/// Render a reset time and its optional banked balance. The browser equivalent
/// is checked against the same examples in quota/reset_display_cases.json.
pub fn format_quota_reset(
    now: u64,
    reset: Option<i64>,
    fallback: Option<&str>,
    style: ResetCountdownStyle,
    banked_resets: Option<u64>,
) -> String {
    let mut display = match reset {
        Some(reset) => match style {
            ResetCountdownStyle::Long => quota_reset_countdown(now, reset),
            ResetCountdownStyle::FiveHour => five_hour_quota_reset_countdown(now, reset),
        },
        None => fallback.unwrap_or_default().to_owned(),
    };
    if let Some(count) = banked_resets.filter(|count| *count > 0) {
        if !display.is_empty() {
            display.push(' ');
        }
        display.push_str(&format!("[{count}]"));
    }
    display
}

pub fn quota_reset_countdown(now: u64, reset_at_epoch_seconds: i64) -> String {
    let Ok(reset) = u64::try_from(reset_at_epoch_seconds) else {
        return "now".into();
    };
    let remaining = reset.saturating_sub(now);
    if remaining == 0 {
        return "now".into();
    }

    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if remaining >= DAY {
        let days = remaining / DAY;
        let hours = remaining % DAY / HOUR;
        format!("{days}d {hours}h")
    } else if remaining >= HOUR {
        let hours = remaining / HOUR;
        let minutes = remaining % HOUR / MINUTE;
        // Under ten hours the minutes decide whether to wait, so show them.
        if hours < 10 && minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if remaining >= MINUTE {
        format!("{}m", remaining / MINUTE)
    } else {
        "<1m".into()
    }
}

pub fn five_hour_quota_reset_countdown(now: u64, reset_at_epoch_seconds: i64) -> String {
    let Ok(reset) = u64::try_from(reset_at_epoch_seconds) else {
        return "now".into();
    };
    let remaining = reset.saturating_sub(now);
    if remaining == 0 {
        "now".into()
    } else if remaining < 60 {
        "<1m".into()
    } else if remaining < 60 * 60 {
        format!("{}m", remaining / 60)
    } else {
        let hours = remaining / (60 * 60);
        let minutes = remaining % (60 * 60) / 60;
        format!("{hours}h {minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_display_matches_shared_browser_cases() {
        #[derive(Deserialize)]
        struct Case {
            name: String,
            now: u64,
            reset: Option<i64>,
            fallback: Option<String>,
            style: ResetCountdownStyle,
            banked_resets: Option<u64>,
            expected: String,
        }
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("quota/reset_display_cases.json")).unwrap();
        for case in cases {
            assert_eq!(
                format_quota_reset(
                    case.now,
                    case.reset,
                    case.fallback.as_deref(),
                    case.style,
                    case.banked_resets
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn a_snapshot_round_trips_and_an_absent_one_decodes_as_empty() {
        let snapshot = QuotaSnapshot {
            reports: BTreeMap::from([(
                "claude".to_owned(),
                ProfileQuota {
                    banked_resets: None,
                    profile_id: "claude".into(),
                    harness: HarnessKind::Claude,
                    windows: Vec::new(),
                    extra: None,
                    error: None,
                    refreshed_at_epoch_seconds: 7,
                },
            )]),
            probing: BTreeSet::from(["claude".to_owned()]),
            cycles: 3,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_str::<QuotaSnapshot>(&json).unwrap(),
            snapshot
        );
    }

    #[test]
    fn old_quota_reports_decode_without_a_banked_balance() {
        let quota: ProfileQuota = serde_json::from_value(serde_json::json!({
            "profile_id": "claude", "harness": "claude", "windows": [],
            "extra": null, "error": null, "refreshed_at_epoch_seconds": 0
        }))
        .unwrap();
        assert_eq!(quota.banked_resets, None);
        assert!(
            serde_json::to_value(&quota)
                .unwrap()
                .get("banked_resets")
                .is_none()
        );
    }
}
