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
