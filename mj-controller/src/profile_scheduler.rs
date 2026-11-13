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
    pub(crate) fn from_report(report: Option<&crate::quota::ProfileQuota>) -> Self {
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
    pub(crate) fn billing(
        &self,
        harness: mj_core::config::HarnessKind,
    ) -> mj_client::quota::QuotaBilling {
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
pub(crate) fn draw_candidates<T: AsRef<SubagentCandidate>>(
    candidates: &mut Vec<T>,
    ranking_anchor: Option<&str>,
    prefer: &[String],
    model: &str,
    now_seconds: i64,
) -> anyhow::Result<()> {
    let mut units = Vec::with_capacity(candidates.len());
    for _ in 0..candidates.len() {
        let bits = getrandom::u64().map_err(|error| anyhow::anyhow!("draw a profile: {error}"))?;
        // 53 random bits as a float in (0, 1], so its logarithm is finite.
        units.push(((bits >> 11) + 1) as f64 / (1u64 << 53) as f64);
    }
    order_by_draw(candidates, ranking_anchor, prefer, &units, model, now_seconds);
    Ok(())
}

/// The ordering `draw_candidates` describes, with one uniform (0, 1] number
/// per candidate in ranked order. Sorting by ln(u) / score is the
/// Efraimidis–Spirakis weighted sample without replacement.
pub(crate) fn order_by_draw<T: AsRef<SubagentCandidate>>(
    candidates: &mut Vec<T>,
    ranking_anchor: Option<&str>,
    prefer: &[String],
    units: &[f64],
    model: &str,
    now_seconds: i64,
) {
    rank_candidates(candidates, ranking_anchor, prefer, model, now_seconds);
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
        quota_rank(left.1.as_ref(), model, now_seconds)
            .tier()
            .cmp(&quota_rank(right.1.as_ref(), model, now_seconds).tier())
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

/// Order candidates by [`QuotaRank`] tier; within a tier, profiles on course
/// to run out of their five-hour window before it resets come last, and
/// subscriptions with quota left are ordered by price-weighted pace score. Then the optional
/// anchor's profile and profile id, so the order never depends on how the
/// configuration happens to list them.
pub(crate) fn rank_candidates<T: AsRef<SubagentCandidate>>(
    candidates: &mut [T],
    ranking_anchor: Option<&str>,
    prefer: &[String],
    model: &str,
    now_seconds: i64,
) {
    candidates.sort_by(|left, right| {
        let (left, right) = (left.as_ref(), right.as_ref());
        let left_rank = quota_rank(left, model, now_seconds);
        let right_rank = quota_rank(right, model, now_seconds);
        left_rank
            .tier()
            .cmp(&right_rank.tier())
            .then_with(|| {
                projects_short_window_exhaustion(left, model, now_seconds)
                    .cmp(&projects_short_window_exhaustion(right, model, now_seconds))
            })
            // A preferred profile leads the profiles quota treats alike.
            .then_with(|| preference_position(left, prefer).cmp(&preference_position(right, prefer)))
            .then_with(|| left_rank.order(right_rank))
            .then_with(|| {
                let left_is_anchor = ranking_anchor == Some(left.profile_id.as_str());
                let right_is_anchor = ranking_anchor == Some(right.profile_id.as_str());
                right_is_anchor.cmp(&left_is_anchor)
            })
            .then_with(|| left.profile_id.cmp(&right.profile_id))
    });
}

/// Where a candidate's quota places it, best first. Usage-priced (API)
/// profiles bill each request, so every subscription that may still run the
/// model comes before them; a subscription known to be out of quota for the
/// model would only fail, so it comes after them.
#[derive(Debug, Clone, Copy, PartialEq)]
enum QuotaRank {
    /// Quota left, with its pace score; a higher score ranks first.
    Available(f64),
    Unknown,
    Paid(f64),
    UsagePriced,
    Exhausted,
}

impl QuotaRank {
    fn tier(self) -> u8 {
        match self {
            Self::Available(_) => 0,
            Self::Unknown => 1,
            Self::Paid(_) => 2,
            Self::UsagePriced => 3,
            Self::Exhausted => 4,
        }
    }

    fn order(self, other: Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Available(left), Self::Available(right))
            | (Self::Paid(left), Self::Paid(right)) => right.total_cmp(&left),
            _ => self.tier().cmp(&other.tier()),
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

/// Capacity classification shared by launches and quota recovery.
pub(crate) fn exhausted(candidate: &SubagentCandidate, model: &str, now_seconds: i64) -> bool {
    matches!(
        quota_rank(candidate, model, now_seconds),
        QuotaRank::Exhausted
    )
}
