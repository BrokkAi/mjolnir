// Port of mj-chat/src/chat/composer.rs.

// Clipboard and image jobs belong to the live composer, independently of its
// chat feed.

package chat

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"

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
	if f == nil || f.task == nil {
		return
	}
	task := f.task
	f.task = nil
	if f.cancel != nil {
		f.cancel()
	}
	go func() {
		if _, err := task.Join(context.Background()); err != nil {
			slog.Error("native result forwarding task failed", "error", err)
		}
	}()
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

// drop is Rust's Drop for a discarded runtime: its forwarding tasks abort
// and its attachment jobs are cancelled.
func (r *composerRuntime) drop() {
	for _, forwarder := range r.receiptPreparations {
		forwarder.drop()
	}
	clear(r.receiptPreparations)
	for _, cancel := range r.attachmentCancellations {
		cancel()
	}
	clear(r.attachmentCancellations)
	for _, stream := range r.remoteStreams {
		stream.drop()
	}
	r.remoteStreams = nil
	r.historyTask.drop()
	r.historyTask = nil
	r.sessionHistoryTask.drop()
	r.sessionHistoryTask = nil
	// Dropping the receiver closes the channel, so late results are refused
	// rather than queued for nobody.
	if r.rx != nil {
		r.rx.Close()
	}
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
	queuedPrompts               []QueuedPrompt
	pendingQueueRemovals        map[string]struct{}
	pendingQueueImages          map[string][]PromptImage
	historySearch               *historySearch
	nextHistorySearchGeneration uint64
	composerRequests            []composerRequest
	sessionHistory              []string
	sessionHistoryError         *string
	sessionHistoryGeneration    uint64
	pendingSubmissions          []PendingSubmission
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
	merged, err := core.MergeDrafts(c.Draft(), prompt.Payload.EncodeDraft())
	if err != nil {
		return err
	}
	payload, err := PromptPayloadDecodeDraft(merged)
	if err != nil {
		return err
	}
	wasEmpty := c.input == ""
	c.input = payload.Text
	c.inputImages = payload.Images
	if wasEmpty {
		c.inputCursor = len(c.input)
	}
	nextNumber := uint64(1)
	for _, image := range c.inputImages {
		nextNumber = max(nextNumber, saturatingAddOne(image.Number))
	}
	c.nextImageNumber = max(c.nextImageNumber, nextNumber)
	c.inputGeneration++
	c.composerRuntime.submitWhenReady = nil
	c.queuedPrompts = slices.DeleteFunc(c.queuedPrompts, func(queued QueuedPrompt) bool {
		return queued.id == prompt.CommandID
	})
	c.settleStartupReceipt(prompt.CommandID)
	c.updateAutocomplete()
	return nil
}

func (c *ChatState) SettleStartupPreview(commandID string) {
	c.settleStartupReceipt(commandID)
	c.queuedPrompts = slices.DeleteFunc(c.queuedPrompts, func(queued QueuedPrompt) bool {
		return queued.id == commandID
	})
	delete(c.pendingQueueRemovals, commandID)
	delete(c.pendingQueueImages, commandID)
	delete(c.composerRuntime.previewOrigins, commandID)
}

func (c *ChatState) RecalledStartupPrompt(commandID string, text string) StartupPrompt {
	images := append([]PromptImage(nil), c.pendingQueueImages[commandID]...)
	if images == nil {
		images = []PromptImage{}
	}
	return StartupPrompt{
		CommandID:       commandID,
		MoveOperationID: c.composerRuntime.previewOrigins[commandID],
		Payload:         PromptPayload{Text: text, Images: images},
	}
}

// StartupPromptWithdrawn settles a withdrawal: withdrawn is the daemon's
// answer when err is nil.
func (c *ChatState) StartupPromptWithdrawn(prompt StartupPrompt, withdrawn bool, err error) (bool, error) {
	delete(c.pendingQueueRemovals, prompt.CommandID)
	delete(c.pendingQueueImages, prompt.CommandID)
	if err == nil {
		if withdrawn {
			return false, nil
		}
		if draft := c.draftPayload(); draft.Equal(&prompt.Payload) {
			c.clearInput()
			return true, nil
		}
		return false, nil
	}
	if draft := c.draftPayload(); draft.Equal(&prompt.Payload) {
		c.clearInput()
	}
	if !slices.ContainsFunc(c.queuedPrompts, func(queued QueuedPrompt) bool { return queued.id == prompt.CommandID }) {
		images := prompt.Payload.Images
		if images == nil {
			images = []PromptImage{}
		}
		c.queuedPrompts = append(c.queuedPrompts, QueuedPrompt{
			id:                     prompt.CommandID,
			text:                   prompt.Payload.Text,
			images:                 images,
			kind:                   core.QueuedCommandKindPrompt{},
			attachmentsUnsupported: false,
		})
	}
	return false, err
}

func (c *ChatState) StartupPrompts() []StartupPrompt {
	prompts := make([]StartupPrompt, 0, len(c.queuedPrompts))
	for _, queued := range c.queuedPrompts {
		images := append([]PromptImage(nil), queued.images...)
		if images == nil {
			images = []PromptImage{}
		}
		prompts = append(prompts, StartupPrompt{
			CommandID:       queued.id,
			MoveOperationID: c.composerRuntime.previewOrigins[queued.id],
			Payload:         PromptPayload{Text: queued.text, Images: images},
		})
	}
	return prompts
}

func (c *ChatState) LatestStartupPrompt() (StartupPrompt, bool) {
	prompts := c.StartupPrompts()
	if len(prompts) == 0 {
		return StartupPrompt{}, false
	}
	return prompts[len(prompts)-1], true
}

// Close is what dropping a ChatState does to its composer in Rust: the
// result forwarders abort, the attachment jobs are cancelled (which drops
// their daemon upload connections), and late results are refused. Go has no
// destructor, so a host that discards a chat calls this.
func (c *ChatState) Close() {
	c.composerRuntime.drop()
}

// HasLiveComposer: a parked worker view forwards results to the composer
// which took its state.
func (c *ChatState) HasLiveComposer() bool {
	return c.composerForward == nil
}

func (c *ChatState) TakeLiveComposer() *LiveComposer {
	if !c.HasLiveComposer() {
		return nil
	}
	c.composerForward = c.composerRuntime.tx
	composer := &LiveComposer{
		input:                       c.input,
		inputCursor:                 c.inputCursor,
		inputImages:                 c.inputImages,
		nextImageNumber:             c.nextImageNumber,
		submittingImages:            c.submittingImages,
		pendingAttachmentMarkers:    c.pendingAttachmentMarkers,
		inputGeneration:             c.inputGeneration,
		promptHistory:               c.promptHistory,
		historyIndex:                c.historyIndex,
		historyDraft:                c.historyDraft,
		historyDraftImages:          c.historyDraftImages,
		killBuffer:                  c.killBuffer,
		killImages:                  c.killImages,
		chainKill:                   c.chainKill,
		preferredColumn:             c.preferredColumn,
		queuedPrompts:               c.queuedPrompts,
		pendingQueueRemovals:        c.pendingQueueRemovals,
		pendingQueueImages:          c.pendingQueueImages,
		historySearch:               c.historySearch,
		nextHistorySearchGeneration: c.nextHistorySearchGeneration,
		composerRequests:            c.composerRequests,
		sessionHistory:              c.sessionHistory,
		sessionHistoryError:         c.sessionHistoryError,
		sessionHistoryGeneration:    c.sessionHistoryGeneration,
		pendingSubmissions:          c.pendingSubmissions,
		submissionRenders:           c.submissionRenders,
		planCommandPending:          c.planCommandPending,
		operationFeedback:           c.operationFeedback,
		deferredFormResults:         c.deferredFormResults,
		conversationNotices:         c.conversationNotices,
		composerRuntime:             c.composerRuntime,
	}
	// Rust moves each field out with mem::take, leaving its default behind;
	// the session history generation is copied.
	c.input = ""
	c.inputCursor = 0
	c.inputImages = nil
	c.nextImageNumber = 0
	c.submittingImages = nil
	c.pendingAttachmentMarkers = map[uint64]uint64{}
	c.inputGeneration = 0
	c.promptHistory = nil
	c.historyIndex = nil
	c.historyDraft = ""
	c.historyDraftImages = nil
	c.killBuffer = ""
	c.killImages = nil
	c.chainKill = false
	c.preferredColumn = nil
	c.queuedPrompts = nil
	c.pendingQueueRemovals = map[string]struct{}{}
	c.pendingQueueImages = map[string][]PromptImage{}
	c.historySearch = nil
	c.nextHistorySearchGeneration = 0
	c.composerRequests = nil
	c.sessionHistory = nil
	c.sessionHistoryError = nil
	c.pendingSubmissions = nil
	c.submissionRenders = nil
	c.planCommandPending = false
	c.operationFeedback = map[string]string{}
	c.deferredFormResults = nil
	c.conversationNotices = nil
	c.composerRuntime = defaultComposerRuntime()
	c.syncInlineSubmissions()
	return composer
}

func (c *ChatState) AdoptLiveComposer(composer *LiveComposer) {
	c.composerForward = nil
	c.input = composer.input
	c.inputCursor = composer.inputCursor
	c.inputImages = composer.inputImages
	c.nextImageNumber = composer.nextImageNumber
	c.submittingImages = composer.submittingImages
	c.pendingAttachmentMarkers = composer.pendingAttachmentMarkers
	c.inputGeneration = composer.inputGeneration
	c.promptHistory = composer.promptHistory
	c.historyIndex = composer.historyIndex
	c.historyDraft = composer.historyDraft
	c.historyDraftImages = composer.historyDraftImages
	c.killBuffer = composer.killBuffer
	c.killImages = composer.killImages
	c.chainKill = composer.chainKill
	c.preferredColumn = composer.preferredColumn
	c.queuedPrompts = composer.queuedPrompts
	c.pendingQueueRemovals = composer.pendingQueueRemovals
	c.pendingQueueImages = composer.pendingQueueImages
	c.historySearch = composer.historySearch
	c.nextHistorySearchGeneration = composer.nextHistorySearchGeneration
	c.composerRequests = composer.composerRequests
	c.sessionHistory = composer.sessionHistory
	c.sessionHistoryError = composer.sessionHistoryError
	c.sessionHistoryGeneration = composer.sessionHistoryGeneration
	c.pendingSubmissions = composer.pendingSubmissions
	c.submissionRenders = composer.submissionRenders
	c.planCommandPending = composer.planCommandPending
	c.operationFeedback = composer.operationFeedback
	c.deferredFormResults = composer.deferredFormResults
	c.conversationNotices = composer.conversationNotices
	// The runtime this chat had is replaced, which drops it as Rust does.
	previous := c.composerRuntime
	c.composerRuntime = composer.composerRuntime
	previous.drop()
	c.syncInlineSubmissions()
	c.updateAutocomplete()
}

// SetComposerMoveOrigin sets the Move this composer's prompts belong to; nil
// is an ordinary composer.
func (c *ChatState) SetComposerMoveOrigin(origin *ComposerMoveOrigin) {
	switch {
	case origin == nil:
		c.composerRuntime.origin = promptOriginOrdinary{}
	case origin.ID == nil:
		c.composerRuntime.origin = promptOriginPreparingMove(origin.Generation)
	default:
		id := *origin.ID
		if deferred := c.composerRuntime.submitWhenReady; deferred != nil {
			if expected, ok := deferred.origin.(promptOriginPreparingMove); ok && uint64(expected) == origin.Generation {
				deferred.origin = promptOriginMove(id)
			}
		}
		c.composerRuntime.origin = promptOriginMove(id)
	}
}

func (c *ChatState) SettleDeferredMoveOrigin(generation uint64, id *string, succeeded bool) {
	deferred := c.composerRuntime.submitWhenReady
	if deferred == nil {
		return
	}
	matches := false
	switch expected := deferred.origin.(type) {
	case promptOriginPreparingMove:
		matches = uint64(expected) == generation
	case promptOriginMove:
		matches = id != nil && string(expected) == *id
	}
	if !matches {
		return
	}
	if succeeded {
		if id != nil {
			deferred.origin = promptOriginMove(*id)
		}
	} else {
		c.KeepDeferredSubmissionAsDraft()
	}
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
	switch action := action.(type) {
	case ChatActionPasteFromClipboard:
		c.readClipboard()
	case ChatActionAttach:
		c.queueAttachment(attachmentSourcePath{path: action.Path}, attachmentFallbackCommand(action.Command))
	case ChatActionAttachPaste:
		cursor := 0
		for _, image := range action.Images {
			c.pastePlain(action.Text[cursor:image.byteRange.Start])
			pasted := action.Text[image.byteRange.Start:image.byteRange.End]
			if !c.queueAttachment(attachmentSourcePath{path: image.path}, attachmentFallbackText(pasted)) {
				c.pastePlain(pasted)
			}
			cursor = image.byteRange.End
		}
		c.pastePlain(action.Text[cursor:])
	}
}

// clipboardReadResult is one clipboard read's content or failure.
type clipboardReadResult struct {
	content ClipboardContent
	err     error
}

func (c *ChatState) readClipboard() {
	if c.composerRuntime.pasteInFlight {
		c.SetNotice("Clipboard read already in progress…")
		return
	}
	c.composerRuntime.pasteInFlight = true
	c.SetNotice("Reading clipboard…")
	updates := c.composerRuntime.tx
	textOnly := c.clipboardIsTextOnly()
	generation := c.InputGeneration()
	target := c.clipboardTarget()
	staging := c.standby
	go func() {
		read := core.SpawnBlocking(func() clipboardReadResult {
			if textOnly {
				text, err := ReadText()
				if err != nil {
					return clipboardReadResult{err: err}
				}
				return clipboardReadResult{content: ClipboardContentText{Text: text}}
			}
			content, err := Read()
			return clipboardReadResult{content: content, err: err}
		})
		result, err := read.Join(context.Background())
		if err != nil {
			result = clipboardReadResult{err: fmt.Errorf("clipboard task failed: %w", err)}
		}
		if err := updates.Send(composerIoUpdateClipboard{
			generation: generation,
			target:     target,
			staging:    staging,
			result:     result.content,
			err:        result.err,
		}); err != nil {
			slog.Debug("clipboard result dropped because the composer closed", "error", err)
		}
	}()
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
	return !c.composerRuntime.rx.IsEmpty()
}

func (c *ChatState) DrainComposerUpdates() {
	for {
		update, ok, _ := c.composerRuntime.rx.TryRecv()
		if !ok {
			return
		}
		c.applyComposerUpdate(update)
	}
}

func (c *ChatState) DrainComposer() (StartupPrompt, bool) {
	c.DrainComposerUpdates()
	c.pumpAttachmentQueue()
	c.composerRuntime.remoteStreams = slices.DeleteFunc(c.composerRuntime.remoteStreams,
		func(stream *remoteResultForwarder) bool {
			if !stream.isFinished() {
				return false
			}
			stream.drop()
			return true
		})
	deferred := c.composerRuntime.submitWhenReady
	if deferred == nil {
		return StartupPrompt{}, false
	}
	if c.composerRuntime.pasteInFlight || slices.ContainsFunc(c.inputImages, func(image PromptImage) bool {
		return image.Image.isPending()
	}) {
		return StartupPrompt{}, false
	}
	if _, preparing := deferred.origin.(promptOriginPreparingMove); preparing {
		return StartupPrompt{}, false
	}
	origin := deferred.origin
	c.composerRuntime.submitWhenReady = nil
	currentOrigin := c.composerRuntime.origin
	c.composerRuntime.origin = origin
	wasStandby := c.standby
	c.standby = true
	action := c.submitInput()
	c.standby = wasStandby
	c.composerRuntime.origin = currentOrigin
	if _, prompt := action.(ChatActionPrompt); prompt {
		return c.LatestStartupPrompt()
	}
	return StartupPrompt{}, false
}

func (c *ChatState) applyComposerUpdate(update composerIoUpdate) {
	switch update := update.(type) {
	case composerIoUpdateSessionHistoryListed:
		if update.err != nil {
			slog.Warn("session chat history listing failed", "error", update.err)
			c.setSessionHistoryUnavailable(update.generation, update.err.Error())
		} else {
			c.setSessionHistory(update.generation, update.result)
		}
	case composerIoUpdateHistorySearchResults:
		c.applyHistorySearchResults(update.generation, update.result, update.err)
	case composerIoUpdateRemote:
		applyChatRemoteResult(c, update.result)
	case composerIoUpdateClipboard:
		c.composerRuntime.pasteInFlight = false
		// An accepted form paste belongs to its request, even when the other
		// presentation (or its history reader) redraws.
		var targetMatches bool
		if _, answer := update.target.(clipboardTargetAnswer); answer {
			targetMatches = clipboardTargetsEqual(update.target, c.elicitationClipboardTarget())
		} else {
			targetMatches = clipboardTargetsEqual(update.target, c.clipboardTarget())
		}
		if update.generation != c.InputGeneration() || !targetMatches {
			return
		}
		c.composerRuntime.acceptingStaged = update.staging
		var submitWhenReady *deferredSubmission
		if update.err == nil && c.composerRuntime.submitWhenReady != nil {
			kept := *c.composerRuntime.submitWhenReady
			submitWhenReady = &kept
		}
		if update.err != nil {
			slog.Warn("clipboard read failed and was shown in the UI", "error", update.err)
			policy := ""
			if !(c.standby || c.promptImagesSupported) {
				policy = "; this agent accepts only clipboard text"
			}
			c.SetNotice(fmt.Sprintf("Paste failed: %v%s", update.err, policy))
		} else {
			switch content := update.result.(type) {
			case ClipboardContentImage:
				c.queueAttachment(attachmentSourceClipboard(content.Image), attachmentFallbackFailedMarker{})
			case ClipboardContentText:
				// Copied files arrive as their paths, which attach the way a
				// pasted path does.
				if clipboardTargetsEqual(update.target, clipboardTargetComposer{}) {
					c.DispatchComposerAction(c.pasteOrAttach(content.Text))
				} else {
					c.handleClipboardContent(content)
				}
			}
		}
		c.composerRuntime.acceptingStaged = false
		c.composerRuntime.submitWhenReady = submitWhenReady
	case composerIoUpdateReceiptPrepared:
		c.finishReceiptPreparation(update.commandID, update.result, update.err)
	case composerIoUpdateAttachmentFinished:
		c.applyAttachmentResult(attachmentResult(update))
		c.pumpAttachmentQueue()
	}
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
	runtime := &c.composerRuntime
	if c.pendingAttachmentMarkers == nil {
		c.pendingAttachmentMarkers = map[uint64]uint64{}
	}
	if runtime.attachmentCancellations == nil {
		runtime.attachmentCancellations = map[uint64]context.CancelFunc{}
	}
	// Older drafts and conversation history may carry inline bytes. Adopt
	// them into the same marker-owned jobs before they can be submitted.
	for index := range c.inputImages {
		image := &c.inputImages[index]
		if image.Image.Reference != nil || image.Image.isPlaceholder() {
			continue
		}
		sequence := runtime.nextAttachmentSequence
		runtime.nextAttachmentSequence++
		source := image.Image
		image.Image = clipboardImagePending()
		c.pendingAttachmentMarkers[sequence] = image.Number
		runtime.attachmentQueue = append(runtime.attachmentQueue, queuedAttachment{
			sequence: sequence,
			source:   attachmentSourceClipboard(source),
			fallback: attachmentFallbackFailedMarker{},
		})
		c.inputGeneration++
	}
	// A job whose marker is gone, or no longer pending, is cancelled; Rust
	// drops its cancellation sender.
	for sequence, cancel := range runtime.attachmentCancellations {
		number, ok := c.pendingAttachmentMarkers[sequence]
		live := ok && slices.ContainsFunc(c.inputImages, func(image PromptImage) bool {
			return image.Number == number && image.Image.isPending()
		})
		if !live {
			cancel()
			delete(runtime.attachmentCancellations, sequence)
		}
	}
	runtime.attachmentQueue = slices.DeleteFunc(runtime.attachmentQueue, func(queued queuedAttachment) bool {
		_, ok := c.pendingAttachmentMarkers[queued.sequence]
		return !ok
	})
	for runtime.attachmentTasksInFlight < maxAttachmentTasks {
		if runtime.sessionID == nil || len(runtime.attachmentQueue) == 0 {
			break
		}
		sessionID := *runtime.sessionID
		job := runtime.attachmentQueue[0]
		runtime.attachmentQueue = runtime.attachmentQueue[1:]
		runtime.attachmentTasksInFlight++
		updates := runtime.tx
		ctx, cancel := context.WithCancel(context.Background())
		runtime.attachmentCancellations[job.sequence] = cancel
		go func() {
			type preparation struct {
				image ClipboardImage
				err   error
			}
			done := make(chan preparation, 1)
			go func() {
				image, err := prepareAttachment(ctx, sessionID, job.source)
				done <- preparation{image: image, err: err}
			}()
			var result preparation
			select {
			case result = <-done:
			case <-ctx.Done():
				result = preparation{err: errors.New("attachment cancelled")}
			}
			if err := updates.Send(composerIoUpdateAttachmentFinished{
				sequence: job.sequence,
				fallback: job.fallback,
				result:   result.image,
				err:      result.err,
			}); err != nil {
				slog.Debug("attachment result dropped because the composer closed", "error", err)
			}
		}()
	}
}

func (c *ChatState) applyAttachmentResult(result attachmentResult) {
	c.composerRuntime.attachmentTasksInFlight = max(c.composerRuntime.attachmentTasksInFlight-1, 0)
	// The job has finished; releasing its context is what dropping the
	// cancellation sender is in Rust.
	if cancel, ok := c.composerRuntime.attachmentCancellations[result.sequence]; ok {
		cancel()
		delete(c.composerRuntime.attachmentCancellations, result.sequence)
	}
	c.finishAttachment(result.sequence, result.result, result.err, result.fallback)
}

// prepareAttachment is the shared codec and daemon-install path for composer
// markers and old receipts.
func prepareAttachment(ctx context.Context, sessionID string, source attachmentSource) (ClipboardImage, error) {
	type installed struct {
		image ClipboardImage
		err   error
	}
	install := core.SpawnBlocking(func() installed {
		switch source := source.(type) {
		case attachmentSourceClipboard:
			image, err := installClipboardImage(sessionID, ClipboardImage(source))
			return installed{image: image, err: err}
		case attachmentSourcePath:
			path, err := source.path.resolve()
			if err != nil {
				return installed{err: err}
			}
			image, err := installPath(sessionID, path)
			return installed{image: image, err: err}
		}
		return installed{err: fmt.Errorf("unknown attachment source %T", source)}
	})
	outcome, err := install.Join(ctx)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("attachment task failed: %w", err)
	}
	if outcome.err != nil {
		return ClipboardImage{}, outcome.err
	}
	return uploadPreparedImage(ctx, sessionID, outcome.image)
}
