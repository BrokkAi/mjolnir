use super::*;

#[derive(Debug)]
pub(super) struct ProjectionAdvancedError {
    pub(super) event_ordinal: u64,
}

impl std::fmt::Display for ProjectionAdvancedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "another projector committed relay event {} first",
            self.event_ordinal
        )
    }
}

impl std::error::Error for ProjectionAdvancedError {}

/// Delay before the next reconnect attempt after `failures` consecutive
/// failures. Doubles from `RECONNECT_INTERVAL` up to the ceiling.
pub(super) fn reconnect_delay(failures: u32) -> Duration {
    let doubling = failures.saturating_sub(1).min(u32::BITS - 1);
    RECONNECT_INTERVAL
        .saturating_mul(1_u32 << doubling)
        .min(RECONNECT_BACKOFF_CEILING)
}
