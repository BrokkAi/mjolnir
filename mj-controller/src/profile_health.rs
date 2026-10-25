//! Temporary profile health learned from provider failures (#1361).
//!
//! A turn that fails because the provider will not serve this account now
//! (rate limit, exhausted plan, provider outage) marks its profile and model
//! unhealthy for a backoff, so the next spawn chooses another profile instead
//! of failing the same way. A malformed request, a refused login (which
//! `RejectedLogins` owns) and network trouble on our side are not marks.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use mj_core::diagnostic::TurnDiagnostic;

/// The first backoff, doubled for each further failure that follows soon
/// after the previous mark ended.
const BASE_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// A failure this long after the previous mark ended starts over at
/// `BASE_BACKOFF`.
const QUIET_RESET: Duration = Duration::from_secs(10 * 60);

/// Why a provider will not serve a profile now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFault {
    RateLimited,
    QuotaExhausted,
    ProviderError,
}

impl ProfileFault {
    /// The fault a failed turn's diagnostic shows, if it is the profile's and
    /// not the request's. Codex reports typed `sessionFailure` categories
    /// (`limit` for a rate limit, `service` for a provider error); other
    /// bridges may report an HTTP status instead.
    pub fn of(diagnostic: &TurnDiagnostic) -> Option<Self> {
        if diagnostic.is_usage_limit() {
            return Some(Self::QuotaExhausted);
        }
        match diagnostic.code.as_deref() {
            Some("limit") => return Some(Self::RateLimited),
            Some("service") => return Some(Self::ProviderError),
            _ => {}
        }
        match diagnostic.http_status? {
            429 => Some(Self::RateLimited),
            402 => Some(Self::QuotaExhausted),
            500 | 502 | 503 | 504 | 529 => Some(Self::ProviderError),
            _ => None,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::RateLimited => "rate-limited",
            Self::QuotaExhausted => "out of quota",
            Self::ProviderError => "failing at the provider",
        }
    }
}

/// A profile and model that should not take new work before `until`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unhealthy {
    pub fault: ProfileFault,
    pub until: SystemTime,
    /// The provider's own words for the failure that set the mark.
    pub message: String,
}

impl Unhealthy {
    /// "rate-limited until 14:05:00 UTC (exceeded retry limit, ...)".
    pub fn describe(&self) -> String {
        let until = chrono::DateTime::<chrono::Utc>::from(self.until);
        format!(
            "{} until {} ({})",
            self.fault.describe(),
            until.format("%H:%M:%S UTC"),
            self.message
        )
    }
}

#[derive(Debug, Clone)]
struct Mark {
    unhealthy: Unhealthy,
    strikes: u32,
}

/// The daemon's one record of profile health, keyed by profile and model.
#[derive(Debug, Default)]
pub struct ProfileHealth {
    marks: BTreeMap<(String, String), Mark>,
}

static SHARED: OnceLock<Mutex<ProfileHealth>> = OnceLock::new();

/// The process-wide owner of provider health marks.
pub fn shared() -> &'static Mutex<ProfileHealth> {
    SHARED.get_or_init(|| Mutex::new(ProfileHealth::default()))
}

impl ProfileHealth {
    /// Record a failed turn of `profile_id` running `model`. Failures that
    /// arrive while the profile is already marked do not lengthen the mark:
    /// several sessions that started before the mark all fail together, and
    /// that is one event, not many. A failure soon after a mark ended doubles
    /// the backoff. A provider reset time, when given, sets the end instead.
    pub fn record(
        &mut self,
        profile_id: &str,
        model: &str,
        fault: ProfileFault,
        diagnostic: &TurnDiagnostic,
        now: SystemTime,
    ) {
        let key = (profile_id.to_owned(), model.to_owned());
        let strikes = match self.marks.get(&key) {
            Some(mark) if mark.unhealthy.until > now => return,
            Some(mark) if mark.unhealthy.until + QUIET_RESET > now => mark.strikes + 1,
            _ => 1,
        };
        let backoff = BASE_BACKOFF
            .saturating_mul(1 << (strikes - 1).min(16))
            .min(MAX_BACKOFF);
        let until = provider_reset(diagnostic, now).unwrap_or(now + backoff);
        self.marks.insert(
            key,
            Mark {
                unhealthy: Unhealthy {
                    fault,
                    until,
                    message: diagnostic.message.clone(),
                },
                strikes,
            },
        );
    }

    /// The mark on `profile_id` and `model` that has not yet ended.
    pub fn unhealthy(&self, profile_id: &str, model: &str, now: SystemTime) -> Option<&Unhealthy> {
        self.marks
            .get(&(profile_id.to_owned(), model.to_owned()))
            .map(|mark| &mark.unhealthy)
            .filter(|unhealthy| unhealthy.until > now)
    }
}

/// The provider's reset time, when it gave one in a form with a timezone:
/// RFC 3339, or seconds (or milliseconds) since the Unix epoch.
fn provider_reset(diagnostic: &TurnDiagnostic, now: SystemTime) -> Option<SystemTime> {
    let value = diagnostic.reset_at.as_deref()?.trim();
    let at = if let Ok(number) = value.parse::<u64>() {
        let millis = if number > 100_000_000_000 {
            number
        } else {
            number.checked_mul(1000)?
        };
        SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
    } else {
        chrono::DateTime::parse_from_rfc3339(value).ok()?.into()
    };
    (at > now).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(message: &str, code: Option<&str>, http_status: Option<u16>) -> TurnDiagnostic {
        TurnDiagnostic {
            message: message.to_owned(),
            code: code.map(str::to_owned),
            http_status,
            reset_at: None,
        }
    }

    // The diagnostics are the ones codex-acp's `sessionFailure` produced in
    // the failures #1361 describes, as the daemon stored them.
    #[test]
    fn only_failures_of_the_account_mark_a_profile() {
        let rate_limit = diagnostic(
            "exceeded retry limit, last status: 429 Too Many Requests",
            Some("limit"),
            None,
        );
        let capacity = diagnostic(
            "Selected model is at capacity. Please try a different model.",
            Some("service"),
            None,
        );
        assert_eq!(
            ProfileFault::of(&rate_limit),
            Some(ProfileFault::RateLimited)
        );
        assert_eq!(
            ProfileFault::of(&capacity),
            Some(ProfileFault::ProviderError)
        );
        assert_eq!(
            ProfileFault::of(&diagnostic(
                "You've hit your usage limit.",
                Some("usageLimitExceeded"),
                None
            )),
            Some(ProfileFault::QuotaExhausted)
        );
        assert_eq!(
            ProfileFault::of(&diagnostic("overloaded", Some("-32603"), Some(529))),
            Some(ProfileFault::ProviderError)
        );
        for not_the_profile in [
            diagnostic("invalid request body", Some("request"), Some(400)),
            diagnostic("Forbidden", Some("-32603"), Some(403)),
            diagnostic("Unauthorized", Some("unauthorized"), Some(401)),
            diagnostic("stream disconnected", Some("connection"), None),
        ] {
            assert_eq!(
                ProfileFault::of(&not_the_profile),
                None,
                "{not_the_profile:?}"
            );
        }
    }

    #[test]
    fn simultaneous_failures_are_one_mark_and_repeats_soon_after_double_it() {
        let failure = diagnostic("429", Some("limit"), None);
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut health = ProfileHealth::default();
        let record = |health: &mut ProfileHealth, at: SystemTime| {
            health.record("codex2", "sol", ProfileFault::RateLimited, &failure, at);
            health.unhealthy("codex2", "sol", at).map(|mark| mark.until)
        };
        let first = record(&mut health, start).unwrap();
        assert_eq!(first, start + BASE_BACKOFF);
        // Five sessions that were already running fail during the mark.
        for second in 1..=5 {
            assert_eq!(
                record(&mut health, start + Duration::from_secs(second)),
                Some(first)
            );
        }
        assert!(health.unhealthy("codex2", "luna", start).is_none());
        assert!(health.unhealthy("codex2", "sol", first).is_none());
        let again = first + Duration::from_secs(30);
        assert_eq!(record(&mut health, again), Some(again + 2 * BASE_BACKOFF));
        let after_quiet = again + 2 * BASE_BACKOFF + QUIET_RESET;
        assert_eq!(
            record(&mut health, after_quiet),
            Some(after_quiet + BASE_BACKOFF)
        );
    }
}
