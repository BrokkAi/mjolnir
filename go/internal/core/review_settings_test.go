// Port of the tests in mj-core/src/review/settings.rs.

package core

import "testing"

func reviewSettingsReviewer() SubagentChoice {
	return SubagentChoice{Model: "gpt-6-astra"}
}

func reviewSettingsOff(t *testing.T, review *ReviewConfig, choice SessionReview, plain bool, want *AutomaticReviewOff) {
	t.Helper()
	got, off := AutomaticReviewOffFor(review, choice, plain)
	if (want == nil) != !off || (want != nil && got != *want) {
		t.Fatalf("AutomaticReviewOffFor(%#v, %v) = (%v, %v), want %v", choice, plain, got, off, want)
	}
}

func TestExplicitSessionReviewWithoutGlobalReviewersTurnsOffOnlyThatSession(t *testing.T) {
	review := ReviewConfig{}
	push := SessionReviewPush{}
	noReviewers := AutomaticReviewOffNoReviewers
	reviewSettingsOff(t, &review, push, false, &noReviewers)
	reviewSettingsOff(t, &review, nil, false, nil)
	empty := []SubagentChoice{}
	emptyPush := SessionReviewPush{Reviewers: &empty}
	emptyInherit := SessionReviewInherit{Reviewers: &empty}
	reviewSettingsOff(t, &review, emptyPush, false, &noReviewers)
	reviewSettingsOff(t, &review, emptyInherit, false, nil)
	review.Reviewers = []SubagentChoice{reviewSettingsReviewer()}
	if review.Trigger() != ReviewTriggerPush {
		t.Fatalf("trigger = %v", review.Trigger())
	}
	reviewSettingsOff(t, &review, emptyPush, false, &noReviewers)
	reviewSettingsOff(t, &review, emptyInherit, false, &noReviewers)
	reviewSettingsOff(t, &review, push, false, nil)
	review = ReviewConfigLegacyDefault()
	reviewSettingsOff(t, &review, push, false, nil)
}

// The rule is evaluated at every launch, never stored: a session created
// with an explicit push choice while no reviewers existed reviews once
// reviewers are configured, and an explicit Off stays Off.
func TestAutomaticRulesFollowTheCurrentReviewConfiguration(t *testing.T) {
	review := ReviewConfig{}
	push := SessionReviewPush{}
	off := SessionReviewOff{}
	launch := func(review *ReviewConfig, choice SessionReview, plain bool) ReviewTrigger {
		return EffectiveReviewConfig(review, choice, plain).Trigger()
	}
	if got := launch(&review, push, false); got != ReviewTriggerOff {
		t.Fatalf("push without reviewers = %v", got)
	}
	review.Reviewers = []SubagentChoice{reviewSettingsReviewer()}
	if got := launch(&review, push, false); got != ReviewTriggerPush {
		t.Fatalf("push = %v", got)
	}
	if got := launch(&review, nil, false); got != ReviewTriggerPush {
		t.Fatalf("inherit = %v", got)
	}
	if got := launch(&review, off, false); got != ReviewTriggerOff {
		t.Fatalf("off = %v", got)
	}
	if got := launch(&review, nil, true); got != ReviewTriggerOff {
		t.Fatalf("plain directory = %v", got)
	}
	plain := AutomaticReviewOffPlainDirectory
	reviewSettingsOff(t, &review, nil, true, &plain)
	reviewSettingsOff(t, &review, off, true, nil)
}
