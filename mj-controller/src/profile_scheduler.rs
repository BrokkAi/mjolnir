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
        windows: Vec<crate::quota::QuotaWindow>,
        paid_credits: Option<mj_client::quota::PaidCredits>,
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
                windows: report.windows.clone(),
                paid_credits: report.paid_credits.clone(),
            },
            QuotaBilling::Unknown => Self::Unknown,
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

/// Order candidates for a spawn so that simultaneous spawns spread out
/// instead of all taking the same profile. Included quota and paid funds each
/// get a weighted order within their tier; included quota always comes first.
pub(crate) fn draw_candidates(
    candidates: &mut Vec<SubagentCandidate>,
    ranking_anchor: Option<&str>,
    model: &str,
    now_seconds: i64,
) -> anyhow::Result<()> {
    let mut units = Vec::with_capacity(candidates.len());
    for _ in 0..candidates.len() {
        let bits = getrandom::u64().map_err(|error| anyhow::anyhow!("draw a profile: {error}"))?;
        // 53 random bits as a float in (0, 1], so its logarithm is finite.
        units.push(((bits >> 11) + 1) as f64 / (1u64 << 53) as f64);
    }
    order_by_draw(candidates, ranking_anchor, &units, model, now_seconds);
    Ok(())
}

/// The ordering `draw_candidates` describes, with one uniform (0, 1] number
/// per candidate in ranked order. Sorting by ln(u) / score is the
/// Efraimidis–Spirakis weighted sample without replacement.
pub(crate) fn order_by_draw(
    candidates: &mut Vec<SubagentCandidate>,
    ranking_anchor: Option<&str>,
    units: &[f64],
    model: &str,
    now_seconds: i64,
) {
    rank_candidates(candidates, ranking_anchor, model, now_seconds);
    let mut keyed = candidates
        .drain(..)
        .zip(units)
        .map(|(candidate, unit)| {
            let key = match quota_rank(&candidate, model, now_seconds) {
                QuotaRank::Available(score) | QuotaRank::Paid(score) => unit.ln() / score,
                _ => f64::NEG_INFINITY,
            };
            (key, candidate)
        })
        .collect::<Vec<_>>();
    // A stable sort keeps the ranked order among the undrawn candidates.
    keyed.sort_by(|left, right| {
        quota_rank(&left.1, model, now_seconds)
            .tier()
            .cmp(&quota_rank(&right.1, model, now_seconds).tier())
            .then_with(|| right.0.total_cmp(&left.0))
    });
    candidates.extend(keyed.into_iter().map(|(_, candidate)| candidate));
}

/// Order candidates by [`QuotaRank`], subscriptions with quota left by pace
/// score; then the optional anchor's profile and profile id, so the order
/// never depends on how the configuration happens to list them.
pub(crate) fn rank_candidates(
    candidates: &mut [SubagentCandidate],
    ranking_anchor: Option<&str>,
    model: &str,
    now_seconds: i64,
) {
    candidates.sort_by(|left, right| {
        quota_rank(left, model, now_seconds)
            .order(quota_rank(right, model, now_seconds))
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
        } => match crate::quota::quota_score(windows, model, now_seconds) {
            Some(score) if score > 0.0 => QuotaRank::Available(score),
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
