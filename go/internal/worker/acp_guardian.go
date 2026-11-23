// Port of mj-worker/src/acp/guardian.rs.

// Worker-side ACP guardian and permission responses. The reviewer's policy,
// evidence framing and verdict parsing are core's (mj_core::guardian).

package worker

import (
	"slices"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// verdictReminder is sent in the same reviewer session when a reply carried
// no verdict.
const verdictReminder = "Your reply did not contain the required verdict. Reply with only the strict JSON verdict described in the output contract, and nothing else."

// ReviewEvidence is what the reviewer is shown about one permission request.
type ReviewEvidence struct {
	request core.GuardianReviewRequest
	// userMessageIDs are the identities of the person's messages, to tell a
	// new message from the history's window moving.
	userMessageIDs []string
}

// reviewEvidenceCollect collects the evidence for a request from the
// session's turn context. A nil projectInstructions is none.
func reviewEvidenceCollect(
	harness core.HarnessKind,
	turnContext *transcript.TurnContext,
	toolCallID string,
	action any,
	projectInstructions *string,
) ReviewEvidence {
	authorization := turnContext.AuthorizationContext()
	userInstructions := []string{}
	userMessageIDs := []string{}
	if authorization != nil {
		for _, message := range authorization.Messages {
			if message.Role == "user" {
				userInstructions = append(userInstructions, message.Text)
				userMessageIDs = append(userMessageIDs, message.ID)
			}
		}
	}
	// No durable history means nothing the person wrote can be vouched for;
	// the reviewer is told so.
	omitted := authorization == nil || !authorization.AuthorizationComplete
	return ReviewEvidence{
		request: core.GuardianReviewRequest{
			RequestID:               toolCallID,
			Harness:                 harness,
			SessionID:               turnContext.SessionID(),
			ToolCallID:              toolCallID,
			UserInstructions:        userInstructions,
			UserInstructionsOmitted: omitted,
			ProjectInstructions:     projectInstructions,
			Transcript:              turnContext.RenderTranscript(core.GuardianTranscriptBytes),
			Action:                  action,
		},
		userMessageIDs: userMessageIDs,
	}
}

// personWroteSince reports whether the person wrote something this evidence
// does not hold, such as a new grant or a revocation. The history's window
// moving on, or the relay republishing it, is not new authorization.
func (e *ReviewEvidence) personWroteSince(earlier *ReviewEvidence) bool {
	for _, id := range e.userMessageIDs {
		if !slices.Contains(earlier.userMessageIDs, id) {
			return true
		}
	}
	return false
}

// GuardianDecision is how the guardian answered one permission request.
type GuardianDecision interface {
	isGuardianDecision()
}

type GuardianDecisionAllow struct{}

type GuardianDecisionDeny struct {
	Rationale string
}

// GuardianDecisionAskPerson: the guardian could not decide; the person
// answers the ordinary form, which shows this reason.
type GuardianDecisionAskPerson struct {
	Reason string
}

func (GuardianDecisionAllow) isGuardianDecision()     {}
func (GuardianDecisionDeny) isGuardianDecision()      {}
func (GuardianDecisionAskPerson) isGuardianDecision() {}

// allowOnceResponse allows this one call, never as a rule the harness would
// remember: the request's one-time allow, or nil when it offers none.
func allowOnceResponse(request *schema.RequestPermissionRequest) *schema.RequestPermissionResponse {
	for _, option := range request.Options {
		if option.Kind == schema.PermissionOptionKindAllowOnce {
			return &schema.RequestPermissionResponse{Outcome: schema.RequestPermissionOutcome{
				Kind:     schema.RequestPermissionOutcomeKindSelected,
				Selected: &schema.SelectedPermissionOutcome{OptionID: option.OptionID},
			}}
		}
	}
	return nil
}

// rejectionResponse refuses this one call: the request's one-time
// rejection, never a rejection the harness would remember as a rule, or a
// cancelled request when it offers none. Both the guardian's denials and
// every request the reviewer itself raises are answered this way.
func rejectionResponse(request *schema.RequestPermissionRequest) schema.RequestPermissionResponse {
	for _, option := range request.Options {
		if option.Kind == schema.PermissionOptionKindRejectOnce {
			return schema.RequestPermissionResponse{Outcome: schema.RequestPermissionOutcome{
				Kind:     schema.RequestPermissionOutcomeKindSelected,
				Selected: &schema.SelectedPermissionOutcome{OptionID: option.OptionID},
			}}
		}
	}
	return schema.RequestPermissionResponse{Outcome: schema.RequestPermissionOutcome{
		Kind:      schema.RequestPermissionOutcomeKindCancelled,
		Cancelled: &schema.RequestPermissionOutcomeCancelled{},
	}}
}

// denialNotice is the transcript notice for a denied action (Rust
// re-exports mj_core::guardian::denial_notice).
func denialNotice(subject, rationale string) string {
	return core.DenialNotice(subject, rationale)
}

// askPersonPreface is the line the person's form opens with when the
// guardian handed the request to them (Rust re-exports
// mj_core::guardian::ask_person_preface).
func askPersonPreface(reason string) string {
	return core.AskPersonPreface(reason)
}
