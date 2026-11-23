// Port of mj-chat/src/chat/submissions.rs.

// Local submission rows bridge input handling and the durable projection.

package chat

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
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
	if slices.ContainsFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == prompt.CommandID
	}) {
		return
	}
	latestSeq := s.latestSeq
	s.pendingSubmissions = append(s.pendingSubmissions, PendingSubmission{
		represented: slices.ContainsFunc(s.queuedPrompts, func(queued QueuedPrompt) bool {
			return queued.id == prompt.CommandID
		}),
		id:       prompt.CommandID,
		kind:     SubmissionKindPrompt,
		payload:  prompt.Payload,
		status:   "Delivery unconfirmed: checking queued prompt",
		finished: true,
		startupOrigin: &StartupRetryOrigin{
			inheritedDraft:  inheritedDraft,
			moveOperationID: prompt.MoveOperationID,
		},
		recordedAtMs:   core.EpochMillis(),
		submittedAfter: &latestSeq,
	})
	s.syncInlineSubmissions()
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
	s.pendingSubmissions = pending
	for index := range s.pendingSubmissions {
		pending := &s.pendingSubmissions[index]
		pending.finished = true
		if !strings.HasPrefix(pending.status, "Delivery unconfirmed") {
			pending.status = "Delivery unconfirmed"
		}
		// Older saved receipts have no frontier. Choose their anchor once on
		// restore from the timestamps of the available durable entries.
		if pending.submittedAfter == nil {
			var anchor uint64
			for entryIndex := range s.entries {
				entry := &s.entries[entryIndex]
				if entry.RecordedAtMs != nil && *entry.RecordedAtMs <= pending.recordedAtMs {
					anchor = max(anchor, entry.StartSeq)
				}
			}
			pending.submittedAfter = &anchor
		}
		pending.represented = slices.ContainsFunc(s.queuedPrompts, func(queued QueuedPrompt) bool {
			return queued.id == pending.id
		}) || slices.ContainsFunc(s.entries, func(entry core.ChatEntry) bool {
			return entry.Source.Item != nil && representsSubmission(entry.Source.Item.StableID, pending.id)
		})
	}
	s.pendingSubmissions = slices.DeleteFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.represented
	})
	s.syncInlineSubmissions()
}

func (s *ChatState) beginSubmission(id string, kind SubmissionKind, payload PromptPayload, status string) {
	// A retry remains the same submission, including its original anchor.
	if index := slices.IndexFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id
	}); index >= 0 {
		pending := &s.pendingSubmissions[index]
		pending.finished = false
		pending.status = status
		now := time.Now()
		pending.startedAt = &now
		s.syncInlineSubmissions()
		return
	}
	latestSeq := s.latestSeq
	now := time.Now()
	s.pendingSubmissions = append(s.pendingSubmissions, PendingSubmission{
		id:             id,
		kind:           kind,
		payload:        payload,
		status:         status,
		recordedAtMs:   core.EpochMillis(),
		submittedAfter: &latestSeq,
		startedAt:      &now,
	})
	s.invalidateRenderCache()
}

func (s *ChatState) recoverableSubmissionID() (string, bool) {
	for index := len(s.pendingSubmissions) - 1; index >= 0; index-- {
		if s.pendingSubmissions[index].canResend() {
			return s.pendingSubmissions[index].id, true
		}
	}
	return "", false
}

// removeReceiptPreparation drops a receipt's image preparation, aborting it
// if it still runs (Rust drops the forwarder when it leaves the map).
func (s *ChatState) removeReceiptPreparation(id string) {
	if forwarder, ok := s.composerRuntime.receiptPreparations[id]; ok {
		delete(s.composerRuntime.receiptPreparations, id)
		forwarder.drop()
	}
}

func (s *ChatState) dismissSubmission(id string) {
	s.removeReceiptPreparation(id)
	s.pendingSubmissions = slices.DeleteFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id && pending.canResend()
	})
	s.syncInlineSubmissions()
}

func (s *ChatState) prepareReceiptRetry(id string, sessionID string) bool {
	if _, ok := s.composerRuntime.receiptPreparations[id]; ok {
		return true
	}
	index := slices.IndexFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id && pending.canResend()
	})
	if index < 0 {
		return false
	}
	receipt := &s.pendingSubmissions[index]
	if !slices.ContainsFunc(receipt.payload.Images, func(image PromptImage) bool {
		return image.Image.Reference == nil
	}) {
		return false
	}
	payload := receipt.payload
	payload.Images = slices.Clone(receipt.payload.Images)
	commandID := id
	updates := s.composerRuntime.tx
	forwarder := spawnRemoteResultForwarder(func(ctx context.Context) {
		var err error
		for index := range payload.Images {
			if payload.Images[index].Image.Reference != nil {
				continue
			}
			var prepared ClipboardImage
			prepared, err = prepareAttachment(ctx, sessionID, attachmentSourceClipboard(payload.Images[index].Image))
			if err != nil {
				break
			}
			payload.Images[index].Image = prepared
		}
		// An aborted task sends nothing.
		if ctx.Err() != nil {
			return
		}
		update := composerIoUpdateReceiptPrepared{commandID: commandID}
		if err != nil {
			update.err = err
		} else {
			update.result = payload
		}
		if sendErr := updates.Send(update); sendErr != nil {
			slog.Debug("receipt preparation result dropped because composer closed", "error", sendErr)
		}
	})
	if s.composerRuntime.receiptPreparations == nil {
		s.composerRuntime.receiptPreparations = map[string]*remoteResultForwarder{}
	}
	s.composerRuntime.receiptPreparations[id] = forwarder
	s.syncInlineSubmissions()
	return true
}

// finishReceiptPreparation takes the prepared payload, or err when image
// preparation failed (Rust's Result<PromptPayload, String>).
func (s *ChatState) finishReceiptPreparation(id string, payload PromptPayload, err error) {
	s.removeReceiptPreparation(id)
	index := slices.IndexFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id && pending.canResend()
	})
	if index < 0 {
		return
	}
	receipt := &s.pendingSubmissions[index]
	if err != nil {
		receipt.status = "Delivery unconfirmed: image preparation failed: " + err.Error()
	} else {
		receipt.payload = payload
		s.composerRuntime.preparedReceiptRetries = append(s.composerRuntime.preparedReceiptRetries, id)
	}
	s.syncInlineSubmissions()
}

// resendSubmission is the operation that sends a resendable receipt again,
// or nil.
func (s *ChatState) resendSubmission(id string) chatRemoteOperation {
	index := slices.IndexFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id && pending.canResend()
	})
	if index < 0 {
		return nil
	}
	pending := &s.pendingSubmissions[index]
	switch pending.kind {
	case SubmissionKindShell:
		return chatRemoteOperationRunShell{
			CommandID: pending.id,
			Command:   pending.payload.Text,
		}
	default:
		return chatRemoteOperationPrompt{
			CommandID: pending.id,
			Text:      pending.payload.Text,
			Images:    slices.Clone(pending.payload.Images),
		}
	}
}

func (s *ChatState) reconcileSubmissions(session *core.MaterializedSession) {
	inlineRetired := false
	for index := range s.pendingSubmissions {
		pending := &s.pendingSubmissions[index]
		if slices.ContainsFunc(session.QueuedPrompts, func(queued core.MaterializedQueuedPrompt) bool {
			return queued.CommandID == pending.id
		}) || slices.ContainsFunc(session.Transcript, func(item *core.TranscriptItem) bool {
			return representsSubmission(item.StableID, pending.id)
		}) {
			if pending.startedAt != nil {
				s.submissionRenders = append(s.submissionRenders, submissionRender{commandID: pending.id, at: *pending.startedAt})
				pending.startedAt = nil
			}
			inlineRetired = pending.isInline() || inlineRetired
			pending.represented = true
		}
	}
	s.retireRepresentedSubmissions()
	if inlineRetired {
		s.syncInlineSubmissions()
	}
}

// finishSubmission returns false when authoritative state already confirmed
// this operation.
func (s *ChatState) finishSubmission(id string, accepted bool) bool {
	index := slices.IndexFunc(s.pendingSubmissions, func(pending PendingSubmission) bool {
		return pending.id == id
	})
	if index < 0 {
		return true
	}
	pending := &s.pendingSubmissions[index]
	represented := pending.represented
	pending.finished = true
	pending.status = "Queued"
	// Once the relay has accepted a `/clear`, its "Clearing context…" line,
	// then its divider or its failure, record it in the conversation. A row of
	// our own would wait for a user message that never comes.
	if represented || !accepted || pending.isContextClear() {
		s.pendingSubmissions = slices.Delete(s.pendingSubmissions, index, index+1)
	}
	s.syncInlineSubmissions()
	return !represented
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
	var position *TranscriptPosition
	if !anchorIsBottom(s.anchor) {
		current := s.TranscriptPosition()
		position = &current
	}
	previousLen := len(s.entries)
	s.entries = slices.DeleteFunc(s.entries, func(entry core.ChatEntry) bool {
		return isInlineSubmissionEntry(&entry)
	})
	removed := len(s.entries) != previousLen
	recoverable, hasRecoverable := s.recoverableSubmissionID()
	var inline []core.ChatEntry
	for index := range s.pendingSubmissions {
		pending := &s.pendingSubmissions[index]
		if !pending.isInline() {
			continue
		}
		text := pending.payload.Text
		if pending.kind == SubmissionKindShell {
			text = "!" + pending.payload.Text
		}
		status := pending.status
		if _, preparing := s.composerRuntime.receiptPreparations[pending.id]; preparing {
			status = "Delivery unconfirmed: preparing images…"
		}
		text = text + "\n\n" + status
		if !s.standby && hasRecoverable && recoverable == pending.id {
			text += "\nCtrl-Alt-R resend · Ctrl-Alt-D dismiss"
		}
		var submittedAfter uint64
		if pending.submittedAfter != nil {
			submittedAfter = *pending.submittedAfter
		}
		recordedAtMs := pending.recordedAtMs
		entry := core.ChatEntryPlain(submittedAfter, core.ChatRoleUser, text).WithRecordedAt(&recordedAtMs)
		messageID := "local-submission:" + pending.id
		entry.MessageID = &messageID
		inline = append(inline, entry)
	}
	if len(inline) == 0 && !removed {
		s.invalidateRenderCache()
		return
	}
	for _, entry := range inline {
		at, _ := slices.BinarySearchFunc(s.entries, entry.StartSeq, func(existing core.ChatEntry, startSeq uint64) int {
			// partition_point(|existing| existing.start_seq <= start_seq)
			if existing.StartSeq <= startSeq {
				return -1
			}
			return 1
		})
		s.entries = slices.Insert(s.entries, at, entry)
	}
	s.invalidateRenderCache()
	if position != nil {
		s.RestoreTranscriptPosition(*position)
	}
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
