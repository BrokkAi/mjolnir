// Port of mj-chat/src/chat/attachments.rs.

// Inline image attachments for the chat composer.
//
// Images are represented by byte ranges into PromptPayload.Text. The marker
// text is part of that string, but only the tracked ranges carry image
// meaning; arbitrary occurrences of `[image N]` remain ordinary text.

package chat

import (
	stdbytes "bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"sort"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/client"
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
//
// The blocks are the stable ACP schema the relay's prompt command carries
// (Rust's agent_client_protocol::v1::schema::ContentBlock).
func (p *PromptPayload) ContentBlocks() []schema.ContentBlock {
	blocks := make([]schema.ContentBlock, 0, len(p.Images)*2+1)
	cursor := 0
	for _, image := range p.Images {
		if !(image.Range.Start >= cursor && image.Range.Start < image.Range.End &&
			image.Range.End <= len(p.Text) &&
			attachmentsIsCharBoundary(p.Text, image.Range.Start) &&
			attachmentsIsCharBoundary(p.Text, image.Range.End)) {
			panic("invalid tracked image range")
		}
		if cursor < image.Range.Start {
			blocks = append(blocks, attachmentTextBlock(p.Text[cursor:image.Range.Start]))
		}
		if image.Image.isPlaceholder() {
			blocks = append(blocks, attachmentTextBlock(p.Text[image.Range.Start:image.Range.End]))
			cursor = image.Range.End
			continue
		}
		if image.Image.Reference != nil {
			// core owns the reference block; carry its image fields over to
			// the stable schema.
			reference := image.Image.Reference.ContentBlock()
			blocks = append(blocks, schema.ContentBlock{
				Kind: schema.ContentBlockKindImage,
				Image: &schema.ImageContent{
					Data:     reference.Image.Data,
					MimeType: reference.Image.MimeType,
					URI:      reference.Image.URI,
				},
			})
		} else {
			blocks = append(blocks, schema.ContentBlock{
				Kind:  schema.ContentBlockKindImage,
				Image: &schema.ImageContent{Data: image.Image.DataBase64, MimeType: image.Image.MimeType},
			})
		}
		cursor = image.Range.End
	}
	if cursor < len(p.Text) {
		blocks = append(blocks, attachmentTextBlock(p.Text[cursor:]))
	}
	return blocks
}

func attachmentTextBlock(text string) schema.ContentBlock {
	return schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}
}

// EncodeDraft encodes a draft for a session record. Text-only drafts retain
// their historical plain-text representation unless the text could be
// mistaken for an encoded envelope.
func (p *PromptPayload) EncodeDraft() string {
	if len(p.Images) == 0 && !strings.HasPrefix(p.Text, ChatDraftPrefix) {
		return p.Text
	}
	body, err := json.Marshal(p)
	if err != nil {
		panic(fmt.Sprintf("prompt draft serialization cannot fail: %v", err))
	}
	return ChatDraftPrefix + string(body)
}

// PromptPayloadDecodeDraft decodes a session record draft, accepting the
// historical plain-text and single-image forms as well as the current
// tracked-image form.
func PromptPayloadDecodeDraft(draft string) (PromptPayload, error) {
	body, ok := strings.CutPrefix(draft, ChatDraftPrefix)
	if !ok {
		return PromptPayloadText(draft), nil
	}
	var payload PromptPayload
	if err := json.Unmarshal([]byte(body), &payload); err != nil {
		return PromptPayload{}, fmt.Errorf("decode chat draft envelope: %w", err)
	}
	return payload, nil
}

// Trimmed removes leading and trailing whitespace while keeping image ranges
// in sync. Marker bytes themselves are never treated as whitespace.
func (p PromptPayload) Trimmed() PromptPayload {
	notSpace := func(character rune) bool { return !unicode.IsSpace(character) }
	leading := strings.IndexFunc(p.Text, notSpace)
	if leading < 0 {
		leading = len(p.Text)
	}
	trailing := 0
	if index := strings.LastIndexFunc(p.Text, notSpace); index >= 0 {
		_, size := utf8.DecodeRuneInString(p.Text[index:])
		trailing = index + size
	}
	if leading == 0 && trailing == len(p.Text) {
		return p
	}
	if leading >= trailing {
		return PromptPayloadText("")
	}
	images := make([]PromptImage, 0, len(p.Images))
	for _, image := range p.Images {
		if !(image.Range.Start >= leading && image.Range.End <= trailing) {
			panic("image marker outside trimmed text")
		}
		image.Range = ByteRange{Start: image.Range.Start - leading, End: image.Range.End - leading}
		images = append(images, image)
	}
	return PromptPayload{Text: p.Text[leading:trailing], Images: images}
}

func decodeWire(wire promptPayloadWire) (PromptPayload, error) {
	switch {
	case wire.Images != nil && wire.Image != nil:
		return PromptPayload{}, errors.New("chat draft contains both images and image fields")
	case wire.Images != nil:
		return validateImages(wire.Text, *wire.Images)
	case wire.Image != nil:
		image, err := validateImage(*wire.Image)
		if err != nil {
			return PromptPayload{}, fmt.Errorf("validate image in chat draft: %w", err)
		}
		return PromptPayloadWithImage(wire.Text, image), nil
	}
	return PromptPayloadText(wire.Text), nil
}

func validateImages(text string, images []PromptImage) (PromptPayload, error) {
	if len(images) > core.MaxImages {
		return PromptPayload{}, fmt.Errorf("a message can contain at most %d images", core.MaxImages)
	}
	previousEnd := 0
	numbers := map[uint64]struct{}{}
	validated := make([]PromptImage, 0, len(images))
	for index, image := range images {
		if image.Number == 0 {
			return PromptPayload{}, fmt.Errorf("chat draft image %d has a non-positive number", index)
		}
		if _, duplicate := numbers[image.Number]; duplicate {
			return PromptPayload{}, fmt.Errorf("chat draft contains duplicate image number %d", image.Number)
		}
		numbers[image.Number] = struct{}{}
		if image.Range.Start >= image.Range.End || image.Range.Start < previousEnd ||
			image.Range.End > len(text) ||
			!attachmentsIsCharBoundary(text, image.Range.Start) ||
			!attachmentsIsCharBoundary(text, image.Range.End) {
			return PromptPayload{}, fmt.Errorf("chat draft image %d has an invalid range", index)
		}
		if text[image.Range.Start:image.Range.End] != marker(image.Number) {
			return PromptPayload{}, fmt.Errorf("chat draft image %d marker does not match its range", image.Number)
		}
		imageBytes, err := validateImage(image.Image)
		if err != nil {
			return PromptPayload{}, fmt.Errorf("validate image %d in chat draft: %w", image.Number, err)
		}
		previousEnd = image.Range.End
		validated = append(validated, PromptImage{Range: image.Range, Number: image.Number, Image: imageBytes})
	}
	return PromptPayload{Text: text, Images: validated}, nil
}

func validateImage(image ClipboardImage) (ClipboardImage, error) {
	if image.isPlaceholder() {
		return image, nil
	}
	if image.Reference != nil {
		if err := image.Reference.Validate(); err != nil {
			return ClipboardImage{}, err
		}
		if image.DataBase64 != "" {
			return ClipboardImage{}, errors.New("attachment references must not carry inline image bytes")
		}
		if image.MimeType != image.Reference.MimeType {
			return ClipboardImage{}, errors.New("attachment reference MIME type does not match image MIME type")
		}
		return image, nil
	}
	return ClipboardImageFromBase64(image.DataBase64, image.MimeType)
}

// installClipboardImage prepares client image bytes for upload. Installation
// belongs to the daemon and must complete before the prompt carries an
// opaque reference. Filesystem and codec work belongs in the caller's
// blocking task.
func installClipboardImage(sessionID string, image ClipboardImage) (ClipboardImage, error) {
	if image.isPlaceholder() {
		return ClipboardImage{}, errors.New("image attachment is still processing or has failed")
	}
	if image.Reference != nil {
		return validateImage(image)
	}
	bytes, err := base64.StdEncoding.Strict().DecodeString(image.DataBase64)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("decode clipboard image: %w", err)
	}
	return installImageBytes(sessionID, bytes)
}

// maxAttachmentInputBytes is the 64 MiB an attached file may hold before the
// optimizer reads it.
const maxAttachmentInputBytes = 64 * 1024 * 1024

// installPath reads and optimizes an image named by `/attach` or a pasted
// path for upload.
func installPath(sessionID string, path string) (ClipboardImage, error) {
	info, err := os.Lstat(path)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("inspect attachment %s: %w", path, err)
	}
	if !info.Mode().IsRegular() {
		return ClipboardImage{}, fmt.Errorf("attachment %s is not a regular file", path)
	}
	file, err := os.Open(path)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("read attachment %s: %w", path, err)
	}
	defer file.Close()
	bytes, err := io.ReadAll(io.LimitReader(file, maxAttachmentInputBytes+1))
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("read attachment %s: %w", path, err)
	}
	if len(bytes) > maxAttachmentInputBytes {
		return ClipboardImage{}, errors.New("image attachment exceeds the 64 MiB input limit")
	}
	image, err := installImageBytes(sessionID, bytes)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("%s could not be read as an image; only image files can be attached: %w", path, err)
	}
	return image, nil
}

func installImageBytes(sessionID string, bytes []byte) (ClipboardImage, error) {
	optimized, err := client.OptimizeImage(bytes)
	if err != nil {
		return ClipboardImage{}, fmt.Errorf("optimize image attachment: %w", err)
	}
	return ClipboardImageFromBase64(base64.StdEncoding.EncodeToString(optimized.Bytes), optimized.MimeType)
}

// uploadPreparedImage uploads prepared bytes on the attachment task's
// cancellable connection.
func uploadPreparedImage(ctx context.Context, sessionID string, image ClipboardImage) (ClipboardImage, error) {
	if image.Reference != nil {
		return image, nil
	}
	bytes, err := base64.StdEncoding.Strict().DecodeString(image.DataBase64)
	if err != nil {
		return ClipboardImage{}, err
	}
	optimized, err := client.OptimizeImage(bytes)
	if err != nil {
		return ClipboardImage{}, err
	}
	reference, err := core.NewAttachmentRef(bytes, image.MimeType, optimized.Width, optimized.Height)
	if err != nil {
		return ClipboardImage{}, err
	}
	daemon, err := client.ConnectExisting(ctx)
	if err != nil {
		return ClipboardImage{}, err
	}
	defer daemon.Close()
	reply, err := daemon.UploadAndConsume(ctx, stdbytes.NewReader(bytes), func(blob string) client.DaemonAction {
		return client.DaemonActionInstallAttachment{SessionID: sessionID, Reference: reference, Blob: blob}
	})
	if err != nil {
		return ClipboardImage{}, err
	}
	installed, ok := reply.(client.DaemonReplyAttachmentInstalled)
	if !ok {
		return ClipboardImage{}, fmt.Errorf("unexpected attachment upload reply %s", client.DaemonReplyDebug(reply))
	}
	return ClipboardImageFromReference(installed.Reference)
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
	start := floorCharBoundary(*text, min(r.Start, len(*text)))
	end := floorCharBoundary(*text, min(r.End, len(*text)))
	if start > end {
		start, end = end, start
	}

	if start == end {
		for _, image := range *images {
			if image.Range.Start < start && start < image.Range.End {
				start, end = image.Range.End, image.Range.End
				break
			}
		}
	} else {
		expandedStart, expandedEnd := start, end
		for _, image := range *images {
			if image.Range.End > expandedStart && image.Range.Start < expandedEnd {
				expandedStart = min(expandedStart, image.Range.Start)
				expandedEnd = max(expandedEnd, image.Range.End)
			}
		}
		start, end = expandedStart, expandedEnd
	}

	removedImages := []PromptImage{}
	for _, image := range *images {
		if image.Range.Start >= start && image.Range.End <= end {
			removedImages = append(removedImages, PromptImage{
				Range:  ByteRange{Start: image.Range.Start - start, End: image.Range.End - start},
				Number: image.Number,
				Image:  image.Image,
			})
		}
	}
	removed := PromptPayload{Text: (*text)[start:end], Images: removedImages}

	var before, after []PromptImage
	for _, image := range *images {
		if image.Range.End <= start {
			before = append(before, image)
		}
	}
	for _, image := range *images {
		if image.Range.Start >= end {
			after = append(after, PromptImage{
				Range:  ByteRange{Start: image.Range.Start - end, End: image.Range.End - end},
				Number: image.Number,
				Image:  image.Image,
			})
		}
	}
	insertedImages := append([]PromptImage(nil), inserted.Images...)

	// Existing IDs are stable whenever possible. Reserve all retained IDs
	// before assigning yanked IDs so an inserted duplicate always gets the
	// fresh number and its marker can be rewritten.
	var maxExisting uint64
	for _, image := range append(append([]PromptImage(nil), before...), after...) {
		if image.Number > 0 {
			maxExisting = max(maxExisting, image.Number)
		}
	}
	nextNumber := maxExisting + 1
	if maxExisting == ^uint64(0) {
		nextNumber = 1
	}
	used := map[uint64]struct{}{}
	assignExistingNumbers(before, used, &nextNumber)
	assignExistingNumbers(after, used, &nextNumber)
	for index := range insertedImages {
		image := &insertedImages[index]
		if _, taken := used[image.Number]; image.Number == 0 || taken {
			image.Number = allocateNumber(used, &nextNumber)
		}
		used[image.Number] = struct{}{}
	}

	beforeText, beforeImages := renderPart((*text)[:start], before)
	insertedText, insertedRendered := renderPart(inserted.Text, insertedImages)
	afterText, afterImages := renderPart((*text)[end:], after)

	beforeLen := len(beforeText)
	insertedLen := len(insertedText)
	shiftImages(insertedRendered, beforeLen)
	shiftImages(afterImages, beforeLen+insertedLen)
	beforeImages = append(beforeImages, insertedRendered...)
	beforeImages = append(beforeImages, afterImages...)

	*text = beforeText + insertedText + afterText
	*images = beforeImages
	return beforeLen + insertedLen, removed
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
	var output strings.Builder
	output.Grow(len(text))
	rendered := make([]PromptImage, 0, len(images))
	cursor := 0
	for _, image := range images {
		if !(image.Range.Start >= cursor && image.Range.Start < image.Range.End &&
			image.Range.End <= len(text) &&
			attachmentsIsCharBoundary(text, image.Range.Start) &&
			attachmentsIsCharBoundary(text, image.Range.End)) {
			panic("invalid tracked image range")
		}
		output.WriteString(text[cursor:image.Range.Start])
		start := output.Len()
		output.WriteString(marker(image.Number))
		rendered = append(rendered, PromptImage{
			Range:  ByteRange{Start: start, End: output.Len()},
			Number: image.Number,
			Image:  image.Image,
		})
		cursor = image.Range.End
	}
	output.WriteString(text[cursor:])
	return output.String(), rendered
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
	position = floorCharBoundary(*text, snapCursor(*images, min(position, len(*text)), 1))
	markerText := marker(number)
	markerLen := len(markerText)
	*text = (*text)[:position] + markerText + (*text)[position:]
	for index := range *images {
		existing := &(*images)[index]
		if existing.Range.Start >= position {
			existing.Range.Start += markerLen
			existing.Range.End += markerLen
		}
	}
	*images = append(*images, PromptImage{
		Range:  ByteRange{Start: position, End: position + markerLen},
		Number: number,
		Image:  image,
	})
	sort.SliceStable(*images, func(a, b int) bool {
		return (*images)[a].Range.Start < (*images)[b].Range.Start
	})
	return position + markerLen
}
