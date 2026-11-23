// Port of mj-chat/src/chat/input_state.rs.

package chat

import (
	"strings"
	"unicode"

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
	panic("unported: mj-chat/src/chat/input_state.rs clipboardTargetsEqual")
}

func (c *ChatState) ApplyEvents(events []core.SequencedEvent) {
	panic("unported: mj-chat/src/chat/input_state.rs ApplyEvents")
}

func (c *ChatState) resetInteraction() {
	panic("unported: mj-chat/src/chat/input_state.rs resetInteraction")
}

func (c *ChatState) setInput(input string) {
	c.setInputPayload(PromptPayloadText(input))
}

func (c *ChatState) setInputPayload(payload PromptPayload) {
	panic("unported: mj-chat/src/chat/input_state.rs setInputPayload")
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
	panic("unported: mj-chat/src/chat/input_state.rs restoreDraft")
}

func (c *ChatState) handleClipboardContent(content ClipboardContent) {
	panic("unported: mj-chat/src/chat/input_state.rs handleClipboardContent")
}

func (c *ChatState) reserveAttachment(sequence uint64) bool {
	panic("unported: mj-chat/src/chat/input_state.rs reserveAttachment")
}

// finishAttachment lands an attachment job's result: ready is the image when
// err is nil.
func (c *ChatState) finishAttachment(sequence uint64, ready ClipboardImage, err error, fallback attachmentFallback) {
	panic("unported: mj-chat/src/chat/input_state.rs finishAttachment")
}

func (c *ChatState) setPromptImagesSupported(supported bool) {
	if c.promptImagesSupported != supported {
		c.promptImagesSupported = supported
		// Invalidate reads made under the previous capability policy.
		c.inputGeneration++
	}
}

func (c *ChatState) elicitationClipboardTarget() clipboardTarget {
	panic("unported: mj-chat/src/chat/input_state.rs elicitationClipboardTarget")
}

func (c *ChatState) clipboardTarget() clipboardTarget {
	panic("unported: mj-chat/src/chat/input_state.rs clipboardTarget")
}

func (c *ChatState) clipboardIsTextOnly() bool {
	return !(c.standby || c.promptImagesSupported) ||
		c.clipboardTarget() != clipboardTarget(clipboardTargetComposer{})
}

func (c *ChatState) allowImageAttachment() bool {
	panic("unported: mj-chat/src/chat/input_state.rs allowImageAttachment")
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
	panic("unported: mj-chat/src/chat/input_state.rs encodedDraft")
}

func (c *ChatState) editLatestQueuedPrompt() ChatAction {
	panic("unported: mj-chat/src/chat/input_state.rs editLatestQueuedPrompt")
}

func (c *ChatState) failQueuedPromptRemoval(id string, text string, kind core.QueuedCommandKind) {
	panic("unported: mj-chat/src/chat/input_state.rs failQueuedPromptRemoval")
}

// QueuedPromptTexts is the texts of the queued-prompt previews, oldest
// first.
func (c *ChatState) QueuedPromptTexts() []string {
	panic("unported: mj-chat/src/chat/input_state.rs QueuedPromptTexts")
}

// RemoveQueuedPromptText removes the oldest queued-prompt preview with this
// text and says whether one was there. Used when a queued prompt could not
// be handed to the daemon and its text goes back into the draft.
func (c *ChatState) RemoveQueuedPromptText(text string) bool {
	panic("unported: mj-chat/src/chat/input_state.rs RemoveQueuedPromptText")
}

func (c *ChatState) submitInput() ChatAction {
	panic("unported: mj-chat/src/chat/input_state.rs submitInput")
}

func (c *ChatState) submitDraft() ChatAction {
	panic("unported: mj-chat/src/chat/input_state.rs submitDraft")
}

func (c *ChatState) submitPrompt(prompt string) ChatAction {
	return c.submitPromptWithHistory(prompt, prompt)
}

func (c *ChatState) submitPromptWithHistory(prompt string, history string) ChatAction {
	panic("unported: mj-chat/src/chat/input_state.rs submitPromptWithHistory")
}
