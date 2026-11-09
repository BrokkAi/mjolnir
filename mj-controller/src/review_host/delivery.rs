use mj_core::mailbox::{MailboxEvent, MailboxEventBody};
use mj_core::review::driver::TurnReviewPhase;
use mj_core::review::verdict::ReviewVerdict;
use mj_core::storage::ActiveReviewGroup;

/// Called inside the outcome save transaction: durable outcomes own whether a
/// report is new, and the outbox commits with them. No delivery flags to recover.
pub(crate) fn partial_review_events(
    owner: &str,
    previous: Option<&ActiveReviewGroup>,
    current: &ActiveReviewGroup,
) -> Vec<MailboxEvent> {
    let snapshot = &current.driver;
    if !matches!(snapshot.phase, TurnReviewPhase::Running { .. }) {
        return Vec::new();
    }
    let total = snapshot.roles.len();
    let completed = snapshot
        .roles
        .iter()
        .filter(|role| role.outcome.is_some())
        .count();
    let previous = previous.filter(|group| group.driver.review_id == snapshot.review_id);
    let mut events = Vec::new();
    for role in &snapshot.roles {
        if previous.is_some_and(|group| {
            group
                .driver
                .roles
                .iter()
                .any(|prior| prior.role == role.role && prior.outcome.is_some())
        }) {
            continue;
        }
        let Some(ReviewVerdict::Findings {
            synthesis: report, ..
        }) = role.outcome.as_ref()
        else {
            continue;
        };
        for delta in &snapshot.deltas {
            let range =
                if let Some(base) = delta.base_commit.as_ref().filter(|base| !base.is_empty()) {
                    format!(
                        "{base}..{}",
                        delta.head_commit.as_deref().unwrap_or_default()
                    )
                } else {
                    format!("up to {}", delta.head_commit.as_deref().unwrap_or_default())
                };
            events.push(MailboxEvent {
                key: format!(
                    "push-review-partial:{owner}:{}:{}:{}",
                    snapshot.review_id,
                    role.role,
                    delta.root.display()
                ),
                source: "review".into(),
                wake: true,
                created_at_ms: mj_core::clock::epoch_millis().max(0) as u64,
                body: MailboxEventBody::PlainText {
                    text: format!(
                        "Review {completed}/{total} — partial result for commits {range} in {} (review {}).\n{report}\n{} of {total} reviewers are still running. Independent reviewer reports may overlap. You can address findings now. Wait for the final result before pushing, calling override_review, or requesting another review.",
                        delta.root.display(),
                        snapshot.review_id,
                        total - completed,
                    ),
                },
            });
        }
    }
    events
}
