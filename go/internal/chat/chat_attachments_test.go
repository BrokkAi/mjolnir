// Tests of mj-chat/src/chat/attachments.rs.

package chat

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"image"
	"image/color"
	"image/png"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func attachmentImage(tag string) ClipboardImage {
	return ClipboardImage{DataBase64: tag, MimeType: "image/png"}
}

func validAttachmentImage(t *testing.T) ClipboardImage {
	t.Helper()
	pixels := image.NewNRGBA(image.Rect(0, 0, 1, 1))
	pixels.SetNRGBA(0, 0, color.NRGBA{R: 255, A: 255})
	var encoded bytes.Buffer
	if err := png.Encode(&encoded, pixels); err != nil {
		t.Fatal(err)
	}
	image, err := ClipboardImageFromPNGBase64(base64.StdEncoding.EncodeToString(encoded.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	return image
}

func tracked(text string, number uint64, image ClipboardImage) PromptPayload {
	start := strings.Index(text, marker(number))
	if start < 0 {
		panic("marker missing")
	}
	return PromptPayload{
		Text:   text,
		Images: []PromptImage{{Range: ByteRange{Start: start, End: start + len(marker(number))}, Number: number, Image: image}},
	}
}

func TestUnicodeOffsetsAreBytes(t *testing.T) {
	text := "λ"
	var images []PromptImage
	cursor := insertImage(&text, &images, len(text), 1, attachmentImage("a"))
	if text != "λ[image 1]" {
		t.Fatalf("text = %q", text)
	}
	if images[0].Range != (ByteRange{Start: 2, End: 11}) {
		t.Fatalf("range = %+v", images[0].Range)
	}
	if cursor != 11 {
		t.Fatalf("cursor = %d", cursor)
	}
	insertedText := "é"
	var insertedImages []PromptImage
	replacement := PromptPayloadText("x")
	_, removed := replaceRange(&insertedText, &insertedImages, ByteRange{Start: 0, End: 2}, &replacement)
	if removed.Text != "é" {
		t.Fatalf("removed = %q", removed.Text)
	}
	if insertedText != "x" {
		t.Fatalf("text = %q", insertedText)
	}
}

func TestDeletingAnyMarkerByteRemovesTheWholeMarker(t *testing.T) {
	payload := tracked("a[image 1]b", 1, attachmentImage("a"))
	markerStart := payload.Images[0].Range.Start
	markerEnd := payload.Images[0].Range.End
	text := payload.Text
	images := append([]PromptImage(nil), payload.Images...)
	empty := PromptPayloadText("")
	_, removed := replaceRange(&text, &images, ByteRange{Start: markerEnd - 1, End: markerEnd}, &empty)
	if text != "ab" {
		t.Fatalf("text = %q", text)
	}
	if len(images) != 0 {
		t.Fatalf("images = %+v", images)
	}
	if removed.Text != "[image 1]" {
		t.Fatalf("removed = %q", removed.Text)
	}
	if removed.Images[0].Range != (ByteRange{Start: 0, End: markerEnd - markerStart}) {
		t.Fatalf("removed range = %+v", removed.Images[0].Range)
	}

	text = payload.Text
	images = append([]PromptImage(nil), payload.Images...)
	replaceRange(&text, &images, ByteRange{Start: markerStart, End: markerStart + 1}, &empty)
	if text != "ab" {
		t.Fatalf("text = %q", text)
	}
	if len(images) != 0 {
		t.Fatalf("images = %+v", images)
	}
}

func TestLiteralMarkerTextIsNotAnImage(t *testing.T) {
	payload := PromptPayloadText("[image 1]")
	blocks := payload.ContentBlocks()
	if len(blocks) != 1 || blocks[0].Text == nil || blocks[0].Text.Text != "[image 1]" {
		t.Fatalf("blocks = %+v", blocks)
	}
}

func TestReplacementPreservesBytesAndReassignsYankedNumbers(t *testing.T) {
	current := tracked("a[image 1]b", 1, attachmentImage("old"))
	yanked := tracked("[image 1]", 1, attachmentImage("new"))
	text := current.Text
	images := current.Images
	replaceRange(&text, &images, ByteRange{Start: 0, End: 1}, &yanked)
	if text != "[image 2][image 1]b" {
		t.Fatalf("text = %q", text)
	}
	if len(images) != 2 || images[0].Number != 2 || images[1].Number != 1 {
		t.Fatalf("images = %+v", images)
	}
	if images[0].Range != (ByteRange{Start: 0, End: 9}) || images[1].Range != (ByteRange{Start: 9, End: 18}) {
		t.Fatalf("ranges = %+v, %+v", images[0].Range, images[1].Range)
	}
}

func TestTrimmingUnicodeWhitespacePreservesImageRanges(t *testing.T) {
	payload := PromptPayloadWithImage("  λ ", attachmentImage("one"))
	payload.Text += " \n"
	trimmed := payload.Trimmed()
	if trimmed.Text != "λ [image 1]" {
		t.Fatalf("text = %q", trimmed.Text)
	}
	if trimmed.Images[0].Range != (ByteRange{Start: 3, End: 12}) {
		t.Fatalf("range = %+v", trimmed.Images[0].Range)
	}
	blocks := trimmed.ContentBlocks()
	if len(blocks) != 2 || blocks[0].Text == nil || blocks[0].Text.Text != "λ " || blocks[1].Image == nil {
		t.Fatalf("blocks = %+v", blocks)
	}
}

func TestMalformedSavedMarkerRangesFailInsteadOfLosingImages(t *testing.T) {
	payload := PromptPayloadWithImage("λ ", validAttachmentImage(t))
	for _, invalidRange := range []ByteRange{{Start: 1, End: 12}, {Start: 3, End: 13}, {Start: 3, End: 4}} {
		invalid := payload
		invalid.Images = append([]PromptImage(nil), payload.Images...)
		invalid.Images[0].Range = invalidRange
		if _, err := PromptPayloadDecodeDraft(invalid.EncodeDraft()); err == nil {
			t.Fatalf("range %+v decoded", invalidRange)
		}
	}
}

func TestOldAndNewDraftEnvelopesDecode(t *testing.T) {
	image := validAttachmentImage(t)
	old, err := json.Marshal(map[string]any{"text": "look", "image": image})
	if err != nil {
		t.Fatal(err)
	}
	oldPayload, err := PromptPayloadDecodeDraft(ChatDraftPrefix + string(old))
	if err != nil {
		t.Fatal(err)
	}
	if oldPayload.Text != "look[image 1]" {
		t.Fatalf("text = %q", oldPayload.Text)
	}
	if oldPayload.Images[0].Range != (ByteRange{Start: 4, End: 13}) {
		t.Fatalf("range = %+v", oldPayload.Images[0].Range)
	}

	current := PromptPayloadWithImage("look", validAttachmentImage(t))
	decoded, err := PromptPayloadDecodeDraft(current.EncodeDraft())
	if err != nil {
		t.Fatal(err)
	}
	if !decoded.Equal(&current) {
		t.Fatalf("decoded = %+v", decoded)
	}
	literal := PromptPayloadText(ChatDraftPrefix + "literal")
	decoded, err = PromptPayloadDecodeDraft(literal.EncodeDraft())
	if err != nil {
		t.Fatal(err)
	}
	if !decoded.Equal(&literal) {
		t.Fatalf("decoded = %+v", decoded)
	}
}

func TestReferenceImagesUseReservedURIAndDraftsCarryNoBytes(t *testing.T) {
	inline := validAttachmentImage(t)
	data, err := base64.StdEncoding.DecodeString(inline.DataBase64)
	if err != nil {
		t.Fatal(err)
	}
	reference, err := core.NewAttachmentRef(data, "image/png", 1, 1)
	if err != nil {
		t.Fatal(err)
	}
	image, err := ClipboardImageFromReference(reference)
	if err != nil {
		t.Fatal(err)
	}
	payload := PromptPayloadWithImage("look", image)
	blocks := payload.ContentBlocks()
	if len(blocks) != 2 || blocks[0].Text == nil || blocks[1].Image == nil {
		t.Fatalf("expected mixed text and reference image blocks, got %+v", blocks)
	}
	var content *schema.ImageContent = blocks[1].Image
	if blocks[0].Text.Text != "look" {
		t.Fatalf("text = %q", blocks[0].Text.Text)
	}
	if content.Data != "" {
		t.Fatalf("reference image carries data %q", content.Data)
	}
	if content.URI == nil || *content.URI != reference.URI() {
		t.Fatalf("uri = %v", content.URI)
	}
	encoded := payload.EncodeDraft()
	if strings.Contains(encoded, base64.StdEncoding.EncodeToString(data)) {
		t.Fatal("the draft carries the image bytes")
	}
	decoded, err := PromptPayloadDecodeDraft(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if !decoded.Equal(&payload) {
		t.Fatalf("decoded = %+v", decoded)
	}
}

func TestDraftsRejectMoreThanTenImages(t *testing.T) {
	image := validAttachmentImage(t)
	payload := PromptPayloadText("")
	for number := uint64(1); number <= 11; number++ {
		end := len(payload.Text)
		payload.Text += marker(number)
		payload.Images = append(payload.Images, PromptImage{
			Range:  ByteRange{Start: end, End: len(payload.Text)},
			Number: number,
			Image:  image,
		})
	}
	if _, err := PromptPayloadDecodeDraft(payload.EncodeDraft()); err == nil {
		t.Fatal("a draft with eleven images decoded")
	}
}
