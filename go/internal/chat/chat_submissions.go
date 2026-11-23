// Port of mj-chat/src/chat/submissions.rs.

// Local submission rows bridge input handling and the durable projection.

package chat

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"time"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PendingSubmission is a submit this view sent and the durable projection
// has not recorded yet. It is saved with the draft (SavedChatDraft.pending).
type PendingSubmission struct {
	id            string
	kind          SubmissionKind
	payload       PromptPayload
	status        string
	startupOrigin *StartupRetryOrigin
	represented   bool
	finished      bool
	recordedAtMs  int64
	// submittedAfter is the frontier the receipt is anchored at; nil in
	// receipts saved by older builds.
	submittedAfter *uint64
	// startedAt is not serialized (serde skip).
	startedAt *time.Time
}

// pendingSubmissionWire is PendingSubmission's serde shape, in field order.
type pendingSubmissionWire struct {
	ID             string              `json:"id"`
	Kind           SubmissionKind      `json:"kind"`
	Payload        PromptPayload       `json:"payload"`
	Status         string              `json:"status"`
	StartupOrigin  *StartupRetryOrigin `json:"startup_origin,omitempty"`
	Represented    bool                `json:"represented"`
	Finished       bool                `json:"finished"`
	RecordedAtMs   int64               `json:"recorded_at_ms"`
	SubmittedAfter *uint64             `json:"submitted_after"`
}

// pendingSubmissionRequired are the members serde requires: every field but
// the defaulted startup_origin and submitted_after.
var pendingSubmissionRequired = []string{"id", "kind", "payload", "status", "represented", "finished", "recorded_at_ms"}

func (p PendingSubmission) MarshalJSON() ([]byte, error) {
	return transcriptMarshalJSON(pendingSubmissionWire{
		ID:             p.id,
		Kind:           p.kind,
		Payload:        p.payload,
		Status:         p.status,
		StartupOrigin:  p.startupOrigin,
		Represented:    p.represented,
		Finished:       p.finished,
		RecordedAtMs:   p.recordedAtMs,
		SubmittedAfter: p.submittedAfter,
	})
}

func (p *PendingSubmission) UnmarshalJSON(data []byte) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	for _, name := range pendingSubmissionRequired {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	var wire pendingSubmissionWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*p = PendingSubmission{
		id:             wire.ID,
		kind:           wire.Kind,
		payload:        wire.Payload,
		status:         wire.Status,
		startupOrigin:  wire.StartupOrigin,
		represented:    wire.Represented,
		finished:       wire.Finished,
		recordedAtMs:   wire.RecordedAtMs,
		submittedAfter: wire.SubmittedAfter,
	}
	return nil
}

// StartupRetryOrigin is what a startup prompt's receipt keeps to retry it
// with its exact identity.
type StartupRetryOrigin struct {
	inheritedDraft  *string
	moveOperationID *string
}

// startupRetryOriginWire is StartupRetryOrigin's serde shape. Both members
// may be missing (serde treats a missing Option as None).
type startupRetryOriginWire struct {
	InheritedDraft  *string `json:"inherited_draft"`
	MoveOperationID *string `json:"move_operation_id"`
}

func (o StartupRetryOrigin) MarshalJSON() ([]byte, error) {
	return transcriptMarshalJSON(startupRetryOriginWire{
		InheritedDraft:  o.inheritedDraft,
		MoveOperationID: o.moveOperationID,
	})
}

func (o *StartupRetryOrigin) UnmarshalJSON(data []byte) error {
	var wire startupRetryOriginWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*o = StartupRetryOrigin{inheritedDraft: wire.InheritedDraft, moveOperationID: wire.MoveOperationID}
	return nil
}

// unconfirmedStatus is "Delivery unconfirmed: <why>", without repeating the
// words when the error itself starts with them.
func unconfirmedStatus(err string) string {
	reason := err
	if rest, ok := strings.CutPrefix(err, client.DeliveryUnconfirmed{}.Error()); ok {
		reason = strings.TrimLeftFunc(strings.TrimLeft(rest, ":"), unicode.IsSpace)
	}
	if reason == "" {
		return "Delivery unconfirmed"
	}
	return "Delivery unconfirmed: " + reason
}

// representsSubmission is whether a transcript item is the projection's
// record of submission id: the user message of a prompt, the row of a shell
// command, or the divider of an accepted /clear, which has no user message of
// its own (R4-4).
func representsSubmission(stableID string, id string) bool {
	prefix, ok := strings.CutSuffix(stableID, id)
	return ok && (prefix == "user:" || prefix == "shell:" || prefix == core.ContextBoundaryPrefix)
}

func isInlineSubmissionEntry(entry *core.ChatEntry) bool {
	return entry.Source.Item == nil &&
		entry.MessageID != nil &&
		strings.HasPrefix(*entry.MessageID, "local-submission:")
}

func (p *PendingSubmission) isInline() bool {
	return p.finished && !p.represented && strings.HasPrefix(p.status, "Delivery unconfirmed")
}

func (p *PendingSubmission) canResend() bool {
	return p.isInline() && p.startupOrigin == nil
}

// isContextClear is whether this is a /clear the relay runs as a context
// reset, not as a prompt.
func (p *PendingSubmission) isContextClear() bool {
	return p.kind == SubmissionKindPrompt &&
		len(p.payload.Images) == 0 &&
		strings.TrimSpace(p.payload.Text) == "/clear"
}

// StartupRetryPrompt is one element of ChatState.StartupRetryPrompts, Rust's
// (StartupPrompt, Option<String>): the prompt and the draft it inherited.
type StartupRetryPrompt struct {
	Prompt         StartupPrompt
	InheritedDraft *string
}

// RetainStartupReceipt preserves exact startup identity across bounded
// shutdown without making it resendable.
func (s *ChatState) RetainStartupReceipt(prompt StartupPrompt, inheritedDraft *string) {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::retain_startup_receipt")
}

func (s *ChatState) StartupRetryPrompts() []StartupRetryPrompt {
	var prompts []StartupRetryPrompt
	for index := range s.pendingSubmissions {
		pending := &s.pendingSubmissions[index]
		if pending.startupOrigin == nil {
			continue
		}
		prompts = append(prompts, StartupRetryPrompt{
			Prompt: StartupPrompt{
				CommandID:       pending.id,
				Payload:         pending.payload,
				MoveOperationID: pending.startupOrigin.moveOperationID,
			},
			InheritedDraft: pending.startupOrigin.inheritedDraft,
		})
	}
	return prompts
}

func (s *ChatState) settleStartupReceipt(commandID string) {
	s.pendingSubmissions = slices.DeleteFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == commandID && pending.startupOrigin != nil
	})
	s.syncInlineSubmissions()
}

func (s *ChatState) restoreSubmissions(pending []PendingSubmission) {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::restore_submissions")
}

func (s *ChatState) beginSubmission(id string, kind SubmissionKind, payload PromptPayload, status string) {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::begin_submission")
}

func (s *ChatState) recoverableSubmissionID() (string, bool) {
	for index := len(s.pendingSubmissions) - 1; index >= 0; index-- {
		if s.pendingSubmissions[index].canResend() {
			return s.pendingSubmissions[index].id, true
		}
	}
	return "", false
}

func (s *ChatState) dismissSubmission(id string) {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::dismiss_submission")
}

func (s *ChatState) prepareReceiptRetry(id string, sessionID string) bool {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::prepare_receipt_retry")
}

// finishReceiptPreparation takes the prepared payload, or err when image
// preparation failed (Rust's Result<PromptPayload, String>).
func (s *ChatState) finishReceiptPreparation(id string, payload PromptPayload, err error) {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::finish_receipt_preparation")
}

// resendSubmission is the operation that sends a resendable receipt again,
// or nil.
func (s *ChatState) resendSubmission(id string) ChatRemoteOperation {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::resend_submission")
}

func (s *ChatState) reconcileSubmissions(session *core.MaterializedSession) {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::reconcile_submissions")
}

// finishSubmission returns false when authoritative state already confirmed
// this operation.
func (s *ChatState) finishSubmission(id string, accepted bool) bool {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::finish_submission")
}

func (s *ChatState) unconfirmAllSubmissions(err string) {
	for index := range s.pendingSubmissions {
		pending := &s.pendingSubmissions[index]
		if !pending.finished {
			pending.finished = true
			pending.status = unconfirmedStatus(err)
		}
	}
	s.retireRepresentedSubmissions()
	s.syncInlineSubmissions()
}

func (s *ChatState) unconfirmSubmission(id string, err string) {
	if index := slices.IndexFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id
	}); index >= 0 {
		s.pendingSubmissions[index].finished = true
		s.pendingSubmissions[index].status = unconfirmedStatus(err)
	}
	s.retireRepresentedSubmissions()
	s.syncInlineSubmissions()
}

// retireRepresentedSubmissions drops receipts the projection records and
// the relay has answered (Rust's retain(|p| !(p.represented && p.finished))).
func (s *ChatState) retireRepresentedSubmissions() {
	s.pendingSubmissions = slices.DeleteFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.represented && pending.finished
	})
}

// syncInlineSubmissions materializes local terminal receipts at their
// owner-recorded frontier. These entries never enter the durable projection
// or the pinned rows.
func (s *ChatState) syncInlineSubmissions() {
	panic("unported: mj-chat/src/chat/submissions.rs ChatState::sync_inline_submissions")
}

func (s *ChatState) hasInlineSubmissions() bool {
	for index := range s.pendingSubmissions {
		if s.pendingSubmissions[index].isInline() {
			return true
		}
	}
	return false
}

func (s *ChatState) submissionStatus(entry *core.ChatEntry) (string, bool) {
	if entry.MessageID == nil {
		return "", false
	}
	for index := range s.pendingSubmissions {
		if s.pendingSubmissions[index].id == *entry.MessageID {
			return s.pendingSubmissions[index].status, true
		}
	}
	return "", false
}

func (s *ChatState) submissionEntries() []core.ChatEntry {
	var entries []core.ChatEntry
	for index := range s.pendingSubmissions {
		pending := &s.pendingSubmissions[index]
		if pending.represented || pending.isInline() {
			continue
		}
		text := pending.payload.Text
		if pending.kind == SubmissionKindShell {
			text = "!" + text
		}
		recordedAtMs := pending.recordedAtMs
		entry := core.ChatEntryPlain(s.latestSeq, core.ChatRoleUser, text).WithRecordedAt(&recordedAtMs)
		messageID := pending.id
		entry.MessageID = &messageID
		entries = append(entries, entry)
	}
	return entries
}
