// Port of the tests in mj-core/src/notification_message.rs.

package core

import (
	"reflect"
	"testing"
)

func TestAPendingRequestOutranksTheQuestionAndTheLastMessageIsOnlyForLegacyHarnesses(t *testing.T) {
	pending, review, question := "Approve?", "Reviewing", "agent:1"
	facts := NotificationMessageFacts{
		PendingRequest:      &pending,
		ReviewActivity:      &review,
		QuestionMessage:     &question,
		SupportsOpenRequest: true,
	}
	got := NotificationMessageSources(NotificationKindNeedsInput, &facts)
	want := []NotificationMessageSource{
		NotificationMessageSourcePendingRequest{"Approve?"},
		NotificationMessageSourceReviewActivity{"Reviewing"},
		NotificationMessageSourceQuestionMessage{"agent:1"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("sources = %#v, want %#v", got, want)
	}
	facts.SupportsOpenRequest = false
	got = NotificationMessageSources(NotificationKindNeedsInput, &facts)
	if last := got[len(got)-1]; last != (NotificationMessageSourceLastAgentMessage{}) {
		t.Fatalf("last source = %#v", last)
	}
	got = NotificationMessageSources(NotificationKindImportant, &facts)
	if !reflect.DeepEqual(got, []NotificationMessageSource{NotificationMessageSourceLastAgentMessage{}}) {
		t.Fatalf("important sources = %#v", got)
	}
}
