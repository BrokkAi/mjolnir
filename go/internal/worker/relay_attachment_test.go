// Port of mj-worker/src/relay/attachment_tests.rs.

package worker

import (
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"
	acpunstable "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func attachmentPhoto() []byte {
	bytes := []byte("\x89PNG\r\n\x1a\n")
	for len(bytes) < core.MaxImageBytes {
		bytes = append(bytes, 7)
	}
	return bytes
}

// attachmentStableBlock carries an attachment block, which the attachment
// module builds in the unstable ACP schema, into the stable schema relay
// prompts use. The two share the wire form.
func attachmentStableBlock(t *testing.T, block acpunstable.ContentBlock) acpschema.ContentBlock {
	t.Helper()
	encoded, err := json.Marshal(block)
	if err != nil {
		t.Fatal(err)
	}
	var stable acpschema.ContentBlock
	if err := json.Unmarshal(encoded, &stable); err != nil {
		t.Fatal(err)
	}
	return stable
}

func TestPhotoQueueSurvivesRestartAndArchiveTransfer(t *testing.T) {
	source := t.TempDir()
	store := core.AttachmentStoreWorker(source)
	var unstablePrompt []acpunstable.ContentBlock
	var prompt []acpschema.ContentBlock
	for index := range 10 {
		bytes := attachmentPhoto()
		bytes[100] = byte(index)
		reference, err := core.NewAttachmentRef(bytes, "image/png", 100, 100)
		if err != nil {
			t.Fatal(err)
		}
		if err := store.Install(&reference, bytes); err != nil {
			t.Fatal(err)
		}
		block := reference.ContentBlock()
		unstablePrompt = append(unstablePrompt, block)
		prompt = append(prompt, attachmentStableBlock(t, block))
	}
	command := core.RelayCommandPrompt{Prompt: prompt}
	relay, err := DurableRelayOpen(source, session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	ordinal := submitRelay(relay, "photos-command-1", command)
	if again := submitRelay(relay, "photos-command-1", command); again != ordinal {
		t.Fatalf("retry accepted at %d, want %d", again, ordinal)
	}
	info, err := os.Stat(filepath.Join(source, core.RelayStateFile))
	if err != nil {
		t.Fatal(err)
	}
	if info.Size() >= 64*1024 {
		t.Fatalf("relay state holds %d bytes; attachments must stay out of it", info.Size())
	}
	relay, err = DurableRelayOpen(source, session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	if again := submitRelay(relay, "photos-command-1", command); again != ordinal {
		t.Fatalf("retry after restart accepted at %d, want %d", again, ordinal)
	}
	target := t.TempDir()
	restored := core.AttachmentStoreWorker(target)
	artifacts, err := store.ArchiveArtifacts()
	if err != nil {
		t.Fatal(err)
	}
	for _, artifact := range artifacts {
		data, err := core.NativeArtifactDataBytesOf(artifact.Data)
		if err != nil {
			t.Fatal(err)
		}
		installed, err := restored.RestoreArtifact(artifact.RelativePath, data)
		if err != nil {
			t.Fatal(err)
		}
		if !installed {
			t.Fatalf("artifact %s was not restored", artifact.RelativePath)
		}
	}
	if err := restored.Resolve(unstablePrompt); err != nil {
		t.Fatal(err)
	}
	for index, block := range unstablePrompt {
		if block.Image == nil {
			t.Fatalf("block %d is not an image", index)
		}
		decoded, err := base64.StdEncoding.DecodeString(block.Image.Data)
		if err != nil {
			t.Fatal(err)
		}
		if decoded[100] != byte(index) {
			t.Fatalf("block %d resolved to photo %d", index, decoded[100])
		}
	}
	missing := t.TempDir()
	relay, err = DurableRelayOpen(missing, session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	response := relay.Handle(relayRequest("missing-photos", core.RelayRequestSubmit{
		CommandID: "photos-command-2",
		Command:   command,
	}))
	if _, refused := response.Body.(core.RelayResponseBodyError); !refused {
		t.Fatalf("a prompt whose photos are missing must be refused, got %#v", response.Body)
	}
	if relay.LatestOrdinal() != 0 {
		t.Fatalf("latest ordinal = %d", relay.LatestOrdinal())
	}
}
