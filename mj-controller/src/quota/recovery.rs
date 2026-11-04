//! Deadline calculation never asks a model to interpret a clock or timezone.
use super::*;

/// Whether a scoped quota window applies to the selected model.
pub(crate) fn window_applies(window: &QuotaWindow, model: &str) -> bool {
    window.model_scope.as_deref().is_none_or(|scope| {
        model
            .to_ascii_lowercase()
            .contains(&scope.to_ascii_lowercase())
    })
}

/// Score quota headroom against its reset deadline. Higher scores mean more
/// sustainable capacity for the requested model.
pub(crate) fn quota_score(windows: &[QuotaWindow], model: &str, now_seconds: i64) -> Option<f64> {
    windows
        .iter()
        .filter(|window| window_applies(window, model))
        .filter_map(|window| {
            let remaining = window.remaining_percent.map(f64::from).or_else(|| {
                window.used.zip(window.limit).and_then(|(used, limit)| {
                    (limit > 0).then(|| {
                        (100.0 * (limit as f64 - used as f64) / limit as f64).clamp(0.0, 100.0)
                    })
                })
            })?;
            let remaining = remaining.min(100.0);
            let score = if remaining == 0.0 {
                0.0
            } else if window
                .resets_at_epoch_seconds
                .is_some_and(|reset| reset <= now_seconds)
            {
                1.0
            } else if let (Some(reset), Some(duration)) = (
                window.resets_at_epoch_seconds,
                window.duration_seconds.filter(|d| *d > 0),
            ) {
                let seconds_left = reset.saturating_sub(now_seconds).clamp(0, duration);
                let pace = 100.0 * seconds_left as f64 / duration as f64;
                if pace <= f64::EPSILON {
                    4.0
                } else {
                    (remaining / pace).min(4.0)
                }
            } else {
                (remaining / 50.0).min(4.0)
            };
            Some(score)
        })
        .reduce(f64::min)
}

/// Fill only missing reset data; fresh usage values always win.
pub(crate) fn merge_reset_windows(windows: &mut Vec<QuotaWindow>, cached: &[QuotaWindow]) -> bool {
    let mut restored = false;
    for old in cached {
        if let Some(window) = windows
            .iter_mut()
            .find(|w| w.label.eq_ignore_ascii_case(&old.label))
        {
            if window.resets_at_epoch_seconds.is_none() && old.resets_at_epoch_seconds.is_some() {
                window.resets_at_epoch_seconds = old.resets_at_epoch_seconds;
                window.resets = old.resets.clone();
                restored = true;
            }
        } else {
            windows.push(old.clone());
            restored |= old.resets_at_epoch_seconds.is_some();
        }
    }
    restored
}

pub(crate) fn message_reset(message: &str, observed_ms: i64) -> Option<i64> {
    let now = DateTime::from_timestamp_millis(observed_ms)?
        .with_timezone(&Local)
        .fixed_offset();
    let lower = message.to_ascii_lowercase();
    let index = lower.find("reset")?;
    let value = message[index + 5..].trim_start_matches('s').trim();
    let value = value
        .strip_prefix("at ")
        .unwrap_or(value)
        .trim_start_matches(':')
        .trim();
    normalize_reset_at(value.lines().next()?.trim().trim_end_matches('.'), now)
        .map(|t| t.timestamp())
}

/// Whether `window` shows no quota left and has not reset by `now_seconds`.
pub(crate) fn exhausted(window: &QuotaWindow, now_seconds: i64) -> bool {
    window
        .resets_at_epoch_seconds
        .is_none_or(|t| t > now_seconds)
        && (window.remaining_percent == Some(0)
            || window
                .used
                .zip(window.limit)
                .is_some_and(|(used, limit)| limit > 0 && used >= limit))
}

/// Only a long allowance with a distant or unknown continuation time merits a Move.
pub(crate) fn long_window_stop(
    windows: &[QuotaWindow],
    model: &str,
    explicit: Option<i64>,
    reset: Option<i64>,
    now_seconds: i64,
) -> bool {
    let exhausted: Vec<_> = windows
        .iter()
        .filter(|w| window_applies(w, model) && exhausted(w, now_seconds))
        .collect();
    if exhausted.is_empty() {
        explicit.is_some_and(|time| time.saturating_sub(now_seconds) > 5 * 60 * 60)
    } else {
        exhausted.iter().any(|w| w.is_weekly_or_longer())
            && reset.is_none_or(|time| time.saturating_sub(now_seconds) > 5 * 60 * 60)
    }
}

/// A reset already used for an unsuccessful retry cannot schedule another retry.
pub(crate) fn recovery_reset(
    windows: &[QuotaWindow],
    explicit: Option<i64>,
    now_seconds: i64,
    consumed: Option<i64>,
) -> Option<i64> {
    let usable = |time: i64| time > now_seconds && consumed.is_none_or(|old| time > old);
    let exhausted: Vec<_> = windows
        .iter()
        .filter(|w| exhausted(w, now_seconds))
        .collect();
    if !exhausted.is_empty() {
        // Unknown exhausted windows must not be silently ignored. A provider's
        // explicit reset can supply the missing bound.
        return exhausted
            .into_iter()
            .map(|w| {
                w.resets_at_epoch_seconds
                    .filter(|t| usable(*t))
                    .or(explicit.filter(|t| usable(*t)))
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max();
    }
    explicit.filter(|t| usable(*t)).or_else(|| {
        windows
            .iter()
            .filter_map(|w| w.resets_at_epoch_seconds)
            .filter(|t| usable(*t))
            .min()
    })
}

#[cfg(test)]
mod scoring_tests {
    use super::*;

    fn window(remaining: u8, reset: Option<i64>, duration: Option<i64>) -> QuotaWindow {
        QuotaWindow {
            label: "window".into(),
            remaining_percent: Some(remaining),
            duration_seconds: duration,
            model_scope: None,
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: reset,
        }
    }

    #[test]
    fn quota_score_accounts_for_reset_pacing_and_scoped_windows() {
        let now = 1_000_000;
        let short = window(5, Some(now + 180), Some(18_000));
        let weekly = window(5, Some(now + 6 * 24 * 60 * 60), Some(604_800));
        let short_score = quota_score(std::slice::from_ref(&short), "claude-opus-5", now).unwrap();
        let weekly_score =
            quota_score(std::slice::from_ref(&weekly), "claude-opus-5", now).unwrap();
        assert_eq!(short_score, 4.0);
        assert!(short_score > weekly_score);

        assert_eq!(
            quota_score(&[window(0, Some(now + 5), Some(10))], "x", now),
            Some(0.0)
        );
        assert_eq!(
            quota_score(&[window(20, Some(now), Some(10))], "x", now),
            Some(1.0)
        );
        assert_eq!(quota_score(&[window(25, None, None)], "x", now), Some(0.5));
        let mut usage = window(0, None, None);
        usage.remaining_percent = None;
        usage.used = Some(75);
        usage.limit = Some(100);
        assert_eq!(quota_score(&[usage], "x", now), Some(0.5));
        assert_eq!(quota_score(&[short, weekly], "x", now), Some(weekly_score));

        let mut scoped = window(0, Some(now + 10), Some(10));
        scoped.model_scope = Some("fable".into());
        assert_eq!(quota_score(&[scoped.clone()], "claude-opus-5", now), None);
        assert_eq!(quota_score(&[scoped], "CLAUDE-FABLE-5-1", now), Some(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(label: &str, remaining: u8, reset: Option<i64>) -> QuotaWindow {
        QuotaWindow {
            label: label.into(),
            remaining_percent: Some(remaining),
            duration_seconds: None,
            model_scope: None,
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: reset,
        }
    }
    #[test]
    fn waits_for_every_exhausted_window_and_never_reuses_a_reset() {
        let mut windows = vec![window("5H", 0, Some(500)), window("Week", 0, Some(900))];
        assert_eq!(recovery_reset(&windows, None, 100, None), Some(900));
        assert_eq!(recovery_reset(&windows, None, 600, Some(500)), Some(900));
        windows[1].remaining_percent = Some(50);
        assert_eq!(recovery_reset(&windows, None, 100, None), Some(500));
        assert_eq!(recovery_reset(&windows, None, 100, Some(500)), None);
        windows[0].resets_at_epoch_seconds = None;
        assert_eq!(recovery_reset(&windows, None, 100, None), None);
        assert_eq!(recovery_reset(&windows, Some(700), 100, None), Some(700));
        windows[0].remaining_percent = Some(20);
        assert_eq!(recovery_reset(&windows, None, 100, None), Some(900));
    }
    #[test]
    fn long_window_decision_respects_scope_unknown_resets_and_five_hour_boundary() {
        let now = 1_000_000;
        let short = window("5H", 0, Some(now + 18_000));
        let weekly = window("Week", 0, Some(now + 18_001));
        let mut scoped = window("Week (Fable)", 0, None);
        scoped.model_scope = Some("fable".into());
        let mut duration = window("Allowance", 0, None);
        duration.duration_seconds = Some(604_800);
        for (name, windows, model, explicit, expected) in [
            (
                "exactly five hours",
                vec![window("Week", 0, Some(now + 18_000))],
                "x",
                None,
                false,
            ),
            ("distant weekly", vec![weekly.clone()], "x", None, true),
            ("short only", vec![short.clone()], "x", None, false),
            (
                "unknown weekly",
                vec![window("Week", 0, None)],
                "x",
                None,
                true,
            ),
            ("both exhausted", vec![short, weekly], "x", None, true),
            (
                "scoped applies",
                vec![scoped.clone()],
                "claude-fable",
                None,
                true,
            ),
            ("scoped excludes", vec![scoped], "claude-opus", None, false),
            ("duration", vec![duration], "x", None, true),
            ("explicit only", vec![], "x", Some(now + 18_001), true),
            ("explicit boundary", vec![], "x", Some(now + 18_000), false),
        ] {
            let applicable: Vec<_> = windows
                .into_iter()
                .filter(|w| window_applies(w, model))
                .collect();
            let reset = recovery_reset(&applicable, explicit, now, None);
            assert_eq!(
                long_window_stop(&applicable, model, explicit, reset, now),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn message_clock_uses_named_zone_and_rejects_dst_ambiguity() {
        let observed = DateTime::parse_from_rfc3339("2026-09-21T16:50:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            message_reset(
                "You've hit your session limit · resets 1:20pm (America/Chicago)",
                observed
            ),
            Some(
                DateTime::parse_from_rfc3339("2026-09-21T18:20:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
        let fall = DateTime::parse_from_rfc3339("2026-11-01T04:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(message_reset("resets 1:30am (America/Chicago)", fall), None);
        let spring = DateTime::parse_from_rfc3339("2026-03-08T04:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            message_reset("resets 2:30am (America/Chicago)", spring),
            None
        );
        assert_eq!(message_reset("resets 1pm (Invalid/Zone)", observed), None);
    }
}
