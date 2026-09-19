use super::*;

impl ReviewSlot {
    pub(super) fn view(&self, session_id: &str) -> RuntimeReviewView {
        let verdict = match self.driver.phase() {
            TurnReviewPhase::Forwarding { synthesis, .. } => Some(VerdictView {
                kind: VerdictKind::Findings,
                text: synthesis.clone(),
                allowed: if matches!(
                    self.driver.phase(),
                    TurnReviewPhase::Forwarding { error: Some(_), .. }
                ) {
                    vec![Resolution::Forwarded, Resolution::Cancelled]
                } else {
                    Vec::new()
                },
            }),
            _ => self.driver.verdict().map(|verdict| match verdict {
                ReviewVerdict::Clean => VerdictView {
                    kind: VerdictKind::Clean,
                    text: String::new(),
                    allowed: Vec::new(),
                },
                ReviewVerdict::Findings { synthesis, .. } => VerdictView {
                    kind: VerdictKind::Findings,
                    text: synthesis.clone(),
                    allowed: vec![
                        Resolution::Forwarded,
                        Resolution::Dismissed,
                        Resolution::Cancelled,
                    ],
                },
                ReviewVerdict::Failed { reason } => VerdictView {
                    kind: VerdictKind::Failed,
                    text: reason.clone(),
                    // A failed review has nothing to forward, and dismissing
                    // it does not advance the baseline: the change stays
                    // unreviewed either way.
                    allowed: vec![Resolution::Dismissed, Resolution::Cancelled],
                },
            }),
        };
        RuntimeReviewView {
            session_id: session_id.to_owned(),
            tier: self.driver.tier(),
            phase: self.driver.phase().clone(),
            roles: self.driver.roles(),
            status: format!("{} · {}", self.reviewer.description(), self.driver.status()),
            verdict,
        }
    }
}

pub(super) fn answer(
    reply: Option<oneshot::Sender<Result<(), StartRefusal>>>,
    result: Result<(), StartRefusal>,
) {
    if let Some(reply) = reply {
        let _ = reply.send(result);
    }
}

pub(super) fn unexpected(outcome: Result<ReviewerOutcome, String>) -> String {
    match outcome {
        Ok(other) => format!("unexpected reviewer response {other:?}"),
        Err(error) => error,
    }
}
