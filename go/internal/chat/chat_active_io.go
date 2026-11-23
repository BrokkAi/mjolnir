// Port of mj-chat/src/chat/active/io.rs.

package chat

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// ActiveChatPump waits for the next background message, applies it, and
// drains whatever queued behind it.
//
// A nil chat means no chat is warm, and the feed never wakes the caller
// before ctx ends. Cancel safe: every arm is a cancel-safe receive, and a
// message is applied only once its arm has won.
func ActiveChatPump(ctx context.Context, chat *ActiveChat) {
	panic("unported: mj-chat/src/chat/active/io.rs ActiveChatPump")
}

func (c *ActiveChat) drain(ctx context.Context) {
	panic("unported: mj-chat/src/chat/active/io.rs drain")
}

func (c *ActiveChat) publishConversationNotices() {
	panic("unported: mj-chat/src/chat/active/io.rs publishConversationNotices")
}

// ApplyReviewViews shows the reviews the daemon is running for this session.
//
// The terminal projects the daemon's status above the ordinary composer. A
// review therefore survives this view closing.
func (c *ActiveChat) ApplyReviewViews(views []client.RuntimeReviewView) {
	c.setTurnReviews(views)
	c.surfaceReviewerElicitations()
}

// requestTurnReview asks the daemon to review committed work.
func (c *ActiveChat) requestTurnReview() {
	sessionID := c.session.SessionID()
	c.SetNotice("Starting a review…")
	c.sendDaemonRequest(ChatDaemonRequestStartTurnReview{SessionID: sessionID})
}

func (c *ActiveChat) runTurnReview(intent TurnReviewRequest) {
	switch intent {
	case TurnReviewIntentStart:
		c.requestTurnReview()
	}
}

// sendDaemonRequest hands one request to the daemon bridge, or reports that
// this chat has none: a chat without a bridge cannot reach the daemon at all.
func (c *ActiveChat) sendDaemonRequest(request ChatDaemonRequest) {
	c.composerRequests = append(c.composerRequests, composerRequestDaemon{request})
	c.dispatchComposerRequests()
}

func (c *ActiveChat) dispatchComposerRequests() {
	session := c.session
	c.DispatchComposerRequests(func(context.Context) (client.SessionHandle, error) {
		return session.Clone(), nil
	}, c.persistence)
}

// ReportReviewRefusal reports a daemon refusal in the prompt status, or as a
// notice when no review is open -- a refused `/review` has nowhere else to
// appear.
func (c *ActiveChat) ReportReviewRefusal(message string) {
	c.reportReviewRefusal(message)
}

// reportWorkerDeath reports a background worker that stopped on its own.
// Cheap enough to check on every wakeup: it only joins a handle that already
// finished.
func (c *ActiveChat) reportWorkerDeath(ctx context.Context) {
	panic("unported: mj-chat/src/chat/active/io.rs reportWorkerDeath")
}

func (c *ActiveChat) applyIoUpdate(update ChatIoUpdate) {
	panic("unported: mj-chat/src/chat/active/io.rs applyIoUpdate")
}

// rebuildTranscriptPrefix restarts the history conversion against the
// session's current snapshot, after the transcript changed under the last
// one. Only the spawn happens here; the conversion itself stays off the event
// loop.
func (c *ActiveChat) rebuildTranscriptPrefix(attempt uint32) {
	view := c.session.View()
	if view.Snapshot == nil {
		return
	}
	pending := pendingPrefixOf(&view.Snapshot.Materialized, c.unconvertedPrefix)
	if pending == nil {
		return
	}
	spawnTranscriptPrefix(*pending, attempt, c.chatIo)
}

// dictationUnavailableNotice is the sentence the dictation key shows when
// dictation cannot start, so the key never does nothing without saying why
// (launch findings A-4 and E-9).
func (c *ActiveChat) dictationUnavailableNotice() string {
	if c.voiceUnavailable != nil {
		return *c.voiceUnavailable
	}
	return dictationNotChecked
}

func (c *ActiveChat) refreshVoiceAvailability() {
	panic("unported: mj-chat/src/chat/active/io.rs refreshVoiceAvailability")
}

func (c *ActiveChat) applyVoiceUpdate(update VoiceUpdate) {
	panic("unported: mj-chat/src/chat/active/io.rs applyVoiceUpdate")
}

// applySessionView applies one view of the session, or the error that closed
// its feed.
func (c *ActiveChat) applySessionView(view client.ManagedSessionView, err error) {
	panic("unported: mj-chat/src/chat/active/io.rs applySessionView")
}

func (c *ActiveChat) applyRemoteResult(result chatRemoteResult) {
	panic("unported: mj-chat/src/chat/active/io.rs applyRemoteResult")
}

func (c *ActiveChat) beginSessionReconnect() {
	panic("unported: mj-chat/src/chat/active/io.rs beginSessionReconnect")
}

// finishSessionReconnect applies the handoff attempt's result: the
// replacement session handle, or err when none appeared.
func (c *ActiveChat) finishSessionReconnect(session client.SessionHandle, err error) {
	panic("unported: mj-chat/src/chat/active/io.rs finishSessionReconnect")
}

// dictationNotChecked is shown until the first availability probe answers.
// A conversation opened without its session's settings never probes, so this
// also covers that.
const dictationNotChecked = "Dictation is unavailable until Mjolnir has checked for the voice helper and a Codex sign-in. Try again in a moment."

const dictationNeedsACodexProfile = "Dictation is unavailable. It transcribes through a Codex profile signed in with a ChatGPT account, and the daemon has no Codex profile signed in that way. Sign a daemon Codex profile in with ChatGPT."
