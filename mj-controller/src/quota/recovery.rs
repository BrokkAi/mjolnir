//! Deadline calculation never asks a model to interpret a clock or timezone.
use super::*;

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
        .filter(|w| w.resets_at_epoch_seconds.is_none_or(|t| t > now_seconds))
        .filter(|w| {
            w.remaining_percent == Some(0)
                || w.used
                    .zip(w.limit)
                    .is_some_and(|(used, limit)| limit > 0 && used >= limit)
        })
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
mod tests {
    use super::*;
    fn window(label: &str, remaining: u8, reset: Option<i64>) -> QuotaWindow {
        QuotaWindow {
            label: label.into(),
            remaining_percent: Some(remaining),
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
