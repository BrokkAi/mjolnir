//! Passive status for a review in the parent conversation.
use mj_client::review::RuntimeReviewView;
use mj_core::review::driver::TurnReviewPhase;

#[derive(Debug)]
pub(super) struct TurnReview {
    pub(super) view: RuntimeReviewView,
    failure: Option<String>,
}

impl TurnReview {
    pub(super) fn new(view: RuntimeReviewView) -> Self {
        Self {
            view,
            failure: None,
        }
    }

    pub(super) fn update(&mut self, view: RuntimeReviewView) -> bool {
        let changed = self.view != view;
        self.view = view;
        changed
    }

    pub(super) fn status(&self) -> String {
        self.failure.as_ref().unwrap_or(&self.view.status).clone()
    }

    pub(super) fn pending_elicitations(
        &self,
    ) -> Vec<(String, mj_core::elicitation::ElicitationRequest)> {
        let TurnReviewPhase::Running { .. } = &self.view.phase else {
            return Vec::new();
        };
        self.view
            .questions
            .iter()
            .filter(|question| self.view.owns_reviewer(&question.role))
            .map(|question| (question.role.clone(), question.request.clone()))
            .collect()
    }

    pub(super) fn report_failure(&mut self, message: impl Into<String>) -> bool {
        let message = message.into();
        if self.failure.as_deref() == Some(message.as_str()) {
            return false;
        }
        self.failure = Some(message);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnReviewIntent {
    Start,
}

impl super::ChatState {
    pub(super) fn turn_review(&self) -> Option<&TurnReview> {
        self.turn_review.as_deref()
    }
    pub(super) fn turn_review_mut(&mut self) -> Option<&mut TurnReview> {
        self.turn_review.as_deref_mut()
    }
    pub(super) fn set_turn_review(&mut self, view: Option<RuntimeReviewView>) {
        match (view, self.turn_review.as_mut()) {
            (Some(view), Some(open)) => {
                open.update(view);
            }
            (Some(view), None) => self.turn_review = Some(Box::new(TurnReview::new(view))),
            (None, _) => self.turn_review = None,
        }
    }
}
