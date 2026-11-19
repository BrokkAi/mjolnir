//! The retry delay shared by background session policies.

use std::time::Duration;

/// The retry shape every background session policy uses: `base` after the
/// first failure, doubling per consecutive failure, never past `cap`.
///
/// Capped rather than unbounded because a target that is broken rather than
/// blipping must keep being probed, just rarely enough to cost nothing.
pub fn backoff_delay(base: Duration, cap: Duration, consecutive_failures: u32) -> Duration {
    let doublings = consecutive_failures.saturating_sub(1).min(u32::BITS - 1);
    base.checked_mul(1 << doublings).unwrap_or(cap).min(cap)
}
