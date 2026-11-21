//! Shared capacity ranking and weighted assignment for profiles.

/// A candidate's last quota reading, scored for the requested model when
/// candidates are ranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateQuota {
    /// No report, a failed one, or one with no usable capacity information.
    Unknown,
    /// Billed per use, with no allowance to run out of.
    UsagePriced,
    Subscription {
        /// Monthly list price as a capacity proxy, when the plan is recognized.
        monthly_price_usd: Option<u32>,
        windows: Vec<crate::quota::QuotaWindow>,
        paid_credits: Option<mj_client::quota::PaidCredits>,
        /// When the windows were read, which is when their usage was measured.
        refreshed_at_epoch_seconds: u64,
    },
}

impl CandidateQuota {
    pub fn from_report(report: Option<&crate::quota::ProfileQuota>) -> Self {
        use mj_client::quota::QuotaBilling;
        let Some(report) = report else {
            return Self::Unknown;
        };
        match report.billing_class() {
            QuotaBilling::Usage => Self::UsagePriced,
            QuotaBilling::Subscription => Self::Subscription {
                monthly_price_usd: report.monthly_price_usd,
                windows: report.windows.clone(),
                paid_credits: report.paid_credits.clone(),
                refreshed_at_epoch_seconds: report.refreshed_at_epoch_seconds,
            },
            QuotaBilling::Unknown => Self::Unknown,
        }
    }

    /// The reading selection should trust.
    ///
    /// A probe with no error wins. A failed probe falls back to `last_good`
    /// when that reading is usage-priced, still has subscription windows, or
    /// records paid credits.
    /// `quota_score` treats a window whose reset is at or before `now_seconds`
    /// as already reset, so an exhausted window stops blocking once its reset
    /// passes. When the reading's billing class is still unknown, a harness
    /// that is billed per use (`dsh`, API-only OpenCode) is usage-priced
    /// rather than unknown.
    pub fn from_publication(
        publication: Option<&crate::quota::QuotaReportPublication>,
        harness: mj_core::config::HarnessKind,
        now_seconds: i64,
    ) -> Self {
        use mj_client::quota::QuotaBilling;
        let report = publication.and_then(|publication| scheduling_report(publication, now_seconds));
        let quota = Self::from_report(report);
        if matches!(quota, Self::Unknown) && quota.billing(harness) == QuotaBilling::Usage {
            Self::UsagePriced
        } else {
            quota
        }
    }
}

fn scheduling_report(
    publication: &crate::quota::QuotaReportPublication,
    now_seconds: i64,
) -> Option<&crate::quota::ProfileQuota> {
    if publication
        .report
        .as_ref()
        .is_some_and(|report| report.error.is_none())
    {
        return publication.report.as_ref();
    }
    publication
        .last_good
        .as_ref()
        .filter(|last_good| last_good_schedulable(last_good, now_seconds))
}

/// A failed probe may reuse `last_good` when it is usage-priced, it still
/// carries subscription windows, or it records paid credits. An expired
/// window is still evidence: scoring treats `resets_at_epoch_seconds <=
/// now_seconds` as a reset. A windowless non-usage reading with no paid
/// credits is not.
fn last_good_schedulable(report: &crate::quota::ProfileQuota, now_seconds: i64) -> bool {
    use mj_client::quota::QuotaBilling;
    if report.billing_class() == QuotaBilling::Usage || report.paid_credits.is_some() {
        return true;
    }
    report.windows.iter().any(|window| {
        window.remaining_percent.is_some()
            || (window.used.is_some() && window.limit.is_some())
            || window
                .resets_at_epoch_seconds
                .is_some_and(|reset| reset <= now_seconds)
    })
}

impl CandidateQuota {
    /// The candidate's billing: its report decides, then what the harness
    /// implies (see [`mj_client::quota::harness_billing`]).
    pub fn billing(&self, harness: mj_core::config::HarnessKind) -> mj_client::quota::QuotaBilling {
        use mj_client::quota::QuotaBilling;
        match self {
            Self::UsagePriced => QuotaBilling::Usage,
            Self::Subscription { .. } => QuotaBilling::Subscription,
            Self::Unknown => mj_client::quota::harness_billing(harness),
        }
    }
}

/// A candidate profile with its discovered choices and last quota reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentCandidate {
    pub profile_id: String,
    pub harness: mj_core::config::HarnessKind,
    pub choices: mj_core::worker_launch::ProfileConfig,
    pub quota: CandidateQuota,
    /// The profile's "Load balancing may consume credits" option. See
    /// [`ineligibility`].
    pub may_consume_credits: bool,
    /// The models this profile should not run now, by model id, from
    /// provider failures its sessions just had (#1361).
    pub unhealthy: std::collections::BTreeMap<String, crate::profile_health::Unhealthy>,
}

impl AsRef<SubagentCandidate> for SubagentCandidate {
    fn as_ref(&self) -> &SubagentCandidate {
        self
    }
}

/// Order candidates for a spawn so that simultaneous spawns spread out
/// instead of all taking the same profile. Included quota and paid funds each
/// get a weighted order within their tier; included quota always comes first.
/// Profiles named in `prefer` (dashboard's load-balancing preference) rank
/// ahead of the rest, in preference order, while their quota tier allows.
/// `placement` decides whether profiles with a five-hour quota window in
/// force keep that order.
pub fn draw_candidates<T: AsRef<SubagentCandidate>>(
    candidates: &mut Vec<T>,
    ranking_anchor: Option<&str>,
    prefer: &[String],
    model: &str,
    now_seconds: i64,
    placement: Placement,
) -> anyhow::Result<()> {
    let mut units = Vec::with_capacity(candidates.len());
    for _ in 0..candidates.len() {
        let bits = getrandom::u64().map_err(|error| anyhow::anyhow!("draw a profile: {error}"))?;
        // 53 random bits as a float in (0, 1], so its logarithm is finite.
        units.push(((bits >> 11) + 1) as f64 / (1u64 << 53) as f64);
    }
    order_by_draw(
        candidates,
        ranking_anchor,
        prefer,
        &units,
        model,
        now_seconds,
        placement,
    );
    Ok(())
}

/// The ordering `draw_candidates` describes, with one uniform (0, 1] number
/// per candidate in ranked order. Sorting by ln(u) / score is the
/// Efraimidis–Spirakis weighted sample without replacement.
pub fn order_by_draw<T: AsRef<SubagentCandidate>>(
    candidates: &mut Vec<T>,
    ranking_anchor: Option<&str>,
    prefer: &[String],
    units: &[f64],
    model: &str,
    now_seconds: i64,
    placement: Placement,
) {
    rank_candidates(
        candidates,
        ranking_anchor,
        prefer,
        model,
        now_seconds,
        placement,
    );
    let mut keyed = candidates
        .drain(..)
        .zip(units)
        .map(|(candidate, unit)| {
            let key = match quota_rank(candidate.as_ref(), model, now_seconds) {
                QuotaRank::Available(score) | QuotaRank::Paid(score) => unit.ln() / score,
                _ => f64::NEG_INFINITY,
            };
            (key, candidate)
        })
        .collect::<Vec<_>>();
    // A stable sort keeps the ranked order among the undrawn candidates.
    keyed.sort_by(|left, right| {
        tier(
            quota_rank(left.1.as_ref(), model, now_seconds),
            left.1.as_ref(),
            placement,
            model,
            now_seconds,
        )
        .cmp(&tier(
            quota_rank(right.1.as_ref(), model, now_seconds),
            right.1.as_ref(),
            placement,
            model,
            now_seconds,
        ))
        .then_with(|| {
            projects_short_window_exhaustion(left.1.as_ref(), model, now_seconds).cmp(
                &projects_short_window_exhaustion(right.1.as_ref(), model, now_seconds),
            )
        })
        .then_with(|| {
            preference_position(left.1.as_ref(), prefer)
                .cmp(&preference_position(right.1.as_ref(), prefer))
        })
        .then_with(|| right.0.total_cmp(&left.0))
    });
    candidates.extend(keyed.into_iter().map(|(_, candidate)| candidate));
}

/// A candidate's position in the load-balancing preference list, if the
/// person named it: preferred profiles lead, in list order.
fn preference_position(candidate: &SubagentCandidate, prefer: &[String]) -> usize {
    prefer
        .iter()
        .position(|id| *id == candidate.profile_id)
        .unwrap_or(usize::MAX)
}

/// Order candidates by quota tier; within a tier, profiles on course to run
/// out of their five-hour window before it resets come last, and subscriptions
/// with quota left are ordered by price-weighted pace score. Then the optional
/// anchor's profile and profile id, so the order never depends on how the
/// configuration happens to list them.
pub fn rank_candidates<T: AsRef<SubagentCandidate>>(
    candidates: &mut [T],
    ranking_anchor: Option<&str>,
    prefer: &[String],
    model: &str,
    now_seconds: i64,
    placement: Placement,
) {
    candidates.sort_by(|left, right| {
        let (left, right) = (left.as_ref(), right.as_ref());
        let left_rank = quota_rank(left, model, now_seconds);
        let right_rank = quota_rank(right, model, now_seconds);
        tier(left_rank, left, placement, model, now_seconds)
            .cmp(&tier(right_rank, right, placement, model, now_seconds))
            .then_with(|| {
                projects_short_window_exhaustion(left, model, now_seconds)
                    .cmp(&projects_short_window_exhaustion(right, model, now_seconds))
            })
            // A preferred profile leads the profiles quota treats alike.
            .then_with(|| {
                preference_position(left, prefer).cmp(&preference_position(right, prefer))
            })
            .then_with(|| left_rank.order(right_rank))
            .then_with(|| {
                let left_is_anchor = ranking_anchor == Some(left.profile_id.as_str());
                let right_is_anchor = ranking_anchor == Some(right.profile_id.as_str());
                right_is_anchor.cmp(&left_is_anchor)
            })
            .then_with(|| left.profile_id.cmp(&right.profile_id))
    });
}

/// Which spawn the ranking serves. Top-level sessions rank profiles with a
/// five-hour quota window in force below included quota without one and
/// unknown quota, but above paid credits and usage-priced profiles: the
/// windowed allowance runs out per window, yet it is already paid for, while
/// paid credits and API profiles bill each request. Sub-agent and reviewer
/// spawns keep the plain quota order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    TopLevel,
    Child,
}

/// Whether a five-hour window is in force for `model`: the report carries
/// one that applies to the model and either has a reset still ahead or, with
/// no reset time, was partly used when read less than one window ago. Claude
/// omits the window until first use, and Codex can report an idle window with
/// no reset time and nothing used; neither is in force, and an undated reading
/// older than the window describes a window that has since ended.
fn has_five_hour_window_in_force(
    candidate: &SubagentCandidate,
    model: &str,
    now_seconds: i64,
) -> bool {
    matches!(
        &candidate.quota,
        CandidateQuota::Subscription { windows, refreshed_at_epoch_seconds, .. }
            if windows.iter().any(|window| {
                window.is_five_hour()
                    && crate::quota::window_applies(window, model)
                    && match window.resets_at_epoch_seconds {
                        Some(reset) => reset > now_seconds,
                        None => {
                            let used = window.remaining_percent.is_some_and(|left| left < 100)
                                || window.used.is_some_and(|used| used > 0);
                            let duration = window
                                .duration_seconds
                                .filter(|duration| *duration > 0)
                                .unwrap_or(5 * 60 * 60);
                            let read_at = i64::try_from(*refreshed_at_epoch_seconds)
                                .unwrap_or(i64::MAX);
                            used && now_seconds.saturating_sub(read_at) < duration
                        }
                    }
            })
    )
}

/// Where a candidate's quota places it, best first. Usage-priced (API)
/// profiles bill each request, so every subscription that may still run the
/// model comes before them (a top-level placement moves five-hour-windowed
/// quota down a tier; see [`Placement`]); a subscription known to be out of
/// quota for the model would only fail, so it always comes last.
#[derive(Debug, Clone, Copy, PartialEq)]
enum QuotaRank {
    /// Quota left, with its pace score; a higher score ranks first.
    Available(f64),
    Unknown,
    Paid(f64),
    UsagePriced,
    Exhausted,
}

/// The rank's tier for this placement. The one place tier order is decided;
/// every comparator asks it.
fn tier(
    rank: QuotaRank,
    candidate: &SubagentCandidate,
    placement: Placement,
    model: &str,
    now_seconds: i64,
) -> u8 {
    if placement == Placement::TopLevel
        && matches!(rank, QuotaRank::Available(_))
        && has_five_hour_window_in_force(candidate, model, now_seconds)
    {
        // Below unwindowed included quota and unknown quota, above paid
        // credits and usage-priced profiles.
        return 2;
    }
    match rank {
        QuotaRank::Available(_) => 0,
        QuotaRank::Unknown => 1,
        QuotaRank::Paid(_) => 3,
        QuotaRank::UsagePriced => 4,
        QuotaRank::Exhausted => 5,
    }
}

impl QuotaRank {
    fn order(self, other: Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Available(left), Self::Available(right))
            | (Self::Paid(left), Self::Paid(right)) => right.total_cmp(&left),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

fn quota_rank(candidate: &SubagentCandidate, model: &str, now_seconds: i64) -> QuotaRank {
    match &candidate.quota {
        CandidateQuota::Unknown => QuotaRank::Unknown,
        CandidateQuota::UsagePriced => QuotaRank::UsagePriced,
        CandidateQuota::Subscription {
            windows,
            paid_credits,
            monthly_price_usd,
            ..
        } => match crate::quota::quota_score(windows, model, now_seconds) {
            // $100 is the reference weight; unreported plans retain unit weight.
            // Price is a proxy: advertised session multipliers do not describe
            // the relative weekly allowances of Claude Max plans (#1519).
            Some(score) if score > 0.0 => QuotaRank::Available(
                score
                    * monthly_price_usd
                        .filter(|price| *price > 0)
                        .map_or(1.0, |price| f64::from(price) / 100.0),
            ),
            score => paid_weight(paid_credits.as_ref())
                .map(QuotaRank::Paid)
                .unwrap_or(
                    if score.is_some() || (windows.is_empty() && paid_credits.is_some()) {
                        QuotaRank::Exhausted
                    } else {
                        QuotaRank::Unknown
                    },
                ),
        },
    }
}

/// Whether the candidate's five-hour window for `model` was being used fast
/// enough to run out before it resets. The pace is measured when the window
/// was read; a window that has reset since then is not projected.
fn projects_short_window_exhaustion(
    candidate: &SubagentCandidate,
    model: &str,
    now_seconds: i64,
) -> bool {
    let CandidateQuota::Subscription {
        windows,
        refreshed_at_epoch_seconds,
        ..
    } = &candidate.quota
    else {
        return false;
    };
    windows.iter().any(|window| {
        window.is_five_hour()
            && crate::quota::window_applies(window, model)
            && window
                .resets_at_epoch_seconds
                .is_some_and(|reset| reset > now_seconds)
            && crate::quota::projects_exhaustion(window, *refreshed_at_epoch_seconds)
    })
}

fn paid_weight(credits: Option<&mj_client::quota::PaidCredits>) -> Option<f64> {
    use mj_client::quota::PaidCredits;
    match credits? {
        PaidCredits::Limited(balance) => balance
            .as_f64()
            .filter(|balance| balance.is_finite() && *balance > 0.0),
        // No finite balance to compare: give known usable accounts equal weight.
        PaidCredits::Unlimited | PaidCredits::Available => Some(1.0),
    }
}

/// Why load balancing must not offer a candidate for `model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    /// The subscription is out of quota for the model and has no paid credits.
    OutOfQuota,
    /// The subscription is out of quota; only paid credits remain, and the
    /// profile's option forbids load balancing from spending them.
    CreditsNotAllowed,
}

impl Ineligible {
    /// "profile codex2 is out of subscription quota for model ..." with what
    /// the user can do about it.
    pub fn describe(self, profile_id: &str, model: &str) -> String {
        match self {
            Self::OutOfQuota => {
                format!("profile {profile_id} is out of subscription quota for model {model:?}")
            }
            Self::CreditsNotAllowed => format!(
                "profile {profile_id} is out of subscription quota for model {model:?}; \
                 enabling \"Load balancing may consume credits\" on the profile would let it \
                 use its paid credits"
            ),
        }
    }
}

/// The one decision whether load balancing may offer a candidate. A
/// subscription out of quota for the model is excluded; one with only paid
/// credits left is excluded unless its profile allows spending them.
/// Usage-priced profiles bill per use by nature, and unknown quota is not
/// evidence of exhaustion, so both stay eligible. A session pinned to a named
/// profile is not load balancing and does not ask.
pub fn ineligibility(
    candidate: &SubagentCandidate,
    model: &str,
    now_seconds: i64,
) -> Option<Ineligible> {
    match quota_rank(candidate, model, now_seconds) {
        QuotaRank::Exhausted => Some(Ineligible::OutOfQuota),
        QuotaRank::Paid(_)
            if matches!(candidate.quota, CandidateQuota::Subscription { .. })
                && !candidate.may_consume_credits =>
        {
            Some(Ineligible::CreditsNotAllowed)
        }
        _ => None,
    }
}

/// Candidate profiles split into those whose choices are known and those
/// whose discovery failed, with the reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentCandidates {
    pub offered: Vec<SubagentCandidate>,
    pub unavailable: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::quota::PaidCredits;

    fn candidate(quota: CandidateQuota, may_consume_credits: bool) -> SubagentCandidate {
        SubagentCandidate {
            profile_id: "p".into(),
            harness: mj_core::config::HarnessKind::Codex,
            choices: mj_core::worker_launch::ProfileConfig {
                model: None,
                models: Vec::new(),
                efforts: Vec::new(),
                observed_at: 1,
            },
            quota,
            may_consume_credits,
            unhealthy: Default::default(),
        }
    }

    fn subscription(remaining: u8, credits: Option<PaidCredits>) -> CandidateQuota {
        CandidateQuota::Subscription {
            monthly_price_usd: None,
            windows: vec![crate::quota::QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(remaining),
                duration_seconds: None,
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            }],
            paid_credits: credits,
            refreshed_at_epoch_seconds: 0,
        }
    }

    #[test]
    fn load_balancing_excludes_subscriptions_without_quota_unless_credits_are_allowed() {
        let ask = |quota: CandidateQuota, option| ineligibility(&candidate(quota, option), "m", 0);
        let credits = Some(PaidCredits::limited(5.0).expect("finite credits"));
        for option in [false, true] {
            assert_eq!(
                ask(subscription(0, None), option),
                Some(Ineligible::OutOfQuota)
            );
            assert_eq!(ask(subscription(50, None), option), None);
            assert_eq!(ask(CandidateQuota::UsagePriced, option), None);
            assert_eq!(ask(CandidateQuota::Unknown, option), None);
        }
        assert_eq!(
            ask(subscription(0, credits.clone()), false),
            Some(Ineligible::CreditsNotAllowed)
        );
        assert_eq!(ask(subscription(0, credits), true), None);
    }

    fn claude_window(remaining: u8, resets_at: Option<i64>, error: Option<&str>) -> crate::quota::ProfileQuota {
        crate::quota::ProfileQuota {
            profile_id: "claude".into(),
            harness: mj_core::config::HarnessKind::Claude,
            monthly_price_usd: Some(200),
            paid_credits: None,
            banked_resets: None,
            windows: vec![crate::quota::QuotaWindow {
                label: "5-hour".into(),
                remaining_percent: Some(remaining),
                duration_seconds: Some(5 * 60 * 60),
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: resets_at,
            }],
            extra: None,
            error: error.map(str::to_owned),
            refreshed_at_epoch_seconds: 1,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        }
    }

    fn publication(
        report: Option<crate::quota::ProfileQuota>,
        last_good: Option<crate::quota::ProfileQuota>,
    ) -> crate::quota::QuotaReportPublication {
        crate::quota::QuotaReportPublication {
            login_error: None,
            cache_identity: "claude-account".into(),
            report,
            last_good,
        }
    }

    /// A non-429 failure after a 0% Claude reading must stay exhausted.
    /// Treating the failure as unknown would schedule it ahead of paid and API.
    #[test]
    fn failed_probe_after_exhausted_reading_stays_exhausted() {
        let now = 1_000_000;
        let last_good = claude_window(0, Some(now + 3_600), None);
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            error: Some("connection reset by peer".into()),
            refreshed_at_epoch_seconds: 2,
            ..last_good.clone()
        };
        let quota = CandidateQuota::from_publication(
            Some(&publication(Some(failed), Some(last_good))),
            mj_core::config::HarnessKind::Claude,
            now,
        );
        assert!(matches!(quota, CandidateQuota::Subscription { .. }));
        let mut candidate = candidate(quota, false);
        candidate.harness = mj_core::config::HarnessKind::Claude;
        assert_eq!(
            ineligibility(&candidate, "claude-opus-5-5", now),
            Some(Ineligible::OutOfQuota)
        );
    }

    /// Once the stale 0% window's reset has passed, it no longer blocks.
    #[test]
    fn expired_last_good_window_stops_blocking() {
        let reset_at = 1_000_000;
        let last_good = claude_window(0, Some(reset_at), None);
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            error: Some("timed out".into()),
            ..last_good.clone()
        };
        let quota = CandidateQuota::from_publication(
            Some(&publication(Some(failed), Some(last_good))),
            mj_core::config::HarnessKind::Claude,
            reset_at,
        );
        assert!(
            matches!(quota, CandidateQuota::Subscription { .. }),
            "an expired window is still the last good reading, scored as reset"
        );
        let mut candidate = candidate(quota, false);
        candidate.harness = mj_core::config::HarnessKind::Claude;
        assert_eq!(
            ineligibility(&candidate, "claude-opus-5-5", reset_at),
            None
        );
    }

    /// Usage-priced harnesses stay on the API tier when the probe never landed.
    #[test]
    fn usage_priced_harness_without_a_report_is_not_unknown() {
        assert_eq!(
            CandidateQuota::from_publication(None, mj_core::config::HarnessKind::Dsh, 0),
            CandidateQuota::UsagePriced
        );
        let failed = claude_window(0, None, Some("dns failure"));
        let failed = crate::quota::ProfileQuota {
            harness: mj_core::config::HarnessKind::Dsh,
            profile_id: "deepseek".into(),
            windows: Vec::new(),
            ..failed
        };
        assert_eq!(
            CandidateQuota::from_publication(
                Some(&publication(Some(failed), None)),
                mj_core::config::HarnessKind::Dsh,
                0,
            ),
            CandidateQuota::UsagePriced
        );
        let claude_failed = claude_window(0, None, Some("dns failure"));
        let claude_failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            ..claude_failed
        };
        assert_eq!(
            CandidateQuota::from_publication(
                Some(&publication(Some(claude_failed), None)),
                mj_core::config::HarnessKind::Claude,
                0,
            ),
            CandidateQuota::Unknown
        );
    }

    /// Paid credits with no windows are still a last-good subscription reading.
    #[test]
    fn paid_credits_without_windows_stay_schedulable() {
        let last_good = crate::quota::ProfileQuota {
            windows: Vec::new(),
            paid_credits: Some(PaidCredits::limited(5.0).expect("finite credits")),
            ..claude_window(0, None, None)
        };
        let failed = crate::quota::ProfileQuota {
            windows: Vec::new(),
            paid_credits: None,
            error: Some("connection reset by peer".into()),
            ..last_good.clone()
        };
        let quota = CandidateQuota::from_publication(
            Some(&publication(Some(failed), Some(last_good))),
            mj_core::config::HarnessKind::Claude,
            0,
        );
        assert!(matches!(
            quota,
            CandidateQuota::Subscription {
                paid_credits: Some(_),
                windows,
                ..
            } if windows.is_empty()
        ));
    }
}
