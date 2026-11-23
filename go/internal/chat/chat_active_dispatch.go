// Port of mj-chat/src/chat/active/dispatch.rs.

package chat

import (
	"fmt"
	"log/slog"

	tea "charm.land/bubbletea/v2"

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
	panic("unported: mj-chat/src/chat/active/dispatch.rs handleSurfaceEvent")
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
	panic("unported: mj-chat/src/chat/active/dispatch.rs dispatchToolDetails")
}

func (c *ActiveChat) dispatchEarlierHistory() {
	panic("unported: mj-chat/src/chat/active/dispatch.rs dispatchEarlierHistory")
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
	panic("unported: mj-chat/src/chat/active/dispatch.rs dispatch")
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
