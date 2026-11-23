// Port of mj-chat/src/chat/active.rs.

// The live conversation: the background feeds behind an open session, and
// the transcript and composer the combined surface asks it to draw into the
// regions it has chosen.

package chat

import (
	"context"
	"log/slog"
	"slices"
	"strings"
	"time"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ChatDaemonRequest is what the chat asks the controller daemon to do:
// durable chat-side state that a host process must ask the daemon to store.
//
// The chat runs in the terminal process, which owns no session state and no
// database: everything here crosses to the daemon, which does. Review start
// requests travel this way too -- the review runs there.
type ChatDaemonRequest interface {
	isChatDaemonRequest()
}

type ChatDaemonRequestRecordAbandonedPrompt struct {
	SessionID string
	BundleID  string
	Text      string
}

// ChatDaemonRequestStartTurnReview reviews the turn this session just
// finished.
type ChatDaemonRequestStartTurnReview struct {
	SessionID string
}

func (ChatDaemonRequestRecordAbandonedPrompt) isChatDaemonRequest() {}
func (ChatDaemonRequestStartTurnReview) isChatDaemonRequest()       {}

const maxDiffstatTasks = 2

const sessionActorReconnectWait = 5 * time.Second

// ChatIoUpdate is one result a background task reports over the chat's I/O
// feed. Each Rust `Result<_, String>` is a value with Err set on failure.
type ChatIoUpdate interface {
	isChatIoUpdate()
}

type ChatIoUpdateToolCallDetail struct {
	Generation uint64
	StableID   string
	Revision   ToolDetailRevision
	Entry      *core.ChatEntry
	Err        error
}

type ChatIoUpdateEarlierMessages struct {
	Generation uint64
	Before     *core.TranscriptCursor
	Lines      []Line
	Err        error
}

// ChatIoUpdateTranscriptPrefix is the history a large session did not
// convert when it opened, built off the event loop. Attempt counts the tries
// so far, so a transcript that keeps changing under the conversion cannot
// retry for ever.
type ChatIoUpdateTranscriptPrefix struct {
	Attempt   uint32
	Entries   []core.ChatEntry
	Diffstats []ToolDiffstatRequest
	Err       error
}

type ChatIoUpdateToolDiffstats struct {
	ToolCallID string
	Revision   uint64
	Diffstats  []string
	Err        error
}

type ChatIoUpdateSessionReconnected struct {
	Session client.SessionHandle
	Err     error
}

// ChatIoUpdateLinkOpenFailed: the system browser could not be started for a
// clicked link.
type ChatIoUpdateLinkOpenFailed struct {
	URL   string
	Error string
}

func (ChatIoUpdateToolCallDetail) isChatIoUpdate()     {}
func (ChatIoUpdateEarlierMessages) isChatIoUpdate()    {}
func (ChatIoUpdateTranscriptPrefix) isChatIoUpdate()   {}
func (ChatIoUpdateToolDiffstats) isChatIoUpdate()      {}
func (ChatIoUpdateSessionReconnected) isChatIoUpdate() {}
func (ChatIoUpdateLinkOpenFailed) isChatIoUpdate()     {}

// maxPrefixConversionAttempts is how many times a refused prefix is rebuilt
// before the view settles for its tail. Compaction rewriting the history
// under a pending conversion is rare, and one rebuild against the current
// snapshot normally lands.
const maxPrefixConversionAttempts uint32 = 3

func dispatchDiffstatRequests(chat *ChatState, updates *core.Unbounded[ChatIoUpdate], inFlight *int) {
	panic("unported: mj-chat/src/chat/active.rs dispatchDiffstatRequests")
}

// PendingPrefix is the history a tail-first open still owes the view: the
// transcript items in front of the loaded tail, and the frontier they are
// converted against. Copying the item slice copies handles, not
// conversations.
type PendingPrefix struct {
	items    []*core.TranscriptItem
	frontier uint64
}

func pendingPrefixOf(session *core.MaterializedSession, length int) *PendingPrefix {
	length = min(length, len(session.Transcript))
	if length <= 0 {
		return nil
	}
	return &PendingPrefix{
		items:    slices.Clone(session.Transcript[:length]),
		frontier: session.AppliedEventOrdinal,
	}
}

// spawnTranscriptPrefix converts the unloaded history on a blocking thread
// and reports it back over the chat's I/O feed, including the failure of the
// conversion itself.
func spawnTranscriptPrefix(pending PendingPrefix, attempt uint32, updates *core.Unbounded[ChatIoUpdate]) {
	panic("unported: mj-chat/src/chat/active.rs spawnTranscriptPrefix")
}

// PrefixRebuild is whether applying an update left history that still has
// to be converted.
type PrefixRebuild interface {
	isPrefixRebuild()
}

type PrefixRebuildNotNeeded struct{}

// PrefixRebuildNeeded: the converted history no longer lines up with the
// tail, so it has to be rebuilt from the session's current snapshot. Attempt
// numbers the try.
type PrefixRebuildNeeded struct {
	Attempt uint32
}

func (PrefixRebuildNotNeeded) isPrefixRebuild() {}
func (PrefixRebuildNeeded) isPrefixRebuild()    {}

func applyChatIoUpdate(chat *ChatState, update ChatIoUpdate) PrefixRebuild {
	panic("unported: mj-chat/src/chat/active.rs applyChatIoUpdate")
}

// applySessionView applies one session view (or the error that ended the
// feed) to the chat. false means this particular actor's feed has closed, so
// it must not be awaited while the chat reacquires the manager's replacement
// actor.
//
// This runs whether or not the chat is on screen: a warm chat behind the
// session list stays as current as one the user is watching.
func applySessionView(state *ChatState, view client.ManagedSessionView, err error) bool {
	panic("unported: mj-chat/src/chat/active.rs applySessionView")
}

// detachChat: the user has left the chat, whether for the session list,
// another conversation, or the shell. Clears the interaction state that
// should not follow them back in and reports how far they have now read,
// which becomes the session's read receipt.
func detachChat(state *ChatState) uint64 {
	lastSeenEventOrdinal := state.LatestSeq()
	state.resetInteraction()
	return lastSeenEventOrdinal
}

// ChatInstance is the opaque ownership identity for one view and its current
// actor attachment.
type ChatInstance struct {
	token *chatInstanceToken
}

// chatInstanceToken is not zero-sized, so distinct tokens have distinct
// addresses.
type chatInstanceToken struct {
	_ byte
}

func newChatInstance() ChatInstance {
	return ChatInstance{token: &chatInstanceToken{}}
}

// ActiveChat is a chat view and every background feed behind it.
//
// The combined surface owns one of these for the conversation on screen. It
// keeps following the worker while another pane has the keyboard, so nothing
// is lost while the user looks elsewhere. Close detaches the proxy and leaves
// the target worker alive.
//
// A live chat is its ChatState plus the session plumbing around it, so every
// read and edit of the conversation reaches the state directly (the embedded
// *ChatState stands for Rust's Deref). Methods that consult the session
// handle or the feed flags are declared on ActiveChat and take precedence.
type ActiveChat struct {
	instance ChatInstance
	*ChatState
	session        client.SessionHandle
	sessionManager client.SessionControl
	context        *ChatSessionContext
	remote         *chatRemoteSupervisor
	// chatIo is held so the receiver never reports the feed closed, and so
	// spawned clipboard and history tasks have somewhere to report.
	chatIo            *core.Unbounded[ChatIoUpdate]
	diffstatsInFlight int
	// voiceUpdates is held for the same reason, and handed to each dictation
	// thread.
	voiceUpdates      *core.Unbounded[VoiceUpdate]
	voiceCancel       *core.Unbounded[VoiceCommand]
	voiceProbeAt      *time.Time
	voiceProbePending bool
	// voiceUnavailable is why the last availability probe found dictation
	// unavailable, as the sentence the dictation key shows. Nil until a probe
	// says.
	voiceUnavailable *string
	voiceFinishing   bool
	// A closed feed reports nothing for ever, which would leave its arm
	// permanently ready. Each flag retires its own arm instead.
	remoteOpen               bool
	sessionOpen              bool
	sessionReconnectInFlight bool
	sessionFeedExpected      bool
	// sessionRetiring is whether this session is being retired on purpose: a
	// stop or destroy is in flight, or its record is no longer pollable. A
	// closed feed is then expected, so it starts no handoff attempt and
	// reports no failure.
	sessionRetiring bool
	// reconnectNoticePendingSync preserves the stronger reconnect result when
	// the initial sync, which independently reacquires the same actor,
	// finishes just afterwards.
	reconnectNoticePendingSync bool
	persistence                ChatDaemonSender
	// deferredElicitationDrafts: a cached question may arrive in the newly
	// attached review stream a few ticks after the chat itself opens. Keep it
	// here until that exact source form surfaces, instead of dropping it or
	// applying it to the primary.
	deferredElicitationDrafts []ChatElicitationDraft
	// earlierTask aborts the running earlier-history read, when one runs.
	earlierTask context.CancelFunc
}

// PreparedChat is sendable chat initialization data. Build this off the UI
// thread, then call Open on the UI thread to create thread-local control
// identities.
type PreparedChat struct {
	session     client.SessionHandle
	bundleID    string
	context     *ChatSessionContext
	control     client.SessionControl
	header      SessionHeaderIdentity
	draft       string
	notices     Notices
	persistence ChatDaemonSender
}

// WithDraft refreshes the local composer after asynchronous preparation. A
// surface may have captured newer input while this attachment was in flight.
func (p PreparedChat) WithDraft(draft string) PreparedChat {
	p.draft = draft
	return p
}

// Open constructs the view without filesystem or database access.
func (p PreparedChat) Open() *ActiveChat {
	return activeChatFromPrepared(p, nil)
}

// OpenWithComposer opens with the composer a surface held while this
// attachment was in flight. The chat adopts it before starting its own reads,
// so session history loads once, into the composer it keeps.
func (p PreparedChat) OpenWithComposer(composer LiveComposer) *ActiveChat {
	return activeChatFromPrepared(p, &composer)
}

// OpenReplacing: a same-session handoff keeps the latest local composer, not
// the saved draft captured before asynchronous preparation. Other sessions
// retain their own saved drafts.
func (p PreparedChat) OpenReplacing(previous *ActiveChat) *ActiveChat {
	if previous != nil && previous.SessionID() == p.session.SessionID() {
		p.draft = previous.Draft()
	}
	return p.Open()
}

// ActiveChatOpen builds the view from the session's current snapshot and
// starts its background feeds. Interactive hosts prepare it with
// ActiveChatPrepareWithPersistence, then construct the view with
// PreparedChat.Open.
//
// draft is the unsent input saved when this session was last detached. Only a
// fresh view takes it: a warm chat the surface kept alive already holds newer
// input than the database copy.
//
// notices is the process-wide notifications bar; it is installed on the new
// state before any notice is raised, so recovery and connection notices land
// in the same shared slot the surface reads.
func ActiveChatOpen(session client.SessionHandle, bundleID string, sessionContext *ChatSessionContext, control client.SessionControl, header SessionHeaderIdentity, draft string, notices Notices) *ActiveChat {
	return ActiveChatOpenWithPersistence(session, bundleID, sessionContext, control, header, draft, notices, nil)
}

// ActiveChatOpenWithPersistence opens a chat whose mutations are forwarded to
// its host's daemon client.
func ActiveChatOpenWithPersistence(session client.SessionHandle, bundleID string, sessionContext *ChatSessionContext, control client.SessionControl, header SessionHeaderIdentity, draft string, notices Notices, persistence ChatDaemonSender) *ActiveChat {
	return ActiveChatPrepareWithPersistence(session, bundleID, sessionContext, control, header, draft, notices, persistence).Open()
}

// ActiveChatPrepareWithPersistence prepares sendable chat data; live focus
// flags are created by PreparedChat.Open.
func ActiveChatPrepareWithPersistence(session client.SessionHandle, bundleID string, sessionContext *ChatSessionContext, control client.SessionControl, header SessionHeaderIdentity, draft string, notices Notices, persistence ChatDaemonSender) PreparedChat {
	return PreparedChat{
		session:     session,
		bundleID:    bundleID,
		context:     sessionContext,
		control:     control,
		header:      header,
		draft:       draft,
		notices:     notices,
		persistence: persistence,
	}
}

func activeChatFromPrepared(prepared PreparedChat, composer *LiveComposer) *ActiveChat {
	panic("unported: mj-chat/src/chat/active.rs activeChatFromPrepared")
}

// AdoptLiveComposer reconciles transferred local state with this
// attachment's authoritative view.
func (c *ActiveChat) AdoptLiveComposer(composer LiveComposer) {
	c.ChatState.AdoptLiveComposer(&composer)
	// A standby composer never asked for this session's history. One a chat
	// parked carries its loaded history or its in-flight read, which a new
	// request would abort.
	if !c.sessionHistoryRequested() {
		c.RequestSessionHistory()
		c.dispatchComposerRequests()
	}
	// The view may have advanced while the composer was parked. Applying it
	// restores the authoritative queue and uses the same submission
	// reconciliation as every feed update, without retaining another copy.
	c.applySessionView(c.session.View(), nil)
}

// TakeLiveComposer transfers native result ownership together with its
// pending submission rows.
func (c *ActiveChat) TakeLiveComposer() *LiveComposer {
	panic("unported: mj-chat/src/chat/active.rs TakeLiveComposer")
}

// Instance is the identity of this owned view, independent of the session it
// displays.
func (c *ActiveChat) Instance() ChatInstance {
	return c.instance
}

func (c *ActiveChat) IsInstance(instance ChatInstance) bool {
	return c.instance.token == instance.token
}

func (c *ActiveChat) SessionID() string {
	return c.session.SessionID()
}

// ElicitationDraft snapshots every pending question so the host can retain
// local answers while this single warm chat is replaced by another session.
func (c *ActiveChat) ElicitationDraft() *ChatElicitationDraft {
	drafts := c.ElicitationDrafts()
	if len(drafts) == 0 {
		return nil
	}
	return &drafts[0]
}

// ElicitationDrafts snapshots every local question that may need to survive
// this chat's replacement. A reviewer form can be deferred while the sidecar
// stream catches up, so it lives alongside the currently visible form.
func (c *ActiveChat) ElicitationDrafts() []ChatElicitationDraft {
	drafts := c.ChatState.ElicitationDrafts()
	return append(drafts, c.deferredElicitationDrafts...)
}

// RestoreElicitationDrafts restores all matching drafts collected before
// this session was detached. The state machine decides whether each request
// is currently visible or must wait for the reviewer stream.
func (c *ActiveChat) RestoreElicitationDrafts(drafts []ChatElicitationDraft) {
	for _, draft := range drafts {
		_ = c.RestoreElicitationDraft(draft)
	}
}

// RestoreElicitationDraft restores a draft only when the newly attached
// projection is already showing the exact same pending request.
func (c *ActiveChat) RestoreElicitationDraft(draft ChatElicitationDraft) bool {
	panic("unported: mj-chat/src/chat/active.rs RestoreElicitationDraft")
}

func (c *ActiveChat) applyDeferredElicitationDraft() {
	drafts := c.deferredElicitationDrafts
	c.deferredElicitationDrafts = nil
	for _, draft := range drafts {
		// Matching reviewer drafts wait behind the current form; ended
		// requests are discarded by the same restoration decision.
		c.RestoreElicitationDraft(draft)
	}
}

// SessionFeedOpen is whether this view is still attached to a live session
// actor.
//
// Pause, destroy, and a replaced target retire the actor and close this feed.
// A visible chat reacquires replacements in place; the flag remains useful
// while that asynchronous handoff is in flight.
func (c *ActiveChat) SessionFeedOpen() bool {
	return c.sessionOpen
}

// SetSessionFeedExpected keeps a visible chat attached when another control
// surface makes its session runnable again. A stopped actor gets one bounded
// handoff attempt on its own; a durable active record means replacement
// should keep being retried until the actor appears or another record retires
// the session.
func (c *ActiveChat) SetSessionFeedExpected(expected bool) {
	c.sessionFeedExpected = expected
	if expected {
		// A session that is runnable again is no longer being retired.
		c.sessionRetiring = false
		if !c.sessionOpen {
			c.beginSessionReconnect()
		}
	}
}

// SetSessionRetiring tells this chat that its session is being retired on
// purpose, so the feed closing is the expected outcome rather than a lost
// actor: the chat stays on screen as a transcript, attempts no handoff, and
// reports no reconnect failure. An expected feed clears it: see
// SetSessionFeedExpected.
func (c *ActiveChat) SetSessionRetiring(retiring bool) {
	c.sessionRetiring = retiring
}

// SessionRetiring is whether this chat is treating a closed feed as a
// deliberate retirement.
func (c *ActiveChat) SessionRetiring() bool {
	return c.sessionRetiring
}

// RefreshContext takes the surface's newer view of the config and this
// session's record, so a chat that stays open across a config reload offers
// the profiles that are configured now rather than the ones that were
// configured when it opened.
//
// A record the daemon no longer publishes leaves the open-time copy in place:
// disappearing from the list is not a reason to lose the last known context.
// A chat opened without a context stays without one.
//
// project is the session whose project identity names the location, so a
// sub-agent child can be named after the parent whose checkout it works in.
// Nil uses the session's own record.
func (c *ActiveChat) RefreshContext(config *core.Config, session *core.SessionRecord, project *core.SessionRecord) {
	panic("unported: mj-chat/src/chat/active.rs RefreshContext")
}

// SetDisplayTitle lets a focused surface supply a readable title for unnamed
// conversations.
func (c *ActiveChat) SetDisplayTitle(title string) {
	c.SetHeaderSummary(c.headerTarget, c.headerProfile, title)
}

func (c *ActiveChat) LatestEventOrdinal() uint64 {
	return c.LatestSeq()
}

// Close is Rust's Drop: it aborts the earlier-history read and stops any
// dictation. A review outlives this view: the daemon owns it, so closing the
// terminal leaves it running and resolvable from the phone.
func (c *ActiveChat) Close() {
	if c.earlierTask != nil {
		c.earlierTask()
		c.earlierTask = nil
	}
	c.cancelDictation()
}

// VoiceUpdate is one report from the dictation and availability tasks.
type VoiceUpdate interface {
	isVoiceUpdate()
}

// VoiceUpdateAvailability is the availability probe's answer: Err when the
// probe itself failed, otherwise Unavailable says why dictation cannot run
// (nil when it can).
type VoiceUpdateAvailability struct {
	Unavailable *string
	Err         error
}

type VoiceUpdateStatus struct {
	Status string
}

type VoiceUpdateFinished struct {
	Text string
	Err  error
}

func (VoiceUpdateAvailability) isVoiceUpdate() {}
func (VoiceUpdateStatus) isVoiceUpdate()       {}
func (VoiceUpdateFinished) isVoiceUpdate()     {}

func spawnDictation(preferredProfile string, updates *core.Unbounded[VoiceUpdate], cancel *core.Unbounded[VoiceCommand]) {
	core.Spawn(func() struct{} {
		text, err := dictationHandoff(context.Background(), preferredProfile, updates, cancel)
		if sendErr := updates.Send(VoiceUpdateFinished{Text: text, Err: err}); sendErr != nil {
			slog.Debug("voice result dropped because the chat closed", "error", sendErr)
		}
		return struct{}{}
	})
}

func dictationHandoff(ctx context.Context, preferredProfile string, updates *core.Unbounded[VoiceUpdate], cancel *core.Unbounded[VoiceCommand]) (string, error) {
	panic("unported: mj-chat/src/chat/active.rs dictationHandoff")
}

func appendDictation(prefix string, transcript string) string {
	prefix = strings.TrimRightFunc(prefix, unicode.IsSpace)
	transcript = strings.TrimSpace(transcript)
	switch {
	case prefix == "":
		return transcript
	case transcript == "":
		return prefix
	}
	return prefix + " " + transcript
}
