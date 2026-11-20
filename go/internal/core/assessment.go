// Port of mj-core/src/assessment.rs.

// One semantic assessment and durable action for a completed physical turn.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"math"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
	data, err := jsonvalue.Marshal(value)
	if err != nil {
		return HandbackProbabilities{}, err
	}
	if err := agentRequireFields(data, "unfinished", "needs_parent_input", "blocked", "permitted_stop", "finished", "unclear"); err != nil {
		return HandbackProbabilities{}, err
	}
	var probabilities HandbackProbabilities
	if err := json.Unmarshal(data, &probabilities); err != nil {
		return HandbackProbabilities{}, err
	}
	values := []float64{
		probabilities.Unfinished,
		probabilities.NeedsParentInput,
		probabilities.Blocked,
		probabilities.PermittedStop,
		probabilities.Finished,
		probabilities.Unclear,
	}
	var sum float64
	for _, p := range values {
		if !assessmentProbability(p) {
			return HandbackProbabilities{}, errors.New("invalid handback probability")
		}
	}
	for _, p := range values {
		sum += p
	}
	// GLiDE rounds individual scores; the working client allows 0.03 total drift.
	if math.Abs(sum-1.0) > 0.03 {
		return HandbackProbabilities{}, errors.New("invalid handback distribution")
	}
	return probabilities, nil
}

// assessmentProbability reports a finite value in [0, 1].
func assessmentProbability(p float64) bool {
	return !math.IsNaN(p) && !math.IsInf(p, 0) && p >= 0 && p <= 1
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
	answers, ok := agentJSONGet(response, "answers")
	if !ok {
		return Verdict{}, errors.New("missing assessment answers")
	}
	failure, err := verdictChoice[AssessmentFailure](answers, "failure", []string{"none", "transient_provider", "quota", "other", "unclear"})
	if err != nil {
		return Verdict{}, err
	}
	input, err := verdictChoice[AssessmentInput](answers, "input", []string{"none", "redundant_request", "required", "unclear"})
	if err != nil {
		return Verdict{}, err
	}
	work, err := verdictChoice[AssessmentWork](answers, "work", []string{"finished", "authorized_unfinished", "waiting", "unclear"})
	if err != nil {
		return Verdict{}, err
	}
	verdict := Verdict{Failure: failure, Input: input, Work: work}
	if answer, present := agentJSONGet(answers, "background"); present && answer != nil {
		background, err := verdictChoice[AssessmentBackground](answers, "background", []string{"needed", "unneeded", "unclear"})
		if err != nil {
			return Verdict{}, err
		}
		verdict.Background = &background
	}
	if answer, present := agentJSONGet(answers, "reply"); present && answer != nil {
		reply, err := verdictChoice[AssessmentReply](answers, "reply", []string{"closing", "continuing", "unclear"})
		if err != nil {
			return Verdict{}, err
		}
		verdict.Reply = &reply
	}
	return verdict, nil
}

// verdictChoice validates one choice answer against its allowed categories.
func verdictChoice[T any](answers any, key string, allowed []string) (Judgment[T], error) {
	answer, _ := agentJSONGet(answers, key)
	if kind, _ := agentJSONString(answer, "type"); kind != "choice" {
		return Judgment[T]{}, fmt.Errorf("invalid %s answer type", key)
	}
	confidenceValue, _ := agentJSONGet(answer, "confidence")
	confidence, isNumber := agentJSONFloat(confidenceValue)
	if !isNumber {
		return Judgment[T]{}, errors.New("missing confidence")
	}
	if !assessmentProbability(confidence) {
		return Judgment[T]{}, errors.New("invalid confidence")
	}
	rawProbabilities, present := agentJSONGet(answer, "probabilities")
	if !present {
		return Judgment[T]{}, fmt.Errorf("missing %s probabilities", key)
	}
	members, isObject := agentJSONMembers(rawProbabilities)
	if !isObject {
		return Judgment[T]{}, fmt.Errorf("invalid %s probabilities", key)
	}
	probabilities := make(map[string]float64, len(members))
	for _, member := range members {
		p, isNumber := agentJSONFloat(member.Value)
		if !isNumber {
			return Judgment[T]{}, fmt.Errorf("invalid %s probabilities", key)
		}
		probabilities[member.Key] = p
	}
	complete := len(probabilities) == len(allowed)
	for _, name := range allowed {
		if _, ok := probabilities[name]; !ok {
			complete = false
		}
	}
	if !complete {
		return Judgment[T]{}, fmt.Errorf("incomplete %s probabilities", key)
	}
	// Sum in key order, as the Rust BTreeMap iterates.
	names := slices.Sorted(maps.Keys(probabilities))
	valid := true
	var sum float64
	for _, name := range names {
		p := probabilities[name]
		valid = valid && assessmentProbability(p)
		sum += p
	}
	if !valid || math.Abs(sum-1.0) > 0.005*float64(len(allowed))+probabilityEpsilon {
		return Judgment[T]{}, fmt.Errorf("invalid %s probability distribution", key)
	}
	selected, isString := agentJSONString(answer, "choice")
	if !isString {
		return Judgment[T]{}, errors.New("missing choice")
	}
	winner, known := probabilities[selected]
	for _, p := range probabilities {
		if !known {
			break
		}
		known = winner+probabilityEpsilon >= p
	}
	if !known {
		return Judgment[T]{}, fmt.Errorf("%s choice is not a probability winner", key)
	}
	encoded, err := json.Marshal(selected)
	if err != nil {
		return Judgment[T]{}, err
	}
	var choice T
	if err := json.Unmarshal(encoded, &choice); err != nil {
		return Judgment[T]{}, err
	}
	return Judgment[T]{Choice: choice, Confidence: float32(confidence), Probabilities: probabilities}, nil
}

func (v *Verdict) RequiresInput() bool {
	if v.Input.Choice != AssessmentInputRequired {
		return false
	}
	winner, ok := v.Input.Probabilities["required"]
	if !ok {
		return false
	}
	runnerUp := 0.0
	for name, probability := range v.Input.Probabilities {
		if name != "required" {
			runnerUp = math.Max(runnerUp, probability)
		}
	}
	return winner+probabilityEpsilon >= RequiredInputProbability &&
		winner+probabilityEpsilon >= RequiredInputRatio*runnerUp
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

// MarshalJSON writes a nil Messages as an empty list, as serde writes a Vec.
func (h ContextHistory) MarshalJSON() ([]byte, error) {
	type plain ContextHistory
	if h.Messages == nil {
		h.Messages = []EvidenceMessage{}
	}
	return agentMarshalJSON(plain(h))
}

// UnmarshalJSON requires the fields serde requires.
func (h *ContextHistory) UnmarshalJSON(data []byte) error {
	if err := agentRequireFields(data, "messages", "authorization_complete", "assistant_history_omitted"); err != nil {
		return err
	}
	type plain ContextHistory
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*h = ContextHistory(decoded)
	return nil
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
	if len(text) <= MessageHeadBytes+MessageTailBytes {
		return text
	}
	headEnd := agentFloorCharBoundary(text, MessageHeadBytes)
	tailStart := agentCeilCharBoundary(text, len(text)-MessageTailBytes)
	return fmt.Sprintf("%s\n[... %d bytes omitted from the middle of this message ...]\n%s",
		text[:headEnd], tailStart-headEnd, text[tailStart:])
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
	protected := h.OpenAssistantID
	index := slices.IndexFunc(h.Messages, func(m EvidenceMessage) bool {
		return m.Role == "assistant" && (protected == nil || m.ID != *protected)
	})
	if index < 0 {
		return false
	}
	if index+1 == len(h.Messages) {
		h.FinalReplyOmitted = true
	}
	h.removeEntry(index)
	h.AssistantHistoryOmitted = true
	return true
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
	first := slices.IndexFunc(h.Messages, func(m EvidenceMessage) bool { return m.Role == "user" })
	if first < 0 {
		return false
	}
	// Without a later user message, keep the replies that follow it.
	end := first + 1
	if next := slices.IndexFunc(h.Messages[first+1:], func(m EvidenceMessage) bool { return m.Role == "user" }); next >= 0 {
		end = first + 1 + next
	}
	if slices.ContainsFunc(h.Messages[:end], func(m EvidenceMessage) bool { return m.Role == "assistant" }) {
		h.AssistantHistoryOmitted = true
	}
	h.drainFront(end)
	return true
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
	h.OpenAssistantID = nil
	var text strings.Builder
	nonText := false
	for _, block := range prompt {
		if block.Text != nil {
			if text.Len() > 0 {
				text.WriteByte('\n')
			}
			text.WriteString(block.Text.Text)
		} else {
			nonText = true
		}
	}
	joined := text.String()
	// A prompt Mjolnir submitted is not the person's, whatever it carries.
	if !(IsUserAuthoredMessage(&id, joined) || handback && GeneratedPromptIsSubagentAssignment(id)) {
		return
	}
	if nonText {
		h.AuthorizationComplete = false
	}
	if strings.TrimSpace(joined) == "" {
		return
	}
	trimmed := TrimMiddle(joined)
	// The byte budget is for user text alone. When it is full, the oldest
	// exchanges give way so the current request is always present. Only a
	// message that cannot be carried whole makes the history incomplete.
	for (h.userBytes()+len(trimmed) > UserBytes || h.userCount() >= MaxMessages) && h.evictOldestExchange() {
	}
	// Assistant entries make room; the wire cap counts both roles. Fewer than
	// MaxMessages user entries remain, so this always succeeds.
	for len(h.Messages) >= MaxMessages && h.evictOldestAssistant() {
	}
	h.Messages = append(h.Messages, EvidenceMessage{ID: "user:" + id, Role: "user", Text: trimmed})
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
	genuine := func(m *EvidenceMessage) bool {
		return m.Role != "user" || IsUserAuthoredMessage(&m.ID, m.Text)
	}
	fence := 0
	if h.SetAsideThrough != nil {
		fence = *h.SetAsideThrough
	}
	afterUser := 0
	for index := len(h.Messages) - 1; index >= 0; index-- {
		if m := &h.Messages[index]; m.Role == "user" && genuine(m) {
			afterUser = index + 1
			break
		}
	}
	messages := []EvidenceMessage{}
	for index := range h.Messages {
		m := &h.Messages[index]
		if genuine(m) && !(m.Role == "assistant" && index >= afterUser && index < fence) {
			messages = append(messages, *m)
		}
	}
	view := *h
	view.Messages = messages
	view.SetAsideThrough = nil
	if h.OpenAssistantID != nil {
		open := *h.OpenAssistantID
		view.OpenAssistantID = &open
	}
	return view
}

func (h *ContextHistory) Assistant(id *string, ordinal uint64, text string) {
	var resolved string
	switch {
	case id != nil:
		resolved = *id
	case h.OpenAssistantID != nil:
		resolved = *h.OpenAssistantID
	default:
		resolved = fmt.Sprintf("agent:%d", ordinal)
	}
	// Text continues the open message only while that message is the last
	// entry and outside the fence; after an answered elicitation it starts a
	// new entry that shares the reply's id.
	fence := 0
	if h.SetAsideThrough != nil {
		fence = *h.SetAsideThrough
	}
	lastIndex := len(h.Messages) - 1
	if h.OpenAssistantID != nil && *h.OpenAssistantID == resolved &&
		lastIndex >= 0 && lastIndex >= fence &&
		h.Messages[lastIndex].Role == "assistant" && h.Messages[lastIndex].ID == resolved {
		last := &h.Messages[lastIndex]
		last.Text += text
		if len(last.Text) > MessageHeadBytes+MessageTailBytes {
			last.Text = TrimMiddle(last.Text)
		}
	} else {
		h.FinalReplyOmitted = false
		h.Messages = append(h.Messages, EvidenceMessage{ID: resolved, Role: "assistant", Text: TrimMiddle(text)})
	}
	h.OpenAssistantID = &resolved
	// Oldest assistant entries give way to the byte budget and the wire cap;
	// the entry being written is never evicted, and user entries are never
	// evicted here.
	for (h.assistantBytes() > AssistantBytes || len(h.Messages) > MaxMessages) && h.evictOldestAssistant() {
	}
}

// ShrinkUntil evicts the oldest assistant entries, never the final reply,
// until fits accepts the history. Returns false when nothing more can go.
func (h *ContextHistory) ShrinkUntil(fits func(*ContextHistory) bool) bool {
	for !fits(h) {
		var protected *string
		if n := len(h.Messages); n > 0 && h.Messages[n-1].Role == "assistant" {
			protected = &h.Messages[n-1].ID
		}
		index := slices.IndexFunc(h.Messages, func(m EvidenceMessage) bool {
			return m.Role == "assistant" && (protected == nil || m.ID != *protected)
		})
		if index < 0 {
			return false
		}
		h.removeEntry(index)
		h.AssistantHistoryOmitted = true
	}
	return true
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
	data, err := jsonvalue.Marshal(value)
	if err != nil {
		return Checkpoint{}, err
	}
	if err := agentRequireFields(data, "version", "continuation"); err != nil {
		return Checkpoint{}, err
	}
	var state Checkpoint
	if err := json.Unmarshal(data, &state); err != nil {
		return Checkpoint{}, err
	}
	if state.Version != 1 {
		return Checkpoint{}, errors.New("unsupported assessment checkpoint version")
	}
	if len(data) > 256*1024 {
		return Checkpoint{}, errors.New("assessment checkpoint exceeds budget")
	}
	return state, nil
}
