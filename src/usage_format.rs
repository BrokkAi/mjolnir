//! Shared formatting for provider quota and rate-limit displays.

use chrono::{DateTime, FixedOffset, Local, TimeZone};

/// Format a Unix reset timestamp as wall-clock time in the machine's local
/// time zone. Accepts seconds or milliseconds and rejects non-finite or
/// out-of-range values.
pub(crate) fn format_reset_local(epoch: f64) -> Option<String> {
    if !epoch.is_finite() {
        return None;
    }
    let seconds = if epoch.abs() >= 1_000_000_000_000.0 {
        (epoch / 1000.0).trunc() as i64
    } else {
        epoch.trunc() as i64
    };
    let local = Local.timestamp_opt(seconds, 0).single()?;
    let zone = iana_time_zone::get_timezone().ok();
    Some(format_reset_label(local.fixed_offset(), zone.as_deref()))
}

pub(crate) fn format_reset_local_seconds(epoch: i64) -> Option<String> {
    format_reset_local(epoch as f64)
}

/// Pure formatter split from local-zone discovery for deterministic tests.
fn format_reset_label(reset: DateTime<FixedOffset>, zone: Option<&str>) -> String {
    let when = reset.format("%b %-d at %-I:%M%P").to_string();
    match zone {
        Some(zone) if !zone.is_empty() => format!("{when} ({zone})"),
        _ => when,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_label_renders_local_wall_clock() {
        let paris = FixedOffset::east_opt(2 * 3600).expect("offset");
        let reset = paris
            .with_ymd_and_hms(2026, 6, 17, 16, 49, 0)
            .single()
            .expect("instant");
        assert_eq!(
            format_reset_label(reset, Some("Europe/Paris")),
            "Jun 17 at 4:49pm (Europe/Paris)"
        );
        assert_eq!(format_reset_label(reset, None), "Jun 17 at 4:49pm");

        let midnight = paris
            .with_ymd_and_hms(2026, 6, 18, 0, 59, 0)
            .single()
            .expect("instant");
        assert_eq!(
            format_reset_label(midnight, Some("Europe/Paris")),
            "Jun 18 at 12:59am (Europe/Paris)"
        );
    }

    #[test]
    fn reset_timestamp_accepts_seconds_and_milliseconds() {
        let seconds = 1_781_712_540_f64;
        assert_eq!(
            format_reset_local(seconds),
            format_reset_local(seconds * 1000.0)
        );
        assert_eq!(
            format_reset_local(seconds),
            format_reset_local_seconds(seconds as i64)
        );
    }
}
