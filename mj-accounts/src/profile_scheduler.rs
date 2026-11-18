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
/// `placement` decides whether profiles with a five-hour quota window keep
/// that order.
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
        )
        .cmp(&tier(
            quota_rank(right.1.as_ref(), model, now_seconds),
            right.1.as_ref(),
            placement,
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
        tier(left_rank, left, placement)
            .cmp(&tier(right_rank, right, placement))
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
/// five-hour quota window below profiles without one, regardless of how much
/// the window has left: the windowed allowance runs out per window while
/// usage-priced profiles keep billing. Sub-agent and reviewer spawns keep the
/// plain quota order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    TopLevel,
    Child,
}

/// Whether the profile's subscription report carries a five-hour window.
fn has_five_hour_window(candidate: &SubagentCandidate) -> bool {
    matches!(
        &candidate.quota,
        CandidateQuota::Subscription { windows, .. }
            if windows.iter().any(|window| window.is_five_hour())
    )
}

/// Where a candidate's quota places it, best first. Usage-priced (API)
/// profiles bill each request, so every subscription that may still run the
/// model comes before them — except for a top-level placement, which they
/// lead instead (see [`Placement`]); a subscription known to be out of quota
/// for the model would only fail, so it always comes last.
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
fn tier(rank: QuotaRank, candidate: &SubagentCandidate, placement: Placement) -> u8 {
    if placement == Placement::TopLevel
        && has_five_hour_window(candidate)
        && matches!(rank, QuotaRank::Available(_) | QuotaRank::Paid(_))
    {
        // Below usage-priced profiles, above an exhausted one.
        return 4;
    }
    match rank {
        QuotaRank::Available(_) => 0,
        QuotaRank::Unknown => 1,
        QuotaRank::Paid(_) => 2,
        QuotaRank::UsagePriced => 3,
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
}
