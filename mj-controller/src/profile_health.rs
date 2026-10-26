//! Temporary profile health learned from provider failures (#1361).
//!
//! A turn that fails because the provider will not serve this account now
//! (rate limit, exhausted plan, provider outage) marks its profile and model
//! unhealthy for a backoff, so the next spawn chooses another profile instead
//! of failing the same way. A later successful turn clears a mark only when
//! it completed after that mark was set. A malformed request, a refused login
//! (which `RejectedLogins` owns) and network trouble on our side are not marks.
//! Quota reports can extend active quota marks to the reported reset.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::quota::QuotaWindow;
use chrono::{DateTime, Local};
use mj_core::diagnostic::TurnDiagnostic;

/// The first backoff, doubled for each further failure that follows soon
/// after the previous mark ended.
const BASE_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const PROVIDER_ERROR_BASE_BACKOFF: Duration = Duration::from_secs(8);
const PROVIDER_ERROR_MAX_BACKOFF: Duration = Duration::from_secs(64);
/// A failure this long after the previous mark ended starts over at
/// the base backoff for the new fault.
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

/// What a finished turn shows about its profile and model.
#[derive(Debug, Clone)]
pub enum TurnHealth {
    Failed(ProfileFault, TurnDiagnostic),
    /// A turn that completed at this time.
    Succeeded(SystemTime),
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
    set_at: SystemTime,
    explicit_reset: Option<i64>,
}

/// The daemon's one record of profile health, keyed by profile and model.
#[derive(Debug, Default)]
pub struct ProfileHealth {
    marks: BTreeMap<(String, String), Mark>,
    quota_windows: BTreeMap<String, Vec<QuotaWindow>>,
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
    /// the backoff using the schedule for its fault. A provider reset time,
    /// when given, sets the end. Quota marks also use the latest quota report.
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
        let (base, maximum) = match fault {
            ProfileFault::ProviderError => {
                (PROVIDER_ERROR_BASE_BACKOFF, PROVIDER_ERROR_MAX_BACKOFF)
            }
            ProfileFault::RateLimited | ProfileFault::QuotaExhausted => (BASE_BACKOFF, MAX_BACKOFF),
        };
        let backoff = base.saturating_mul(1 << (strikes - 1).min(16)).min(maximum);
        let explicit_reset_at = provider_reset(diagnostic, now);
        let explicit_reset = explicit_reset_at
            .map(|at| DateTime::<chrono::Utc>::from(at).timestamp())
            .or_else(|| {
                (fault == ProfileFault::QuotaExhausted)
                    .then(|| {
                        let now_ms = DateTime::<chrono::Utc>::from(now).timestamp_millis();
                        crate::quota::message_reset(&diagnostic.message, now_ms)
                    })
                    .flatten()
            });
        let until = if fault == ProfileFault::QuotaExhausted {
            quota_reset(
                self.quota_windows
                    .get(profile_id)
                    .map_or(&[], Vec::as_slice),
                model,
                explicit_reset,
                now,
            )
            .unwrap_or(now + backoff)
        } else {
            explicit_reset_at.unwrap_or(now + backoff)
        };
        self.marks.insert(
            key,
            Mark {
                unhealthy: Unhealthy {
                    fault,
                    until,
                    message: diagnostic.message.clone(),
                },
                strikes,
                set_at: now,
                explicit_reset,
            },
        );
    }

    /// Record what a finished turn of `profile_id` running `model` shows.
    pub fn observe(&mut self, profile_id: &str, model: &str, turn: TurnHealth, now: SystemTime) {
        match turn {
            TurnHealth::Failed(fault, diagnostic) => {
                self.record(profile_id, model, fault, &diagnostic, now)
            }
            TurnHealth::Succeeded(completed_at) => {
                self.record_success(profile_id, model, completed_at)
            }
        }
    }

    /// Clear a mark, and its strikes, for a successful turn that completed
    /// after the mark was set. An older success says nothing about the
    /// failure that set the mark, for example when a session catches up on
    /// events recorded before it.
    pub fn record_success(&mut self, profile_id: &str, model: &str, completed_at: SystemTime) {
        let key = (profile_id.to_owned(), model.to_owned());
        if self
            .marks
            .get(&key)
            .is_some_and(|mark| completed_at > mark.set_at)
        {
            self.marks.remove(&key);
        }
    }

    /// Whether any mark still affects selection or the next failure's
    /// backoff at `now`. A mark past its quiet reset is the same as none.
    pub fn has_marks(&self, now: SystemTime) -> bool {
        self.marks
            .values()
            .any(|mark| mark.unhealthy.until + QUIET_RESET > now)
    }

    /// Replace the latest quota windows for a profile and extend active quota
    /// marks when the report gives a later reset. A report never shortens a
    /// mark; only a successful turn ends one early.
    pub fn observe_quota(&mut self, profile_id: &str, windows: &[QuotaWindow], now: SystemTime) {
        self.quota_windows
            .insert(profile_id.to_owned(), windows.to_vec());
        for ((marked_profile, model), mark) in &mut self.marks {
            if marked_profile != profile_id
                || mark.unhealthy.fault != ProfileFault::QuotaExhausted
                || mark.unhealthy.until <= now
            {
                continue;
            }
            if let Some(reset) = quota_reset(windows, model, mark.explicit_reset, now) {
                mark.unhealthy.until = mark.unhealthy.until.max(reset);
            }
        }
    }

    /// Forget a profile's quota windows when the profile is no longer enabled.
    pub fn forget_quota(&mut self, profile_id: &str) {
        self.quota_windows.remove(profile_id);
    }

    /// The mark on `profile_id` and `model` that has not yet ended.
    pub fn unhealthy(&self, profile_id: &str, model: &str, now: SystemTime) -> Option<&Unhealthy> {
        self.marks
            .get(&(profile_id.to_owned(), model.to_owned()))
            .map(|mark| &mark.unhealthy)
            .filter(|unhealthy| unhealthy.until > now)
    }
}

/// When exhausted quota comes back, as autocontinue decides it, but from the
/// exhausted windows only. A window with quota left says nothing about the
/// limit that failed, so its reset must not hold the mark.
fn quota_reset(
    windows: &[QuotaWindow],
    model: &str,
    explicit_reset: Option<i64>,
    now: SystemTime,
) -> Option<SystemTime> {
    let now_seconds = DateTime::<chrono::Utc>::from(now).timestamp();
    let exhausted = windows
        .iter()
        .filter(|window| crate::quota::window_applies(window, model))
        .filter(|window| crate::quota::exhausted(window, now_seconds))
        .cloned()
        .collect::<Vec<_>>();
    crate::quota::recovery_reset(&exhausted, explicit_reset, now_seconds, None)
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
}

/// The provider's reset time, when it gave one in a supported form.
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
        let local_now = DateTime::<chrono::Utc>::from(now)
            .with_timezone(&Local)
            .fixed_offset();
        crate::quota::normalize_reset_at(value, local_now)?.into()
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

    fn quota_window(label: &str, remaining: u8, reset: Option<i64>) -> QuotaWindow {
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

    #[test]
    fn provider_errors_use_short_capped_backoff_while_rate_limits_keep_their_schedule() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let failure = diagnostic("provider unavailable", Some("service"), Some(503));
        let mut health = ProfileHealth::default();
        let mut at = start;
        for expected in [8, 16, 32, 64, 64] {
            health.record("codex2", "sol", ProfileFault::ProviderError, &failure, at);
            let until = health.unhealthy("codex2", "sol", at).unwrap().until;
            assert_eq!(until, at + Duration::from_secs(expected));
            at = until + Duration::from_secs(1);
        }

        let rate_limit = diagnostic("429", Some("limit"), None);
        let mut health = ProfileHealth::default();
        let mut at = start;
        for expected in [60, 120, 240, 480, 900, 900] {
            health.record("codex2", "sol", ProfileFault::RateLimited, &rate_limit, at);
            let until = health.unhealthy("codex2", "sol", at).unwrap().until;
            assert_eq!(until, at + Duration::from_secs(expected));
            at = until + Duration::from_secs(1);
        }
    }

    #[test]
    fn later_success_clears_mark_and_next_failure_starts_at_first_strike() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let failure = diagnostic("429", Some("limit"), None);
        let mut health = ProfileHealth::default();
        health.record("codex2", "sol", ProfileFault::RateLimited, &failure, start);
        health.record_success("codex2", "sol", start + Duration::from_secs(1));
        assert!(health.unhealthy("codex2", "sol", start).is_none());
        let next = start + Duration::from_secs(2);
        health.record("codex2", "sol", ProfileFault::RateLimited, &failure, next);
        assert_eq!(
            health.unhealthy("codex2", "sol", next).unwrap().until,
            next + BASE_BACKOFF
        );
    }

    #[test]
    fn a_success_after_a_mark_ends_still_resets_its_strikes() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let failure = diagnostic("429", Some("limit"), None);
        let mut health = ProfileHealth::default();
        health.record("codex2", "sol", ProfileFault::RateLimited, &failure, start);
        let ended = start + BASE_BACKOFF + Duration::from_secs(1);
        assert!(health.unhealthy("codex2", "sol", ended).is_none());
        assert!(
            health.has_marks(ended),
            "the strike still counts until the quiet reset"
        );
        assert!(!health.has_marks(start + BASE_BACKOFF + QUIET_RESET));

        health.observe("codex2", "sol", TurnHealth::Succeeded(ended), ended);
        let next = ended + Duration::from_secs(1);
        health.observe(
            "codex2",
            "sol",
            TurnHealth::Failed(ProfileFault::RateLimited, failure),
            next,
        );
        assert_eq!(
            health.unhealthy("codex2", "sol", next).unwrap().until,
            next + BASE_BACKOFF
        );
    }

    #[test]
    fn old_success_does_not_clear_new_mark() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let failure = diagnostic("429", Some("limit"), None);
        let mut health = ProfileHealth::default();
        health.record("codex2", "sol", ProfileFault::RateLimited, &failure, start);
        health.record_success("codex2", "sol", start - Duration::from_secs(1));
        assert_eq!(
            health.unhealthy("codex2", "sol", start).unwrap().until,
            start + BASE_BACKOFF
        );
    }

    #[test]
    fn clock_reset_text_sets_the_mark_end() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-21T16:50:00Z")
            .unwrap()
            .with_timezone(&Local)
            .fixed_offset();
        let now_system = SystemTime::from(now);
        let failure = TurnDiagnostic {
            reset_at: Some("23:00 UTC".into()),
            ..diagnostic("429", Some("limit"), None)
        };
        let expected = SystemTime::from(
            crate::quota::normalize_reset_at("23:00 UTC", now)
                .unwrap()
                .with_timezone(&chrono::Utc),
        );
        let mut health = ProfileHealth::default();
        health.record(
            "codex2",
            "sol",
            ProfileFault::RateLimited,
            &failure,
            now_system,
        );
        assert_eq!(
            health.unhealthy("codex2", "sol", now_system).unwrap().until,
            expected
        );
    }

    #[test]
    fn exhausted_quota_uses_reported_reset_without_the_backoff_cap() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let reset = 1_002_000_i64;
        let mut health = ProfileHealth::default();
        health.observe_quota("codex2", &[quota_window("Week", 0, Some(reset))], now);
        health.record(
            "codex2",
            "sol",
            ProfileFault::QuotaExhausted,
            &diagnostic("quota exhausted", Some("usageLimitExceeded"), None),
            now,
        );
        assert_eq!(
            health.unhealthy("codex2", "sol", now).unwrap().until,
            SystemTime::UNIX_EPOCH + Duration::from_secs(reset as u64)
        );
    }

    #[test]
    fn scoped_exhaustion_holds_only_the_matching_model_mark() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let reset = 1_002_000_i64;
        let mut fable_window = quota_window("Week (Fable)", 0, Some(reset));
        fable_window.model_scope = Some("fable".into());
        let mut health = ProfileHealth::default();
        health.observe_quota("claude", &[fable_window], now);
        let failure = diagnostic("quota exhausted", Some("usageLimitExceeded"), None);
        health.record(
            "claude",
            "claude-fable-5-1",
            ProfileFault::QuotaExhausted,
            &failure,
            now,
        );
        health.record(
            "claude",
            "claude-opus-5",
            ProfileFault::QuotaExhausted,
            &failure,
            now,
        );

        assert_eq!(
            health
                .unhealthy("claude", "claude-fable-5-1", now)
                .unwrap()
                .until,
            SystemTime::UNIX_EPOCH + Duration::from_secs(reset as u64)
        );
        assert_eq!(
            health
                .unhealthy("claude", "claude-opus-5", now)
                .unwrap()
                .until,
            now + BASE_BACKOFF
        );
    }

    #[test]
    fn exhausted_quota_without_a_reset_uses_the_first_backoff() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut health = ProfileHealth::default();
        health.record(
            "codex2",
            "sol",
            ProfileFault::QuotaExhausted,
            &diagnostic("quota exhausted", Some("usageLimitExceeded"), None),
            now,
        );
        assert_eq!(
            health.unhealthy("codex2", "sol", now).unwrap().until,
            now + BASE_BACKOFF
        );
    }

    #[test]
    fn quota_failure_uses_reset_from_message() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-21T16:50:00Z")
            .unwrap()
            .timestamp_millis();
        let now_system = SystemTime::from(chrono::DateTime::from_timestamp_millis(now).unwrap());
        let expected = crate::quota::message_reset(
            "You've hit your quota; resets at 6:20pm (America/Chicago)",
            now,
        )
        .unwrap();
        let mut health = ProfileHealth::default();
        health.record(
            "codex2",
            "sol",
            ProfileFault::QuotaExhausted,
            &diagnostic(
                "You've hit your quota; resets at 6:20pm (America/Chicago)",
                None,
                Some(402),
            ),
            now_system,
        );
        assert_eq!(
            health.unhealthy("codex2", "sol", now_system).unwrap().until,
            SystemTime::UNIX_EPOCH + Duration::from_secs(expected as u64)
        );
    }

    #[test]
    fn quota_reports_only_lengthen_active_quota_marks() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let failure = diagnostic("failure", None, Some(402));
        let mut health = ProfileHealth::default();
        health.record(
            "codex2",
            "quota",
            ProfileFault::QuotaExhausted,
            &failure,
            now,
        );
        health.record("codex2", "rate", ProfileFault::RateLimited, &failure, now);
        health.record(
            "codex2",
            "provider",
            ProfileFault::ProviderError,
            &failure,
            now,
        );
        let long_reset = 1_002_000_i64;
        health.observe_quota("codex2", &[quota_window("Week", 0, Some(long_reset))], now);
        assert_eq!(
            health.unhealthy("codex2", "quota", now).unwrap().until,
            SystemTime::UNIX_EPOCH + Duration::from_secs(long_reset as u64)
        );
        let shorter_reset = 1_001_000_i64;
        health.observe_quota(
            "codex2",
            &[quota_window("Week", 0, Some(shorter_reset))],
            now,
        );
        assert_eq!(
            health.unhealthy("codex2", "quota", now).unwrap().until,
            SystemTime::UNIX_EPOCH + Duration::from_secs(long_reset as u64)
        );
        assert_eq!(
            health.unhealthy("codex2", "rate", now).unwrap().until,
            now + BASE_BACKOFF
        );
        assert_eq!(
            health.unhealthy("codex2", "provider", now).unwrap().until,
            now + PROVIDER_ERROR_BASE_BACKOFF
        );
    }

    #[test]
    fn windows_with_quota_left_do_not_hold_a_quota_mark() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let failure = diagnostic("quota exhausted", Some("usageLimitExceeded"), None);
        let mut health = ProfileHealth::default();
        health.observe_quota("codex2", &[quota_window("5h", 30, Some(1_014_400))], now);
        health.record("codex2", "sol", ProfileFault::QuotaExhausted, &failure, now);
        assert_eq!(
            health.unhealthy("codex2", "sol", now).unwrap().until,
            now + BASE_BACKOFF
        );
        health.observe_quota("codex2", &[quota_window("5h", 30, Some(1_014_400))], now);
        assert_eq!(
            health.unhealthy("codex2", "sol", now).unwrap().until,
            now + BASE_BACKOFF
        );
    }

    #[test]
    fn rate_limit_ignores_stored_quota_windows() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut health = ProfileHealth::default();
        health.observe_quota("codex2", &[quota_window("Week", 0, Some(2_000_000))], now);
        health.record(
            "codex2",
            "sol",
            ProfileFault::RateLimited,
            &diagnostic("429", None, Some(429)),
            now,
        );
        assert_eq!(
            health.unhealthy("codex2", "sol", now).unwrap().until,
            now + BASE_BACKOFF
        );
    }
}
