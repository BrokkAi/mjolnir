// Port of the tests in mj-core/src/attachment.rs.

package core

import (
	"bytes"
	"encoding/base64"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	acp "github.com/BrokkAi/acp-go/schema/unstable"
)

func attachmentTestPhoto() []byte {
	photo := []byte("\x89PNG\r\n\x1a\n")
	return append(photo, bytes.Repeat([]byte{7}, MaxImageBytes-len(photo))...)
}

func attachmentTestReference(t *testing.T, photo []byte) AttachmentRef {
	t.Helper()
	reference, err := NewAttachmentRef(photo, "image/png", 100, 100)
	if err != nil {
		t.Fatal(err)
	}
	return reference
}

func attachmentTestBlocks(reference *AttachmentRef, count int) []acp.ContentBlock {
	blocks := make([]acp.ContentBlock, count)
	for i := range blocks {
		blocks[i] = reference.ContentBlock()
	}
	return blocks
}

func TestTenLargeImagesResolveWithoutLargeControlMessages(t *testing.T) {
	store := NewAttachmentStore(filepath.Join(t.TempDir(), "images"))
	photo := attachmentTestPhoto()
	reference := attachmentTestReference(t, photo)
	if err := store.Install(&reference, photo); err != nil {
		t.Fatal(err)
	}
	if err := store.Install(&reference, photo); err != nil {
		t.Fatal(err)
	}
	prompt := attachmentTestBlocks(&reference, 10)
	encoded, err := transcriptMarshal(prompt)
	if err != nil {
		t.Fatal(err)
	}
	if len(encoded) >= 8192 {
		t.Fatalf("the prompt is %d bytes", len(encoded))
	}
	if err := store.Resolve(prompt); err != nil {
		t.Fatal(err)
	}
	for _, block := range prompt {
		if block.Image == nil {
			t.Fatal("not an image")
		}
		decoded, err := base64.StdEncoding.DecodeString(block.Image.Data)
		if err != nil || !bytes.Equal(decoded, photo) {
			t.Fatal("the image bytes differ")
		}
	}
	prompt = append(prompt, reference.ContentBlock())
	if _, err := References(prompt); err == nil {
		t.Fatal("eleven images were accepted")
	}
}

func TestDeletedSessionRejectsLateUploadCompletion(t *testing.T) {
	store := AttachmentStoreWorker(t.TempDir())
	photo := attachmentTestPhoto()
	reference := attachmentTestReference(t, photo)
	if err := store.Install(&reference, photo); err != nil {
		t.Fatal(err)
	}
	if err := store.RemoveSessionData(); err != nil {
		t.Fatal(err)
	}
	if store.Install(&reference, photo) == nil {
		t.Fatal("a late upload was installed")
	}
	if _, err := os.Stat(store.Root()); !os.IsNotExist(err) {
		t.Fatalf("the store exists: %v", err)
	}
}

func TestReferencesAndStoredBytesAreVerified(t *testing.T) {
	store := NewAttachmentStore(filepath.Join(t.TempDir(), "images"))
	photo := attachmentTestPhoto()
	reference := attachmentTestReference(t, photo)
	parsed, err := AttachmentRefFromURI(reference.URI())
	if err != nil || parsed == nil || !reflect.DeepEqual(*parsed, reference) {
		t.Fatalf("got %+v %v", parsed, err)
	}
	if contains, err := store.Contains(&reference); err != nil || contains {
		t.Fatalf("got %v %v", contains, err)
	}
	if err := store.Install(&reference, photo); err != nil {
		t.Fatal(err)
	}
	corrupt := bytes.Clone(photo)
	corrupt[100] ^= 1
	if err := os.WriteFile(filepath.Join(store.Root(), reference.Sha256), corrupt, 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Read(&reference); err == nil {
		t.Fatal("a corrupt blob was read")
	}
	if contains, err := store.Contains(&reference); err != nil || contains {
		t.Fatalf("got %v %v", contains, err)
	}
	if err := store.Install(&reference, photo); err != nil {
		t.Fatal(err)
	}
	if read, err := store.Read(&reference); err != nil || !bytes.Equal(read, photo) {
		t.Fatalf("read: %v", err)
	}
	bad := reference
	bad.Sha256 = "../escape"
	if _, err := store.Contains(&bad); err == nil {
		t.Fatal("an escaping digest was accepted")
	}
}

func TestLegacyInlineImageListsAreAcceptedButReferencedListsAreBounded(t *testing.T) {
	inline := make([]acp.ContentBlock, 11)
	for i := range inline {
		inline[i] = acp.ContentBlock{Image: &acp.ImageContent{Data: "legacy", MimeType: "image/png"}}
	}
	if _, err := References(inline); err != nil {
		t.Fatal(err)
	}
	photo := attachmentTestPhoto()
	reference := attachmentTestReference(t, photo)
	if _, err := References(attachmentTestBlocks(&reference, 11)); err == nil {
		t.Fatal("eleven referenced images were accepted")
	}
}
