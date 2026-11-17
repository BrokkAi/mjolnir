//! Deadline calculation never asks a model to interpret a clock or timezone.
use super::*;

/// Whether a scoped quota window applies to the selected model.
pub(crate) fn window_applies(window: &QuotaWindow, model: &str) -> bool {
    window.applies_to_model(model)
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
            let score = if window
                .resets_at_epoch_seconds
                .is_some_and(|reset| reset <= now_seconds)
            {
                1.0
            } else if remaining == 0.0 {
                0.0
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
            .find(|w| w.label.eq_ignore_ascii_case(&old.label) && w.model_scope == old.model_scope)
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

/// A reset deadline a provider stated in its own message, and whether it was
/// stated as a duration from the refusal (Antigravity: "Resets in 40s").
/// A relative statement is only true of the moment it was read: repeating it
/// after a retry says nothing new about the deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatedReset {
    pub at: i64,
    pub relative: bool,
}

pub(crate) fn message_reset(message: &str, observed_ms: i64) -> Option<StatedReset> {
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
    let value = value.lines().next()?.trim().trim_end_matches('.');
    // Some providers name the wait as a duration from the refusal
    // (Antigravity: "Resets in 40s") rather than a wall-clock time.
    if let Some(seconds) = duration_seconds(value) {
        return Some(StatedReset {
            at: now.timestamp().checked_add(seconds)?,
            relative: true,
        });
    }
    normalize_reset_at(value, now).map(|at| StatedReset {
        at: at.timestamp(),
        relative: false,
    })
}

/// Seconds in a duration stated in words, as in Antigravity's "Resets in 40s"
/// or "Resets in 102h19m11s". Anything else (`at 14:00`, `Aug 14 at 4am`,
/// `2026-11-12T19:52:53Z`) is not a duration and the caller keeps its
/// wall-clock parsing.
fn duration_seconds(value: &str) -> Option<i64> {
    let value = value.trim();
    let value = value
        .strip_prefix("in ")
        .map(str::trim_start)
        .unwrap_or(value);
    let mut chars = value.chars().peekable();
    let mut total: i64 = 0;
    let mut stated = false;
    loop {
        while chars
            .peek()
            .is_some_and(|ch| ch.is_whitespace() || *ch == ',')
        {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut amount = String::new();
        while chars.peek().is_some_and(char::is_ascii_digit) {
            amount.push(chars.next()?);
        }
        while chars.peek().is_some_and(|ch| ch.is_whitespace()) {
            chars.next();
        }
        let mut unit = String::new();
        while chars.peek().is_some_and(char::is_ascii_alphabetic) {
            unit.push(chars.next()?);
        }
        if amount.is_empty() || unit.is_empty() {
            return None;
        }
        let unit_seconds = match unit.to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
            "d" | "day" | "days" => 24 * 60 * 60,
            "w" | "wk" | "wks" | "week" | "weeks" => 7 * 24 * 60 * 60,
            _ => return None,
        };
        total = total.checked_add(amount.parse::<i64>().ok()?.checked_mul(unit_seconds)?)?;
        stated = true;
    }
    stated.then_some(total)
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

/// The deadline for the next attempt after a refusal. The provider's own
/// message names the limit that just refused the turn, while its report can be
/// minutes older and cannot say which limit refused it, so a stated deadline
/// may bring the retry forward, never push it back.
///
/// A duration is read against the refusal that stated it, and the same limit
/// repeats it on every refusal, so honoring a repeat would retry that limit
/// forever. It is accepted again only after `report_refreshed_at` shows the
/// report was read from the provider after the refused retry: one such
/// re-reading can schedule at most one further retry, so a provider that keeps
/// saying "in 40s" cannot drive a loop.
pub(crate) fn retry_deadline(
    windows: &[QuotaWindow],
    stated: Option<StatedReset>,
    now_seconds: i64,
    consumed: Option<i64>,
    report_refreshed_at: Option<i64>,
) -> Option<i64> {
    // The statement is never an input here: a wait read back from it would be
    // the retry this function is deciding whether to schedule again.
    let measured = recovery_reset(windows, None, now_seconds, consumed);
    let refreshed_since_retry = match consumed {
        None => true,
        Some(consumed) => report_refreshed_at.is_some_and(|at| at > consumed),
    };
    let stated = stated
        .filter(|stated| stated.at > now_seconds && consumed.is_none_or(|old| stated.at > old))
        .filter(|stated| !stated.relative || refreshed_since_retry)
        .map(|stated| stated.at);
    stated.into_iter().chain(measured).min()
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
        assert_eq!(
            quota_score(&[window(0, Some(now), Some(10))], "x", now),
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
            Some(StatedReset {
                at: DateTime::parse_from_rfc3339("2026-09-21T18:20:00Z")
                    .unwrap()
                    .timestamp(),
                relative: false,
            })
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

    // Hard-won: #1555: Antigravity states a quota reset as a duration from the
    // refusal, which was unreadable, so its five-hour reset was overlooked and
    // the weekly window's days-away reset was the wait recorded for the session.
    #[test]
    fn antigravity_duration_reset_becomes_the_retry_instead_of_the_weekly_window() {
        let observed = DateTime::parse_from_rfc3339("2026-11-16T23:32:00Z")
            .unwrap()
            .timestamp_millis();
        let now = observed / 1000;
        let refusal = "⚠ Individual quota reached. Please upgrade your subscription to \
                        increase your limits. Resets in 40s.";
        let stated = message_reset(refusal, observed);
        assert_eq!(
            stated.map(|stated| stated.at - now),
            Some(40),
            "the short limit's reset is a duration from the refusal"
        );
        // The same harness states a long allowance with compound units, and a
        // line without a reset says nothing about a deadline.
        assert_eq!(
            message_reset(
                "⚠ Individual quota reached. Resets in 102h19m11s.",
                observed
            )
            .map(|stated| stated.at - now),
            Some(102 * 3_600 + 19 * 60 + 11)
        );
        assert_eq!(
            message_reset("You have used some of your 5-hour limit", observed),
            None
        );
        // A wall-clock reset is not a duration, so nothing here is relative.
        assert_eq!(
            message_reset("Quota reached; resets at 2026-11-17T02:00:00Z", observed),
            Some(StatedReset {
                at: DateTime::parse_from_rfc3339("2026-11-17T02:00:00Z")
                    .unwrap()
                    .timestamp(),
                relative: false,
            })
        );
        // The report the daemon already has is the one #1555 saw: the weekly
        // allowance exhausted with a reset six days out, and the five-hour
        // allowance with quota left, so its own reset says nothing. Only the
        // message names a usable deadline.
        let mut five_hour = window("5H", 55, Some(now + 3_600));
        five_hour.duration_seconds = Some(5 * 3_600);
        let weekly = window("Week", 0, Some(now + 6 * 86_400));
        let windows = vec![five_hour, weekly];
        let read_earlier = Some(now - 1_800);
        assert_eq!(
            retry_deadline(&windows, stated, now, None, read_earlier),
            Some(now + 40),
            "the five-hour limit's 40s must replace the weekly window's six days"
        );
        // The retry it scheduled was refused and the provider repeated the
        // same duration, with no newer reading. Reading it again would retry
        // forever, so the wait falls back to what the report names.
        let consumed = Some(now + 40);
        assert_eq!(
            retry_deadline(&windows, stated, now + 100, consumed, read_earlier),
            Some(now + 6 * 86_400),
            "a repeated duration must not schedule another retry"
        );
        // With no report at all the same guard holds, because the deadline
        // must never be read back from the statement itself.
        assert_eq!(retry_deadline(&[], stated, now, None, None), Some(now + 40));
        assert_eq!(retry_deadline(&[], stated, now + 100, consumed, None), None);
        // A second limit that resets in 40s is believed again only once the
        // provider was re-read after the refused retry: new evidence, and at
        // most one further retry per reading.
        let repeated = message_reset(refusal, (now + 100) * 1_000);
        assert_eq!(
            retry_deadline(&windows, repeated, now + 100, consumed, Some(now + 90)),
            Some(now + 140),
            "a reading taken after the refused retry makes the new duration credible"
        );
        assert_eq!(
            retry_deadline(&windows, repeated, now + 100, consumed, Some(now + 40)),
            Some(now + 6 * 86_400),
            "a reading from before the retry is not new evidence"
        );
        // A statement later than the reported wait never lengthens it, and an
        // elapsed one is no deadline at all.
        let reported = vec![window("Week", 0, Some(now + 600))];
        assert_eq!(
            retry_deadline(
                &reported,
                Some(StatedReset {
                    at: now + 3_600,
                    relative: false
                }),
                now,
                None,
                read_earlier
            ),
            Some(now + 600)
        );
        assert_eq!(
            retry_deadline(
                &reported,
                Some(StatedReset {
                    at: now,
                    relative: true
                }),
                now,
                None,
                read_earlier
            ),
            Some(now + 600)
        );
        // An absolute statement is used once too: retrying at that time leaves
        // it in the past, and a report that cannot name a reset schedules none.
        let absolute = message_reset("resets at 2026-11-17T02:00:00Z", observed);
        assert_eq!(
            retry_deadline(&[], absolute, now, None, None),
            Some(now + 2 * 3_600 + 28 * 60)
        );
        assert_eq!(
            retry_deadline(
                &[],
                absolute,
                now + 2 * 3_600 + 28 * 60 + 60,
                Some(now + 2 * 3_600 + 28 * 60),
                None
            ),
            None
        );
        assert_eq!(retry_deadline(&[], None, now, None, None), None);
    }
}
