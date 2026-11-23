// Port of mj-chat/src/chat/active/io.rs.

package chat

import (
	"context"
	"fmt"
	"log/slog"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ActiveChatPump waits for the next background message, applies it, and
// drains whatever queued behind it.
//
// A nil chat means no chat is warm, and the feed never wakes the caller
// before ctx ends. Cancel safe: every arm is a cancel-safe receive, and a
// message is applied only once its arm has won.
func ActiveChatPump(ctx context.Context, chat *ActiveChat) {
	if chat == nil {
		<-ctx.Done()
		return
	}
	wakeups := chat.awaitWakeups(ctx)
	if len(wakeups) == 0 {
		return
	}
	for _, wakeup := range wakeups {
		switch wakeup := wakeup.(type) {
		case pumpWakeupRemote:
			if wakeup.closed {
				chat.remoteOpen = false
			} else {
				chat.applyRemoteResult(wakeup.result)
			}
		case pumpWakeupIo:
			chat.applyIoUpdate(wakeup.update)
		case pumpWakeupComposer:
			chat.applyComposerUpdate(wakeup.update)
		case pumpWakeupVoice:
			chat.applyVoiceUpdate(wakeup.update)
		case pumpWakeupView:
			chat.applySessionView(wakeup.view, wakeup.err)
		}
	}
	chat.drain(ctx)
	chat.reportWorkerDeath(ctx)
}

// pumpWakeup is one feed's message that woke the pump.
type pumpWakeup interface{ isPumpWakeup() }

// pumpWakeupRemote is a remote result, or the remote feed closing for good.
type pumpWakeupRemote struct {
	result chatRemoteResult
	closed bool
}

type pumpWakeupIo struct{ update ChatIoUpdate }

type pumpWakeupComposer struct{ update composerIoUpdate }

type pumpWakeupVoice struct{ update VoiceUpdate }

type pumpWakeupView struct {
	view client.ManagedSessionView
	err  error
}

func (pumpWakeupRemote) isPumpWakeup()   {}
func (pumpWakeupIo) isPumpWakeup()       {}
func (pumpWakeupComposer) isPumpWakeup() {}
func (pumpWakeupVoice) isPumpWakeup()    {}
func (pumpWakeupView) isPumpWakeup()     {}

// awaitWakeups waits until one of the chat's feeds has a message, or ctx
// ends. Each feed is received on its own goroutine; once one has a message
// the rest are cancelled, and every message any receive already took is
// returned in arrival order, so none is lost. A closed feed reports nothing
// for ever, so the remote and view arms run only while their flags say the
// feed is open. The senders for the I/O and voice feeds live in the chat,
// so those receivers cannot report a closed channel.
func (c *ActiveChat) awaitWakeups(ctx context.Context) []pumpWakeup {
	armCtx, stop := context.WithCancel(ctx)
	defer stop()
	arrived := make(chan pumpWakeup, 5)
	var arms sync.WaitGroup
	arm := func(receive func() (pumpWakeup, bool)) {
		arms.Add(1)
		go func() {
			defer arms.Done()
			if wakeup, ok := receive(); ok {
				arrived <- wakeup
			}
		}()
	}
	if c.remoteOpen {
		remote := c.remote
		arm(func() (pumpWakeup, bool) {
			result, ok := remote.recv(armCtx)
			if ok {
				return pumpWakeupRemote{result: result}, true
			}
			// A cancelled arm is not a closed feed.
			return pumpWakeupRemote{closed: true}, armCtx.Err() == nil
		})
	}
	chatIo := c.chatIo
	arm(func() (pumpWakeup, bool) {
		update, ok := chatIo.Recv(armCtx)
		return pumpWakeupIo{update: update}, ok
	})
	composer := c.composerRuntime.rx
	arm(func() (pumpWakeup, bool) {
		update, ok := composer.Recv(armCtx)
		return pumpWakeupComposer{update: update}, ok
	})
	voice := c.voiceUpdates
	arm(func() (pumpWakeup, bool) {
		update, ok := voice.Recv(armCtx)
		return pumpWakeupVoice{update: update}, ok
	})
	if c.sessionOpen {
		session := c.session
		arm(func() (pumpWakeup, bool) {
			view, err := session.Changed(armCtx)
			if err != nil && armCtx.Err() != nil {
				return nil, false
			}
			return pumpWakeupView{view: view, err: err}, true
		})
	}
	finished := make(chan struct{})
	go func() {
		arms.Wait()
		close(finished)
	}()
	var wakeups []pumpWakeup
	select {
	case wakeup := <-arrived:
		wakeups = append(wakeups, wakeup)
	case <-armCtx.Done():
	case <-finished:
	}
	stop()
	<-finished
	close(arrived)
	for wakeup := range arrived {
		wakeups = append(wakeups, wakeup)
	}
	return wakeups
}

// tryRecv takes the next queued message without waiting.
func tryRecv[T any](feed *core.Unbounded[T]) (T, bool) {
	return feed.Recv(remoteTryRecvContext)
}

func (c *ActiveChat) drain(ctx context.Context) {
	if prompt, ok := c.DrainComposer(); ok {
		c.composerRuntime.preparedStartup = append(c.composerRuntime.preparedStartup, prompt)
	}
	for len(c.composerRuntime.preparedReceiptRetries) > 0 {
		commandID := c.composerRuntime.preparedReceiptRetries[0]
		c.composerRuntime.preparedReceiptRetries = c.composerRuntime.preparedReceiptRetries[1:]
		c.dispatch(ChatActionResendSubmission{CommandID: commandID})
	}
	c.refreshVoiceAvailability()
	for {
		result, ok := c.remote.tryRecv()
		if !ok {
			break
		}
		c.applyRemoteResult(result)
	}
	for {
		update, ok := tryRecv(c.chatIo)
		if !ok {
			break
		}
		c.applyIoUpdate(update)
	}
	for {
		update, ok := tryRecv(c.voiceUpdates)
		if !ok {
			break
		}
		c.applyVoiceUpdate(update)
	}
	for c.sessionOpen {
		changed, err := c.session.HasChanged()
		if err != nil || !changed {
			break
		}
		// A change is pending, so this returns at once; the caller's context
		// ending must not read as the feed failing.
		view, err := c.session.Changed(context.Background())
		c.applySessionView(view, err)
	}
	c.publishConversationNotices()
	// Applying an update can queue a history page or listing; every wakeup
	// ends here, so neither waits for a key press.
	c.dispatchComposerRequests()
}

func (c *ActiveChat) publishConversationNotices() {
	appliedOrdinal := c.latestSeq
	for index := range c.conversationNotices {
		notice := &c.conversationNotices[index]
		if notice.Submitted {
			continue
		}
		permit, ok := c.remote.operations().tryReserve()
		if !ok {
			slog.Warn("could not queue conversation notice", "error", "no available capacity")
			continue
		}
		permit.send(chatRemoteOperationRecordNotice{ID: notice.ID, Text: notice.Text})
		notice.Submitted = true
		notice.SentAt = appliedOrdinal
	}
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
	finished, err := c.remote.takeFinished()
	if !finished {
		return
	}
	message := "Chat background worker stopped unexpectedly"
	if err != nil {
		message = fmt.Sprintf("Chat background worker failed: %v", err)
	}
	c.applyRemoteResult(chatRemoteResultWorkerFailed{Error: message})
}

func (c *ActiveChat) applyIoUpdate(update ChatIoUpdate) {
	if reconnected, ok := update.(ChatIoUpdateSessionReconnected); ok {
		c.finishSessionReconnect(reconnected.Session, reconnected.Err)
		return
	}
	if _, ok := update.(ChatIoUpdateToolDiffstats); ok {
		c.diffstatsInFlight = max(c.diffstatsInFlight-1, 0)
	}
	if rebuild, ok := applyChatIoUpdate(c.ChatState, update).(PrefixRebuildNeeded); ok {
		c.rebuildTranscriptPrefix(rebuild.Attempt)
	}
	dispatchDiffstatRequests(c.ChatState, c.chatIo, &c.diffstatsInFlight)
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
	if c.context == nil {
		return
	}
	preferred := c.context.Session.LastProfile
	if c.voiceProbePending || (c.voiceProbeAt != nil && time.Since(*c.voiceProbeAt) < 30*time.Second) {
		return
	}
	c.voiceProbePending = true
	now := time.Now()
	c.voiceProbeAt = &now
	updates := c.voiceUpdates
	core.Spawn(func() struct{} {
		update := probeVoiceAvailability(context.Background(), preferred)
		if err := updates.Send(update); err != nil {
			slog.Debug("dictation availability dropped because the chat closed", "error", err)
		}
		return struct{}{}
	})
}

// probeVoiceAvailability asks this build, then the daemon, whether
// dictation can run for preferred.
func probeVoiceAvailability(ctx context.Context, preferred string) VoiceUpdateAvailability {
	type local struct {
		reason      string
		unavailable bool
	}
	reason, err := core.SpawnBlocking(func() local {
		reason, unavailable := VoiceInputUnavailableReason()
		return local{reason: reason, unavailable: unavailable}
	}).Join(ctx)
	if err != nil {
		return VoiceUpdateAvailability{Err: err}
	}
	if reason.unavailable {
		return VoiceUpdateAvailability{Unavailable: &reason.reason}
	}
	daemon, err := client.ConnectExisting(ctx)
	if err != nil {
		return VoiceUpdateAvailability{Err: err}
	}
	reply, err := daemon.Request(ctx, client.DaemonActionTranscriptionAvailable{PreferredProfile: preferred})
	if err != nil {
		return VoiceUpdateAvailability{Err: err}
	}
	if available, ok := reply.(client.DaemonReplyTranscriptionAvailable); ok {
		if available.Available {
			return VoiceUpdateAvailability{}
		}
		needsProfile := dictationNeedsACodexProfile
		return VoiceUpdateAvailability{Unavailable: &needsProfile}
	}
	return VoiceUpdateAvailability{Err: fmt.Errorf("unexpected transcription availability reply: %s", client.DaemonReplyDebug(reply))}
}

func (c *ActiveChat) applyVoiceUpdate(update VoiceUpdate) {
	switch update := update.(type) {
	case VoiceUpdateAvailability:
		c.voiceProbePending = false
		switch {
		case update.Err != nil:
			c.setVoiceAvailable(false)
			unavailable := fmt.Sprintf("Dictation is unavailable: %v", update.Err)
			c.voiceUnavailable = &unavailable
			c.SetNotice(update.Err.Error())
		case update.Unavailable != nil:
			c.setVoiceAvailable(false)
			reason := *update.Unavailable
			c.voiceUnavailable = &reason
		default:
			c.setVoiceAvailable(true)
			c.voiceUnavailable = nil
		}
	case VoiceUpdateStatus:
		c.SetNotice(update.Status)
	case VoiceUpdateFinished:
		c.voiceActive = false
		c.voiceCancel = nil
		c.voiceFinishing = false
		if update.Err != nil {
			c.SetNotice(DictationErrorMessage(update.Err))
			return
		}
		if strings.TrimSpace(update.Text) != "" {
			c.setInput(appendDictation(c.input, update.Text))
		}
		c.clearNotice()
	}
}

// applySessionView applies one view of the session, or the error that closed
// its feed.
func (c *ActiveChat) applySessionView(view client.ManagedSessionView, err error) {
	if c.sessionRetiring && err != nil {
		// The feed closing is the expected end of a deliberate stop or
		// destroy, so it is neither a lost connection nor a reason to chase
		// a replacement actor. The transcript stays readable.
		slog.Debug("session feed closed because the session is being retired",
			"error", err.Error(), "session_id", c.session.SessionID())
		c.sessionOpen = false
		return
	}
	c.sessionOpen = applySessionView(c.ChatState, view, err)
	c.applyDeferredElicitationDraft()
	c.surfaceReviewerElicitations()
	if !c.sessionOpen && !c.sessionRetiring {
		c.beginSessionReconnect()
	}
	dispatchDiffstatRequests(c.ChatState, c.chatIo, &c.diffstatsInFlight)
}

func (c *ActiveChat) applyRemoteResult(result chatRemoteResult) {
	if destination := c.composerForward; destination != nil {
		if err := destination.Send(composerIoUpdateRemote{result: result}); err != nil {
			slog.Warn("could not deliver a result to the transferred composer", "error", err)
		}
		return
	}
	sync, syncFinished := result.(chatRemoteResultSync)
	syncSucceeded := syncFinished && sync.Err == nil
	applyChatRemoteResult(c.ChatState, result)
	if syncFinished {
		if syncSucceeded && c.reconnectNoticePendingSync {
			c.connectionFeedback = nil
		}
		c.reconnectNoticePendingSync = false
	}
}

func (c *ActiveChat) beginSessionReconnect() {
	if c.sessionReconnectInFlight {
		return
	}
	c.sessionReconnectInFlight = true
	sessionID := c.session.SessionID()
	sessionManager := c.sessionManager
	updates := c.chatIo
	core.Spawn(func() struct{} {
		session, err := sessionManager.WaitForSession(context.Background(), sessionID, sessionActorReconnectWait)
		if sendErr := updates.Send(ChatIoUpdateSessionReconnected{Session: session, Err: err}); sendErr != nil {
			slog.Debug("session reconnect result dropped because the chat closed",
				"error", sendErr, "session_id", sessionID)
		}
		return struct{}{}
	})
}

// finishSessionReconnect applies the handoff attempt's result: the
// replacement session handle, or err when none appeared.
func (c *ActiveChat) finishSessionReconnect(session client.SessionHandle, err error) {
	c.sessionReconnectInFlight = false
	if err != nil {
		if c.sessionRetiring {
			// The session was stopped or destroyed on purpose, so losing
			// its actor is the expected outcome, not a failure.
			slog.Debug("session relay handoff ended because the session is being retired",
				"error", err, "session_id", c.session.SessionID())
			return
		}
		c.setConnectionNotice(fmt.Sprintf("Could not reconnect to session relay: %v", err))
		if c.sessionFeedExpected {
			c.beginSessionReconnect()
		}
		return
	}
	view := session.View()
	c.instance = newChatInstance()
	c.session = session
	c.sessionOpen = applySessionView(c.ChatState, view, nil)
	c.applyDeferredElicitationDraft()
	if c.sessionOpen {
		c.reconnectNoticePendingSync = true
		c.connectionFeedback = nil
	} else {
		c.beginSessionReconnect()
	}
}

// dictationNotChecked is shown until the first availability probe answers.
// A conversation opened without its session's settings never probes, so this
// also covers that.
const dictationNotChecked = "Dictation is unavailable until Mjolnir has checked for the voice helper and a Codex sign-in. Try again in a moment."

const dictationNeedsACodexProfile = "Dictation is unavailable. It transcribes through a Codex profile signed in with a ChatGPT account, and the daemon has no Codex profile signed in that way. Sign a daemon Codex profile in with ChatGPT."
