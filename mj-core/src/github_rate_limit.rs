//! Reading GitHub's rate-limit refusals, shared by every GitHub API caller.

use chrono::{DateTime, Utc};

/// When a GitHub API response is a rate-limit refusal, the moment it may be
/// retried; `None` when it is not one. Covers the primary limit (remaining
/// requests exhausted), HTTP 429, and secondary limits, which GitHub signals
/// with `Retry-After` while requests remain.
pub fn github_rate_limit_retry_at(
    status: http::StatusCode,
    headers: &http::HeaderMap,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
    let retry_after = headers
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .map(|seconds| now + chrono::Duration::seconds(seconds.min(i64::MAX as u64) as i64))
                .or_else(|| {
                    DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(|date| date.with_timezone(&Utc))
                })
        });
    let exhausted = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "0");
    (exhausted || status == http::StatusCode::TOO_MANY_REQUESTS || retry_after.is_some()).then(
        || {
            retry_after
                .into_iter()
                .chain(reset)
                .max()
                .unwrap_or_else(|| now + chrono::Duration::minutes(1))
        },
    )
}
