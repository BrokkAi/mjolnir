// Port of mj-chat/src/chat.rs.

// Minimal full-screen chat for one persistent Hel worker.
//
// The view state lives here; the concerns around it are split into the
// other chat_*.go files: input edits the composer, history recalls earlier
// prompts, autocomplete parses and completes slash commands, transcript
// projects and draws the conversation, remote runs the relay operations a key
// press asks for, and active wires a live session to all of them.

package chat

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"slices"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/acp-go/schema"
	"github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PlanControl is mj_core::acp::PlanControl, re-exported by the chat module.
type PlanControl = core.PlanControl

// TurnReviewRequest is turn_review::TurnReviewIntent, re-exported under the
// name hosts use.
type TurnReviewRequest = TurnReviewIntent

const mouseScrollRows = 3

// VoiceControl is the one control of the microphone button's form.
type VoiceControl int

const (
	VoiceControlMicrophone VoiceControl = iota
)

// BackgroundTaskControl is a control owned by the background-task dialog.
// The index is only a frame-local form identity; the task's opaque id is
// what crosses the remote boundary and is retained in pending state.
type BackgroundTaskControl interface {
	isBackgroundTaskControl()
}

// BackgroundTaskControlStop is the Stop button on the row at this index.
type BackgroundTaskControlStop int

func (BackgroundTaskControlStop) isBackgroundTaskControl() {}

func voiceForm() Form[VoiceControl] {
	form := NewForm[VoiceControl]()
	form.Declare(VoiceControlMicrophone, ControlKindButton{})
	form.EndFrame(VoiceControlMicrophone)
	return form
}

// ChatDaemonSender is the nonblocking host bridge shared by all Prompt editor
// requests (an unbounded sender).
type ChatDaemonSender = *core.Unbounded[ChatDaemonRequest]

// ReviewStatusLine is what `/review status` reports, on every surface.
//
// It answers the two questions a person actually has: does review run before
// push, and is one being reviewed right now.
func ReviewStatusLine(review *core.ReviewConfig, open bool) string {
	var armed string
	switch review.Trigger() {
	case core.ReviewTriggerPush:
		armed = "Review trigger is before push"
	case core.ReviewTriggerOff:
		armed = "Automatic review is off; /review reviews commits"
	}
	if count := len(review.Reviewers); count == 0 {
		armed += "; no reviewers"
	} else if count == 1 {
		armed = fmt.Sprintf("%s; %d reviewer", armed, count)
	} else {
		armed = fmt.Sprintf("%s; %d reviewers", armed, count)
	}
	if open {
		return armed + ". A review is open now."
	}
	return armed
}

// ChatRegions is where a host surface has told the chat to draw itself.
//
// Transcript and Prompt are the *outer* rectangles including each block's
// border. While busy, the transcript's last row holds activity above the
// prompt. Footer is non-nil only when the host wants the chat to own the
// footer row, which it does while the composer has focus. Overlay is the
// rectangle this conversation owns: dialogs and the autocomplete popup are
// centred and clamped inside it rather than inside the bands above.
// TitleControls is how many columns the host draws its own chips into at the
// right of the transcript's title row, which the title must stop short of.
// PaneFocused says the host has given this pane the keyboard and wants the
// transcript's border drawn in the focused style; a host with a single pane
// leaves it clear.
type ChatRegions struct {
	Transcript    uv.Rectangle
	Prompt        uv.Rectangle
	Footer        *ChatFooter
	Overlay       uv.Rectangle
	TitleControls int
	// TitleLead is how many columns the host draws its own label into at the
	// left of the transcript's title row. The title starts after them, so the
	// label does not cover the title's first words.
	TitleLead   int
	PaneFocused bool
}

// ChatFooter is the footer area and global hints supplied by the host's
// command registry.
//
// Banner replaces the groups entirely while the host is in the middle of
// something the row has to report instead, such as a pending prefix chord.
type ChatFooter struct {
	Area   uv.Rectangle
	Chords []string
	// ChordPrefix is the words that lead the chord group — `ctrl+b then `.
	// The renderer puts them on the first chord that fits, so a narrow row
	// never shows chord keys as if they were plain keys.
	ChordPrefix string
	Functions   []string
	Banner      *Line
}

// FooterCommandArea is one visible host footer command: its index through
// the supplied chords then functions, where it was drawn, and the text drawn
// there.
type FooterCommandArea struct {
	Command int
	Area    uv.Rectangle
	Text    string
}

// ChatElicitationDraft is the local form state saved while the dashboard
// attaches another session. The reviewer metadata is part of the identity
// because reviewer answers are delivered to a different harness than
// primary-agent answers.
type ChatElicitationDraft struct {
	form         ElicitationDraft
	reviewer     bool
	reviewerRole *string
}

// MarshalJSON writes the draft as serde writes the Rust struct: form,
// reviewer, reviewer_role (null when absent).
func (d ChatElicitationDraft) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Form         ElicitationDraft `json:"form"`
		Reviewer     bool             `json:"reviewer"`
		ReviewerRole *string          `json:"reviewer_role"`
	}{Form: d.form, Reviewer: d.reviewer, ReviewerRole: d.reviewerRole})
}

// UnmarshalJSON requires form and reviewer, as serde does; a missing
// reviewer_role is None.
func (d *ChatElicitationDraft) UnmarshalJSON(data []byte) error {
	var wire struct {
		Form         json.RawMessage `json:"form"`
		Reviewer     *bool           `json:"reviewer"`
		ReviewerRole *string         `json:"reviewer_role"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Form == nil {
		return errors.New("missing field `form`")
	}
	if wire.Reviewer == nil {
		return errors.New("missing field `reviewer`")
	}
	var form ElicitationDraft
	if err := json.Unmarshal(wire.Form, &form); err != nil {
		return err
	}
	*d = ChatElicitationDraft{form: form, reviewer: *wire.Reviewer, reviewerRole: wire.ReviewerRole}
	return nil
}

// ElicitationIdentity is a logical form target, independent of presentation
// and editable values.
type ElicitationIdentity struct {
	request      core.ElicitationRequest
	reviewer     bool
	reviewerRole *string
}

// equal is Rust's derived PartialEq.
func (i ElicitationIdentity) equal(other ElicitationIdentity) bool {
	return i.reviewer == other.reviewer && optionalStringEqual(i.reviewerRole, other.reviewerRole) &&
		i.request.Equal(&other.request)
}

func (d *ChatElicitationDraft) identity() ElicitationIdentity {
	return ElicitationIdentity{
		request:      *d.request(),
		reviewer:     d.reviewer,
		reviewerRole: d.reviewerRole,
	}
}

// Matches reports whether the draft belongs to request.
func (d *ChatElicitationDraft) Matches(request *core.ElicitationRequest) bool {
	return d.form.Matches(request)
}

// request stays crate-private; hosts only need Matches while ChatState uses
// it to reconcile delayed reviewer streams.
func (d *ChatElicitationDraft) request() *core.ElicitationRequest {
	return d.form.request()
}

func (d *ChatElicitationDraft) Reviewer() bool {
	return d.reviewer
}

func (d *ChatElicitationDraft) ReviewerRole() (string, bool) {
	if d.reviewerRole == nil {
		return "", false
	}
	return *d.reviewerRole, true
}

// ChatEventOutcome is what one terminal event asked the chat to do.
//
// ChatEventOutcomeNone means the event only changed local state, which lets
// the caller keep draining a paste burst before it redraws. Every exit
// reports the ordinal the user has now seen, which becomes the session's read
// receipt.
type ChatEventOutcome interface {
	isChatEventOutcome()
}

type ChatEventOutcomeNone struct{}

type ChatEventOutcomeHandled struct{}

// ChatEventOutcomeCycleFocus is Tab or Shift-Tab from the composer. The host
// surface owns focus, so the chat only reports which way to walk.
type ChatEventOutcomeCycleFocus struct {
	Reverse bool
}

type ChatEventOutcomeOpenSubagents struct{}

type ChatEventOutcomeOpenReviewers struct{}

// ChatEventOutcomeRecallQueuedPrompt: the host decides whether the durable
// startup queue or the worker owns this ID.
type ChatEventOutcomeRecallQueuedPrompt struct {
	SessionID string
	ID        string
	Text      string
	Kind      core.QueuedCommandKind
}

// ChatEventOutcomeCopyText puts Text on the clipboard and then shows Notice.
// The host owns the clipboard, including the terminal clipboard used over
// SSH.
type ChatEventOutcomeCopyText struct {
	Text   string
	Notice string
}

type ChatEventOutcomeQuitDetach struct {
	LastSeenEventOrdinal uint64
}

func (ChatEventOutcomeNone) isChatEventOutcome()               {}
func (ChatEventOutcomeHandled) isChatEventOutcome()            {}
func (ChatEventOutcomeCycleFocus) isChatEventOutcome()         {}
func (ChatEventOutcomeOpenSubagents) isChatEventOutcome()      {}
func (ChatEventOutcomeOpenReviewers) isChatEventOutcome()      {}
func (ChatEventOutcomeRecallQueuedPrompt) isChatEventOutcome() {}
func (ChatEventOutcomeCopyText) isChatEventOutcome()           {}
func (ChatEventOutcomeQuitDetach) isChatEventOutcome()         {}

// ChatAction is the work one key or pointer event asks the active chat to
// dispatch.
type ChatAction interface {
	isChatAction()
}

type ChatActionTurnControl struct {
	Command core.RelayCommand
}

type ChatActionNone struct{}

type ChatActionOpenSubagents struct{}

type ChatActionOpenReviewers struct{}

type ChatActionPrompt struct {
	Text string
}

type ChatActionResendSubmission struct {
	CommandID string
}

type ChatActionDismissSubmission struct {
	CommandID string
}

type ChatActionRunShell struct {
	Command string
}

type ChatActionRemoveQueuedPrompt struct {
	ID   string
	Text string
	Kind core.QueuedCommandKind
}

// ChatActionStopBackgroundTask stops one provider-owned background task
// without ending the session or cancelling its foreground turn.
type ChatActionStopBackgroundTask struct {
	ID string
}

type ChatActionGoalControl struct {
	Action core.GoalControlAction
}

type ChatActionSetConfig struct {
	Key   string
	Value string
}

type ChatActionPlanCommand struct {
	Original        string
	Control         PlanControl
	RequestedActive bool
	Prompt          *string
}

type ChatActionCancel struct{}

type ChatActionRespondElicitation struct {
	Request  core.ElicitationRequest
	Response core.ElicitationResponse
}

// ChatActionStartTurnReview reviews the turn that just finished, on the
// user's explicit request. Auto-review starts the same path without a key
// press.
type ChatActionStartTurnReview struct{}

// ChatActionTurnReview is work the turn-review view asked the session to
// perform.
type ChatActionTurnReview struct {
	Intent TurnReviewIntent
}

// ChatActionRespondReviewerElicitation is an answer to a form a reviewing
// harness is waiting on. It is routed to the role that asked, never to the
// primary: a turn review can have several harnesses waiting at once.
type ChatActionRespondReviewerElicitation struct {
	Draft    ChatElicitationDraft
	Response core.ElicitationResponse
}

type ChatActionPasteFromClipboard struct{}

// ChatActionOpenLink opens an http(s) link the user clicked in the
// transcript in the system's default browser.
type ChatActionOpenLink struct {
	URL string
}

// ChatActionCopyLink copies a clicked transcript link that does not open in
// a browser.
type ChatActionCopyLink struct {
	URL string
}

type ChatActionAttach struct {
	Path    PastedPath
	Command string
}

// ChatActionAttachPaste is a paste made only of image paths: attach each
// path in Images and keep the rest of Text, the whitespace between them, as
// typed.
type ChatActionAttachPaste struct {
	Text   string
	Images []PastedImagePath
}

type ChatActionToggleVoice struct{}

// ChatActionCycleFocus is Tab or Shift-Tab with no completion popup open.
type ChatActionCycleFocus struct {
	Reverse bool
}

type ChatActionQuitDetach struct{}

func (ChatActionTurnControl) isChatAction()                {}
func (ChatActionNone) isChatAction()                       {}
func (ChatActionOpenSubagents) isChatAction()              {}
func (ChatActionOpenReviewers) isChatAction()              {}
func (ChatActionPrompt) isChatAction()                     {}
func (ChatActionResendSubmission) isChatAction()           {}
func (ChatActionDismissSubmission) isChatAction()          {}
func (ChatActionRunShell) isChatAction()                   {}
func (ChatActionRemoveQueuedPrompt) isChatAction()         {}
func (ChatActionStopBackgroundTask) isChatAction()         {}
func (ChatActionGoalControl) isChatAction()                {}
func (ChatActionSetConfig) isChatAction()                  {}
func (ChatActionPlanCommand) isChatAction()                {}
func (ChatActionCancel) isChatAction()                     {}
func (ChatActionRespondElicitation) isChatAction()         {}
func (ChatActionStartTurnReview) isChatAction()            {}
func (ChatActionTurnReview) isChatAction()                 {}
func (ChatActionRespondReviewerElicitation) isChatAction() {}
func (ChatActionPasteFromClipboard) isChatAction()         {}
func (ChatActionOpenLink) isChatAction()                   {}
func (ChatActionCopyLink) isChatAction()                   {}
func (ChatActionAttach) isChatAction()                     {}
func (ChatActionAttachPaste) isChatAction()                {}
func (ChatActionToggleVoice) isChatAction()                {}
func (ChatActionCycleFocus) isChatAction()                 {}
func (ChatActionQuitDetach) isChatAction()                 {}

// QueuedPrompt is one durable queue entry shown above the composer.
type QueuedPrompt struct {
	id     string
	text   string
	kind   core.QueuedCommandKind
	images []PromptImage
	// attachmentsUnsupported: the durable queue contained an image we cannot
	// safely materialize for terminal editing. Keep the entry visible and
	// refuse an edit instead of silently dropping that content.
	attachmentsUnsupported bool
}

// TurnControlIntent is what Esc does to the running turn.
type TurnControlIntent int

const (
	TurnControlIntentCancel TurnControlIntent = iota
	TurnControlIntentSteer
	TurnControlIntentApplyQueued
)

func (i TurnControlIntent) escapeHint() string {
	switch i {
	case TurnControlIntentSteer:
		return "Esc steers next"
	case TurnControlIntentApplyQueued:
		return "Esc applies queued · Ctrl-Esc interrupts"
	}
	return "Esc interrupts"
}

func (i TurnControlIntent) failureNotice(err string) string {
	action := "Cancellation"
	switch i {
	case TurnControlIntentSteer:
		action = "Steering request"
	case TurnControlIntentApplyQueued:
		action = "Applying queued input"
	}
	return fmt.Sprintf("%s failed: %s", action, err)
}

// SavedChatDraft is the stored composer draft. Drafts saved by older builds
// may also carry `unsent` and `unanswered` fields. They are ignored, so those
// drafts still restore the composer and pending rows.
type SavedChatDraft struct {
	// composer is flattened into the draft object.
	composer PromptPayload
	// pending defaults to empty when absent.
	pending []PendingSubmission
}

// MarshalJSON writes the composer's fields followed by "pending", as serde's
// flatten does.
func (d SavedChatDraft) MarshalJSON() ([]byte, error) {
	composer, err := json.Marshal(d.composer)
	if err != nil {
		return nil, err
	}
	if len(composer) < 2 || composer[0] != '{' || composer[len(composer)-1] != '}' {
		return nil, errors.New("flattened composer did not serialize as a JSON object")
	}
	pending := d.pending
	if pending == nil {
		pending = []PendingSubmission{}
	}
	tail, err := json.Marshal(pending)
	if err != nil {
		return nil, err
	}
	out := append([]byte{}, composer[:len(composer)-1]...)
	if len(composer) > 2 {
		out = append(out, ',')
	}
	out = append(out, `"pending":`...)
	out = append(out, tail...)
	return append(out, '}'), nil
}

// UnmarshalJSON reads the flattened composer from the whole object and
// "pending" when present.
func (d *SavedChatDraft) UnmarshalJSON(data []byte) error {
	var composer PromptPayload
	if err := json.Unmarshal(data, &composer); err != nil {
		return err
	}
	var wire struct {
		Pending []PendingSubmission `json:"pending"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*d = SavedChatDraft{composer: composer, pending: wire.Pending}
	return nil
}

// SubmissionKind says whether a submit is a prompt or a shell command. Both
// can carry the same text, and a shell command is shown and restored with
// its `!` prefix.
type SubmissionKind string

const (
	SubmissionKindPrompt SubmissionKind = "Prompt"
	SubmissionKindShell  SubmissionKind = "Shell"
)

// UnmarshalJSON accepts only the two variant names serde writes.
func (k *SubmissionKind) UnmarshalJSON(data []byte) error {
	var name string
	if err := json.Unmarshal(data, &name); err != nil {
		return err
	}
	switch kind := SubmissionKind(name); kind {
	case SubmissionKindPrompt, SubmissionKindShell:
		*k = kind
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `Prompt` or `Shell`", name)
}

// notSentHeadline is the start of the conversation notice for a submit the
// relay refused.
func (k SubmissionKind) notSentHeadline() string {
	if k == SubmissionKindShell {
		return "Shell command was not sent"
	}
	return "Prompt was not sent"
}

// PlanReviewFollowup is what a plan-review answer still has to do once the
// answer is delivered.
type PlanReviewFollowup struct {
	desiredActive bool
	// control is nil when no plan control follows.
	control PlanControl
	prompt  *string
}

// queueLabel is the label shown above the composer. A queued configuration
// change is marked so it is never mistaken for a prompt waiting to be sent.
func (p *QueuedPrompt) queueLabel() string {
	isPrompt := p.kind == nil || p.kind.IsPrompt()
	switch {
	case isPrompt && p.attachmentsUnsupported:
		return "queued attachments"
	case isPrompt && len(p.images) > 0:
		return "queued image"
	case isPrompt:
		return "queued"
	}
	return "queued config"
}

// ReviewerCounts is reviewer history membership and the open sessions
// displayed on its control.
type ReviewerCounts struct {
	// Total is all reviewer children, including retained history, for
	// navigation.
	Total int
	// Open is reviewer sessions still open, displayed on the navigation
	// control.
	Open int
}

// SessionHeaderIdentity is the stable columns the conversation's title
// shows, snapshotted when the chat opens.
type SessionHeaderIdentity struct {
	// Target is the session-list target label, including the project suffix
	// for bare targets.
	Target string
	// Profile is the profile column from the session list's live-session
	// summary.
	Profile string
	// Title is the display title from the session list, including any user
	// override.
	Title string
	// HarnessKind is the harness the session runs, so the chat can answer
	// harness-specific questions (like whether Codex exposes plan mode)
	// without a recovery context, which the daemon now owns.
	HarnessKind *core.HarnessKind
	// SubagentCount is the direct children owned by this parent session.
	SubagentCount int
	// Reviewers is reviewer children, kept separate from ordinary subagents.
	Reviewers ReviewerCounts
}

// PromptControl is a navigation control on the prompt's bottom border.
type PromptControl int

const (
	PromptControlTasks PromptControl = iota
	PromptControlSubagents
	PromptControlReviewers
)

// ChatSessionContext is the session facts the chat needs to run reviewers
// and keep per-workspace review settings: the config's harness profiles and
// targets, and this session's record (workspace, target, identity).
// Snapshotted when the chat opens and refreshed by the surface when the
// daemon publishes newer state; the recovery observer that used to travel
// with these stayed in the daemon.
type ChatSessionContext struct {
	Config  core.Config
	Session core.SessionRecord
}

// diffstatKey is one scheduled tool diff summary: the tool call and its
// revision.
type diffstatKey struct {
	toolCallID string
	revision   uint64
}

// submissionRender is a submission row's command id and when it was first
// rendered.
type submissionRender struct {
	commandID string
	at        time.Time
}

// revealedAnchor is the revealed reply's start_seq and the newest entry's
// start_seq when the reveal happened.
type revealedAnchor struct {
	replyStartSeq  uint64
	newestStartSeq uint64
}

// configChipArea pairs the config key a chip's click opens with its hitbox.
type configChipArea struct {
	key  string
	area uv.Rectangle
}

// taskDialogControlID is the opaque task id a visible Stop control stands
// for.
type taskDialogControlID struct {
	control BackgroundTaskControl
	id      string
}

// ReviewerPendingElicitation is a reviewer form still waiting: the role that
// asked (nil for the plan reviewer) and its request.
type ReviewerPendingElicitation struct {
	Role    *string
	Request core.ElicitationRequest
}

// ChatState is the conversation view state.
//
// Field note: Rust also has same-named private getters for inputGeneration,
// promptInFlight, reviewConfig, sessionActivity, subagentCount,
// taskDialogOpen, turnReviews and unconvertedPrefix. Go forbids a field and a
// method of the same name, so the plain getters read the field directly here.
type ChatState struct {
	clearContextSupported bool
	promptImagesSupported bool
	sessionID             string
	bundleID              *string
	phase                 core.WorkerPhase
	latestSeq             uint64
	lastCompactionSeq     uint64
	entries               []core.ChatEntry
	earlier               *EarlierMessages
	earlierGeneration     uint64
	pendingDiffstats      []ToolDiffstatRequest
	scheduledDiffstats    map[diffstatKey]struct{}
	// unconvertedPrefix is the leading transcript items that are not
	// converted to entries yet, because a large session opens on its tail
	// and converts the rest off the event loop. Zero whenever the projection
	// is complete.
	unconvertedPrefix int
	// prefixSeam is the last unconverted transcript item: the projection item
	// a pending prefix has to end at to be spliced in front of the tail. Nil
	// whenever the projection is complete. Explicitly loaded history stays
	// available when the live tail slides.
	prefixSeam *core.TranscriptItem
	// transcriptLoading: the session actor has not produced its first relay
	// projection yet. Empty transcripts render a loading marker until that
	// connection attempt either yields a snapshot or fails.
	transcriptLoading bool
	// standby is a composer parked in front of a session that is not attached
	// yet: a Starting/Resuming transition or an in-flight attach. It is the
	// real composer — same key handling, same rendering — but nothing can be
	// sent while no session is live, and no command completion is offered
	// because no session can answer commands.
	standby         bool
	composerForward *core.Unbounded[composerIoUpdate]
	composerRuntime composerRuntime
	input           string
	inputCursor     int
	// inputImages: image bytes belong to tracked marker ranges in the
	// composer text.
	inputImages     []PromptImage
	nextImageNumber uint64
	// submittingImages is moved out of inputImages while the remote submit is
	// in flight, so a failed submission can restore the images alongside
	// their text.
	submittingImages []PromptImage
	// pendingAttachmentMarkers maps background job sequence numbers to their
	// tracked marker. If the user removes a pending marker, the late result
	// has nowhere to land.
	pendingAttachmentMarkers map[uint64]uint64
	// inputGeneration changes whenever the composer text or tracked images
	// change. Clipboard reads capture this value so a slow native read cannot
	// paste into a new draft after the user has continued editing.
	inputGeneration uint64
	// sessionHistory is the stored prompts from this session, oldest-first.
	sessionHistory      []string
	sessionHistoryError *string
	// sessionHistoryGeneration is the latest session-history listing
	// requested; an older answer that arrives after a newer request is
	// dropped.
	sessionHistoryGeneration uint64
	promptHistory            []string
	historyIndex             *int
	historyDraft             string
	historyDraftImages       []PromptImage
	killBuffer               string
	killImages               []PromptImage
	// chainKill is set by Ctrl-K so the next Ctrl-K appends instead of
	// replacing.
	chainKill                   bool
	preferredColumn             *int
	historySearch               *historySearch
	nextHistorySearchGeneration uint64
	composerRequests            []composerRequest
	queuedPrompts               []QueuedPrompt
	pendingSubmissions          []PendingSubmission
	submissionRenders           []submissionRender
	// pendingQueueRemovals: queue entries optimistically moved back into the
	// composer. A relay snapshot can still contain one until its removal
	// command is projected, so keep its identity hidden across those stale
	// snapshots.
	pendingQueueRemovals map[string]struct{}
	// pendingQueueImages: images peeled back from queued prompts while their
	// removal command is still in flight. Keeping them by queue id makes a
	// failed removal exact.
	pendingQueueImages    map[string][]PromptImage
	activeUserShells      []string
	activeAgentTerminals  []core.ActiveAgentTerminal
	claimedAgentTerminals map[string]int64
	elicitation           *elicitationDialog
	// pendingElicitations is the latest primary pending forms, retained while
	// a reviewer form owns the display so the primary can surface immediately
	// when that review request is answered or withdrawn.
	pendingElicitations []core.ElicitationRequest
	// turnReviews is the daemon-owned review projected into prompt status and
	// actions.
	turnReviews []TurnReview
	// reviewProjectionKnown: None becomes authoritative only after the host
	// supplies its first projection.
	reviewProjectionKnown bool
	// reviewConfig is what `[review]` says, mirrored from the config the TUI
	// already drains, so `/review status` and the composer title can report
	// it.
	reviewConfig core.ReviewConfig
	// sessionReview is this session's own review choice, applied over
	// reviewConfig; nil when the session has none.
	sessionReview core.SessionReview
	// reviewUnavailable is why review cannot run for this session, which
	// `/review status` says instead of describing `[review]`.
	reviewUnavailable *string
	// elicitationIsReviewers is whether the dialog on screen belongs to the
	// reviewer rather than the primary, so its answer is routed to the
	// harness that asked.
	elicitationIsReviewers bool
	// elicitationRole is which reviewing role asked the form on screen, when
	// one did.
	elicitationRole    *string
	goalState          core.GoalState
	goalPromptActive   bool
	acpSurface         core.AcpSessionSurface
	planCommandPending bool
	commandChoices     []commandChoice
	modelValues        []core.SessionConfigChoice
	effortValues       []core.SessionConfigChoice
	// policyConfigKeys is the config keys owned by this session's execution
	// policy.
	policyConfigKeys []string
	autocomplete     *autocomplete
	// configPicker is the `/model` / `/effort` value selector, when one is
	// open. It owns the keyboard while it is up, though an arriving
	// elicitation still wins.
	configPicker *configPicker
	anchor       TranscriptAnchor
	// revealLatestAgentOnDraw: on entry, reveal the response advertised by
	// the session list when later tool activity would otherwise push it above
	// the first viewport.
	revealLatestAgentOnDraw bool
	// revealedAnchor is set while the view still rests on the revealed reply.
	// Newer content then returns the view to the tail.
	revealedAnchor      *revealedAnchor
	lastViewportHeight  int
	renderMode          client.TranscriptRenderMode
	toolDisplay         ToolDisplay
	renderCache         TranscriptRenderCache
	transcriptScrollbar TranscriptScrollbarState
	// expandedToolCalls is completed tool calls the user has opened in the
	// Rich transcript. Presentation state is local to this chat and keyed by
	// durable entry.
	expandedToolCalls         map[uint64]struct{}
	toolDetailGeneration      uint64
	pendingToolDetailRequests []core.ChatEntry
	// expandedMailboxEvents is open mailbox events and incoming messages,
	// keyed by durable identity.
	expandedMailboxEvents map[MailboxDisclosureId]struct{}
	// transcriptClickTargets is screen-coordinate targets rebuilt with every
	// transcript frame.
	transcriptClickTargets []TranscriptClickTarget
	// transcriptLinkFrame is where the last transcript frame drew its rows,
	// for resolving a click on a link against the rows that frame painted.
	transcriptLinkFrame *TranscriptLinkFrame
	notices             Notices
	feedback            Notices
	connectionFeedback  *string
	operationFeedback   map[string]string
	deferredFormResults []chatRemoteResult
	conversationNotices []conversationNotice
	// voiceAvailable is whether Codex OAuth credentials and the voice helper
	// are available. The runtime owns discovering this asynchronously; the
	// chat starts disabled until the host reports a successful probe.
	voiceAvailable bool
	voiceActive    bool
	// voiceButtonArea is the last frame's microphone button, which sits on
	// the prompt's upper-left border rather than inside its selectable text
	// surface.
	voiceButtonArea *uv.Rectangle
	voiceForm       Form[VoiceControl]
	// configChipAreas is the last frame's model and effort chips on the
	// prompt's top border.
	configChipAreas []configChipArea
	// promptControlFocus is the embedded background-task control and its
	// dialog.
	promptControlFocus   *PromptControl
	taskDialogOpen       bool
	taskDialogScroll     int
	taskDialogMaxScroll  int
	taskDialogForm       Form[BackgroundTaskControl]
	taskControlArea      *uv.Rectangle
	subagentCount        int
	subagentWorkingCount int
	reviewers            ReviewerCounts
	reviewerControlArea  *uv.Rectangle
	subagentControlArea  *uv.Rectangle
	taskDialogArea       *uv.Rectangle
	// taskDialogControlIDs is the opaque ids represented by the last frame's
	// visible Stop controls. Keeping this separate from row indices prevents
	// a snapshot reorder between mouse-down and mouse-up from stopping a
	// different task.
	taskDialogControlIDs []taskDialogControlID
	// pendingBackgroundStops is task ids for which a stop request has been
	// submitted. A successful provider acknowledgement deliberately leaves
	// the id here until the next operational snapshot removes the task.
	pendingBackgroundStops map[string]struct{}
	promptContentWidth     int
	// headerTarget, headerProfile and headerTitle are the session-list
	// identity snapshotted when the chat opened.
	headerTarget              string
	headerProfile             string
	headerTitle               string
	spinnerStyle              core.SpinnerStyle
	turnStartedAtEpochSeconds *uint64
	detailedActivityClocks    bool
	activityReachable         bool
	// promptInFlight is whether a prompt of ours is in flight. phase also
	// goes Running for a turn the harness started on its own, so cancellation
	// and the composer's cancel hint key on this and on
	// harnessTurnStoppable.
	promptInFlight               bool
	steeringSupported            *bool
	targetedTurnControlSupported bool
	turnControls                 *core.TurnControls
	activePromptID               *string
	steering                     *core.SteeringOperation
	cancellingPromptID           *string
	turnControlSubmitting        bool
	turnControlError             *string
	turnControlTarget            *string
	turnControlAwaitingState     *string
	turnControlDialogOpen        bool
	turnControlDialog            Dialog[Control]
	// sessionActivity is what the session is doing beyond phase: the turn the
	// harness started on its own, and the commands the agent left running.
	sessionActivity client.SessionActivity
	// currentStepStartedAtMs is when the step the agent is on began, so the
	// pane title can age it.
	currentStepStartedAtMs *uint64
	// frameSurfaces is the selectable surfaces, rebuilt by every frame in
	// render order so the selection engine can hit-test the screen the user
	// is looking at.
	frameSurfaces FrameSurfaces
	// footerCommandAreas is the visible host shortcuts, indexed through
	// chords followed by function keys.
	footerCommandAreas []FooterCommandArea
	// transcriptSelection is the row space transcript selections are
	// measured in, re-pinned by every frame the engine is not holding a
	// transcript selection through.
	transcriptSelection *TranscriptSelectionSpace
	// transcriptSelectionInvalid: the frozen row space stopped describing the
	// rows on screen. Read and cleared after each draw; the caller drops the
	// selection.
	transcriptSelectionInvalid bool
	// renderCacheGeneration is bumped whenever the cached rows are dropped
	// wholesale, so a frozen row space can tell that the rows it was pinned
	// against are gone.
	renderCacheGeneration uint64
	lastClockText         *string
	lastAnimationFrame    *Line
}

// NewChatState is ChatState::new: the view of snapshot after replaying
// events.
func NewChatState(snapshot *core.WorkerSnapshot, events []core.SequencedEvent) *ChatState {
	configuration := decodedConfiguration(snapshot.Config)
	goalState, err := core.GoalStateFromConfiguration(configuration)
	if err != nil {
		slog.Warn("invalid projected goal state", "error", err)
		goalState = core.GoalState{}
	}
	promptInFlight := snapshot.ActivePrompt != nil
	state := &ChatState{
		sessionID:                snapshot.SessionID,
		phase:                    snapshot.Phase,
		scheduledDiffstats:       map[diffstatKey]struct{}{},
		composerRuntime:          defaultComposerRuntime(),
		nextImageNumber:          1,
		pendingAttachmentMarkers: map[uint64]uint64{},
		pendingQueueRemovals:     map[string]struct{}{},
		pendingQueueImages:       map[string][]PromptImage{},
		claimedAgentTerminals:    map[string]int64{},
		reviewConfig:             core.ReviewConfig{},
		goalState:                goalState,
		goalPromptActive:         snapshot.ActivePrompt != nil && promptInvokesCommand(snapshot.ActivePrompt.Text, "goal"),
		acpSurface:               core.AcpSessionSurfaceFromConfiguration(configuration),
		commandChoices:           builtinCommandChoices(),
		anchor:                   TranscriptAnchorBottom{},
		revealLatestAgentOnDraw:  true,
		renderMode:               client.TranscriptRenderModeRich,
		toolDisplay:              DefaultToolDisplay(),
		renderCache:              defaultTranscriptRenderCache(),
		transcriptScrollbar:      defaultTranscriptScrollbarState(),
		expandedToolCalls:        map[uint64]struct{}{},
		expandedMailboxEvents:    map[MailboxDisclosureId]struct{}{},
		notices:                  NewNotices(),
		feedback:                 NewNotices(),
		operationFeedback:        map[string]string{},
		voiceForm:                voiceForm(),
		taskDialogForm:           NewForm[BackgroundTaskControl](),
		pendingBackgroundStops:   map[string]struct{}{},
		promptContentWidth:       1,
		spinnerStyle:             core.DefaultSpinnerStyle(),
		activityReachable:        true,
		promptInFlight:           promptInFlight,
		sessionActivity:          client.SessionActivity{PromptInFlight: promptInFlight},
		turnControlDialog:        turnControlDialog(),
		frameSurfaces:            NewFrameSurfaces(),
	}
	state.ApplyEvents(events)
	// Bootstrap replays the full canonical log for transcript projection,
	// while the snapshot is authoritative for the queue at that frontier.
	state.queuedPrompts = make([]QueuedPrompt, 0, len(snapshot.QueuedPrompts))
	for _, prompt := range snapshot.QueuedPrompts {
		state.queuedPrompts = append(state.queuedPrompts, QueuedPrompt{
			id:     prompt.ID,
			text:   prompt.Text,
			kind:   core.QueuedCommandKindPrompt{},
			images: []PromptImage{},
		})
	}
	state.latestSeq = max(state.latestSeq, snapshot.LatestSeq)
	return state
}

// decodedConfiguration is a worker snapshot's configuration as the decoded
// JSON values the projected-configuration readers take. A value that does
// not decode is dropped, as it could not have come from serde.
func decodedConfiguration(config map[string]json.RawMessage) map[string]any {
	configuration := make(map[string]any, len(config))
	for key, raw := range config {
		var value any
		if err := json.Unmarshal(raw, &value); err != nil {
			continue
		}
		configuration[key] = value
	}
	return configuration
}

// ChatStateStandby is the real composer for a session that is not attached
// yet: parked behind a Starting/Resuming transition or an in-flight attach.
// It edits exactly like the attached composer — the whole readline chord set
// — and Enter on a plain prompt clears the input, shows the text as a queued
// preview, and returns ChatActionPrompt so the host can have the daemon
// deliver it once the session is live. A command keeps the draft and
// explains, because no session can answer it yet, and command completion
// stays off for the same reason. The draft survives until the host carries it
// into the attached chat.
func ChatStateStandby(sessionID string, config *core.Config, header SessionHeaderIdentity, notices Notices) *ChatState {
	snapshot := core.WorkerSnapshotSummary(sessionID, core.WorkerPhaseIdle, 0)
	state := NewChatState(&snapshot, nil)
	state.notices = notices
	state.standby = true
	state.BindComposerSession(&sessionID)
	state.SetSubagentCount(header.SubagentCount)
	state.SetReviewerCounts(header.Reviewers)
	state.SetHeaderSummary(header.Target, header.Profile, header.Title)
	if header.HarnessKind != nil {
		state.SetHarnessKind(*header.HarnessKind)
	}
	state.SetReviewConfig(config.Review)
	state.SetSpinnerStyle(config.Spinner)
	state.SetDetailedActivityClocks(config.Advanced.DetailedActivityClocks)
	state.SetToolDisplay(ToolDisplayFromConfig(&config.Advanced))
	return state
}

// SetDraft replaces the draft with draft, cursor at the end. An empty draft
// leaves the composer alone.
func (s *ChatState) SetDraft(draft string) {
	s.restoreDraft(draft)
}

// Draft is the composer's current draft, including any embedded image
// markers.
func (s *ChatState) Draft() string {
	return s.encodedDraft()
}

// DesiredPromptHeight is the rows the composer wants at width: the wrapped
// input, up to three queued-prompt previews with a separating row, and the
// block's borders. It never asks for fewer than three text rows, so a short
// draft still leaves an easy mouse target.
func (s *ChatState) DesiredPromptHeight(width int) int {
	contentWidth := promptContentWidth(width)
	inputRows := InputVisualRows(s.input, contentWidth)
	queued := min(len(s.queuedPrompts), 3)
	rows := inputRows + queued + 2
	if queued > 0 {
		rows++
	}
	return max(rows, 5)
}

// DrawPromptBand draws only the composer band into area, for hosts that show
// the real prompt while the session is not attached. Clears and re-registers
// this band's chat surfaces; the host merges them into its own frame
// surfaces. note adds a left-aligned line to the bottom border.
func (s *ChatState) DrawPromptBand(frame *Frame, area uv.Rectangle, promptFocused bool, note *Line) {
	s.frameSurfaces.Clear()
	s.footerCommandAreas = s.footerCommandAreas[:0]
	s.configChipAreas = s.configChipAreas[:0]
	renderComposerBand(frame, area, s, promptFocused, note)
}

func ChatStateFromTail(sessionID string, phase core.WorkerPhase, latestSeq uint64, entries []core.ChatEntry) *ChatState {
	snapshot := core.WorkerSnapshotSummary(sessionID, phase, latestSeq)
	state := NewChatState(&snapshot, nil)
	state.entries = entries
	return state
}

func ChatStateFromMaterialized(session *core.MaterializedSession, configOptions []schema.SessionConfigOption, availableCommands []schema.AvailableCommand) *ChatState {
	return chatStateFromMaterializedWithPrefix(session, configOptions, availableCommands, 0)
}

// ChatStateFromMaterializedTail is like ChatStateFromMaterialized, but a
// session longer than TailSeedItems converts only its tail here. The caller
// converts the recorded prefix off the event loop and hands it back to
// spliceTranscriptPrefix, so opening a long conversation costs the tail
// rather than the history.
func ChatStateFromMaterializedTail(session *core.MaterializedSession, configOptions []schema.SessionConfigOption, availableCommands []schema.AvailableCommand) *ChatState {
	prefix := max(len(session.Transcript)-client.TailSeedItems, 0)
	return chatStateFromMaterializedWithPrefix(session, configOptions, availableCommands, prefix)
}

func chatStateFromMaterializedWithPrefix(session *core.MaterializedSession, configOptions []schema.SessionConfigOption, availableCommands []schema.AvailableCommand, unconvertedPrefix int) *ChatState {
	snapshot := core.WorkerSnapshotSummary(session.SessionID, materializedPhase(session.Execution), session.AppliedEventOrdinal)
	state := NewChatState(&snapshot, nil)
	state.latestSeq = math.MaxUint64
	state.unconvertedPrefix = unconvertedPrefix
	state.ApplyMaterialized(session, configOptions, availableCommands)
	return state
}

// materializedPhase is the worker phase a materialized execution state shows.
func materializedPhase(execution core.MaterializedExecutionState) core.WorkerPhase {
	switch execution.(type) {
	case core.MaterializedExecutionStateRunning:
		return core.WorkerPhaseRunning
	case core.MaterializedExecutionStateClosing:
		return core.WorkerPhaseClosing
	case core.MaterializedExecutionStateClosed:
		return core.WorkerPhaseClosed
	}
	return core.WorkerPhaseIdle
}

func (s *ChatState) ApplyMaterialized(session *core.MaterializedSession, configOptions []schema.SessionConfigOption, availableCommands []schema.AvailableCommand) {
	goal := core.GoalState{}
	if session.Configuration.Goal != nil {
		goal = *session.Configuration.Goal
	}
	s.applyMaterializedWithGoal(session, configOptions, availableCommands, &goal)
}

func (s *ChatState) applyMaterializedWithGoal(session *core.MaterializedSession, configOptions []schema.SessionConfigOption, availableCommands []schema.AvailableCommand, goal *core.GoalState) {
	switchedSession := s.sessionID != session.SessionID
	if switchedSession {
		s.AdoptSessionID(session.SessionID)
	}
	s.goalState = *goal
	started := time.Now()
	s.reconcileSubmissions(session)
	s.reconcileNotices(session)
	rebuildProjection := switchedSession ||
		session.AppliedEventOrdinal != s.latestSeq ||
		!elicitationRequestsEqual(session.PendingElicitations, s.pendingElicitations)
	if phase := materializedPhase(session.Execution); s.phase != phase {
		s.phase = phase
	}
	turnStartedAt := turnStartedAtEpochSeconds(session.Execution)
	if !optionalUint64Equal(s.turnStartedAtEpochSeconds, turnStartedAt) {
		s.turnStartedAtEpochSeconds = turnStartedAt
	}
	s.latestSeq = session.AppliedEventOrdinal
	s.syncElicitation(session.PendingElicitations)
	if rebuildProjection {
		// Entries can move when the canonical window slides or history is
		// rewritten. Preserve identity, including a deferred restoration.
		// Bottom needs no remapping and keeps its pending opening reveal.
		var position *TranscriptPosition
		if _, bottom := s.anchor.(TranscriptAnchorBottom); !bottom {
			saved := s.TranscriptPosition()
			position = &saved
		}
		// While a prefix conversion is in flight the entries stand for the
		// tail only, so the rebuild has to line up with the same tail.
		// Compaction can shrink the transcript under the recorded prefix;
		// reseat it on the current tail rather than rebuilding the whole
		// history here. The pending prefix then fails its alignment check
		// and is rebuilt off the loop.
		if s.unconvertedPrefix > len(session.Transcript) {
			s.unconvertedPrefix = max(len(session.Transcript)-client.TailSeedItems, 0)
			s.entries = nil
			s.invalidateRenderCache()
		}
		previous := s.entries
		s.entries = nil
		s.entries = client.MaterializedChatEntriesReusing(session, s.unconvertedPrefix, previous)
		if s.hasInlineSubmissions() {
			s.syncInlineSubmissions()
		}
		s.refreshExpandedToolDetails()
		// Reusing entry rows is safe only after the collapse topology is
		// recomputed. A tool can become completed without changing the
		// transcript length, joining or splitting a collapsed streak.
		s.invalidateRenderCache()
		if position != nil {
			s.RestoreTranscriptPosition(*position)
		}
		// Re-read the seam from the projection that produced this tail, so
		// a prefix converted against replaced history is refused.
		s.prefixSeam = nil
		if index := s.unconvertedPrefix - 1; index >= 0 && index < len(session.Transcript) {
			s.prefixSeam = session.Transcript[index]
		}
		if s.unconvertedPrefix < len(session.Transcript) {
			for _, item := range session.Transcript[s.unconvertedPrefix:] {
				request, ok := toolDiffstatRequestFromItem(item)
				if !ok {
					continue
				}
				key := diffstatKey{toolCallID: request.toolCallID, revision: request.revision}
				if _, scheduled := s.scheduledDiffstats[key]; scheduled {
					continue
				}
				s.scheduledDiffstats[key] = struct{}{}
				s.pendingDiffstats = append(s.pendingDiffstats, request)
			}
		}
	}
	// Queue persistence can reach the materialized view independently of
	// transcript projection. Keep the small queue authoritative even when
	// the transcript frontier has not moved and its expensive rebuild is
	// correctly skipped.
	projectedQueueIDs := make(map[string]struct{}, len(session.QueuedPrompts))
	for _, prompt := range session.QueuedPrompts {
		projectedQueueIDs[prompt.CommandID] = struct{}{}
	}
	for id := range s.pendingQueueRemovals {
		if _, projected := projectedQueueIDs[id]; !projected {
			delete(s.pendingQueueRemovals, id)
		}
	}
	for id := range s.pendingQueueImages {
		if _, projected := projectedQueueIDs[id]; !projected {
			delete(s.pendingQueueImages, id)
		}
	}
	queuedPrompts := make([]QueuedPrompt, 0, len(session.QueuedPrompts))
	for _, prompt := range session.QueuedPrompts {
		if _, removing := s.pendingQueueRemovals[prompt.CommandID]; removing {
			continue
		}
		payload, attachmentsUnsupported := materializedContentPrompt(prompt.Content)
		kind := prompt.Kind
		if kind == nil {
			kind = core.QueuedCommandKindPrompt{}
		}
		queuedPrompts = append(queuedPrompts, QueuedPrompt{
			id:                     prompt.CommandID,
			text:                   payload.Text,
			kind:                   kind,
			images:                 payload.Images,
			attachmentsUnsupported: attachmentsUnsupported,
		})
	}
	if !queuedPromptsEqual(s.queuedPrompts, queuedPrompts) {
		s.queuedPrompts = queuedPrompts
	}
	slog.Debug("terminal projection applied",
		"target", "mj_chat::latency",
		"ordinal", session.AppliedEventOrdinal,
		"elapsed_ms", float64(time.Since(started).Microseconds())/1000)
	s.setConfigOptions(configOptions)
	s.acpSurface.ApplyProjectedConfiguration(session.Configuration.Values)
	s.acpSurface.SetAgentCommands(slices.Clone(availableCommands))
	s.rebuildCommandChoices()
}

// elicitationRequestsEqual compares two lists of requests as Rust's derived
// PartialEq does.
func elicitationRequestsEqual(left, right []core.ElicitationRequest) bool {
	return slices.EqualFunc(left, right, func(a, b core.ElicitationRequest) bool { return a.Equal(&b) })
}

// containsElicitationRequest is Vec::contains for requests.
func containsElicitationRequest(requests []core.ElicitationRequest, request *core.ElicitationRequest) bool {
	return slices.ContainsFunc(requests, func(candidate core.ElicitationRequest) bool { return candidate.Equal(request) })
}

func optionalUint64Equal(left, right *uint64) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

func optionalStringEqual(left, right *string) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

// queuedPromptsEqual is Rust's derived PartialEq over the queue.
func queuedPromptsEqual(left, right []QueuedPrompt) bool {
	return slices.EqualFunc(left, right, func(a, b QueuedPrompt) bool {
		return a.id == b.id && a.text == b.text && a.kind == b.kind &&
			a.attachmentsUnsupported == b.attachmentsUnsupported &&
			slices.EqualFunc(a.images, b.images, func(x, y PromptImage) bool { return x.Equal(&y) })
	})
}

func (s *ChatState) takeDiffstatRequests(maximum int) []ToolDiffstatRequest {
	count := min(maximum, len(s.pendingDiffstats))
	taken := slices.Clone(s.pendingDiffstats[:count])
	s.pendingDiffstats = s.pendingDiffstats[count:]
	return taken
}

func (s *ChatState) queueDiffstatRequests(requests []ToolDiffstatRequest) {
	for _, request := range requests {
		key := diffstatKey{toolCallID: request.toolCallID, revision: request.revision}
		if _, scheduled := s.scheduledDiffstats[key]; scheduled {
			continue
		}
		s.scheduledDiffstats[key] = struct{}{}
		s.pendingDiffstats = append(s.pendingDiffstats, request)
	}
}

// applyDiffstats records the diff summary computed for one tool call
// revision, or reports why it could not be computed (err non-nil).
func (s *ChatState) applyDiffstats(toolCallID string, revision uint64, diffstats []string, err error) {
	key := diffstatKey{toolCallID: toolCallID, revision: revision}
	var entry *core.ChatEntry
	for index := len(s.entries) - 1; index >= 0; index-- {
		candidate := &s.entries[index]
		if candidate.ToolCallID != nil && *candidate.ToolCallID == toolCallID && candidate.Revision == revision {
			entry = candidate
			break
		}
	}
	if entry == nil {
		delete(s.scheduledDiffstats, key)
		return
	}
	if err == nil {
		entry.ToolDiffstats = diffstats
		s.invalidateRenderCache()
		return
	}
	delete(s.scheduledDiffstats, key)
	slog.Warn("could not calculate a tool diff summary",
		"tool_call_id", toolCallID, "revision", revision, "error", err)
	s.SetNotice(fmt.Sprintf("Could not calculate diff summary: %v", err))
}

func (s *ChatState) syncElicitation(pending []core.ElicitationRequest) {
	if !elicitationRequestsEqual(s.pendingElicitations, pending) {
		s.pendingElicitations = slices.Clone(pending)
	}
	// A reviewer's form is not in the primary's pending list, so the
	// primary's projection must not take it down.
	if s.elicitationIsReviewers {
		return
	}
	if s.elicitation != nil {
		if containsElicitationRequest(pending, s.elicitation.request()) {
			return
		}
		// An answer or cancellation removed the request. Drop the local
		// form immediately so no later relay snapshot can resurrect it.
		s.elicitation = nil
	}
	var next *elicitationDialog
	if len(pending) > 0 {
		next = newElicitationDialog(pending[0])
	}
	if (next != nil) != (s.elicitation != nil) {
		s.elicitation = next
	}
}

// reconcileReviewerElicitation reconciles the reviewer form against the
// sidecar's latest pending requests. The primary projection intentionally
// cannot do this because reviewer requests are kept in a different stream.
func (s *ChatState) reconcileReviewerElicitation(pending []ReviewerPendingElicitation) {
	if !s.reviewerElicitationOpen() {
		return
	}
	matches := false
	if s.elicitation != nil {
		for _, candidate := range pending {
			if optionalStringEqual(candidate.Role, s.elicitationRole) && candidate.Request.Equal(s.elicitation.request()) {
				matches = true
				break
			}
		}
	}
	if !matches {
		s.elicitation = nil
		s.elicitationIsReviewers = false
		s.elicitationRole = nil
		if len(s.pendingElicitations) > 0 {
			s.elicitation = newElicitationDialog(s.pendingElicitations[0])
		}
	}
}

// showReviewRoleElicitation puts the reviewer's form on screen, remembering
// its role when present. Turn review has one reviewer role; nil identifies
// the plan reviewer.
//
// The primary's own dialog wins the screen: an answer the planning harness is
// blocked on matters more than one its reviewer is.
func (s *ChatState) showReviewRoleElicitation(role *string, request core.ElicitationRequest) bool {
	if s.elicitation != nil {
		return false
	}
	s.elicitationIsReviewers = true
	s.elicitationRole = role
	s.elicitation = newElicitationDialog(request)
	return true
}

// reviewerElicitationOpen is whether a reviewer's form is currently on
// screen.
func (s *ChatState) reviewerElicitationOpen() bool {
	return s.elicitationIsReviewers && s.elicitation != nil
}

func (s *ChatState) elicitationIdentity() *ElicitationIdentity {
	if s.elicitation == nil {
		return nil
	}
	return &ElicitationIdentity{
		request:      *s.elicitation.request(),
		reviewer:     s.elicitationIsReviewers,
		reviewerRole: s.elicitationRole,
	}
}

func (s *ChatState) elicitationSource() (bool, *string) {
	return s.elicitationIsReviewers, s.elicitationRole
}

func (s *ChatState) pendingReviewerMatches(draft *ChatElicitationDraft) bool {
	if !draft.reviewer {
		return false
	}
	role, hasRole := draft.ReviewerRole()
	for index := range s.turnReviews {
		for _, pending := range s.turnReviews[index].pendingElicitations() {
			if hasRole && role == pending.Role && pending.Request.Equal(draft.request()) {
				return true
			}
		}
	}
	return false
}

// ElicitationDrafts is the form recovery snapshots owned by this composer,
// including parked results. Take one before this chat is replaced by another
// session; the snapshot carries reviewer routing metadata as well as the form
// values, because those answers do not all go to the primary agent.
func (s *ChatState) ElicitationDrafts() []ChatElicitationDraft {
	var drafts []ChatElicitationDraft
	if draft := s.elicitationDraft(); draft != nil {
		drafts = append(drafts, *draft)
	}
	return append(drafts, s.deferredFormResultDrafts()...)
}

func (s *ChatState) elicitationDraft() *ChatElicitationDraft {
	if s.elicitation == nil {
		return nil
	}
	return &ChatElicitationDraft{
		form:         s.elicitation.draft(),
		reviewer:     s.elicitationIsReviewers,
		reviewerRole: s.elicitationRole,
	}
}

// restoreElicitationDraft restores a matching snapshot onto the currently
// pending request. A request must already be present in the newly attached
// projection; this prevents a late attach or an externally answered form from
// reviving a stale draft.
func (s *ChatState) restoreElicitationDraft(draft ChatElicitationDraft) bool {
	if s.elicitation == nil {
		return false
	}
	request := *s.elicitation.request()
	reviewer, role := s.elicitationSource()
	if reviewer != draft.reviewer || !optionalStringEqual(role, draft.reviewerRole) {
		return false
	}
	dialog := elicitationDialogFromDraft(request, draft.form)
	if dialog == nil {
		return false
	}
	s.elicitation = dialog
	s.elicitationIsReviewers = draft.reviewer
	s.elicitationRole = draft.reviewerRole
	return true
}

func (s *ChatState) restoreFailedReviewerElicitation(draft ChatElicitationDraft, err string) bool {
	if !s.pendingReviewerMatches(&draft) {
		return true
	}
	current := s.elicitationIdentity()
	matches := current != nil && current.equal(draft.identity())
	if s.elicitation != nil && !matches {
		// Another form owns focus. Keep the whole result with its draft,
		// rather than losing it or replacing the visible question.
		parked := draft
		s.deferredFormResults = append(s.deferredFormResults, chatRemoteResultReviewerAnswer{
			Draft: &parked,
			Err:   errors.New(err),
		})
		return false
	}
	if s.elicitation == nil {
		s.showReviewRoleElicitation(draft.reviewerRole, *draft.request())
	}
	s.restoreElicitationDraft(draft)
	if s.elicitation != nil {
		s.elicitation.setResponseError("Answer was not sent: " + err)
	}
	return true
}

func (s *ChatState) restoreFailedElicitation(request core.ElicitationRequest, err string) {
	if !containsElicitationRequest(s.pendingElicitations, &request) {
		return
	}
	s.restoreElicitation(request)
	if reviewer, role := s.elicitationSource(); !reviewer && role == nil &&
		s.elicitation != nil && s.elicitation.request().Equal(&request) {
		s.elicitation.setResponseError(err)
	}
}

func (s *ChatState) restoreElicitation(request core.ElicitationRequest) {
	if s.elicitation == nil {
		s.elicitation = newElicitationDialog(request)
	}
}

// boundedEntries is the tail of the entries bounded for the dashboard. Only
// tests use this.
func (s *ChatState) boundedEntries(maximumEntries, maximumBytes int) []core.ChatEntry {
	start := max(len(s.entries)-maximumEntries, 0)
	entries := make([]core.ChatEntry, 0, len(s.entries)-start)
	for _, entry := range s.entries[start:] {
		entries = append(entries, entry.BoundedForDashboard())
	}
	for len(entries) > 1 {
		body, err := json.Marshal(entries)
		if err != nil || len(body) <= maximumBytes {
			break
		}
		entries = entries[1:]
	}
	return entries
}

func planControlErrorMessage(err core.PlanControlError) string {
	switch err {
	case core.PlanControlErrorCodexIncompatible:
		return "This Codex ACP version does not expose collaboration_mode with plan/default values."
	case core.PlanControlErrorGrokIncompatible:
		return "This Grok Build version does not expose compatible plan/default modes."
	}
	return "This ACP harness does not expose compatible plan/default modes."
}

func isCompactionArtifact(payload json.RawMessage) bool {
	var object map[string]json.RawMessage
	_ = json.Unmarshal(payload, &object)
	update := object
	if inner, ok := object["update"]; ok {
		update = nil
		_ = json.Unmarshal(inner, &update)
	}
	var kind string
	if raw, ok := update["sessionUpdate"]; ok && json.Unmarshal(raw, &kind) == nil {
		switch kind {
		case "compaction", "context_compaction", "compaction_summary":
			return true
		}
	}
	_, encrypted := update["encrypted_content"]
	_, encryptedCamel := update["encryptedContent"]
	return encrypted || encryptedCamel
}

// normalizeKey folds raw control characters and uppercase letters into the
// key and modifiers the bindings match on.
//
// Only character keys are folded. Bubble Tea gives Tab, Enter, Escape and
// Backspace the control codes crossterm reports as their own KeyCodes, so
// those are not characters here.
func normalizeKey(code rune, modifiers tea.KeyMod) (rune, tea.KeyMod) {
	if !keyCodeIsCharacter(code) {
		return code, modifiers
	}
	if modifiers == 0 && code >= 1 && code <= 26 {
		return code - 1 + 'a', modifiers | tea.ModCtrl
	}
	if code >= 'A' && code <= 'Z' {
		modifiers |= tea.ModShift
		if modifiers&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) != 0 {
			return code - 'A' + 'a', modifiers
		}
	}
	return code, modifiers
}

// keyCodeIsCharacter is whether a Bubble Tea key code is crossterm's
// KeyCode::Char rather than a named key.
func keyCodeIsCharacter(code rune) bool {
	switch code {
	case tea.KeyTab, tea.KeyEnter, tea.KeyEscape, tea.KeyBackspace:
		return false
	}
	return code >= 0 && code < tea.KeyExtended
}

func queuedPromptPreview(prompt string) string {
	const width = 72
	collapsed := strings.Join(strings.Fields(prompt), " ")
	if utf8.RuneCountInString(collapsed) <= width {
		return collapsed
	}
	return string([]rune(collapsed)[:width-1]) + "…"
}

func materializedContentPrompt(content []any) (PromptPayload, bool) {
	payload := PromptPayloadText("")
	unsupported := false
	for _, value := range content {
		var block unstable.ContentBlock
		encoded, err := json.Marshal(value)
		if err == nil {
			err = json.Unmarshal(encoded, &block)
		}
		switch {
		case err == nil && block.Image != nil:
			var image ClipboardImage
			reference, referenceErr := core.ImageReference(block.Image)
			switch {
			case referenceErr != nil:
				err = referenceErr
			case reference != nil:
				image, err = ClipboardImageFromReference(*reference)
			default:
				image, err = ClipboardImageFromBase64(block.Image.Data, block.Image.MimeType)
			}
			if err != nil {
				unsupported = true
				continue
			}
			number := uint64(len(payload.Images)) + 1
			insertImage(&payload.Text, &payload.Images, len(payload.Text), number, image)
		case err == nil && block.Text != nil:
			payload.Text += block.Text.Text
		default:
			if text, ok := value.(string); ok {
				payload.Text += text
			} else {
				unsupported = true
			}
		}
	}
	return payload, unsupported
}

func promptContentBlocks(text string, images []PromptImage) []any {
	payload := PromptPayload{Text: text, Images: slices.Clone(images)}
	blocks := payload.ContentBlocks()
	values := make([]any, 0, len(blocks))
	for _, block := range blocks {
		encoded, err := json.Marshal(block)
		if err != nil {
			panic(fmt.Sprintf("ACP content serializes: %v", err))
		}
		var value any
		if err := json.Unmarshal(encoded, &value); err != nil {
			panic(fmt.Sprintf("ACP content serializes: %v", err))
		}
		values = append(values, value)
	}
	return values
}

// NoticeMinimumDisplay is how long a notice is guaranteed on screen before an
// unrelated key press may dismiss it. Background failures report through
// this bar and nowhere else, so a keystroke that races one must not wipe it
// unread.
const NoticeMinimumDisplay = 4 * time.Second

// Notice is the notice on the bar.
type Notice struct {
	text      string
	setAt     time.Time
	protected bool
}

// NoticeHistory is how many past notices the log keeps.
const NoticeHistory = 30

// NoticeRecord is one notice as the log remembers it.
type NoticeRecord struct {
	Text string
	At   time.Time
	// Failure is whether it was a failure, which the log draws in the warning
	// color.
	Failure bool
}

// NoticeSlot is the shared state behind Notices.
type NoticeSlot struct {
	notice            *Notice
	persistentFailure *string
	// generation is bumped when the displayed text changes, so a dirty-gated
	// renderer can tell that the bar moved without keeping a copy of its
	// text.
	generation uint64
	// history is the last NoticeHistory notices, oldest first, so a burst of
	// background failures that overwrote each other can still be read.
	history []NoticeRecord
	// stackedFailures is failures that arrived while an earlier failure was
	// still protected on the bar; the bar names their count until it is
	// cleared or dismissed.
	stackedFailures int
}

func (s *NoticeSlot) write(notice *Notice) {
	if notice != nil && (len(s.history) == 0 || s.history[len(s.history)-1].Text != notice.text) {
		s.history = append(s.history, NoticeRecord{
			Text:    notice.text,
			At:      notice.setAt,
			Failure: notice.protected,
		})
		if excess := len(s.history) - NoticeHistory; excess > 0 {
			s.history = slices.Delete(s.history, 0, excess)
		}
	}
	if notice == nil {
		s.stackedFailures = 0
	}
	var currentText, nextText *string
	if s.notice != nil {
		currentText = &s.notice.text
	}
	if notice != nil {
		nextText = &notice.text
	}
	displayedTextChanged := !optionalStringEqual(currentText, nextText)
	s.notice = notice
	if displayedTextChanged {
		s.generation++
	}
}

// Notices is the one-line notifications bar shared by every view. Copies
// share the same underlying slot; the latest notice wins and a clear in one
// view clears it for all. Make one with NewNotices (Rust's Default).
//
// Each notice carries the time it was set. That is what lets an incidental
// key press dismiss a notice the user has had a chance to read while leaving
// a fresh one standing.
type Notices struct {
	shared *noticesShared
}

type noticesShared struct {
	mu   sync.Mutex
	slot NoticeSlot
}

// NewNotices is a fresh, empty notifications bar.
func NewNotices() Notices {
	return Notices{shared: &noticesShared{}}
}

// Set sets the notice, replacing whatever is showing. Sanitizes the text so
// escape sequences or stray carriage returns from background work cannot
// corrupt the footer row.
func (n Notices) Set(notice string) {
	text := core.SanitizeTerminalText(notice)
	slot, unlock := n.lock()
	defer unlock()
	if slot.notice != nil && slot.notice.protected && time.Since(slot.notice.setAt) < NoticeMinimumDisplay {
		return
	}
	slot.write(&Notice{text: text, setAt: time.Now(), protected: false})
}

// SetFailure sets a failure notice that routine background updates cannot
// replace before it has been readable for NoticeMinimumDisplay. A newer
// failure still replaces it immediately.
func (n Notices) SetFailure(notice string) {
	text := core.SanitizeTerminalText(notice)
	slot, unlock := n.lock()
	defer unlock()
	// A failure landing on a failure the person has not had time to read:
	// keep the newest visible and say how many there were.
	stacking := slot.notice != nil && slot.notice.protected &&
		time.Since(slot.notice.setAt) < NoticeMinimumDisplay && slot.notice.text != text
	if stacking {
		slot.stackedFailures = max(slot.stackedFailures, 1) + 1
	} else {
		slot.stackedFailures = 1
	}
	stacked := slot.stackedFailures
	slot.write(&Notice{text: text, setAt: time.Now(), protected: true})
	if stacked > 1 && slot.notice != nil {
		// The log keeps the plain text; only the bar carries the count.
		slot.notice.text = fmt.Sprintf("%d failures · latest: %s", stacked, text)
		slot.generation++
	}
}

// History is the notices seen so far, newest first, for the log overlay.
func (n Notices) History() []NoticeRecord {
	slot, unlock := n.lock()
	defer unlock()
	history := make([]NoticeRecord, 0, len(slot.history))
	for index := len(slot.history) - 1; index >= 0; index-- {
		history = append(history, slot.history[index])
	}
	return history
}

// ReplaceIf replaces the notice only if it still reads expected, so a
// background task can upgrade its own "in progress" notice to a result
// without clobbering whatever replaced it in the meantime. Returns whether
// the replacement happened. The replacement is a new report, so it starts its
// own display period.
func (n Notices) ReplaceIf(expected string, replacement string) bool {
	slot, unlock := n.lock()
	defer unlock()
	if slot.notice == nil || slot.notice.text != expected {
		return false
	}
	text := core.SanitizeTerminalText(replacement)
	slot.write(&Notice{text: text, setAt: time.Now(), protected: false})
	return true
}

// Clear clears the notice everywhere it is shown, however recent it is. This
// is for callers that know the notice no longer applies; a key press that
// merely happened to arrive uses Dismiss.
func (n Notices) Clear() {
	slot, unlock := n.lock()
	defer unlock()
	if slot.notice != nil {
		slot.write(nil)
	}
}

// Dismiss clears the notice if it has been showing for at least
// NoticeMinimumDisplay at now. Returns whether the bar is clear afterwards,
// so a caller can tell a survivor from a dismissal.
func (n Notices) Dismiss(now time.Time) bool {
	slot, unlock := n.lock()
	defer unlock()
	if slot.persistentFailure != nil {
		return false
	}
	if slot.notice == nil {
		return true
	}
	if now.Sub(slot.notice.setAt) < NoticeMinimumDisplay {
		return false
	}
	slot.write(nil)
	return true
}

// Current is the current notice, if any.
func (n Notices) Current() (string, bool) {
	slot, unlock := n.lock()
	defer unlock()
	if slot.persistentFailure != nil {
		return *slot.persistentFailure, true
	}
	if slot.notice != nil {
		return slot.notice.text, true
	}
	return "", false
}

// SetPersistentFailure keeps a continuing feed failure visible until its
// owner reports recovery (nil). Transient notices remain available after
// recovery and in the notice log.
func (n Notices) SetPersistentFailure(err *string) {
	var failure *string
	if err != nil {
		text := core.SanitizeTerminalText(*err)
		failure = &text
	}
	slot, unlock := n.lock()
	defer unlock()
	if optionalStringEqual(slot.persistentFailure, failure) {
		return
	}
	if failure != nil {
		slot.history = append(slot.history, NoticeRecord{Text: *failure, At: time.Now(), Failure: true})
		if excess := len(slot.history) - NoticeHistory; excess > 0 {
			slot.history = slices.Delete(slot.history, 0, excess)
		}
	}
	slot.persistentFailure = failure
	slot.generation++
}

// Generation counts displayed-text changes to the shared slot. A renderer
// that records this with each frame can tell that the bar changed since it
// last drew, which is what keeps a notice set by background work from being
// missed by a dirty-gated draw.
func (n Notices) Generation() uint64 {
	slot, unlock := n.lock()
	defer unlock()
	return slot.generation
}

func (n Notices) lock() (*NoticeSlot, func()) {
	n.shared.mu.Lock()
	return &n.shared.slot, n.shared.mu.Unlock
}

// TurnBandColor: active work and ready sessions share the terminal's
// semantic palette.
func TurnBandColor(turnInFlight bool) Color {
	if turnInFlight {
		return ThemePalette().Accent
	}
	return ThemePalette().Success
}

// turnStartedAtEpochSeconds is when the session's current turn started, in
// epoch seconds. Nil means no turn is in flight.
func turnStartedAtEpochSeconds(execution core.MaterializedExecutionState) *uint64 {
	running, ok := execution.(core.MaterializedExecutionStateRunning)
	if !ok || running.StartedAtMs < 0 {
		return nil
	}
	seconds := uint64(running.StartedAtMs) / 1_000
	return &seconds
}
