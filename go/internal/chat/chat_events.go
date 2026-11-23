// Port of mj-chat/src/chat/events.rs.

package chat

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"slices"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// eventConsumed reports whether an event belonged to the chat even when its
// handler had no action. This keeps a clamped cursor, an empty composer, and
// a modal's no-op key distinguishable from an event the host should route
// on.
//
// Events are Bubble Tea messages: tea.KeyPressMsg (IsRepeat for crossterm's
// Repeat kind), tea.KeyReleaseMsg, tea.MouseMsg and tea.PasteMsg.
func (s *ChatState) eventConsumed(event tea.Msg, action ChatAction) bool {
	if action != nil {
		if _, none := action.(ChatActionNone); !none {
			return true
		}
	}
	switch event := event.(type) {
	case tea.PasteMsg:
		return true
	case tea.MouseMsg:
		if wheel, ok := event.(tea.MouseWheelMsg); ok &&
			(wheel.Button == tea.MouseWheelUp || wheel.Button == tea.MouseWheelDown) {
			return true
		}
		return s.ComponentHandlesMouse(event)
	case tea.KeyPressMsg:
		// A repeat is crossterm's Repeat kind, not a press.
		if event.IsRepeat {
			return false
		}
		if s.earlier != nil ||
			s.elicitation != nil ||
			s.configPickerActive() ||
			s.taskDialogOpen ||
			s.promptControlFocus != nil ||
			s.historySearch != nil {
			return true
		}
		ordinary := event.Mod&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) == 0
		return ordinary && composerOwnsKey(event.Code)
	}
	return false
}

// composerOwnsKey reports whether an unmodified key with this code is one the
// composer answers: a character, Enter, Tab, Shift-Tab, Backspace, Delete,
// the arrows, Home, End, Page Up, Page Down or Esc. Keypad keys count as the
// keys crossterm reports them as; KeyExtended is a multi-rune character.
func composerOwnsKey(code rune) bool {
	if code <= tea.KeyExtended {
		return true
	}
	switch code {
	case tea.KeyDelete, tea.KeyLeft, tea.KeyRight, tea.KeyUp, tea.KeyDown,
		tea.KeyHome, tea.KeyEnd, tea.KeyPgUp, tea.KeyPgDown,
		tea.KeyKpEnter, tea.KeyKpEqual, tea.KeyKpMultiply, tea.KeyKpPlus,
		tea.KeyKpComma, tea.KeyKpMinus, tea.KeyKpDecimal, tea.KeyKpDivide,
		tea.KeyKp0, tea.KeyKp1, tea.KeyKp2, tea.KeyKp3, tea.KeyKp4,
		tea.KeyKp5, tea.KeyKp6, tea.KeyKp7, tea.KeyKp8, tea.KeyKp9,
		tea.KeyKpSep, tea.KeyKpUp, tea.KeyKpDown, tea.KeyKpLeft, tea.KeyKpRight,
		tea.KeyKpPgUp, tea.KeyKpPgDown, tea.KeyKpHome, tea.KeyKpEnd, tea.KeyKpDelete:
		return true
	}
	return false
}

func (s *ChatState) setAnchor(anchor TranscriptAnchor) {
	// Explicit navigation owns follow intent, even when the chosen row is
	// the same row the automatic opening reveal selected.
	s.revealLatestAgentOnDraw = false
	s.revealedAnchor = nil
	s.anchor = anchor
}

func (s *ChatState) finishReviewerElicitationResponse(form ElicitationDraft, response core.ElicitationResponse) ChatAction {
	role := s.elicitationRole
	s.elicitationRole = nil
	return ChatActionRespondReviewerElicitation{
		Draft: ChatElicitationDraft{
			form:         form,
			reviewer:     true,
			reviewerRole: role,
		},
		Response: response,
	}
}

func (s *ChatState) applyEvent(event *core.SequencedEvent) {
	switch worker := event.Event.(type) {
	case core.WorkerEventPromptAccepted:
		// The prompt may have come from the web viewer or the API. Every
		// surface records a prompt before submitting it, so the listing this
		// asks for already holds it.
		s.RequestSessionHistory()
		s.markPromptSubmitted(worker.Text)
		s.startTurnClock(event.RecordedAtMs)
		s.entries = append(s.entries,
			core.ChatEntryPlain(event.Seq, core.ChatRoleUser, worker.Text).WithRecordedAt(event.RecordedAtMs))
	case core.WorkerEventTurnCompleted:
		s.phase = core.WorkerPhaseIdle
		s.promptInFlight = false
		s.sessionActivity.PromptInFlight = false
		s.sessionActivity.HarnessTurnStartedAtMs = nil
		s.goalPromptActive = false
		s.turnStartedAtEpochSeconds = nil
	// The durable worker records cancellation acceptance before the ACP
	// prompt future resolves. Keep the chat busy until the later
	// TurnCompleted event so a queued prompt cannot race the runtime.
	case core.WorkerEventCancelled:
		if s.phase != core.WorkerPhaseRunning {
			s.phase = core.WorkerPhaseRunning
		}
	case core.WorkerEventClosing:
		if s.phase != core.WorkerPhaseClosing {
			s.phase = core.WorkerPhaseClosing
		}
	case core.WorkerEventClosed:
		s.phase = core.WorkerPhaseClosed
		s.promptInFlight = false
	case core.WorkerEventCheckpointed:
	case core.WorkerEventAdapter:
		if isCompactionArtifact(worker.Payload) {
			s.lastCompactionSeq = event.Seq
		}
		s.applyAdapter(event.Seq, event.RecordedAtMs, worker.Payload)
	case core.WorkerEventQueuedPromptAdded:
		s.RequestSessionHistory()
		if _, removing := s.pendingQueueRemovals[worker.Prompt.ID]; !removing {
			s.queuedPrompts = append(s.queuedPrompts, QueuedPrompt{
				id:                     worker.Prompt.ID,
				text:                   worker.Prompt.Text,
				kind:                   core.QueuedCommandKindPrompt{},
				images:                 []PromptImage{},
				attachmentsUnsupported: false,
			})
		}
	case core.WorkerEventQueuedPromptRemoved:
		s.queuedPrompts = slices.DeleteFunc(s.queuedPrompts, func(prompt QueuedPrompt) bool {
			return prompt.id == worker.QueueID
		})
		delete(s.pendingQueueRemovals, worker.QueueID)
		delete(s.pendingQueueImages, worker.QueueID)
	case core.WorkerEventQueuedPromptPromoted:
		s.queuedPrompts = slices.DeleteFunc(s.queuedPrompts, func(queued QueuedPrompt) bool {
			return queued.id == worker.Prompt.ID
		})
		delete(s.pendingQueueRemovals, worker.Prompt.ID)
		delete(s.pendingQueueImages, worker.Prompt.ID)
		s.phase = core.WorkerPhaseRunning
		s.promptInFlight = true
		s.startTurnClock(event.RecordedAtMs)
		s.entries = append(s.entries,
			core.ChatEntryPlain(event.Seq, core.ChatRoleUser, worker.Prompt.Text).WithRecordedAt(event.RecordedAtMs))
	case core.WorkerEventQueuedPromptsCleared:
		s.queuedPrompts = s.queuedPrompts[:0]
		clear(s.pendingQueueRemovals)
	case core.WorkerEventConfigChanged:
	}
}

func (s *ChatState) applyAdapter(seq uint64, recordedAtMs *int64, payload json.RawMessage) {
	runtime, err := core.UnmarshalRuntimeEvent(payload)
	if err != nil {
		slog.Warn("ignoring malformed persisted runtime event", "seq", seq, "error", err)
		return
	}
	runtime = transcript.ApplyRuntimeEventToEntries(&s.entries, seq, recordedAtMs, runtime)
	switch runtime := runtime.(type) {
	case core.RuntimeEventSessionUpdate:
		s.applySessionUpdateAt(seq, recordedAtMs, runtime.Update)
	case core.RuntimeEventSessionConfigured:
		s.setConfigOptions(runtime.ConfigOptions)
	case core.RuntimeEventSessionModesConfigured:
		s.SetSessionModes(runtime.Modes)
	}
}

// applySessionUpdateAt projects one typed ACP update into stable transcript
// items. The runtime keeps JSON at the persistence boundary so old event
// logs remain wire compatible; rendering never guesses at arbitrary JSON
// shapes. (The test-only apply_session_update is in chat_events_test.go.)
func (s *ChatState) applySessionUpdateAt(seq uint64, recordedAtMs *int64, update json.RawMessage) {
	// Rust reads the stable v1 update, so an update only the unstable schema
	// knows is invalid here as it is there; the transcript reads the unstable
	// form of the same update.
	var parsed schema.SessionUpdate
	if err := json.Unmarshal(update, &parsed); err != nil {
		slog.Debug("ignoring invalid ACP session update", "error", err)
		return
	}
	changed, err := s.goalState.Apply(&parsed)
	switch {
	case err != nil:
		s.SetNotice(fmt.Sprintf("Could not read goal update: %v", err))
	case changed:
		s.rebuildCommandChoices()
	}
	var entries acp.SessionUpdate
	if err := json.Unmarshal(update, &entries); err != nil {
		slog.Debug("ignoring invalid ACP session update", "error", err)
		return
	}
	if _, surface := transcript.ApplySessionUpdateToEntries(&s.entries, seq, recordedAtMs, entries); !surface {
		return
	}
	switch parsed.Kind {
	case schema.SessionUpdateKindAvailableCommandsUpdate:
		s.acpSurface.SetAgentCommands(parsed.AvailableCommandsUpdate.AvailableCommands)
		s.rebuildCommandChoices()
	case schema.SessionUpdateKindConfigOptionUpdate:
		s.setConfigOptions(parsed.ConfigOptionUpdate.ConfigOptions)
	case schema.SessionUpdateKindCurrentModeUpdate:
		s.acpSurface.ApplyCurrentModeUpdate(string(parsed.CurrentModeUpdate.CurrentModeID))
	}
}
