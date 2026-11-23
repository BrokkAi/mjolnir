// Port of mj-chat/src/chat/input_state.rs.

package chat

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"reflect"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// slashCommandName is the command name when text is shaped like a slash
// command: a slash followed by a word of letters, digits, `-`, `_`, `:` or
// `.`. A path such as `/tmp/log is empty` is not a command.
func slashCommandName(text string) (string, bool) {
	rest, ok := strings.CutPrefix(text, "/")
	if !ok {
		return "", false
	}
	name := rest
	if index := strings.IndexFunc(rest, unicode.IsSpace); index >= 0 {
		name = rest[:index]
	}
	if name == "" {
		return "", false
	}
	for _, character := range name {
		// Rust's char::is_alphanumeric: Alphabetic or Numeric.
		alphanumeric := unicode.IsLetter(character) || unicode.IsNumber(character) ||
			unicode.Is(unicode.Other_Alphabetic, character)
		if !alphanumeric && !strings.ContainsRune("-_:.", character) {
			return "", false
		}
	}
	return name, true
}

// attachmentFallback is what replaces an attachment's marker when its file
// cannot be read.
type attachmentFallback interface {
	isAttachmentFallback()
}

// attachmentFallbackFailedMarker keeps the marker, shown as failed: a
// clipboard image has no text.
type attachmentFallbackFailedMarker struct{}

// attachmentFallbackCommand removes the marker, restoring the `/attach`
// command if it was alone.
type attachmentFallbackCommand string

// attachmentFallbackText puts back the text that was pasted in its place.
type attachmentFallbackText string

func (attachmentFallbackFailedMarker) isAttachmentFallback() {}
func (attachmentFallbackCommand) isAttachmentFallback()      {}
func (attachmentFallbackText) isAttachmentFallback()         {}

const attachUnsupportedNotice = "/attach adds image files only, and this agent does not accept images"
const imagePasteUnsupportedNotice = "This agent does not accept images; only text can be pasted"
const imageCapabilityNotice = "This agent has not advertised image support; paste text or remove image markers before sending"

// clipboardTarget is where a clipboard read lands. Compare two targets with
// clipboardTargetsEqual: == compares an Answer's identity by pointer.
type clipboardTarget interface {
	isClipboardTarget()
}

type clipboardTargetComposer struct{}
type clipboardTargetSearch struct{}
type clipboardTargetAnswer struct {
	identity *ElicitationIdentity
}
type clipboardTargetUnavailable struct{}

func (clipboardTargetComposer) isClipboardTarget()    {}
func (clipboardTargetSearch) isClipboardTarget()      {}
func (clipboardTargetAnswer) isClipboardTarget()      {}
func (clipboardTargetUnavailable) isClipboardTarget() {}

// clipboardTargetsEqual is Rust's derived PartialEq: Answer targets compare
// their form identities by value.
func clipboardTargetsEqual(a, b clipboardTarget) bool {
	answerA, aIsAnswer := a.(clipboardTargetAnswer)
	answerB, bIsAnswer := b.(clipboardTargetAnswer)
	if aIsAnswer || bIsAnswer {
		if !aIsAnswer || !bIsAnswer {
			return false
		}
		if answerA.identity == nil || answerB.identity == nil {
			return answerA.identity == nil && answerB.identity == nil
		}
		return reflect.DeepEqual(*answerA.identity, *answerB.identity)
	}
	return a == b
}

// queuedKindIsPrompt is QueuedCommandKind::is_prompt; a queue entry without a
// kind is a prompt.
func queuedKindIsPrompt(kind core.QueuedCommandKind) bool {
	return kind == nil || kind.IsPrompt()
}

func (c *ChatState) ApplyEvents(events []core.SequencedEvent) {
	for index := range events {
		event := &events[index]
		if event.Seq <= c.latestSeq {
			continue
		}
		c.applyEvent(event)
		c.latestSeq = event.Seq
	}
	if len(events) > 0 {
		c.invalidateRenderCache()
	}
}

func (c *ChatState) resetInteraction() {
	c.promptHistory = nil
	c.historyIndex = nil
	c.historyDraft = ""
	c.historyDraftImages = nil
	c.preferredColumn = nil
	c.historySearch = nil
	c.queuedPrompts = nil
	c.autocomplete = nil
	c.anchor = TranscriptAnchorBottom{}
	c.revealLatestAgentOnDraw = true
	c.revealedAnchor = nil
	c.lastViewportHeight = 0
	c.renderMode = client.TranscriptRenderModeRich
	c.transcriptScrollbar.clear()
	if _, ok := c.feedback.Current(); ok {
		c.feedback.Clear()
	}
	c.voiceActive = false
	c.submittingImages = nil
	clear(c.pendingAttachmentMarkers)
	c.inputGeneration++
	c.feedback.Clear()
}

func (c *ChatState) setInput(input string) {
	c.setInputPayload(PromptPayloadText(input))
}

func (c *ChatState) setInputPayload(payload PromptPayload) {
	c.composerRuntime.submitWhenReady = nil
	clear(c.pendingAttachmentMarkers)
	clear(c.composerRuntime.legacyDraftImages)
	images := append([]PromptImage(nil), payload.Images...)
	nextNumber := uint64(1)
	for index := range images {
		if images[index].Image.isPending() {
			images[index].Image = clipboardImageFailed()
		}
		nextNumber = max(nextNumber, saturatingAddOne(images[index].Number))
	}
	c.nextImageNumber = max(c.nextImageNumber, nextNumber)
	c.input = payload.Text
	c.inputImages = images
	c.inputCursor = len(payload.Text)
	c.inputGeneration++
	c.feedback.Clear()
	c.historyIndex = nil
	c.preferredColumn = nil
	c.updateAutocomplete()
}

// splitOnceWhitespace is Rust's `str::split_once(char::is_whitespace)`: the
// text before the first whitespace character and the text after it.
func splitOnceWhitespace(text string) (string, string, bool) {
	index := strings.IndexFunc(text, unicode.IsSpace)
	if index < 0 {
		return text, "", false
	}
	_, size := utf8.DecodeRuneInString(text[index:])
	return text[:index], text[index+size:], true
}

// saturatingAddOne is Rust's `u64::saturating_add(1)`.
func saturatingAddOne(value uint64) uint64 {
	if value == ^uint64(0) {
		return value
	}
	return value + 1
}

func (c *ChatState) clearInput() {
	c.setInput("")
}

// ClearDraft empties the composer's input.
func (c *ChatState) ClearDraft() {
	c.clearInput()
}

// restoreDraft reinstates the input saved when the user last detached,
// leaving the cursor at the end. An empty draft leaves the composer alone.
func (c *ChatState) restoreDraft(draft string) {
	if draft == "" {
		return
	}
	payload, err := PromptPayloadDecodeDraft(draft)
	if err != nil {
		slog.Warn("saved chat draft could not be decoded", "error", err)
		fallback := ""
		if body, ok := strings.CutPrefix(draft, ChatDraftPrefix); ok {
			var value map[string]any
			if json.Unmarshal([]byte(body), &value) == nil {
				if text, ok := value["text"].(string); ok {
					fallback = text
				}
			}
		}
		c.setInput(fallback)
		c.SetNotice(fmt.Sprintf("Saved image draft was discarded: %v", err))
		return
	}
	interrupted := false
	legacy := map[uint64]ClipboardImage{}
	for _, image := range payload.Images {
		interrupted = interrupted || image.Image.isPending()
		if image.Image.Reference == nil && !image.Image.isPlaceholder() {
			legacy[image.Number] = image.Image
		}
	}
	c.setInputPayload(payload)
	c.composerRuntime.legacyDraftImages = legacy
	if body, ok := strings.CutPrefix(draft, ChatDraftPrefix); ok {
		var saved SavedChatDraft
		if err := json.Unmarshal([]byte(body), &saved); err != nil {
			c.SetNotice(fmt.Sprintf("Could not restore pending prompts: %v", err))
		} else {
			c.restoreSubmissions(saved.pending)
		}
	}
	if interrupted {
		c.SetNotice("An image upload was interrupted. Re-paste the failed attachment to retry.")
	}
}

func (c *ChatState) handleClipboardContent(content ClipboardContent) {
	if clipboardTargetsEqual(c.clipboardTarget(), clipboardTargetUnavailable{}) && c.elicitation == nil {
		return
	}
	if c.elicitation != nil {
		switch content := content.(type) {
		case ClipboardContentText:
			c.handlePaste(content.Text)
		case ClipboardContentImage:
			c.SetNotice("Image paste is unavailable in a text answer field")
		}
		return
	}
	if c.historySearch != nil {
		switch content := content.(type) {
		case ClipboardContentText:
			c.handlePaste(content.Text)
		case ClipboardContentImage:
			c.SetNotice("Image paste is unavailable while searching history")
		}
		return
	}
	switch content := content.(type) {
	case ClipboardContentText:
		c.handlePaste(content.Text)
	case ClipboardContentImage:
		if !c.allowImageAttachment() {
			return
		}
		if len(c.inputImages) >= core.MaxImages {
			c.SetNotice(fmt.Sprintf("A prompt can contain at most %d images", core.MaxImages))
			return
		}
		c.inputCursor = insertImage(&c.input, &c.inputImages, c.inputCursor, c.nextImageNumber, content.Image)
		c.nextImageNumber++
		c.inputGeneration++
		c.feedback.Clear()
		c.historyIndex = nil
		c.preferredColumn = nil
		c.updateAutocomplete()
	}
}

func (c *ChatState) reserveAttachment(sequence uint64) bool {
	if !c.allowImageAttachment() {
		return false
	}
	if len(c.inputImages) >= core.MaxImages {
		c.SetNotice(fmt.Sprintf("A prompt can contain at most %d images", core.MaxImages))
		return false
	}
	number := c.nextImageNumber
	c.inputCursor = insertImage(&c.input, &c.inputImages, c.inputCursor, number, clipboardImagePending())
	c.nextImageNumber = saturatingAddOne(c.nextImageNumber)
	if c.pendingAttachmentMarkers == nil {
		c.pendingAttachmentMarkers = map[uint64]uint64{}
	}
	c.pendingAttachmentMarkers[sequence] = number
	c.inputGeneration++
	c.feedback.Clear()
	return true
}

// finishAttachment lands an attachment job's result: ready is the image when
// err is nil.
func (c *ChatState) finishAttachment(sequence uint64, ready ClipboardImage, err error, fallback attachmentFallback) {
	number, ok := c.pendingAttachmentMarkers[sequence]
	if !ok {
		// The marker was deleted or the draft was replaced while the blocking
		// task ran. Its result is intentionally discarded.
		return
	}
	delete(c.pendingAttachmentMarkers, sequence)
	index := -1
	for candidate := range c.inputImages {
		if c.inputImages[candidate].Number == number && c.inputImages[candidate].Image.isPending() {
			index = candidate
			break
		}
	}
	if index < 0 {
		return
	}
	if err == nil {
		delete(c.composerRuntime.legacyDraftImages, number)
		c.inputImages[index].Image = ready
		c.inputGeneration++
		c.feedback.Clear()
		return
	}
	switch fallback := fallback.(type) {
	case attachmentFallbackFailedMarker:
		c.inputImages[index].Image = clipboardImageFailed()
		c.inputGeneration++
		c.feedback.Clear()
	case attachmentFallbackCommand:
		r := c.inputImages[index].Range
		onlyPlaceholder := len(c.inputImages) == 1 && r.Start == 0 && r.End == len(c.input)
		empty := PromptPayloadText("")
		c.replaceInputRange(r, &empty)
		if onlyPlaceholder {
			c.setInput(string(fallback))
		}
	case attachmentFallbackText:
		// Put the pasted text where the marker was, leaving the cursor where
		// the user has since moved it.
		r := c.inputImages[index].Range
		cursor := c.inputCursor
		text := PromptPayloadText(string(fallback))
		c.replaceInputRange(r, &text)
		if cursor >= r.End {
			c.inputCursor = cursor - r.Len() + len(fallback)
		} else {
			c.inputCursor = cursor
		}
		c.updateAutocomplete()
	}
	c.SetNotice(fmt.Sprintf("Attachment failed: %v", err))
}

func (c *ChatState) setPromptImagesSupported(supported bool) {
	if c.promptImagesSupported != supported {
		c.promptImagesSupported = supported
		// Invalidate reads made under the previous capability policy.
		c.inputGeneration++
	}
}

func (c *ChatState) elicitationClipboardTarget() clipboardTarget {
	identity := c.elicitationIdentity()
	if identity == nil {
		return clipboardTargetUnavailable{}
	}
	return clipboardTargetAnswer{identity: identity}
}

func (c *ChatState) clipboardTarget() clipboardTarget {
	switch {
	case c.earlierReaderHasFocus():
		return clipboardTargetUnavailable{}
	case c.elicitation != nil:
		return c.elicitationClipboardTarget()
	case c.turnControlDialogOpen || c.taskDialogOpen || c.configPickerActive():
		return clipboardTargetUnavailable{}
	case c.historySearch != nil:
		return clipboardTargetSearch{}
	}
	return clipboardTargetComposer{}
}

func (c *ChatState) clipboardIsTextOnly() bool {
	return !(c.standby || c.promptImagesSupported) ||
		!clipboardTargetsEqual(c.clipboardTarget(), clipboardTargetComposer{})
}

func (c *ChatState) allowImageAttachment() bool {
	if !(c.standby || c.composerRuntime.acceptingStaged || c.promptImagesSupported) {
		c.SetNotice(imageCapabilityNotice)
		return false
	}
	if !clipboardTargetsEqual(c.clipboardTarget(), clipboardTargetComposer{}) {
		c.SetNotice("Image paste is unavailable in this input field")
		return false
	}
	return true
}

func (c *ChatState) takeSubmittingImages() []PromptImage {
	images := c.submittingImages
	c.submittingImages = nil
	return images
}

func (c *ChatState) draftPayload() PromptPayload {
	return PromptPayload{Text: c.input, Images: append([]PromptImage(nil), c.inputImages...)}
}

// InputGeneration is Rust's crate-private `input_generation()`, exported
// because Go cannot give a method the name of the inputGeneration field.
func (c *ChatState) InputGeneration() uint64 {
	return c.inputGeneration
}

func (c *ChatState) encodedDraft() string {
	composer := c.draftPayload()
	// Already-durable legacy bytes stay durable until installation succeeds.
	for index := range composer.Images {
		image := &composer.Images[index]
		if !image.Image.isPlaceholder() {
			continue
		}
		if legacy, ok := c.composerRuntime.legacyDraftImages[image.Number]; ok {
			image.Image = legacy
		}
	}
	if len(c.pendingSubmissions) == 0 {
		return composer.EncodeDraft()
	}
	saved := SavedChatDraft{
		composer: composer,
		pending:  append([]PendingSubmission(nil), c.pendingSubmissions...),
	}
	encoded, err := json.Marshal(saved)
	if err != nil {
		panic(fmt.Sprintf("chat draft serialization cannot fail: %v", err))
	}
	return ChatDraftPrefix + string(encoded)
}

func (c *ChatState) editLatestQueuedPrompt() ChatAction {
	if n := len(c.queuedPrompts); n > 0 && c.queuedPrompts[n-1].attachmentsUnsupported {
		c.SetNotice("Queued prompt has unsupported attachments and cannot be edited")
		return ChatActionNone{}
	}
	n := len(c.queuedPrompts)
	if n == 0 {
		return ChatActionNone{}
	}
	queued := c.queuedPrompts[n-1]
	c.queuedPrompts = c.queuedPrompts[:n-1]
	if c.pendingQueueRemovals == nil {
		c.pendingQueueRemovals = map[string]struct{}{}
	}
	c.pendingQueueRemovals[queued.id] = struct{}{}
	if c.pendingQueueImages == nil {
		c.pendingQueueImages = map[string][]PromptImage{}
	}
	c.pendingQueueImages[queued.id] = append([]PromptImage(nil), queued.images...)
	c.setInputPayload(PromptPayload{Text: queued.text, Images: queued.images})
	if queuedKindIsPrompt(queued.kind) {
		c.SetNotice("Editing the most recently queued prompt")
	} else {
		c.SetNotice("Editing the most recently queued configuration change")
	}
	return ChatActionRemoveQueuedPrompt{ID: queued.id, Text: queued.text, Kind: queued.kind}
}

func (c *ChatState) failQueuedPromptRemoval(id string, text string, kind core.QueuedCommandKind) {
	delete(c.pendingQueueRemovals, id)
	images := c.pendingQueueImages[id]
	delete(c.pendingQueueImages, id)
	if images == nil {
		images = []PromptImage{}
	}
	for _, prompt := range c.queuedPrompts {
		if prompt.id == id {
			return
		}
	}
	c.queuedPrompts = append(c.queuedPrompts, QueuedPrompt{
		id:                     id,
		text:                   text,
		kind:                   kind,
		images:                 images,
		attachmentsUnsupported: false,
	})
}

// QueuedPromptTexts is the texts of the queued-prompt previews, oldest
// first.
func (c *ChatState) QueuedPromptTexts() []string {
	texts := make([]string, 0, len(c.queuedPrompts))
	for _, queued := range c.queuedPrompts {
		texts = append(texts, queued.text)
	}
	return texts
}

// RemoveQueuedPromptText removes the oldest queued-prompt preview with this
// text and says whether one was there. Used when a queued prompt could not
// be handed to the daemon and its text goes back into the draft.
func (c *ChatState) RemoveQueuedPromptText(text string) bool {
	for index, queued := range c.queuedPrompts {
		if queued.text == text {
			c.queuedPrompts = append(c.queuedPrompts[:index:index], c.queuedPrompts[index+1:]...)
			return true
		}
	}
	return false
}

func (c *ChatState) submitInput() ChatAction {
	draft := c.input
	command, isCommand := slashCommandName(strings.TrimSpace(draft))
	action := c.submitDraft()
	// A slash command the chat refused leaves nothing for the user to edit:
	// keeping it in the draft only makes the next command append to it
	// ("/model/effort high"). Clear it, keep it in history, and name the
	// command in the notice. /clear keeps its own handling, and a draft
	// holding images or waiting for a session is kept.
	if !isCommand {
		return action
	}
	if _, none := action.(ChatActionNone); !none || command == "clear" || c.standby ||
		len(c.inputImages) > 0 || c.input != draft {
		return action
	}
	notice, ok := c.Notice()
	if !ok {
		return action
	}
	c.RecordPromptHistory(strings.TrimSpace(draft))
	c.clearInput()
	named := "/" + command
	if strings.Contains(notice, named) {
		c.SetNotice(notice)
	} else {
		c.SetNotice(fmt.Sprintf("%s: %s", named, notice))
	}
	return action
}

// workerClosing reports the phases in which the worker accepts no new work.
func (c *ChatState) workerClosing() bool {
	return c.phase == core.WorkerPhaseClosing || c.phase == core.WorkerPhaseClosed
}

func (c *ChatState) submitDraft() ChatAction {
	if _, preparing := c.composerRuntime.origin.(promptOriginPreparingMove); c.standby && preparing {
		c.deferStartupSubmission()
		c.SetNotice("Preparing the move before queueing the prompt…")
		return ChatActionNone{}
	}
	if c.standby && c.composerRuntime.pasteInFlight {
		c.deferStartupSubmission()
		c.SetNotice("Reading clipboard before queueing the prompt…")
		return ChatActionNone{}
	}
	if c.standby {
		for _, image := range c.inputImages {
			if image.Image.isPending() {
				c.deferStartupSubmission()
				c.SetNotice("Preparing images before queueing the prompt…")
				return ChatActionNone{}
			}
		}
	}
	if !c.standby && len(c.inputImages) > 0 && !c.promptImagesSupported {
		c.SetNotice(imageCapabilityNotice)
		return ChatActionNone{}
	}
	prompt := strings.TrimSpace(c.input)
	commandInput := prompt
	if len(c.inputImages) > 0 {
		commandInput = strings.TrimSpace(textWithoutImages(c.input, c.inputImages))
	}
	parsedCommand, args, parsed := parseLocalCommand(commandInput)
	if prompt == "" && len(c.inputImages) == 0 {
		return ChatActionNone{}
	}
	// No session is attached yet. A plain prompt is handed to the host,
	// which asks the daemon to deliver it once the session is live, and stays
	// visible here as a queued preview. Commands need a live session to
	// answer them, so they keep the draft and say so.
	if c.standby {
		if prompt == "" || parsed || strings.HasPrefix(commandInput, "!") {
			c.SetNotice("Commands open when the session is live; the draft is kept.")
			return ChatActionNone{}
		}
		for _, image := range c.inputImages {
			if image.Image.isPlaceholder() {
				c.SetNotice("An image attachment failed; remove its marker or attach it again")
				return ChatActionNone{}
			}
		}
		id, err := client.NewCommandID("standby")
		if err != nil {
			c.SetNotice(fmt.Sprintf("Could not identify the prompt: %v", err))
			return ChatActionNone{}
		}
		var origin *string
		if move, ok := c.composerRuntime.origin.(promptOriginMove); ok {
			moveID := string(move)
			origin = &moveID
		}
		if c.composerRuntime.previewOrigins == nil {
			c.composerRuntime.previewOrigins = map[string]*string{}
		}
		c.composerRuntime.previewOrigins[id] = origin
		payload := c.draftPayload().Trimmed()
		c.clearInput()
		images := payload.Images
		if images == nil {
			images = []PromptImage{}
		}
		c.queuedPrompts = append(c.queuedPrompts, QueuedPrompt{
			id:                     id,
			text:                   payload.Text,
			kind:                   core.QueuedCommandKindPrompt{},
			images:                 images,
			attachmentsUnsupported: false,
		})
		return ChatActionPrompt{Text: prompt}
	}
	if _, goal := parsedCommand.(localCommandGoalControl); c.planCommandPending && !(parsed && goal) {
		c.SetNotice("A plan-mode transition is still in progress")
		return ChatActionNone{}
	}
	if len(c.inputImages) > 0 &&
		(strings.HasPrefix(commandInput, "!") || (parsed && parsedCommand != localCommand(localCommandAttach{}))) {
		c.SetNotice("Send the image with a message, or delete its marker before using a command")
		return ChatActionNone{}
	}
	if command, ok := strings.CutPrefix(prompt, "!"); ok {
		if strings.TrimSpace(command) == "" {
			c.SetNotice("usage: !<bash command>")
			return ChatActionNone{}
		}
		if c.workerClosing() {
			c.SetNotice("The worker is closing; this shell command was not sent")
			return ChatActionNone{}
		}
		c.RecordPromptHistory(prompt)
		c.clearInput()
		return ChatActionRunShell{Command: command}
	}
	if parsed {
		return c.submitLocalCommand(parsedCommand, args, prompt, commandInput)
	}
	if name, ok := slashCommandName(commandInput); ok {
		known := false
		for _, choice := range c.commandChoices {
			fields := strings.Fields(choice.name)
			if textASCIIEqualFold(choice.name, name) || (len(fields) > 0 && textASCIIEqualFold(fields[0], name)) {
				known = true
				break
			}
		}
		if !known {
			c.SetNotice(fmt.Sprintf("/%s is not a Mjolnir or agent command; it was not sent", name))
			return ChatActionNone{}
		}
	}
	return c.submitPrompt(prompt)
}

// submitLocalCommand runs a slash command the chat handles itself (the
// `match command` arm of Rust's submit_draft).
func (c *ChatState) submitLocalCommand(command localCommand, args string, prompt string, commandInput string) ChatAction {
	switch command := command.(type) {
	case localCommandGoalControl:
		action := core.GoalControlAction(command)
		if !c.goalState.Supports(action) {
			message := fmt.Sprintf("/goal %s is not supported by this adapter.", action.AsStr())
			if c.goalState.Supports(core.GoalControlActionClear) {
				message += " Use /goal clear to remove the goal."
			}
			c.SetNotice(message)
			return ChatActionNone{}
		}
		if c.workerClosing() {
			c.SetNotice("The worker is closing; the goal command was not sent")
			return ChatActionNone{}
		}
		c.RecordPromptHistory(prompt)
		c.clearInput()
		return ChatActionGoalControl{Action: action}
	case localCommandClear:
		if !c.clearContextSupported {
			c.SetNotice("This worker does not support /clear; update the worker or start a new session")
			return ChatActionNone{}
		}
		if args != "" {
			c.SetNotice("usage: /clear")
			return ChatActionNone{}
		}
		if c.phase != core.WorkerPhaseIdle || len(c.queuedPrompts) > 0 {
			c.SetNotice("/clear requires an idle session with no queued work")
			return ChatActionNone{}
		}
		return c.submitPrompt(prompt)
	case localCommandHelp:
		c.clearInput()
		c.showHelp()
		return ChatActionNone{}
	case localCommandDetach:
		// There is no second screen to return to any more, so /detach now
		// means what the word says: leave Hel with the session still running
		// on its target.
		c.clearInput()
		return ChatActionQuitDetach{}
	case localCommandModel, localCommandEffort:
		key := "effort"
		if _, model := command.(localCommandModel); model {
			key = "model"
		}
		if args == "" {
			if c.workerClosing() {
				c.SetNotice("The worker is closing; this configuration change was not sent")
				return ChatActionNone{}
			}
			if c.openConfigPicker(key) {
				c.clearInput()
			} else {
				c.SetNotice(fmt.Sprintf("The agent does not advertise %s values; usage: /%s <value>", key, key))
			}
			return ChatActionNone{}
		}
		if c.workerClosing() {
			c.SetNotice("The worker is closing; this configuration change was not sent")
			return ChatActionNone{}
		}
		// A harness with no selector for this key cannot apply the change at
		// all. Say so now: sending it would report the relay's acceptance in
		// the footer and leave the harness's refusal to arrive in the
		// transcript seconds later.
		if len(c.advertisedConfigValues(key)) == 0 {
			c.clearInput()
			c.SetNotice(core.MissingConfigSelectorRefusal(c.acpSurface.HarnessKind(), key))
			return ChatActionNone{}
		}
		// A busy agent does not refuse the change: it waits in the command
		// queue and applies when its turn comes.
		c.clearInput()
		return ChatActionSetConfig{Key: key, Value: args}
	case localCommandConfig:
		if c.workerClosing() {
			c.SetNotice("The worker is closing; this configuration change was not sent")
			return ChatActionNone{}
		}
		args = strings.TrimSpace(args)
		if args == "" {
			if c.openConfigMenu() {
				c.clearInput()
			} else {
				c.SetNotice("The agent offers no configurable session settings")
			}
			return ChatActionNone{}
		}
		key, value, hasValue := splitOnceWhitespace(args)
		value = strings.TrimSpace(value)
		if !c.configKeyUserSettable(key) {
			c.clearInput()
			c.SetNotice(c.policyConfigNotice())
			return ChatActionNone{}
		}
		values := c.configValues(key)
		if len(values) == 0 {
			c.clearInput()
			c.SetNotice(fmt.Sprintf("This agent does not advertise %s; run /config to list its settings", key))
			return ChatActionNone{}
		}
		if !hasValue || value == "" {
			if c.openConfigValueDialog(key) {
				c.clearInput()
			} else {
				c.SetNotice(fmt.Sprintf("The agent no longer advertises values for %s", key))
			}
			return ChatActionNone{}
		}
		offered := false
		names := make([]string, 0, len(values))
		for _, choice := range values {
			offered = offered || choice.Value == value
			names = append(names, choice.Value)
		}
		if !offered {
			c.clearInput()
			c.SetNotice(fmt.Sprintf("The agent does not offer %s for %s; choices: %s",
				core.RustDebugString(value), key, strings.Join(names, ", ")))
			return ChatActionNone{}
		}
		c.clearInput()
		return ChatActionSetConfig{Key: key, Value: value}
	case localCommandFast:
		if args != "" {
			c.SetNotice("usage: /fast")
			return ChatActionNone{}
		}
		if !c.supportsFastMode() {
			c.SetNotice("Fast mode is unavailable for the active Codex model")
			return ChatActionNone{}
		}
		if c.workerClosing() {
			c.SetNotice("The worker is closing; this configuration change was not sent")
			return ChatActionNone{}
		}
		value := "on"
		if c.fastModeActive() {
			value = "off"
		}
		c.clearInput()
		return ChatActionSetConfig{Key: "fast-mode", Value: value}
	case localCommandPlan:
		if c.acpSurface.ForwardsPlanCommand() {
			return c.submitPromptWithHistory(prompt, prompt)
		}
		var requested bool
		var followup *string
		switch textASCIILower(args) {
		case "":
			requested = !c.planModeActive()
		case "on":
			requested = true
		case "off":
			requested = false
		default:
			requested = true
			owned := args
			followup = &owned
		}
		if c.phase != core.WorkerPhaseIdle {
			c.SetNotice("/plan is only available while the agent is idle")
			return ChatActionNone{}
		}
		if requested && c.planModeActive() {
			if followup != nil {
				return c.submitPromptWithHistory(*followup, prompt)
			}
			c.RecordPromptHistory(prompt)
			c.clearInput()
			c.SetNotice("Plan mode is already on")
			return ChatActionNone{}
		}
		if !requested && !c.planModeActive() && textASCIIEqualFold(args, "off") {
			c.RecordPromptHistory(prompt)
			c.clearInput()
			c.SetNotice("Plan mode is already off")
			return ChatActionNone{}
		}
		control, err := c.planControl(requested)
		if err != nil {
			c.SetNotice(err.Error())
			return ChatActionNone{}
		}
		c.RecordPromptHistory(prompt)
		c.clearInput()
		c.beginPlanModeChange(requested)
		c.planCommandPending = true
		return ChatActionPlanCommand{Original: prompt, Control: control, RequestedActive: requested, Prompt: followup}
	case localCommandReview:
		c.RecordPromptHistory(prompt)
		c.clearInput()
		review := c.ReviewConfig()
		switch textASCIILower(strings.TrimSpace(args)) {
		case "":
			// Bare `/review` reviews committed work, whether or not
			// automatic review is armed.
			return ChatActionStartTurnReview{}
		case "on", "off":
			// Arming is configuration, not a session gesture: a slash command
			// that edited config.toml would change a machine-wide setting
			// from inside one conversation.
			c.SetNotice("review runs before push when reviewers are set in Settings › Code Review")
			return ChatActionNone{}
		case "status":
			if c.reviewUnavailable != nil {
				c.SetNotice(*c.reviewUnavailable)
			} else {
				c.SetNotice(ReviewStatusLine(&review, c.hasTurnReview()))
			}
			return ChatActionNone{}
		}
		c.SetNotice("usage: /review [status]")
		return ChatActionNone{}
	case localCommandAttach:
		if !c.promptImagesSupported {
			c.SetNotice(attachUnsupportedNotice)
			return ChatActionNone{}
		}
		if !c.allowImageAttachment() {
			return ChatActionNone{}
		}
		if args == "" {
			c.SetNotice("usage: /attach <path>")
			return ChatActionNone{}
		}
		if c.workerClosing() {
			c.SetNotice("The worker is closing; this attachment was not processed")
			return ChatActionNone{}
		}
		if len(c.inputImages) >= core.MaxImages {
			c.SetNotice(fmt.Sprintf("A prompt can contain at most %d images", core.MaxImages))
			return ChatActionNone{}
		}
		path := pathArgument(args)
		attachCommand := commandInput
		c.RecordPromptHistory(attachCommand)
		if len(c.inputImages) == 0 {
			c.clearInput()
		} else {
			slashStart := -1
			for _, r := range untrackedRanges(c.input, c.inputImages) {
				if offset := strings.IndexFunc(c.input[r.Start:r.End], func(character rune) bool {
					return !unicode.IsSpace(character)
				}); offset >= 0 {
					slashStart = r.Start + offset
					break
				}
			}
			if slashStart >= 0 {
				ranges := untrackedRanges(c.input, c.inputImages)
				for index := len(ranges) - 1; index >= 0; index-- {
					r := ranges[index]
					start := max(r.Start, slashStart)
					if start < r.End {
						empty := PromptPayloadText("")
						c.replaceInputRange(ByteRange{Start: start, End: r.End}, &empty)
					}
				}
			}
		}
		return ChatActionAttach{Path: path, Command: attachCommand}
	case localCommandImplement:
		if _, err := c.planControl(false); err != nil {
			c.SetNotice(err.Error())
			return ChatActionNone{}
		}
		instruction := args
		if args == "" {
			instruction = "Implement the approved plan."
		}
		if !c.planModeActive() {
			return c.submitPromptWithHistory(instruction, prompt)
		}
		if c.phase != core.WorkerPhaseIdle {
			c.SetNotice("/implement is only available while the agent is idle")
			return ChatActionNone{}
		}
		control, err := c.planControl(false)
		if err != nil {
			c.SetNotice(err.Error())
			return ChatActionNone{}
		}
		c.RecordPromptHistory(prompt)
		c.clearInput()
		c.beginPlanModeChange(false)
		c.planCommandPending = true
		return ChatActionPlanCommand{Original: prompt, Control: control, RequestedActive: false, Prompt: &instruction}
	}
	panic(errors.New("unhandled local command"))
}

func (c *ChatState) submitPrompt(prompt string) ChatAction {
	return c.submitPromptWithHistory(prompt, prompt)
}

func (c *ChatState) submitPromptWithHistory(prompt string, history string) ChatAction {
	if c.workerClosing() {
		c.SetNotice("The worker is closing; this prompt was not sent")
		return ChatActionNone{}
	}
	if history != "" {
		c.RecordPromptHistory(history)
	}
	payload := PromptPayloadText(prompt)
	if len(c.inputImages) > 0 {
		payload = c.draftPayload().Trimmed()
	}
	for _, image := range payload.Images {
		if image.Image.isPlaceholder() {
			c.SetNotice("Remove failed images or wait for attachments to finish")
			return ChatActionNone{}
		}
	}
	c.submittingImages = payload.Images
	c.clearInput()
	return ChatActionPrompt{Text: payload.Text}
}
