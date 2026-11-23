// Port of mj-chat/src/chat/attachments.rs.

// Inline image attachments for the chat composer.
//
// Images are represented by byte ranges into PromptPayload.Text. The marker
// text is part of that string, but only the tracked ranges carry image
// meaning; arbitrary occurrences of `[image N]` remain ordinary text.

package chat

import (
	"context"
	"encoding/json"
	"strconv"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ChatDraftPrefix is the prefix used for drafts whose payload is encoded as
// JSON.
const ChatDraftPrefix = core.ChatDraftPrefix

// ByteRange is a half-open byte range into a string (Rust's Range<usize>),
// serialized as serde serializes a Range: {"start":..,"end":..}.
type ByteRange struct {
	Start int `json:"start"`
	End   int `json:"end"`
}

// UnmarshalJSON requires both bounds, as serde does.
func (r *ByteRange) UnmarshalJSON(data []byte) error {
	if err := clipboardRequireFields(data, "start", "end"); err != nil {
		return err
	}
	type plain ByteRange
	return json.Unmarshal(data, (*plain)(r))
}

func (r ByteRange) Len() int {
	if r.End < r.Start {
		return 0
	}
	return r.End - r.Start
}

func (r ByteRange) IsEmpty() bool {
	return r.End <= r.Start
}

// PromptPayload is the text and inline images currently in the composer.
type PromptPayload struct {
	Text   string        `json:"text"`
	Images []PromptImage `json:"images"`
}

// MarshalJSON writes images as an array even when there are none, as serde
// writes an empty Vec.
func (p PromptPayload) MarshalJSON() ([]byte, error) {
	images := p.Images
	if images == nil {
		images = []PromptImage{}
	}
	return json.Marshal(struct {
		Text   string        `json:"text"`
		Images []PromptImage `json:"images"`
	}{p.Text, images})
}

// UnmarshalJSON accepts the historical single-image form as well as the
// current tracked-image form, validating either.
func (p *PromptPayload) UnmarshalJSON(data []byte) error {
	if err := clipboardRequireFields(data, "text"); err != nil {
		return err
	}
	var wire promptPayloadWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	decoded, err := decodeWire(wire)
	if err != nil {
		return err
	}
	*p = decoded
	return nil
}

// Equal is Rust's derived PartialEq.
func (p *PromptPayload) Equal(other *PromptPayload) bool {
	if p.Text != other.Text || len(p.Images) != len(other.Images) {
		return false
	}
	for index := range p.Images {
		if !p.Images[index].Equal(&other.Images[index]) {
			return false
		}
	}
	return true
}

// PromptImage is one image and the marker that displays it in
// PromptPayload.Text.
type PromptImage struct {
	Range  ByteRange      `json:"range"`
	Number uint64         `json:"number"`
	Image  ClipboardImage `json:"image"`
}

// UnmarshalJSON requires every field, as serde does.
func (i *PromptImage) UnmarshalJSON(data []byte) error {
	if err := clipboardRequireFields(data, "range", "number", "image"); err != nil {
		return err
	}
	type plain PromptImage
	return json.Unmarshal(data, (*plain)(i))
}

// Equal is Rust's derived PartialEq.
func (i *PromptImage) Equal(other *PromptImage) bool {
	return i.Range == other.Range && i.Number == other.Number && i.Image.Equal(&other.Image)
}

type promptPayloadWire struct {
	Text   string          `json:"text"`
	Images *[]PromptImage  `json:"images"`
	Image  *ClipboardImage `json:"image"`
}

// PromptPayloadText builds a text-only payload.
func PromptPayloadText(text string) PromptPayload {
	return PromptPayload{Text: text, Images: []PromptImage{}}
}

// PromptPayloadWithImage builds a payload with one image marker appended
// after text.
func PromptPayloadWithImage(text string, image ClipboardImage) PromptPayload {
	start := len(text)
	text += marker(1)
	return PromptPayload{
		Text:   text,
		Images: []PromptImage{{Range: ByteRange{Start: start, End: start + len(marker(1))}, Number: 1, Image: image}},
	}
}

// ContentBlocks builds ACP content while omitting the inline marker bytes
// from text blocks. Only tracked ranges are interpreted as images.
func (p *PromptPayload) ContentBlocks() []schema.ContentBlock {
	panic("unported: mj-chat/src/chat/attachments.rs ContentBlocks")
}

// EncodeDraft encodes a draft for a session record. Text-only drafts retain
// their historical plain-text representation unless the text could be
// mistaken for an encoded envelope.
func (p *PromptPayload) EncodeDraft() string {
	panic("unported: mj-chat/src/chat/attachments.rs EncodeDraft")
}

// PromptPayloadDecodeDraft decodes a session record draft, accepting the
// historical plain-text and single-image forms as well as the current
// tracked-image form.
func PromptPayloadDecodeDraft(draft string) (PromptPayload, error) {
	panic("unported: mj-chat/src/chat/attachments.rs PromptPayloadDecodeDraft")
}

// Trimmed removes leading and trailing whitespace while keeping image ranges
// in sync. Marker bytes themselves are never treated as whitespace.
func (p PromptPayload) Trimmed() PromptPayload {
	panic("unported: mj-chat/src/chat/attachments.rs Trimmed")
}

func decodeWire(wire promptPayloadWire) (PromptPayload, error) {
	panic("unported: mj-chat/src/chat/attachments.rs decodeWire")
}

func validateImages(text string, images []PromptImage) (PromptPayload, error) {
	panic("unported: mj-chat/src/chat/attachments.rs validateImages")
}

func validateImage(image ClipboardImage) (ClipboardImage, error) {
	panic("unported: mj-chat/src/chat/attachments.rs validateImage")
}

// installClipboardImage prepares client image bytes for upload. Installation
// belongs to the daemon and must complete before the prompt carries an
// opaque reference. Filesystem and codec work belongs in the caller's
// blocking task.
func installClipboardImage(sessionID string, image ClipboardImage) (ClipboardImage, error) {
	panic("unported: mj-chat/src/chat/attachments.rs installClipboardImage")
}

// installPath reads and optimizes an image named by `/attach` or a pasted
// path for upload.
func installPath(sessionID string, path string) (ClipboardImage, error) {
	panic("unported: mj-chat/src/chat/attachments.rs installPath")
}

func installImageBytes(sessionID string, bytes []byte) (ClipboardImage, error) {
	panic("unported: mj-chat/src/chat/attachments.rs installImageBytes")
}

// uploadPreparedImage uploads prepared bytes on the attachment task's
// cancellable connection.
func uploadPreparedImage(ctx context.Context, sessionID string, image ClipboardImage) (ClipboardImage, error) {
	panic("unported: mj-chat/src/chat/attachments.rs uploadPreparedImage")
}

func marker(number uint64) string {
	return "[image " + strconv.FormatUint(number, 10) + "]"
}

// textWithoutImages returns the composer text after removing the marker
// ranges that carry image meaning. This is used only for recognizing a
// command typed beside existing attachments; the original text and image
// ranges remain untouched.
func textWithoutImages(text string, images []PromptImage) string {
	plain := ""
	for _, r := range untrackedRanges(text, images) {
		plain += text[r.Start:r.End]
	}
	return plain
}

// untrackedRanges returns the portions of a composer that are ordinary text
// rather than tracked image markers. Ranges are in the input's original
// coordinates and are therefore safe to remove from the end toward the
// beginning.
func untrackedRanges(text string, images []PromptImage) []ByteRange {
	ranges := make([]ByteRange, 0, len(images)+1)
	cursor := 0
	for _, image := range images {
		if cursor < image.Range.Start {
			ranges = append(ranges, ByteRange{Start: cursor, End: image.Range.Start})
		}
		cursor = image.Range.End
	}
	if cursor < len(text) {
		ranges = append(ranges, ByteRange{Start: cursor, End: len(text)})
	}
	return ranges
}

// replaceRange replaces a byte range in a composer, treating tracked markers
// as atomic. The returned payload is the removed range with image ranges made
// relative to that payload; the int is the cursor after the inserted text.
func replaceRange(text *string, images *[]PromptImage, r ByteRange, inserted *PromptPayload) (int, PromptPayload) {
	panic("unported: mj-chat/src/chat/attachments.rs replaceRange")
}

func assignExistingNumbers(images []PromptImage, used map[uint64]struct{}, nextNumber *uint64) {
	for index := range images {
		image := &images[index]
		if _, taken := used[image.Number]; image.Number == 0 || taken {
			image.Number = allocateNumber(used, nextNumber)
		}
		used[image.Number] = struct{}{}
	}
}

func allocateNumber(used map[uint64]struct{}, nextNumber *uint64) uint64 {
	candidate := max(*nextNumber, 1)
	for {
		if _, taken := used[candidate]; !taken {
			*nextNumber = max(candidate+1, 1)
			return candidate
		}
		candidate = max(candidate+1, 1)
	}
}

func renderPart(text string, images []PromptImage) (string, []PromptImage) {
	panic("unported: mj-chat/src/chat/attachments.rs renderPart")
}

func shiftImages(images []PromptImage, amount int) {
	for index := range images {
		images[index].Range.Start += amount
		images[index].Range.End += amount
	}
}

func floorCharBoundary(text string, position int) int {
	for position > 0 && position < len(text) && !attachmentsIsCharBoundary(text, position) {
		position--
	}
	return position
}

// attachmentsIsCharBoundary is Rust's `str::is_char_boundary` for an in-range offset.
func attachmentsIsCharBoundary(text string, position int) bool {
	if position == 0 || position == len(text) {
		return true
	}
	if position < 0 || position > len(text) {
		return false
	}
	return text[position]&0xC0 != 0x80
}

// snapCursor snaps a cursor that lies strictly inside a marker to the marker
// boundary.
func snapCursor(images []PromptImage, position int, direction int) int {
	for _, image := range images {
		if image.Range.Start < position && position < image.Range.End {
			if direction < 0 {
				return image.Range.Start
			}
			return image.Range.End
		}
	}
	return position
}

// insertImage inserts one tracked marker at position, returning the cursor
// after it.
func insertImage(text *string, images *[]PromptImage, position int, number uint64, image ClipboardImage) int {
	panic("unported: mj-chat/src/chat/attachments.rs insertImage")
}
