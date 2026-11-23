// Port of mj-chat/src/chat/active/dispatch.rs.

package chat

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"

	tea "charm.land/bubbletea/v2"
	"github.com/cli/browser"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// HandleEvent applies one terminal event and reports what it asked for.
func (c *ActiveChat) HandleEvent(event tea.Msg) ChatEventOutcome {
	result := c.HandleEventResult(event)
	if result.Action == nil {
		return ChatEventOutcomeNone{}
	}
	return result.Action
}

// HandleEventResult applies one terminal event, reporting whether the
// conversation consumed it and whatever action it asks the host to take.
func (c *ActiveChat) HandleEventResult(event tea.Msg) EventResult[ChatEventOutcome] {
	c.selectElicitationSurface(ElicitationSurfaceConversation)
	return c.handleSurfaceEvent(event)
}

// HandleElicitationEvent: the host routes notification input through the
// same action dispatcher.
func (c *ActiveChat) HandleElicitationEvent(event tea.Msg) EventResult[ChatEventOutcome] {
	// Geometry belongs to this dialog instance, so a newly arrived form
	// cannot consume keys against the previous form's card rectangle.
	if !c.ElicitationInputAvailableIn(ElicitationSurfaceNotification) {
		return EventResult[ChatEventOutcome]{}
	}
	c.selectElicitationSurface(ElicitationSurfaceNotification)
	return c.handleSurfaceEvent(event)
}

func (c *ActiveChat) handleSurfaceEvent(event tea.Msg) EventResult[ChatEventOutcome] {
	var action ChatAction
	switch event := event.(type) {
	case tea.KeyMsg:
		action = c.HandleKey(event)
	case tea.PasteMsg:
		action = c.handleTerminalPaste(event.Content)
	case tea.MouseMsg:
		action = c.HandleMouse(event)
	default:
		// Resize and focus changes are handled by the host's geometry.
		action = ChatActionNone{}
	}
	consumed := c.eventConsumed(event, action)
	dispatched := c.dispatch(action)
	c.dispatchComposerRequests()
	c.dispatchEarlierHistory()
	c.dispatchToolDetails()
	var outcome ChatEventOutcome
	if _, none := dispatched.(ChatEventOutcomeNone); !none {
		outcome = dispatched
	}
	return EventResult[ChatEventOutcome]{
		Consumed: consumed || outcome != nil,
		Action:   outcome,
	}
}

// RemoveWorkerQueuedPrompt is called only after the host's startup owner
// says this is a worker queue item.
func (c *ActiveChat) RemoveWorkerQueuedPrompt(id string, text string, kind core.QueuedCommandKind) {
	commandID, ok := c.commandID("remove-prompt")
	if !ok {
		c.failQueuedPromptRemoval(id, text, kind)
		return
	}
	queueChatRemoteOperation(
		c.remote.operations(),
		chatRemoteOperationRemoveQueuedPrompt{CommandID: commandID, ID: id, Text: text, Kind: kind},
		c.ChatState,
	)
}

func (c *ActiveChat) dispatchToolDetails() {
	generation := c.toolDetailGeneration
	requests := c.pendingToolDetailRequests
	c.pendingToolDetailRequests = nil
	for _, entry := range requests {
		revision, hasRevision := entryToolDetailRevision(&entry)
		if entry.StableID == nil || !hasRevision {
			continue
		}
		stableID := *entry.StableID
		session := c.session.Clone()
		updates := c.chatIo
		core.Spawn(func() struct{} {
			full, err := loadToolCallDetail(session, stableID, entry)
			if sendErr := updates.Send(ChatIoUpdateToolCallDetail{
				Generation: generation,
				StableID:   stableID,
				Revision:   revision,
				Entry:      full,
				Err:        err,
			}); sendErr != nil {
				slog.Debug("chat closed while loading tool details", "error", sendErr)
			}
			return struct{}{}
		})
	}
}

// loadToolCallDetail reloads a trimmed tool row's full detail and renders it
// as the entry the row would have been had it never been trimmed.
func loadToolCallDetail(session client.SessionHandle, stableID string, entry core.ChatEntry) (*core.ChatEntry, error) {
	detail, err := session.ToolCallDetail(context.Background(), stableID)
	if err != nil {
		return nil, err
	}
	switch detail := detail.(type) {
	case core.ToolCallDetailResultFull:
		full, err := core.SpawnBlocking(func() *core.ChatEntry {
			original := entry.Source.Item
			if original == nil {
				panic("materialized tool row")
			}
			rendered := client.ToolDetailChatEntry(original, entry.Seq, detail.Body)
			rendered.EndedAfterInterrupt = entry.EndedAfterInterrupt
			rendered.AwaitingApproval = entry.AwaitingApproval
			return &rendered
		}).Join(context.Background())
		if err != nil {
			return nil, fmt.Errorf("render tool call detail: %w", err)
		}
		return full, nil
	case core.ToolCallDetailResultUnavailable:
		return nil, errors.New(detail.Reason)
	case nil:
		return nil, errors.New("the transcript item is no longer available")
	}
	return nil, fmt.Errorf("unexpected tool call detail result %T", detail)
}

func (c *ActiveChat) dispatchEarlierHistory() {
	reader := c.earlier
	if reader == nil {
		if c.earlierTask != nil {
			c.earlierTask()
			c.earlierTask = nil
		}
		return
	}
	if !reader.Requested || reader.Loading {
		return
	}
	reader.Requested = false
	reader.Loading = true
	reader.Error = nil
	before := reader.Before
	generation := reader.Generation
	session := c.session.Clone()
	updates := c.chatIo
	ctx, cancel := context.WithCancel(context.Background())
	// A replaced read is left to finish, as dropping a Tokio JoinHandle
	// detaches it; its generation keeps a late page out of the reader.
	c.earlierTask = cancel
	core.Spawn(func() struct{} {
		update := ChatIoUpdateEarlierMessages{Generation: generation}
		page, err := session.TranscriptHistory(ctx, before)
		if err == nil {
			type rendered struct {
				before *core.TranscriptCursor
				lines  []Line
			}
			var result rendered
			result, err = core.SpawnBlocking(func() rendered {
				var lines []Line
				for _, entry := range client.HistoryEntries(&page) {
					lines = append(lines, StyledLine(entry.Role, Title(true)))
					for _, line := range rustStrLines(entry.Text) {
						lines = append(lines, RawLine(sanitizeTerminalText(line)))
					}
					lines = append(lines, RawLine(""))
				}
				return rendered{before: page.Before, lines: lines}
			}).Join(ctx)
			if err != nil {
				err = fmt.Errorf("history rendering task failed: %w", err)
			}
			update.Before, update.Lines = result.before, result.lines
		}
		update.Err = err
		if sendErr := updates.Send(update); sendErr != nil {
			slog.Debug("earlier history reader closed", "error", sendErr)
		}
		return struct{}{}
	})
}

// commandID is a command ID for one remote operation. false means the system
// random source failed, which leaves the command unsent rather than closing
// the view the user is reading.
func (c *ActiveChat) commandID(prefix string) (string, bool) {
	commandID, err := client.NewCommandID(prefix)
	if err != nil {
		c.SetNotice(fmt.Sprintf("Could not identify the command: %v", err))
		return "", false
	}
	return commandID, true
}

func (c *ActiveChat) dispatch(action ChatAction) ChatEventOutcome {
	switch action := action.(type) {
	case nil, ChatActionNone:
		return ChatEventOutcomeNone{}
	case ChatActionOpenSubagents:
		return ChatEventOutcomeOpenSubagents{}
	case ChatActionOpenReviewers:
		return ChatEventOutcomeOpenReviewers{}
	case ChatActionDismissSubmission:
		c.dismissSubmission(action.CommandID)
	case ChatActionResendSubmission:
		if c.prepareReceiptRetry(action.CommandID, c.session.SessionID()) {
			return ChatEventOutcomeHandled{}
		}
		if operation := c.resendSubmission(action.CommandID); operation != nil {
			queueChatRemoteOperation(c.remote.operations(), operation, c.ChatState)
		}
	case ChatActionPrompt:
		images := c.takeSubmittingImages()
		if len(images) > 0 && !c.promptImagesSupported {
			restoreUnsentPrompt(c.ChatState, action.Text, images)
			c.SetNotice(imageCapabilityNotice)
			return ChatEventOutcomeHandled{}
		}
		commandID, ok := c.commandID("prompt")
		if !ok {
			restoreUnsentPrompt(c.ChatState, action.Text, images)
			return ChatEventOutcomeHandled{}
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationPrompt{
			CommandID: commandID,
			Text:      action.Text,
			Images:    images,
		}, c.ChatState)
	case ChatActionAttach, ChatActionAttachPaste:
		c.DispatchComposerAction(action)
	case ChatActionRunShell:
		commandID, ok := c.commandID("shell")
		if !ok {
			restoreUnsentInput(c.ChatState, "!"+action.Command)
			return ChatEventOutcomeHandled{}
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationRunShell{
			CommandID: commandID,
			Command:   action.Command,
		}, c.ChatState)
	case ChatActionRemoveQueuedPrompt:
		return ChatEventOutcomeRecallQueuedPrompt{
			SessionID: c.SessionID(),
			ID:        action.ID,
			Text:      action.Text,
			Kind:      action.Kind,
		}
	case ChatActionStopBackgroundTask:
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationStopBackgroundTask{ID: action.ID}, c.ChatState)
	case ChatActionGoalControl:
		commandID, ok := c.commandID("goal-control")
		if !ok {
			restoreUnsentInput(c.ChatState, "/goal "+action.Action.AsStr())
			return ChatEventOutcomeHandled{}
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationGoalControl{
			CommandID: commandID,
			Action:    action.Action,
		}, c.ChatState)
	case ChatActionSetConfig:
		commandID, ok := c.commandID("set-config")
		if !ok {
			restoreUnsentInput(c.ChatState, core.ConfigCommandText(action.Key, action.Value))
			return ChatEventOutcomeHandled{}
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationSetConfig{
			CommandID: commandID,
			Key:       action.Key,
			Value:     action.Value,
		}, c.ChatState)
	case ChatActionPlanCommand:
		commandID, ok := c.commandID("plan-mode")
		if !ok {
			c.planCommandPending = false
			c.finishPlanModeChange(!action.RequestedActive)
			restoreUnsentInput(c.ChatState, action.Original)
			return ChatEventOutcomeHandled{}
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationPlanCommand{
			CommandID:       commandID,
			Original:        action.Original,
			Control:         action.Control,
			RequestedActive: action.RequestedActive,
			Prompt:          action.Prompt,
		}, c.ChatState)
	case ChatActionTurnControl:
		commandID, ok := c.commandID("turn-control")
		if !ok {
			return ChatEventOutcomeHandled{}
		}
		c.turnControlSubmitting = true
		var activePromptID *string
		switch command := action.Command.(type) {
		case core.RelayCommandCancelTurnFor:
			activePromptID = &command.ActivePromptID
		case core.RelayCommandApplyQueuedFor:
			activePromptID = &command.ActivePromptID
		}
		if activePromptID != nil {
			target := *activePromptID
			awaiting := commandID
			c.turnControlTarget = &target
			c.turnControlAwaitingState = &awaiting
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationCancel{
			CommandID:       commandID,
			Intent:          TurnControlIntentCancel,
			CancelAgent:     true,
			Command:         action.Command,
			ShellCommandIDs: []string{},
		}, c.ChatState)
	case ChatActionCancel:
		commandID, ok := c.commandID("cancel")
		if !ok {
			return ChatEventOutcomeHandled{}
		}
		intent := c.turnControlIntent()
		command := c.escapeCommand()
		if command == nil && len(c.activeUserShellIDs()) == 0 {
			return ChatEventOutcomeHandled{}
		}
		cancelAgent := command != nil &&
			(c.turnControls != nil ||
				c.promptInFlight ||
				c.harnessTurnStoppable() ||
				c.sessionActivity.CapacityRetry != nil ||
				c.sessionActivity.QuotaRecovery != nil)
		c.turnControlSubmitting = true
		c.turnControlError = nil
		c.turnControlTarget = cloneString(c.activePromptID)
		c.turnControlAwaitingState = nil
		if c.activePromptID != nil {
			awaiting := commandID
			c.turnControlAwaitingState = &awaiting
		}
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationCancel{
			CommandID:       commandID,
			Intent:          intent,
			Command:         command,
			CancelAgent:     cancelAgent,
			ShellCommandIDs: c.activeUserShellIDs(),
		}, c.ChatState)
	case ChatActionStartTurnReview:
		c.requestTurnReview()
	case ChatActionTurnReview:
		c.runTurnReview(action.Intent)
	case ChatActionRespondReviewerElicitation:
		draft := action.Draft
		c.answerReviewer(cloneString(draft.reviewerRole), draft.request().ID, action.Response, &draft)
	case ChatActionRespondElicitation:
		planFollowup := c.planReviewFollowup(&action.Request, &action.Response)
		queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationRespondElicitation{
			Request:      action.Request,
			Response:     action.Response,
			PlanFollowup: planFollowup,
		}, c.ChatState)
	case ChatActionOpenLink:
		url := action.URL
		c.SetNotice(fmt.Sprintf("Opening %s in the browser…", url))
		updates := c.chatIo
		// The launcher can wait on the browser process, so it runs on its
		// own goroutine rather than on the event loop.
		go func() {
			if err := browser.OpenURL(url); err != nil {
				failed := ChatIoUpdateLinkOpenFailed{URL: url, Error: err.Error()}
				if sendErr := updates.Send(failed); sendErr != nil {
					slog.Debug("chat closed before a link failure arrived", "error", sendErr)
				}
			}
		}()
	case ChatActionCopyLink:
		notice := fmt.Sprintf("Copied %s · only http and https links open in the browser", action.URL)
		return ChatEventOutcomeCopyText{Text: action.URL, Notice: notice}
	case ChatActionPasteFromClipboard:
		c.DispatchComposerAction(ChatActionPasteFromClipboard{})
	case ChatActionToggleVoice:
		if cancel := c.voiceCancel; cancel != nil {
			command, notice := VoiceCommandCancel, "Cancelling dictation…"
			if !c.voiceFinishing {
				c.voiceFinishing = true
				command, notice = VoiceCommandFinish, "Finishing dictation… click the microphone again to cancel"
			}
			if err := cancel.Send(command); err != nil {
				c.SetNotice(fmt.Sprintf("Dictation worker stopped: %v", err))
			} else {
				c.SetNotice(notice)
			}
		} else if c.voiceAvailable {
			cancel := core.NewUnbounded[VoiceCommand]()
			c.voiceCancel = cancel
			c.voiceFinishing = false
			c.voiceActive = true
			c.SetNotice("Starting microphone… click it again to transcribe")
			preferredProfile := ""
			if c.context != nil {
				preferredProfile = c.context.Session.LastProfile
			}
			spawnDictation(preferredProfile, c.voiceUpdates, cancel)
		}
	case ChatActionCycleFocus:
		// Moving the keyboard to another pane is not leaving the
		// conversation: it stays on screen, so nothing is detached and
		// dictation keeps running.
		return ChatEventOutcomeCycleFocus{Reverse: action.Reverse}
	case ChatActionQuitDetach:
		return c.Detach()
	}
	return ChatEventOutcomeHandled{}
}

func cloneString(value *string) *string {
	if value == nil {
		return nil
	}
	cloned := *value
	return &cloned
}

func init() {
	// The browser launcher's own output would land on the terminal the TUI
	// draws on; the webbrowser crate suppresses it the same way.
	browser.Stdout = io.Discard
	browser.Stderr = io.Discard
}

// ToggleTranscriptRendering switches the transcript between rendered
// Markdown and raw source.
//
// The key that runs this belongs to the host's command registry, so the host
// calls it directly rather than through a composer key.
func (c *ActiveChat) ToggleTranscriptRendering() {
	c.ToggleRenderMode()
}

// InterruptTurn interrupts the running turn, exactly as Esc in the composer
// does. Returns false when there is no turn to interrupt, or an interrupt is
// already on its way, so the host can say so.
//
// The command that runs this belongs to the host's registry, so it answers
// from the palette and from a pane as well as from the composer.
func (c *ActiveChat) InterruptTurn() bool {
	if c.turnControlPending() || !c.turnInterruptible() {
		return false
	}
	action := c.interruptAction()
	_ = c.dispatch(action)
	return true
}

// ToggleDictation starts, finishes or cancels dictation, exactly as clicking
// the microphone does. Nothing happens while dictation is unavailable and no
// recording is running; then the answer is why, for the host to put in its
// shared notices as it does for every chord that cannot run, so the reason is
// kept in Recent messages rather than cut at the edge of this conversation's
// own status line (launch finding R5-6).
func (c *ActiveChat) ToggleDictation() (string, bool) {
	action := c.DictationToggleAction()
	if _, none := action.(ChatActionNone); none {
		reason := c.dictationUnavailableNotice()
		slog.Info("dictation key pressed while dictation is unavailable", "session_id", c.SessionID(), "reason", reason)
		return reason, true
	}
	// Dictation never asks the host to leave the conversation, so there is
	// no outcome to forward.
	_ = c.dispatch(action)
	return "", false
}

// Detach leaves the conversation: stops any dictation and reports how far
// the transcript has been read, which the host turns into the session's read
// receipt and its saved draft.
//
// The detach key is a host binding, so the host catches it before the
// composer sees the key and calls this directly; `/detach` reaches it through
// ChatActionQuitDetach. Both paths must do the same bookkeeping.
func (c *ActiveChat) Detach() ChatEventOutcome {
	c.cancelDictation()
	return ChatEventOutcomeQuitDetach{LastSeenEventOrdinal: detachChat(c.ChatState)}
}
