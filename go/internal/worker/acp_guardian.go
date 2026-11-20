// Port of mj-worker/src/acp/guardian.rs.

// Mjolnir's guardian: automatic review of the tool approvals sent by a
// harness that has no reviewer of its own (Kimi Code, OpenCode, DSH; see
// core.HarnessKind.Guardian).
//
// This ports OpenAI Codex's synchronous guardian reviewer
// (<https://github.com/openai/codex>, commit 076ac78; Apache License 2.0,
// see `data/acp_guardian/OPENAI_CODEX_NOTICE.txt`):
//
//   - `data/acp_guardian/policy.md` is Codex's
//     `prompts/templates/guardian/policy.md` unchanged, inserted into
//     `data/acp_guardian/policy_template.md`, Codex's template with its
//     evidence-handling, investigation and execution-environment passages
//     describing Mjolnir's sessions and reviewer instead of Codex's sandbox
//     and tools.
//   - The prompt framing follows `guardian-context` (`composition.rs`,
//     `action.rs`), the output contract and verdict parser follow
//     `ext/guardian-reviewer/src/assessment.rs`, and the deadline, attempt
//     budget and per-turn denial breaker follow `ext/guardian-reviewer`.
//
// The reviewer is a fresh session of the same harness on the session's own
// model, run by ReviewerHost.

package worker

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// policy is Codex's security policy, inserted into the template below.
//
//go:embed data/acp_guardian/policy.md
var policy string

// policyTemplate is Codex's reviewer instructions, adapted to Mjolnir's
// execution environment.
//
//go:embed data/acp_guardian/policy_template.md
var policyTemplate string

const policyPlaceholder = "{{ tenant_policy_config }}"

// outputContract is Codex's `guardian_output_contract_prompt`, with its
// first sentence naming the only tools this reviewer has.
const outputContract = `You may read files to gather any additional context you need before deciding. When you are ready to answer, your final message must be strict JSON.

For low-risk actions, give the final answer directly: {"outcome":"allow"}.

For anything else, use this JSON schema:
{
  "risk_level": "low" | "medium" | "high" | "critical",
  "user_authorization": "unknown" | "low" | "medium" | "high",
  "outcome": "allow" | "deny",
  "rationale": string
}`

// verdictReminder is sent in the same reviewer session when a reply carried
// no verdict.
const verdictReminder = "Your reply did not contain the required verdict. Reply with only the strict JSON verdict described in the output contract, and nothing else."

// transcriptBytes is Codex's synchronous transcript budget
// (`ContextProfile::synchronous`: 20,000 message tokens and 10,000 tool
// tokens), at Codex's own estimate of four bytes per token
// (`codex_utils_output_truncation`).
const transcriptBytes = (20_000 + 10_000) * 4

// reviewerInstructions are the instructions that open every review: the
// security policy and the output contract.
func reviewerInstructions() string {
	// Substitute only the template's own placeholder, so placeholder-like
	// text inside the policy stays literal (Codex's
	// `GuardianPolicyInstructions`). The template opens with its attribution
	// comment, which is not prompt.
	template := policyTemplate
	if _, body, ok := strings.Cut(policyTemplate, "-->\n"); ok {
		template = body
	}
	template = strings.TrimRightFunc(template, unicode.IsSpace)
	instructions := strings.ReplaceAll(template, policyPlaceholder, strings.TrimSpace(policy))
	return instructions + "\n\n" + outputContract + "\n"
}

// ReviewEvidence is what the reviewer is shown about one permission request.
type ReviewEvidence struct {
	harness   core.HarnessKind
	sessionID string
	// userInstructions are the whole recent messages the person wrote,
	// oldest first.
	userInstructions []string
	// userMessageIDs are their identities, to tell a new message from the
	// history's window moving.
	userMessageIDs []string
	// userInstructionsOmitted: older messages from the person were left out.
	userInstructionsOmitted bool
	// transcript is the recent transcript, rendered by transcript.Summary.
	transcript string
	// action is the planned action as pretty JSON.
	action string
}

// reviewEvidenceCollect collects the evidence for a request from the
// session's turn context.
func reviewEvidenceCollect(harness core.HarnessKind, turnContext *transcript.TurnContext, action any) ReviewEvidence {
	panic("unported: mj-worker/src/acp/guardian.rs ReviewEvidence::collect")
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

// requestText is the review request, framed as Codex frames a synchronous
// full review.
func (e *ReviewEvidence) requestText() string {
	panic("unported: mj-worker/src/acp/guardian.rs ReviewEvidence::request_text")
}

// plannedAction is the exact action under review (a JSON value): the tool
// call the harness asked about, with each file edit as a unified patch
// rather than two whole-file copies. A nil input is no input.
func plannedAction(harness core.HarnessKind, request *schema.RequestPermissionRequest, input any, cwd string, nativeChild bool) any {
	panic("unported: mj-worker/src/acp/guardian.rs planned_action")
}

type RiskLevel string

const (
	RiskLevelLow      RiskLevel = "low"
	RiskLevelMedium   RiskLevel = "medium"
	RiskLevelHigh     RiskLevel = "high"
	RiskLevelCritical RiskLevel = "critical"
)

func (l *RiskLevel) UnmarshalJSON(data []byte) error {
	text, err := guardianVariant(data, "low", "medium", "high", "critical")
	*l = RiskLevel(text)
	return err
}

type UserAuthorization string

const (
	UserAuthorizationUnknown UserAuthorization = "unknown"
	UserAuthorizationLow     UserAuthorization = "low"
	UserAuthorizationMedium  UserAuthorization = "medium"
	UserAuthorizationHigh    UserAuthorization = "high"
)

func (a *UserAuthorization) UnmarshalJSON(data []byte) error {
	text, err := guardianVariant(data, "unknown", "low", "medium", "high")
	*a = UserAuthorization(text)
	return err
}

type Outcome string

const (
	OutcomeAllow Outcome = "allow"
	OutcomeDeny  Outcome = "deny"
)

func (o *Outcome) UnmarshalJSON(data []byte) error {
	text, err := guardianVariant(data, "allow", "deny")
	*o = Outcome(text)
	return err
}

// guardianVariant decodes a unit variant named by a JSON string, refusing
// any but variants as serde does.
func guardianVariant(data []byte, variants ...string) (string, error) {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return "", err
	}
	if !slices.Contains(variants, text) {
		return "", fmt.Errorf("unknown variant `%s`, expected one of `%s`", text, strings.Join(variants, "`, `"))
	}
	return text, nil
}

// Assessment is a reviewer's parsed verdict (Codex's `GuardianAssessment`).
type Assessment struct {
	riskLevel         RiskLevel
	userAuthorization UserAuthorization
	outcome           Outcome
	rationale         string
}

// AssessmentPayload is the verdict as the reviewer writes it; only the
// outcome is required.
type AssessmentPayload struct {
	RiskLevel         *RiskLevel         `json:"risk_level"`
	UserAuthorization *UserAuthorization `json:"user_authorization"`
	Outcome           Outcome            `json:"outcome"`
	Rationale         *string            `json:"rationale"`
}

func (p *AssessmentPayload) UnmarshalJSON(data []byte) error {
	if err := acpFeaturesRequireFields(data, "outcome"); err != nil {
		return err
	}
	type plain AssessmentPayload
	return json.Unmarshal(data, (*plain)(p))
}

// parseAssessment is Codex's `parse_guardian_assessment`, with Codex's
// defaults for omitted fields: the reply is strict JSON, or prose that ends
// with the verdict object (a closing markdown fence aside). Codex takes the
// span from the first `{` to the last `}`, relying on the output schema it
// can also enforce; nothing enforces one over ACP, so only the object the
// reply ends with counts, and an object quoted from the evidence earlier in
// the reply is never taken for the verdict.
func parseAssessment(text string) (Assessment, error) {
	panic("unported: mj-worker/src/acp/guardian.rs parse_assessment")
}

// enforcedOutcome is the outcome the policy's own thresholds permit: a
// `critical` action is always denied, and a `high` one unless the user
// authorized it at least in substance. A reply that pairs either with
// "allow" cannot override the policy (muse-acp enforces the same).
func (a *Assessment) enforcedOutcome() Outcome {
	switch {
	case a.riskLevel == RiskLevelCritical,
		a.riskLevel == RiskLevelHigh &&
			(a.userAuthorization == UserAuthorizationUnknown || a.userAuthorization == UserAuthorizationLow):
		return OutcomeDeny
	}
	return a.outcome
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

// denialNotice is the transcript notice for a denied action: what was
// refused, as the permission form would show it, then Codex's "Reason:"
// line.
func denialNotice(subject, rationale string) string {
	return fmt.Sprintf("Mjolnir guardian denied %s\nReason: %s", subject, rationale)
}

// askPersonPreface is the line the person's form opens with when the
// guardian handed the request to them.
func askPersonPreface(reason string) string {
	return fmt.Sprintf("Mjolnir guardian could not decide this request (%s). Answer it yourself.\n", reason)
}
