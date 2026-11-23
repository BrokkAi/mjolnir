// Port of mj-chat/src/chat/composer.rs.

// Clipboard and image jobs belong to the live composer, independently of its
// chat feed.

package chat

import (
	"context"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const maxAttachmentTasks = 2

type attachmentResult struct {
	sequence uint64
	fallback attachmentFallback
	// result is the installed image when err is nil.
	result ClipboardImage
	err    error
}

type attachmentSource interface {
	isAttachmentSource()
}

type attachmentSourceClipboard ClipboardImage

type attachmentSourcePath struct {
	path PastedPath
}

func (attachmentSourceClipboard) isAttachmentSource() {}
func (attachmentSourcePath) isAttachmentSource()      {}

// composerIoUpdate is a background result delivered to the composer. A
// Rust `Result<T, String>` is carried as the value plus err.
type composerIoUpdate interface {
	isComposerIoUpdate()
}

type composerIoUpdateClipboard struct {
	generation uint64
	target     clipboardTarget
	staging    bool
	result     ClipboardContent
	err        error
}

type composerIoUpdateSessionHistoryListed struct {
	generation uint64
	result     []core.PromptHistoryEntry
	err        error
}

type composerIoUpdateHistorySearchResults struct {
	generation uint64
	result     core.PromptHistoryPage
	err        error
}

type composerIoUpdateAttachmentFinished attachmentResult

type composerIoUpdateReceiptPrepared struct {
	commandID string
	result    PromptPayload
	err       error
}

type composerIoUpdateRemote struct {
	result chatRemoteResult
}

func (composerIoUpdateClipboard) isComposerIoUpdate()            {}
func (composerIoUpdateSessionHistoryListed) isComposerIoUpdate() {}
func (composerIoUpdateHistorySearchResults) isComposerIoUpdate() {}
func (composerIoUpdateAttachmentFinished) isComposerIoUpdate()   {}
func (composerIoUpdateReceiptPrepared) isComposerIoUpdate()      {}
func (composerIoUpdateRemote) isComposerIoUpdate()               {}

// remoteResultForwarder owns a task that forwards results to the composer.
// Rust aborts the task when the forwarder is dropped; Go has no drop, so
// every owner that discards one calls drop.
type remoteResultForwarder struct {
	task   *core.JoinHandle[struct{}]
	cancel context.CancelFunc
}

// spawnRemoteResultForwarder starts work as a forwarder's task; its context
// ends when the forwarder is dropped.
func spawnRemoteResultForwarder(work func(ctx context.Context)) *remoteResultForwarder {
	ctx, cancel := context.WithCancel(context.Background())
	task := core.Spawn(func() struct{} {
		work(ctx)
		return struct{}{}
	})
	return &remoteResultForwarder{task: task, cancel: cancel}
}

func (f *remoteResultForwarder) isFinished() bool {
	if f == nil || f.task == nil {
		return true
	}
	select {
	case <-f.task.Done():
		return true
	default:
		return false
	}
}

// drop is Rust's Drop: abort the task if it is still running and report a
// failure other than the cancellation.
func (f *remoteResultForwarder) drop() {
	panic("unported: mj-chat/src/chat/composer.rs drop")
}

type promptOrigin interface {
	isPromptOrigin()
}

type promptOriginOrdinary struct{}

// promptOriginPreparingMove carries the Move preparation's generation.
type promptOriginPreparingMove uint64

// promptOriginMove carries the Move operation's stable id.
type promptOriginMove string

func (promptOriginOrdinary) isPromptOrigin()      {}
func (promptOriginPreparingMove) isPromptOrigin() {}
func (promptOriginMove) isPromptOrigin()          {}

type deferredSubmission struct {
	origin promptOrigin
}

// queuedAttachment is one attachment job waiting for a task slot.
type queuedAttachment struct {
	sequence uint64
	source   attachmentSource
	fallback attachmentFallback
}

type composerRuntime struct {
	// tx and rx are the two ends of one unbounded channel.
	tx                      *core.Unbounded[composerIoUpdate]
	rx                      *core.Unbounded[composerIoUpdate]
	pasteInFlight           bool
	receiptPreparations     map[string]*remoteResultForwarder
	preparedReceiptRetries  []string
	legacyDraftImages       map[uint64]ClipboardImage
	attachmentQueue         []queuedAttachment
	attachmentTasksInFlight int
	nextAttachmentSequence  uint64
	attachmentCancellations map[uint64]context.CancelFunc
	sessionID               *string
	acceptingStaged         bool
	submitWhenReady         *deferredSubmission
	origin                  promptOrigin
	previewOrigins          map[string]*string
	preparedStartup         []StartupPrompt
	remoteStreams           []*remoteResultForwarder
	historyTask             *remoteResultForwarder
	sessionHistoryTask      *remoteResultForwarder
}

// defaultComposerRuntime is Rust's `ComposerRuntime::default()`.
func defaultComposerRuntime() composerRuntime {
	updates := core.NewUnbounded[composerIoUpdate]()
	return composerRuntime{
		tx:                      updates,
		rx:                      updates,
		receiptPreparations:     map[string]*remoteResultForwarder{},
		legacyDraftImages:       map[uint64]ClipboardImage{},
		attachmentCancellations: map[uint64]context.CancelFunc{},
		origin:                  promptOriginOrdinary{},
		previewOrigins:          map[string]*string{},
	}
}

// StartupPrompt is a queued startup prompt; it retains its stable identity
// and image-preserving draft.
type StartupPrompt struct {
	CommandID       string
	Payload         PromptPayload
	MoveOperationID *string
}

// Equal is Rust's derived PartialEq.
func (p *StartupPrompt) Equal(other *StartupPrompt) bool {
	if p.CommandID != other.CommandID || !p.Payload.Equal(&other.Payload) {
		return false
	}
	if p.MoveOperationID == nil || other.MoveOperationID == nil {
		return p.MoveOperationID == nil && other.MoveOperationID == nil
	}
	return *p.MoveOperationID == *other.MoveOperationID
}

// submissionRender is one pending submission's first render: its command id
// and when the submit started (Rust's `(String, Instant)`).
type submissionRender struct {
	commandID string
	started   time.Time
}

// LiveComposer is an in-memory handoff only: pending jobs and their
// destination travel with the draft.
type LiveComposer struct {
	input                       string
	inputCursor                 int
	inputImages                 []PromptImage
	nextImageNumber             uint64
	submittingImages            []PromptImage
	pendingAttachmentMarkers    map[uint64]uint64
	inputGeneration             uint64
	promptHistory               []string
	historyIndex                *int
	historyDraft                string
	historyDraftImages          []PromptImage
	killBuffer                  string
	killImages                  []PromptImage
	chainKill                   bool
	preferredColumn             *int
	queuedPrompts               []queuedPrompt
	pendingQueueRemovals        map[string]struct{}
	pendingQueueImages          map[string][]PromptImage
	historySearch               *historySearch
	nextHistorySearchGeneration uint64
	composerRequests            []composerRequest
	sessionHistory              []string
	sessionHistoryError         *string
	sessionHistoryGeneration    uint64
	pendingSubmissions          []pendingSubmission
	submissionRenders           []submissionRender
	planCommandPending          bool
	operationFeedback           map[string]string
	deferredFormResults         []chatRemoteResult
	conversationNotices         []conversationNotice
	composerRuntime             composerRuntime
}

// ComposerMoveOrigin is the Move a composer's prompts belong to (Rust's
// `(u64, Option<String>)`): ID is nil while Move preparation has not
// supplied its stable identity yet.
type ComposerMoveOrigin struct {
	Generation uint64
	ID         *string
}

// RestoreStartupPrompt recovers refused content without resetting newer
// pending image jobs.
func (c *ChatState) RestoreStartupPrompt(prompt StartupPrompt) error {
	panic("unported: mj-chat/src/chat/composer.rs RestoreStartupPrompt")
}

func (c *ChatState) SettleStartupPreview(commandID string) {
	panic("unported: mj-chat/src/chat/composer.rs SettleStartupPreview")
}

func (c *ChatState) RecalledStartupPrompt(commandID string, text string) StartupPrompt {
	panic("unported: mj-chat/src/chat/composer.rs RecalledStartupPrompt")
}

// StartupPromptWithdrawn settles a withdrawal: withdrawn is the daemon's
// answer when err is nil.
func (c *ChatState) StartupPromptWithdrawn(prompt StartupPrompt, withdrawn bool, err error) (bool, error) {
	panic("unported: mj-chat/src/chat/composer.rs StartupPromptWithdrawn")
}

func (c *ChatState) StartupPrompts() []StartupPrompt {
	panic("unported: mj-chat/src/chat/composer.rs StartupPrompts")
}

func (c *ChatState) LatestStartupPrompt() (StartupPrompt, bool) {
	prompts := c.StartupPrompts()
	if len(prompts) == 0 {
		return StartupPrompt{}, false
	}
	return prompts[len(prompts)-1], true
}

// HasLiveComposer: a parked worker view forwards results to the composer
// which took its state.
func (c *ChatState) HasLiveComposer() bool {
	return c.composerForward == nil
}

func (c *ChatState) TakeLiveComposer() *LiveComposer {
	panic("unported: mj-chat/src/chat/composer.rs TakeLiveComposer")
}

func (c *ChatState) AdoptLiveComposer(composer *LiveComposer) {
	panic("unported: mj-chat/src/chat/composer.rs AdoptLiveComposer")
}

// SetComposerMoveOrigin sets the Move this composer's prompts belong to; nil
// is an ordinary composer.
func (c *ChatState) SetComposerMoveOrigin(origin *ComposerMoveOrigin) {
	panic("unported: mj-chat/src/chat/composer.rs SetComposerMoveOrigin")
}

func (c *ChatState) SettleDeferredMoveOrigin(generation uint64, id *string, succeeded bool) {
	panic("unported: mj-chat/src/chat/composer.rs SettleDeferredMoveOrigin")
}

func (c *ChatState) deferStartupSubmission() {
	c.composerRuntime.submitWhenReady = &deferredSubmission{origin: c.composerRuntime.origin}
}

// BindComposerSession binds the attachment store: a launch has none until
// registration supplies the real ID.
func (c *ChatState) BindComposerSession(sessionID *string) {
	if sessionID == nil {
		c.composerRuntime.sessionID = nil
	} else {
		owned := *sessionID
		c.composerRuntime.sessionID = &owned
	}
	c.pumpAttachmentQueue()
}

func (c *ChatState) HandleComposerPaste(pasted string) {
	action := c.handleTerminalPaste(pasted)
	c.DispatchComposerAction(action)
}

func (c *ChatState) DispatchComposerAction(action ChatAction) {
	panic("unported: mj-chat/src/chat/composer.rs DispatchComposerAction")
}

func (c *ChatState) readClipboard() {
	panic("unported: mj-chat/src/chat/composer.rs readClipboard")
}

// KeepDeferredSubmissionAsDraft: returning to a retained worker must not send
// a prompt intended for its replacement.
func (c *ChatState) KeepDeferredSubmissionAsDraft() {
	if c.composerRuntime.submitWhenReady != nil {
		c.composerRuntime.submitWhenReady = nil
		c.SetNotice("The operation ended before the prompt was ready; the draft is kept.")
	}
}

func (c *ChatState) TakePreparedStartupPrompts() []StartupPrompt {
	prompts := c.composerRuntime.preparedStartup
	c.composerRuntime.preparedStartup = nil
	return prompts
}

func (c *ChatState) HasComposerUpdates() bool {
	panic("unported: mj-chat/src/chat/composer.rs HasComposerUpdates")
}

func (c *ChatState) DrainComposerUpdates() {
	panic("unported: mj-chat/src/chat/composer.rs DrainComposerUpdates")
}

func (c *ChatState) DrainComposer() (StartupPrompt, bool) {
	panic("unported: mj-chat/src/chat/composer.rs DrainComposer")
}

func (c *ChatState) applyComposerUpdate(update composerIoUpdate) {
	panic("unported: mj-chat/src/chat/composer.rs applyComposerUpdate")
}

// queueAttachment reserves a marker at the cursor and reads the attachment
// behind it. False when no marker could be reserved; the notice says why.
func (c *ChatState) queueAttachment(source attachmentSource, fallback attachmentFallback) bool {
	sequence := c.composerRuntime.nextAttachmentSequence
	if !c.reserveAttachment(sequence) {
		return false
	}
	c.composerRuntime.nextAttachmentSequence++
	c.composerRuntime.attachmentQueue = append(c.composerRuntime.attachmentQueue,
		queuedAttachment{sequence: sequence, source: source, fallback: fallback})
	c.pumpAttachmentQueue()
	return true
}

func (c *ChatState) pumpAttachmentQueue() {
	panic("unported: mj-chat/src/chat/composer.rs pumpAttachmentQueue")
}

func (c *ChatState) applyAttachmentResult(result attachmentResult) {
	c.composerRuntime.attachmentTasksInFlight = max(c.composerRuntime.attachmentTasksInFlight-1, 0)
	delete(c.composerRuntime.attachmentCancellations, result.sequence)
	c.finishAttachment(result.sequence, result.result, result.err, result.fallback)
}

// prepareAttachment is the shared codec and daemon-install path for composer
// markers and old receipts.
func prepareAttachment(ctx context.Context, sessionID string, source attachmentSource) (ClipboardImage, error) {
	panic("unported: mj-chat/src/chat/composer.rs prepareAttachment")
}
