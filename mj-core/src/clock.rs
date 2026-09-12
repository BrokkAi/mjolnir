//! Small, dependency-free helpers for reading the wall clock as Unix epoch
//! values. Kept free of other `crate::` modules so anything can use it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. Reads before the epoch report as 0.
pub fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Milliseconds since the Unix epoch, saturating at `i64::MAX`. Reads before
/// the epoch report as 0.
pub fn epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_seconds_matches_epoch_millis_within_a_second() {
        let seconds = epoch_seconds();
        let millis = epoch_millis();
        assert!((millis / 1000 - seconds as i64).abs() <= 1);
    }

    #[test]
    fn epoch_seconds_is_after_this_codebases_epoch() {
        // 2020-01-01T00:00:00Z, a sanity floor well before this code existed.
        assert!(epoch_seconds() > 1_577_836_800);
    }
}
