//! Small helpers for the words Mjolnir shows people.

/// `count` and the noun that agrees with it: `1 command`, `2 commands`,
/// `0 entries`. Counts shown to a person go through this, so none reads
/// "1 commands" or "2 session(s)" (launch findings C-12 and R5-10).
pub fn counted<T>(count: T, singular: &str, plural: &str) -> String
where
    T: std::fmt::Display + PartialEq + From<u8>,
{
    let noun = if count == T::from(1) {
        singular
    } else {
        plural
    };
    format!("{count} {noun}")
}

/// A byte count as `512B`, `1.5K`, `3.2M`, `14.2G` or `1.0T`.
pub fn format_resource_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;

    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / KIB)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / MIB)
    } else if bytes < 1024_u64.pow(4) {
        format!("{:.1}G", bytes as f64 / GIB)
    } else {
        format!("{:.1}T", bytes as f64 / TIB)
    }
}

/// How long ago `refreshed` was, as `12s ago`, `5m ago` or `3h ago`; a zero
/// timestamp means the time is not known.
pub fn refresh_age(now: u64, refreshed: u64) -> String {
    if refreshed == 0 {
        return "unknown".into();
    }
    let age = now.saturating_sub(refreshed);
    let (value, unit) = if age < 60 {
        (age, "s")
    } else if age < 3_600 {
        (age / 60, "m")
    } else {
        (age / 3_600, "h")
    };
    format!("{value}{unit} ago")
}

#[cfg(test)]
mod tests {
    use super::counted;

    // Hard-won: 25c6af83: Setup and Close showed user-facing counts with malformed noun suffixes.
    #[test]
    fn counted_agrees_the_noun_with_the_count() {
        assert_eq!(counted(0_usize, "entry", "entries"), "0 entries");
        assert_eq!(counted(1_usize, "entry", "entries"), "1 entry");
        assert_eq!(counted(2_i64, "command", "commands"), "2 commands");
    }
}
