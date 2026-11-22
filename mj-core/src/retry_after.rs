//! The `Retry-After` header of an HTTP reply.

use std::time::Duration;

/// A `Retry-After` value: a number of seconds, or an HTTP date. A date that
/// has passed, or anything else, says nothing.
pub fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (date.with_timezone(&chrono::Utc) - now).to_std().ok()
}

pub fn retry_after_header(headers: &http::HeaderMap) -> Option<Duration> {
    let value = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, chrono::Utc::now())
}
