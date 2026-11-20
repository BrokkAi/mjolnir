// Port of mj-core/src/assessment.rs.

// One semantic assessment and durable action for a completed physical turn.

package core

import (
	"github.com/BrokkAi/acp-go/schema"
)

const Protocol uint32 = 25
const AutomationConfidence float32 = 0.90

// HandbackUnfinishedShare: a handback bounces when unfinished leads finished
// by at least this share of the two, and the stop categories together stay
// at or below HandbackStopProbability. Measured 2026-11-11 on 428 labeled
// handbacks: the recall of P(unfinished) >= .50 with half its false bounces,
// which were mostly permitted stops. Chosen after the holdout was read;
// unconfirmed.
const HandbackUnfinishedShare = 0.30
const HandbackStopProbability = 0.30
const RequiredInputProbability = 0.50
const RequiredInputRatio = 2.5
const FinishedWorkProbability = 0.80

// probabilityEpsilon: provider scores are rounded; tolerate floating point
// representation at a boundary.
const probabilityEpsilon = 1e-12

type AssessmentFailure string

const (
	AssessmentFailureNone              AssessmentFailure = "none"
	AssessmentFailureTransientProvider AssessmentFailure = "transient_provider"
	AssessmentFailureQuota             AssessmentFailure = "quota"
	AssessmentFailureOther             AssessmentFailure = "other"
	AssessmentFailureUnclear           AssessmentFailure = "unclear"
)

type AssessmentInput string

const (
	AssessmentInputNone             AssessmentInput = "none"
	AssessmentInputRedundantRequest AssessmentInput = "redundant_request"
	AssessmentInputRequired         AssessmentInput = "required"
	AssessmentInputUnclear          AssessmentInput = "unclear"
)

type AssessmentWork string

const (
	AssessmentWorkFinished             AssessmentWork = "finished"
	AssessmentWorkAuthorizedUnfinished AssessmentWork = "authorized_unfinished"
	AssessmentWorkWaiting              AssessmentWork = "waiting"
	AssessmentWorkUnclear              AssessmentWork = "unclear"
)

// AssessmentBackground is whether anyone still depends on the background
// commands the session holds. Asked only when leftover processes are the one
// thing keeping a session from being quiet; see QuietAt.
type AssessmentBackground string

const (
	AssessmentBackgroundNeeded   AssessmentBackground = "needed"
	AssessmentBackgroundUnneeded AssessmentBackground = "unneeded"
	AssessmentBackgroundUnclear  AssessmentBackground = "unclear"
)

// AssessmentReply is whether the latest reply appears to be closing the turn
// or continuing it.
type AssessmentReply string

const (
	AssessmentReplyClosing    AssessmentReply = "closing"
	AssessmentReplyContinuing AssessmentReply = "continuing"
	AssessmentReplyUnclear    AssessmentReply = "unclear"
)

type Judgment[T any] struct {
	Choice     T       `json:"choice"`
	Confidence float32 `json:"confidence"`
	// Probabilities is the full provider distribution, separate from
	// confidence. Empty only in old durable judgments; their previously
	// stored action remains authoritative.
	Probabilities map[string]float64 `json:"probabilities,omitempty"`
}

type Verdict struct {
	Failure Judgment[AssessmentFailure] `json:"failure"`
	Input   Judgment[AssessmentInput]   `json:"input"`
	Work    Judgment[AssessmentWork]    `json:"work"`
	// Background is absent from answers older proxies return, and ignored
	// unless the request listed background commands.
	Background *Judgment[AssessmentBackground] `json:"background,omitempty"`
	// Reply is absent from frozen v6 and older answers.
	Reply *Judgment[AssessmentReply] `json:"reply,omitempty"`
}

// HandbackProbabilities is the GLiDE handback distribution; all six
// categories are required at the boundary.
type HandbackProbabilities struct {
	Unfinished       float64 `json:"unfinished"`
	NeedsParentInput float64 `json:"needs_parent_input"`
	Blocked          float64 `json:"blocked"`
	PermittedStop    float64 `json:"permitted_stop"`
	Finished         float64 `json:"finished"`
	Unclear          float64 `json:"unclear"`
}

func (v *HandbackProbabilities) UnmarshalJSON(data []byte) error {
	type plain HandbackProbabilities
	return agentDecodeStrict(data, (*plain)(v))
}

// HandbackProbabilitiesParse decodes and validates a handback distribution
// (a decoded JSON value).
func HandbackProbabilitiesParse(value any) (HandbackProbabilities, error) {
	panic("unported: mj-core/src/assessment.rs HandbackProbabilities::parse")
}

func HandbackNeedsMoreWork(p *HandbackProbabilities) bool {
	decided := p.Unfinished + p.Finished
	stops := p.NeedsParentInput + p.Blocked + p.PermittedStop
	return decided > 0 &&
		p.Unfinished+probabilityEpsilon >= HandbackUnfinishedShare*decided &&
		stops <= HandbackStopProbability+probabilityEpsilon
}

// VerdictParse validates and decodes a classifier response (a decoded JSON
// value).
func VerdictParse(response any) (Verdict, error) {
	panic("unported: mj-core/src/assessment.rs Verdict::parse")
}

// verdictChoice validates one choice answer against its allowed categories.
func verdictChoice[T any](answers any, key string, allowed []string) (Judgment[T], error) {
	panic("unported: mj-core/src/assessment.rs Verdict::parse::choice")
}

func (v *Verdict) RequiresInput() bool {
	panic("unported: mj-core/src/assessment.rs Verdict::requires_input")
}

type AssessmentAction string

const (
	AssessmentActionRetryProvider AssessmentAction = "retry_provider"
	AssessmentActionRecoverQuota  AssessmentAction = "recover_quota"
	// AssessmentActionContinue is legacy: decoded from stored assessments;
	// no longer produced by the GLiDE rule, and nothing acts on it.
	AssessmentActionContinue   AssessmentAction = "continue"
	AssessmentActionAwaitInput AssessmentAction = "await_input"
	AssessmentActionFinished   AssessmentAction = "finished"
	AssessmentActionWait       AssessmentAction = "wait"
	AssessmentActionUncertain  AssessmentAction = "uncertain"
)

type Status string

const (
	StatusPending    Status = "pending"
	StatusAssessed   Status = "assessed"
	StatusDeferred   Status = "deferred"
	StatusScheduled  Status = "scheduled"
	StatusConsumed   Status = "consumed"
	StatusSuperseded Status = "superseded"
	StatusFailed     Status = "failed"
)

// InferredQuestion is the identity of the completed turn whose inferred
// question a person can dismiss.
type InferredQuestion struct {
	TurnID           string `json:"turn_id"`
	CompletedOrdinal uint64 `json:"completed_ordinal"`
}

// OpenRequest: a request remains open across agent-only turns until the
// person responds.
type OpenRequest struct {
	Question        InferredQuestion `json:"question"`
	QuestionMessage *string          `json:"question_message"`
}

type TurnAssessment struct {
	Attention        *AssessmentAttention `json:"attention,omitempty"`
	TurnID           string               `json:"turn_id"`
	CompletedOrdinal uint64               `json:"completed_ordinal"`
	Revision         uint64               `json:"revision"`
	CompletedAtMs    int64                `json:"completed_at_ms"`
	Completion       CompletionEvidence   `json:"completion"`
	Evidence         *TurnEvidence        `json:"evidence"`
	Verdict          *Verdict             `json:"verdict"`
	// Glide holds the validated completed-turn GLiDE distributions; action
	// remains authoritative on replay.
	Glide     *Probabilities    `json:"glide,omitempty"`
	Action    *AssessmentAction `json:"action"`
	Status    Status            `json:"status"`
	Reason    string            `json:"reason"`
	Failures  uint32            `json:"failures"`
	RetryAtMs *int64            `json:"retry_at_ms"`
	// QuestionMessage is the transcript stable id of the assistant reply that
	// holds the open request, located after an AwaitInput assessment.
	// Clients scroll to it when the user opens the session. Unset when no
	// reply was located.
	QuestionMessage *string `json:"question_message,omitempty"`
}

func TurnAssessmentPending(turnID string, ordinal uint64, at int64, completion CompletionEvidence) TurnAssessment {
	return TurnAssessment{
		TurnID:           turnID,
		CompletedOrdinal: ordinal,
		Revision:         ordinal,
		CompletedAtMs:    at,
		Completion:       completion,
		Status:           StatusPending,
		Reason:           "awaiting_classification",
	}
}

// ClassifyAgain classifies this turn again at revision: everything derived
// from the previous verdict goes, including which reply held its question,
// so the next verdict and its question locator start from nothing.
func (a *TurnAssessment) ClassifyAgain(revision uint64) {
	a.Revision = revision
	a.Status = StatusPending
	a.Evidence = nil
	a.Verdict = nil
	a.Glide = nil
	a.Attention = nil
	a.Action = nil
	a.QuestionMessage = nil
}

func (a *TurnAssessment) Current() bool {
	return a.Status != StatusConsumed && a.Status != StatusSuperseded
}

func (a *TurnAssessment) InferredQuestion() *InferredQuestion {
	if !a.Current() || a.Action == nil || *a.Action != AssessmentActionAwaitInput {
		return nil
	}
	return &InferredQuestion{TurnID: a.TurnID, CompletedOrdinal: a.CompletedOrdinal}
}

func (a *TurnAssessment) QuestionDismissed() bool {
	return a.Status == StatusSuperseded && a.Reason == "question_dismissed"
}

func (a *TurnAssessment) MatchesCompletion(outcome *MaterializedTurnOutcome) bool {
	return a.TurnID == outcome.CommandID && a.CompletedOrdinal == outcome.CompletedOrdinal
}

// AppliesTo: only the assessment of this exact completion may interpret its
// reply.
func (a *TurnAssessment) AppliesTo(outcome *MaterializedTurnOutcome) bool {
	return a.MatchesCompletion(outcome) && a.Current()
}

func (a *TurnAssessment) NeedsClassification(now int64) bool {
	return (a.Status == StatusPending || a.Status == StatusFailed) &&
		(a.RetryAtMs == nil || *a.RetryAtMs <= now)
}

// Summary is the public status without transcript evidence or provider
// diagnostics.
type Summary struct {
	TurnID    string            `json:"turn_id"`
	Revision  uint64            `json:"revision"`
	Status    Status            `json:"status"`
	Action    *AssessmentAction `json:"action"`
	Reason    string            `json:"reason"`
	RetryAtMs *int64            `json:"retry_at_ms"`
}

// SummaryFrom is Rust's `From<&TurnAssessment> for Summary`.
func SummaryFrom(a *TurnAssessment) Summary {
	return Summary{
		TurnID:    a.TurnID,
		Revision:  a.Revision,
		Status:    a.Status,
		Action:    a.Action,
		Reason:    a.Reason,
		RetryAtMs: a.RetryAtMs,
	}
}

// ContextHistory is whole authorization messages, retained independently of
// the rendering summary. Use DefaultContextHistory for the Rust default
// (AuthorizationComplete is true there).
type ContextHistory struct {
	Messages                []EvidenceMessage `json:"messages"`
	AuthorizationComplete   bool              `json:"authorization_complete"`
	AssistantHistoryOmitted bool              `json:"assistant_history_omitted"`
	OpenAssistantID         *string           `json:"open_assistant_id"`
	FinalReplyOmitted       bool              `json:"final_reply_omitted"`
	// SetAsideThrough is how many leading stored entries the person set
	// aside without a message of their own (a dismissed `?` or an answered
	// elicitation): an exclusive end position in Messages. Every method that
	// removes stored entries moves it with them. It is never cleared at write
	// time: a genuine person message after it ends its effect, and whether a
	// delivered prompt is genuine is decided at read time by today's rules
	// (see ReaderView).
	SetAsideThrough *int `json:"set_aside_through,omitempty"`
}

func DefaultContextHistory() ContextHistory {
	return ContextHistory{Messages: []EvidenceMessage{}, AuthorizationComplete: true}
}

// MessageHeadBytes and MessageTailBytes: most messages fit whole; a pasted
// document is kept by its head and tail with an explicit marker, so one
// paste cannot use up the whole budget.
const MessageHeadBytes = 4 * 1024
const MessageTailBytes = 2 * 1024

// MaxMessages is the total entries the wire contract allows
// (ContinuationEvidence.Validate and the proxy validator). Assistant entries
// give way first; only a history with this many user messages on its own is
// refused.
const MaxMessages = 256

// TrimMiddle keeps the first MessageHeadBytes and last MessageTailBytes of a
// long message, with the omitted byte count in between.
func TrimMiddle(text string) string {
	panic("unported: mj-core/src/assessment.rs trim_middle")
}

func (h *ContextHistory) userBytes() int {
	total := 0
	for _, m := range h.Messages {
		if m.Role == "user" {
			total += len(m.Text)
		}
	}
	return total
}

func (h *ContextHistory) userCount() int {
	count := 0
	for _, m := range h.Messages {
		if m.Role == "user" {
			count++
		}
	}
	return count
}

func (h *ContextHistory) assistantBytes() int {
	total := 0
	for _, m := range h.Messages {
		if m.Role == "assistant" {
			total += len(m.Text)
		}
	}
	return total
}

// evictOldestAssistant evicts the oldest assistant entry that is not the one
// being written. Returns false when none remains.
func (h *ContextHistory) evictOldestAssistant() bool {
	panic("unported: mj-core/src/assessment.rs ContextHistory::evict_oldest_assistant")
}

// removeEntry removes one stored entry. All removals go through here or
// drainFront so SetAsideThrough keeps naming the same entries.
func (h *ContextHistory) removeEntry(i int) {
	h.Messages = append(h.Messages[:i], h.Messages[i+1:]...)
	if h.SetAsideThrough != nil && i < *h.SetAsideThrough {
		fence := *h.SetAsideThrough - 1
		h.SetAsideThrough = &fence
	}
	h.checkFence()
}

func (h *ContextHistory) drainFront(end int) {
	h.Messages = append(h.Messages[:0], h.Messages[end:]...)
	if h.SetAsideThrough != nil {
		fence := max(*h.SetAsideThrough-end, 0)
		h.SetAsideThrough = &fence
	}
	h.checkFence()
}

// checkFence is a debug assertion in Rust; it panics here.
func (h *ContextHistory) checkFence() {
	if h.SetAsideThrough != nil && *h.SetAsideThrough > len(h.Messages) {
		panic("set_aside_through points past the stored entries")
	}
}

// evictOldestExchange drops the oldest user message together with any
// assistant entries before the next user message, which answered it or an
// earlier one. The history stays complete: every user message still in it is
// whole, and the window is the newest part of the conversation, so a later
// instruction is never dropped before an earlier one. Returns false when no
// user entry remains.
func (h *ContextHistory) evictOldestExchange() bool {
	panic("unported: mj-core/src/assessment.rs ContextHistory::evict_oldest_exchange")
}

func (h *ContextHistory) User(id string, prompt []schema.ContentBlock) {
	h.userWithSource(id, prompt, false)
}

// HandbackUser includes the parent's assignments only in handback-check
// evidence.
func (h *ContextHistory) HandbackUser(id string, prompt []schema.ContentBlock) {
	h.userWithSource(id, prompt, true)
}

func (h *ContextHistory) userWithSource(id string, prompt []schema.ContentBlock, handback bool) {
	panic("unported: mj-core/src/assessment.rs ContextHistory::user_with_source")
}

// SetAsideReplies: the person set aside every stored entry. A dismissal
// belongs to a completed turn, so an open assistant message at the end
// (throughOpen false) is a later turn's text and stays visible. An answered
// elicitation arrives mid-turn and ends the current reply (throughOpen
// true): the open message is set aside too, and the next chunk starts a new
// entry. OpenAssistantID is unchanged, so the reply keeps one identity for
// the question message.
func (h *ContextHistory) SetAsideReplies(throughOpen bool) {
	n := len(h.Messages)
	openAtEnd := n > 0 && h.Messages[n-1].Role == "assistant" &&
		h.OpenAssistantID != nil && h.Messages[n-1].ID == *h.OpenAssistantID
	fence := n
	if openAtEnd && !throughOpen {
		fence = n - 1
	}
	h.SetAsideThrough = &fence
}

// ReaderView is the history as readers must see it. It is stored as written
// (journal digests cover it), so this is the one place that interprets it:
//   - user messages that today's rules attribute to Mjolnir are dropped;
//   - assistant entries inside the set-aside fence and after the latest
//     genuine person message are dropped; a fence older than that message
//     drops nothing.
//
// Both positions are stored indices. The result carries no fence.
func (h *ContextHistory) ReaderView() ContextHistory {
	panic("unported: mj-core/src/assessment.rs ContextHistory::reader_view")
}

func (h *ContextHistory) Assistant(id *string, ordinal uint64, text string) {
	panic("unported: mj-core/src/assessment.rs ContextHistory::assistant")
}

// ShrinkUntil evicts the oldest assistant entries, never the final reply,
// until fits accepts the history. Returns false when nothing more can go.
func (h *ContextHistory) ShrinkUntil(fits func(*ContextHistory) bool) bool {
	panic("unported: mj-core/src/assessment.rs ContextHistory::shrink_until")
}

// HasGeneratedPrompts reports whether a user message in this history is one
// that today's rules attribute to Mjolnir. Histories are stored as written
// (journal event digests cover them), so one kept by an earlier worker can
// hold a prompt a newer rule recognizes, such as another session's message.
func (h *ContextHistory) HasGeneratedPrompts() bool {
	for _, m := range h.Messages {
		if m.Role == "user" && !IsUserAuthoredMessage(&m.ID, m.Text) {
			return true
		}
	}
	return false
}

func (h *ContextHistory) Evidence() ContinuationEvidence {
	return ContinuationEvidence{
		Messages:                append([]EvidenceMessage(nil), h.Messages...),
		AssistantHistoryOmitted: h.AssistantHistoryOmitted,
	}
}

// CheckpointOpenRequest is the exact request publication and person-input
// frontier at a checkpoint cut.
type CheckpointOpenRequest struct {
	Request            *OpenRequest `json:"request"`
	PersonInputThrough uint64       `json:"person_input_through"`
}

// Checkpoint is kept at the archive boundary as JSON so canonical equality
// remains exact.
type Checkpoint struct {
	OpenRequest    *CheckpointOpenRequest `json:"open_request,omitempty"`
	Version        uint32                 `json:"version"`
	Context        *ContextHistory        `json:"context"`
	Assessment     *TurnAssessment        `json:"assessment"`
	Continuation   ContinuationState      `json:"continuation"`
	CapacityRetry  *CapacityRetry         `json:"capacity_retry"`
	TurnCompletion *TurnCompletion        `json:"turn_completion"`
}

// UnmarshalJSON refuses unknown members (serde deny_unknown_fields).
func (c *Checkpoint) UnmarshalJSON(data []byte) error {
	type plain Checkpoint
	return agentDecodeStrict(data, (*plain)(c))
}

// CheckpointDecode decodes and validates an assessment checkpoint (a decoded
// JSON value).
func CheckpointDecode(value any) (Checkpoint, error) {
	panic("unported: mj-core/src/assessment.rs Checkpoint::decode")
}
