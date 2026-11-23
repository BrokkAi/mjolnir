// Tests of mj-chat/src/chat/remote.rs.

package chat

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"image"
	"image/color"
	"image/png"
	"net"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"
	"github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func remoteValidImage(t *testing.T) ClipboardImage {
	t.Helper()
	picture := image.NewRGBA(image.Rect(0, 0, 2, 2))
	picture.Set(0, 0, color.RGBA{255, 0, 0, 255})
	picture.Set(1, 0, color.RGBA{0, 255, 0, 255})
	picture.Set(0, 1, color.RGBA{0, 0, 255, 255})
	picture.Set(1, 1, color.RGBA{255, 255, 0, 255})
	var encoded bytes.Buffer
	if err := png.Encode(&encoded, picture); err != nil {
		t.Fatal(err)
	}
	image, err := ClipboardImageFromPNGBase64(base64.StdEncoding.EncodeToString(encoded.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	return image
}

// remoteTranscriptShows reports whether any transcript row contains text, at
// a width wide enough that nothing under test wraps.
func remoteTranscriptShows(chat *ChatState, text string) bool {
	return slices.ContainsFunc(transcriptText(chat, 100), func(line string) bool {
		return strings.Contains(line, text)
	})
}

func remoteChat() *ChatState {
	value := snapshot()
	return NewChatState(&value, nil)
}

// remoteSend queues operation on the supervisor's channel, as Rust's
// `operations().send(..).await` does with room to spare.
func remoteSend(t *testing.T, remote *chatRemoteSupervisor, operation chatRemoteOperation) {
	t.Helper()
	permit, ok := remote.operations().tryReserve()
	if !ok {
		t.Fatal("the operation queue is full")
	}
	permit.send(operation)
}

func remoteRecv(t *testing.T, remote *chatRemoteSupervisor, timeout time.Duration) chatRemoteResult {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	result, ok := remote.recv(ctx)
	if !ok {
		t.Fatal("no remote result arrived")
	}
	return result
}

// Hard-won: 4c23909e0: a late rejected answer labeled the next request with the old request's failure.
func TestRejectedElicitationDoesNotLabelANewerForm(t *testing.T) {
	chat := remoteChat()
	old := core.ElicitationRequest{ID: "old", Message: "Old approval", Fields: []core.ElicitationField{}}
	newer := old
	newer.ID = "new"
	newer.Message = "New approval"
	chat.restoreElicitation(newer)
	applyChatRemoteResult(chat, chatRemoteResultRespondElicitation{
		Request: old, Answered: false, Err: errors.New("late rejection"),
	})
	if !chat.elicitation.request().Equal(&newer) {
		t.Fatalf("the dialog shows %+v", chat.elicitation.request())
	}
	lines := drawnTranscript(chat, 100, 24)
	formStart := chat.frameSurfaces.Surface(SurfaceIdElicitationMessage{}).Rect.Min.Y
	for _, line := range lines[formStart:] {
		if strings.Contains(line, "Answer was not sent") {
			t.Fatalf("the newer form is labeled with the old failure: %q", line)
		}
	}
	if !remoteTranscriptShows(chat, "Answer was not sent: late rejection") {
		t.Fatal("the failure is not in the transcript")
	}
	// Hard-won: 57e492d: identical request payloads from two harnesses shared a primary error.
	chat.elicitation = nil
	reviewer := "reviewer"
	chat.showReviewRoleElicitation(&reviewer, newer)
	chat.pendingElicitations = []core.ElicitationRequest{newer}
	applyChatRemoteResult(chat, chatRemoteResultRespondElicitation{
		Request: newer, Answered: false, Err: errors.New("primary-only failure"),
	})
	draft, err := json.Marshal(chat.elicitationDraft())
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(draft), "primary-only failure") {
		t.Fatal("the reviewer's form carries the primary's error")
	}
	// The following independent scenario has no reviewer surface.
	chat.elicitation = nil
	chat.elicitationIsReviewers = false
	chat.elicitationRole = nil
	// Hard-won: 5dc2084: a parked primary rejection revived a request removed by the refreshed projection.
	var config core.Config
	if err := json.Unmarshal([]byte(`{"version":0}`), &config); err != nil {
		t.Fatal(err)
	}
	parked := ChatStateStandby("parked-primary", &config, SessionHeaderIdentity{}, Notices{})
	parked.AdoptLiveComposer(chat.TakeLiveComposer())
	applyChatRemoteResult(parked, chatRemoteResultRespondElicitation{
		Request: old, Answered: false, Err: errors.New("ended request"),
	})
	empty := core.MaterializedSessionEmpty("parked-primary")
	chat.ApplyMaterialized(&empty, nil, nil)
	chat.AdoptLiveComposer(parked.TakeLiveComposer())
	chat.applyDeferredFormResults()
	if chat.elicitation != nil {
		t.Fatal("the ended request came back")
	}
}

func TestRejectedSteeringIsNotReportedAsFailedCancellation(t *testing.T) {
	chat := remoteChat()
	applyChatRemoteResult(chat, chatRemoteResultCancel{Intent: TurnControlIntentSteer, Err: errors.New("session disconnected")})
	if !remoteTranscriptShows(chat, "Steering request failed: session disconnected") {
		t.Fatal("the steering failure is not reported")
	}
}

func TestPromptReacquiresReplacementActorBeforeDispatch(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("session-replaced", 73)
	remote := spawnChatRemoteSupervisor(fixture.Stopped, fixture.Control)
	defer remote.close()
	remoteSend(t, remote, chatRemoteOperationPrompt{CommandID: "prompt-1", Text: "keep going"})
	result := remoteRecv(t, remote, time.Second)
	prompt, ok := result.(chatRemoteResultPrompt)
	if !ok || prompt.Err != nil || prompt.Ordinal != 73 || prompt.Text != "keep going" || len(prompt.Images) != 0 {
		t.Fatalf("result = %#v", result)
	}
}

// remoteInIsolatedStore runs test alone with a data directory of its own,
// since installing an image writes the session's attachment store under the
// data directory. It reports whether this process is that run.
func remoteInIsolatedStore(t *testing.T, test string) bool {
	t.Helper()
	const child = "MJ_CHAT_ISOLATED_STORE_CHILD"
	if _, ok := os.LookupEnv(child); ok {
		return true
	}
	directory := t.TempDir()
	command := exec.Command(os.Args[0], "-test.run", "^"+test+"$", "-test.v")
	command.Env = append(os.Environ(),
		child+"=1",
		"MJ_DATA_DIR="+filepath.Join(directory, "data"),
		"MJ_CONFIG_DIR="+filepath.Join(directory, "config"),
	)
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("isolated %s failed\nstdout:\n%s\nstderr:\n%s", test, output.Stdout, output.Stderr)
	}
	return false
}

// Hard-won: #1514: upload blocked chat actions and oversized persisted drafts.
func TestImageBytesReachTheSessionActorAndOversizedPromptsAreRefusedIntact(t *testing.T) {
	if !remoteInIsolatedStore(t, "TestImageBytesReachTheSessionActorAndOversizedPromptsAreRefusedIntact") {
		return
	}
	// The session actor fixture and the control endpoint change
	// independently: require upload acknowledgement before the actor
	// receives a reference.
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         netip.MustParseAddrPort(listener.Addr().String()),
		Token:           "test-upload-token",
		StartedAt:       "now",
		BuildVersion:    "test",
		InstanceID:      "chat-upload",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	metadataPath := client.MetadataPath()
	if err := os.MkdirAll(filepath.Dir(metadataPath), 0o777); err != nil {
		t.Fatal(err)
	}
	encodedMetadata, err := json.Marshal(metadata)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(metadataPath, encodedMetadata, 0o666); err != nil {
		t.Fatal(err)
	}
	started, legacyStarted, receiptStarted := make(chan struct{}), make(chan struct{}), make(chan struct{})
	uploads := core.Spawn(func() struct{} {
		for attempt := range 6 {
			stream, err := listener.Accept()
			if err != nil {
				panic(err)
			}
			var body []byte
			for {
				request, err := client.ReadFrame[client.RequestEnvelope](stream)
				if err != nil {
					panic(err)
				}
				if request.Token != metadata.Token {
					panic("wrong token")
				}
				chunk, ok := request.Action.(client.DaemonActionUploadChunk)
				if !ok {
					panic("expected upload fragment")
				}
				body = append(body, chunk.Bytes...)
				if chunk.Sha256 == nil {
					continue
				}
				if attempt < 2 || attempt == 4 {
					switch attempt {
					case 0:
						close(started)
					case 1:
						close(legacyStarted)
					default:
						close(receiptStarted)
					}
					if n, _ := stream.Read(make([]byte, 1)); n != 0 {
						panic("cancel drops the upload connection")
					}
					break
				}
				if err := client.WriteFrame(stream, client.ResponseEnvelope{
					ProtocolVersion: client.ProtocolVersion,
					RequestID:       request.RequestID,
					Result:          client.DaemonOk[client.DaemonReply](client.DaemonReplyBlobUploaded{Blob: "fixture-blob"}),
				}); err != nil {
					panic(err)
				}
				request, err = client.ReadFrame[client.RequestEnvelope](stream)
				if err != nil {
					panic(err)
				}
				install, ok := request.Action.(client.DaemonActionInstallAttachment)
				if !ok {
					panic("expected attachment installation")
				}
				if install.Blob != "fixture-blob" {
					panic("wrong blob")
				}
				store, err := core.AttachmentStoreController(install.SessionID)
				if err != nil {
					panic(err)
				}
				if err := store.Install(&install.Reference, body); err != nil {
					panic(err)
				}
				if err := client.WriteFrame(stream, client.ResponseEnvelope{
					ProtocolVersion: client.ProtocolVersion,
					RequestID:       request.RequestID,
					Result:          client.DaemonOk[client.DaemonReply](client.DaemonReplyAttachmentInstalled{Reference: install.Reference}),
				}); err != nil {
					panic(err)
				}
				break
			}
		}
		return struct{}{}
	})
	fixture := client.NewReplacementSessionTestFixture("image-session", 19)
	materialized := core.MaterializedSessionEmpty("image-session")
	relay := core.NewRelaySnapshot("image-session")
	operational := relay.OperationalState()
	imageCapable := func() *schema.AgentCapabilities {
		var capabilities schema.AgentCapabilities
		if err := json.Unmarshal([]byte(`{"promptCapabilities": {"image": true}}`), &capabilities); err != nil {
			t.Fatal(err)
		}
		return &capabilities
	}
	operational.AgentCapabilities = imageCapable()
	fixture.ReplacementView.SendReplace(client.ManagedSessionView{
		Connected: true,
		Snapshot: &core.ManagedSessionSnapshot{
			Window:              core.ProjectionWindowOf(&materialized),
			Materialized:        materialized,
			Operational:         core.NewSharedSnapshot(operational),
			SubagentRequests:    core.NewSharedSnapshot([]core.SubagentToolRequest{}),
			SubagentCompletions: core.NewSharedSnapshot([]core.SubagentCompletion{}),
		},
	})
	// setCapabilities replaces the advertised agent capabilities, as Rust's
	// send_modify does.
	setCapabilities := func(capabilities *schema.AgentCapabilities) {
		view := fixture.ReplacementView.Borrow()
		snapshot := *view.Snapshot
		state := *snapshot.Operational.Get()
		state.AgentCapabilities = capabilities
		snapshot.Operational = core.NewSharedSnapshot(state)
		view.Snapshot = &snapshot
		fixture.ReplacementView.SendReplace(view)
	}
	remote := spawnChatRemoteSupervisor(fixture.Stopped, fixture.Control)
	defer remote.close()
	raw, err := base64.StdEncoding.DecodeString(remoteValidImage(t).DataBase64)
	if err != nil {
		t.Fatal(err)
	}
	raw = append(raw, make([]byte, 9*1024*1024-len(raw))...)
	pasted := ClipboardImage{DataBase64: base64.StdEncoding.EncodeToString(raw), MimeType: "image/png"}
	session := "image-session"
	encodedDraftLen := func(chat *ChatState) int {
		payload := chat.draftPayload()
		return len(payload.EncodeDraft())
	}
	waitForReference := func(composer *ChatState) {
		deadline := time.Now().Add(10 * time.Second)
		for {
			composer.DrainComposer()
			if composer.draftPayload().Images[0].Image.Reference != nil {
				return
			}
			if time.Now().After(deadline) {
				t.Fatal("the attachment never became a reference")
			}
			time.Sleep(time.Millisecond)
		}
	}
	composer := grokChat()
	composer.setPromptImagesSupported(true)
	composer.BindComposerSession(&session)
	if !composer.queueAttachment(attachmentSourceClipboard(pasted), attachmentFallbackFailedMarker{}) {
		t.Fatal("the attachment was not queued")
	}
	if encodedDraftLen(composer) >= 8*1024*1024 {
		t.Fatal("the draft holds the image bytes")
	}
	<-started
	composer.restoreElicitation(core.ElicitationRequest{ID: "during-upload", Message: "Answer now", Fields: []core.ElicitationField{}})
	if composer.elicitation == nil {
		t.Fatal("a question during the upload is not shown")
	}
	composer.elicitation = nil
	composer.clearInput()
	composer.DrainComposer()
	// Legacy drafts carry inline bytes and must re-enter preparation.
	legacy := PromptPayloadWithImage("legacy", remoteValidImage(t))
	composer.restoreDraft(legacy.EncodeDraft())
	composer.DrainComposer()
	<-legacyStarted
	if !composer.draftPayload().Images[0].Image.isPending() {
		t.Fatal("the legacy image is not pending")
	}
	saved := composer.encodedDraft()
	decoded, err := PromptPayloadDecodeDraft(saved)
	if err != nil {
		t.Fatal(err)
	}
	valid := remoteValidImage(t)
	if !decoded.Images[0].Image.Equal(&valid) {
		t.Fatal("the saved draft lost the legacy image")
	}
	// Dropping the composer in Rust cancels its uploads; Close is the Go stand-in.
	composer.Close()
	composer = grokChat() // Exit with an unacknowledged legacy upload.
	composer.setPromptImagesSupported(true)
	composer.restoreDraft(saved)
	if !composer.draftPayload().Images[0].Image.Equal(&valid) {
		t.Fatal("the restored draft lost the legacy image")
	}
	composer.BindComposerSession(&session)
	if encodedDraftLen(composer) >= 8*1024*1024 {
		t.Fatal("the draft holds the image bytes")
	}
	waitForReference(composer)
	if composer.draftPayload().Images[0].Image.DataBase64 != "" {
		t.Fatal("the referenced image keeps its bytes")
	}
	decoded, err = PromptPayloadDecodeDraft(composer.encodedDraft())
	if err != nil || decoded.Images[0].Image.Reference == nil {
		t.Fatalf("the saved draft has no reference: %v", err)
	}
	composer.clearInput()
	if !composer.queueAttachment(attachmentSourceClipboard(pasted), attachmentFallbackFailedMarker{}) {
		t.Fatal("the attachment was not queued")
	}
	waitForReference(composer)
	draft := composer.encodedDraft()
	if len(draft) >= 8*1024*1024 {
		t.Fatal("the draft holds the image bytes")
	}
	restored, err := PromptPayloadDecodeDraft(draft)
	if err != nil {
		t.Fatal(err)
	}
	imageRef := restored.Images[0].Image
	if imageRef.DataBase64 != "" {
		t.Fatal("the restored image keeps its bytes")
	}
	var normalizedImage *ClipboardImage
	for _, text := range []string{"", "inspect this"} {
		payload := PromptPayloadWithImage(text, imageRef)
		remoteSend(t, remote, chatRemoteOperationPrompt{CommandID: "image-" + text, Text: payload.Text, Images: payload.Images})
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		command, ok := fixture.Submitted.Recv(ctx)
		cancel()
		if !ok {
			t.Fatal("no command reached the actor")
		}
		prompt, ok := command.(core.RelayCommandPrompt)
		if !ok {
			t.Fatalf("expected image prompt command, got %#v", command)
		}
		var imageBlock *schema.ImageContent
		for _, block := range prompt.Prompt {
			if block.Image != nil {
				imageBlock = block.Image
				break
			}
		}
		if imageBlock == nil {
			t.Fatal("image prompt contains an image block")
		}
		if imageBlock.Data != "" {
			t.Fatal("the image block carries bytes")
		}
		// The attachment module reads the unstable ACP schema; the two
		// schemas share the wire form, so the block crosses by its JSON.
		encodedBlock, err := json.Marshal(imageBlock)
		if err != nil {
			t.Fatal(err)
		}
		var unstableBlock unstable.ImageContent
		if err := json.Unmarshal(encodedBlock, &unstableBlock); err != nil {
			t.Fatal(err)
		}
		reference, err := core.ImageReference(&unstableBlock)
		if err != nil || reference == nil {
			t.Fatalf("image prompt contains an attachment reference: %v", err)
		}
		if reference.Width != 2 || reference.Height != 2 {
			t.Fatalf("reference size = %dx%d", reference.Width, reference.Height)
		}
		result, ok := remoteRecv(t, remote, 10*time.Second).(chatRemoteResultPrompt)
		if !ok || result.Err != nil || result.Ordinal != 19 {
			t.Fatalf("expected successful image prompt result, got %#v", result)
		}
		if len(result.Images) != 1 || !reflect.DeepEqual(result.Images[0].Image.Reference, reference) {
			t.Fatalf("result images = %#v", result.Images)
		}
		normalized := result.Images[0].Image
		normalizedImage = &normalized
	}
	// The supervisor must consult the latest view, even if the composer
	// queued the image while support was still advertised.
	setCapabilities(nil)
	revoked := PromptPayloadWithImage("keep this draft", imageRef)
	remoteSend(t, remote, chatRemoteOperationPrompt{CommandID: "capability-revoked", Text: revoked.Text, Images: revoked.Images})
	result, ok := remoteRecv(t, remote, 2*time.Second).(chatRemoteResultPrompt)
	if !ok || result.Text != revoked.Text || len(result.Images) != 1 || result.Err == nil ||
		!strings.Contains(result.Err.Error(), "advertised image support") {
		t.Fatalf("result = %#v", result)
	}
	if _, delivered := fixture.Submitted.Recv(remoteTryRecvContext); delivered {
		t.Fatal("a refused prompt reached the actor")
	}

	huge := PromptPayloadWithImage(strings.Repeat("x", core.RelayCommandByteBudget), imageRef)
	remoteSend(t, remote, chatRemoteOperationPrompt{CommandID: "too-big", Text: huge.Text, Images: huge.Images})
	result, ok = remoteRecv(t, remote, 2*time.Second).(chatRemoteResultPrompt)
	expectedImages := slices.Clone(huge.Images)
	if normalizedImage == nil {
		t.Fatal("successful image was normalized")
	}
	expectedImages[0].Image = *normalizedImage
	if !ok || result.Err == nil || result.Text != huge.Text || !reflect.DeepEqual(result.Images, expectedImages) {
		t.Fatalf("result = %#v", result)
	}
	if _, delivered := fixture.Submitted.Recv(remoteTryRecvContext); delivered {
		t.Fatal("oversized input must not reach the actor")
	}
	// Old receipt bytes stay durable during preparation; retry identity and
	// the different visible draft both survive closing the first composer.
	composer.setInput("different visible draft")
	composer.beginSubmission("old-receipt", SubmissionKindPrompt, PromptPayloadWithImage("old image", valid), "Sending…")
	composer.unconfirmSubmission("old-receipt", "lost acknowledgement")
	saved = composer.encodedDraft()
	composer.restoreDraft(saved)
	if !composer.prepareReceiptRetry("old-receipt", "image-session") {
		t.Fatal("the retry was not prepared")
	}
	if !composer.prepareReceiptRetry("old-receipt", "image-session") { // one job per ID
		t.Fatal("the second retry was not prepared")
	}
	<-receiptStarted
	saved = composer.encodedDraft()
	if !strings.Contains(saved, valid.DataBase64) {
		t.Fatal("the receipt's bytes are not durable during preparation")
	}
	composer.Close()
	composer = grokChat()
	composer.restoreDraft(saved)
	if composer.input != "different visible draft" {
		t.Fatalf("input = %q", composer.input)
	}
	if !composer.prepareReceiptRetry("old-receipt", "image-session") {
		t.Fatal("the retry was not prepared")
	}
	deadline := time.Now().Add(10 * time.Second)
	for {
		composer.DrainComposer()
		if len(composer.composerRuntime.preparedReceiptRetries) > 0 {
			composer.composerRuntime.preparedReceiptRetries = composer.composerRuntime.preparedReceiptRetries[1:]
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the receipt retry was never prepared")
		}
		time.Sleep(time.Millisecond)
	}
	if composer.input != "different visible draft" {
		t.Fatalf("input = %q", composer.input)
	}
	if strings.Contains(composer.encodedDraft(), valid.DataBase64) {
		t.Fatal("the prepared receipt keeps its bytes")
	}
	operation := composer.resendSubmission("old-receipt")
	prompt, ok := operation.(chatRemoteOperationPrompt)
	if !ok || prompt.CommandID != "old-receipt" || prompt.Images[0].Image.Reference == nil {
		t.Fatalf("resend = %#v", operation)
	}
	setCapabilities(imageCapable())
	remoteSend(t, remote, operation)
	if command, ok := fixture.Submitted.Recv(context.Background()); !ok {
		t.Fatal("the resend did not reach the actor")
	} else if _, isPrompt := command.(core.RelayCommandPrompt); !isPrompt {
		t.Fatalf("command = %#v", command)
	}
	if result, ok := remoteRecv(t, remote, 10*time.Second).(chatRemoteResultPrompt); !ok ||
		result.CommandID != "old-receipt" || result.Err != nil || result.Ordinal != 19 {
		t.Fatalf("result = %#v", result)
	}
	if _, delivered := fixture.Submitted.Recv(remoteTryRecvContext); delivered {
		t.Fatal("an extra command reached the actor")
	}
	if _, err := uploads.Join(context.Background()); err != nil {
		t.Fatal(err)
	}
}

func TestMalformedInlineImageIsRefusedBeforeActorDispatch(t *testing.T) {
	fixture := client.NewReplacementSessionTestFixture("malformed-image-session", 23)
	remote := spawnChatRemoteSupervisor(fixture.Stopped, fixture.Control)
	defer remote.close()
	image := ClipboardImage{DataBase64: "not-base64", MimeType: "image/png"}
	payload := PromptPayloadWithImage("inspect", image)
	remoteSend(t, remote, chatRemoteOperationPrompt{CommandID: "malformed-image", Text: payload.Text, Images: payload.Images})
	result, ok := remoteRecv(t, remote, 2*time.Second).(chatRemoteResultPrompt)
	if !ok || result.Err == nil || result.Text != payload.Text || !reflect.DeepEqual(result.Images, payload.Images) {
		t.Fatalf("result = %#v", result)
	}
	if _, delivered := fixture.Submitted.Recv(remoteTryRecvContext); delivered {
		t.Fatal("malformed image must not reach the actor")
	}
}

func TestFullRemoteQueueRestoresUnsentInputWithoutBlocking(t *testing.T) {
	operations, _ := newChatRemoteOperationQueue(1)
	permit, ok := operations.tryReserve()
	if !ok {
		t.Fatal("the queue refused its first operation")
	}
	permit.send(chatRemoteOperationSync{})
	chat := remoteChat()
	chat.setInput("new draft")
	queueChatRemoteOperation(operations, chatRemoteOperationPrompt{CommandID: "prompt-1", Text: "unsent prompt"}, chat)
	if chat.input != "unsent prompt\n\nnew draft" {
		t.Fatalf("input = %q", chat.input)
	}
	if notice, ok := chat.Notice(); !ok || !strings.Contains(notice, "queue is full") {
		t.Fatalf("notice = %q, %t", notice, ok)
	}
}

func TestFailedPlanControlRestoresTheFullCommandAndRollsBackMode(t *testing.T) {
	for _, c := range []struct {
		controlApplied bool
		expectedMode   string
	}{{false, "default"}, {true, "plan"}} {
		chat := grokChat()
		if !c.controlApplied {
			chat.finishPlanModeChange(true)
		}
		chat.planCommandPending = true
		message := "rejected"
		if c.controlApplied {
			message = "mode changed, but prompt failed"
		}
		applyChatRemoteResult(chat, chatRemoteResultPlanCommand{
			CommandID:       "test-submit",
			Original:        "/plan inspect this",
			RequestedActive: true,
			ControlApplied:  c.controlApplied,
			Err:             errors.New(message),
		})
		if mode, ok := chat.currentMode(); !ok || mode != c.expectedMode {
			t.Fatalf("mode = %q, %t, want %q", mode, ok, c.expectedMode)
		}
		if chat.input != "/plan inspect this" {
			t.Fatalf("input = %q", chat.input)
		}
		if chat.planCommandPending {
			t.Fatal("the plan command is still pending")
		}
	}
}

func TestFullRemoteQueueRestoresAShellCommandWithItsPrefix(t *testing.T) {
	operations, _ := newChatRemoteOperationQueue(1)
	permit, ok := operations.tryReserve()
	if !ok {
		t.Fatal("the queue refused its first operation")
	}
	permit.send(chatRemoteOperationSync{})
	chat := remoteChat()
	queueChatRemoteOperation(operations, chatRemoteOperationRunShell{CommandID: "shell-1", Command: "cargo test"}, chat)
	if chat.input != "!cargo test" {
		t.Fatalf("input = %q", chat.input)
	}
}

// Hard-won: 81e6e3b: refused prompts vanished on projection rebuild, implying they had been sent
func TestARefusedPromptStaysInTheTranscriptAfterAProjectionRebuild(t *testing.T) {
	chat := remoteChat()
	chat.setInput("later draft")
	applyChatRemoteResult(chat, chatRemoteResultPrompt{
		CommandID: "test-submit",
		Text:      "read the journal\nand summarise it",
		Err:       errors.New("relay attach failed"),
	})
	// The failure remains in the conversation and preserves the draft.
	if _, ok := chat.Notice(); ok {
		t.Fatal("the refusal is a pinned notice")
	}
	if chat.input != "read the journal\nand summarise it\n\nlater draft" {
		t.Fatalf("input = %q", chat.input)
	}
	for _, want := range []string{
		"Prompt was not sent: relay attach failed",
		"read the journal and summarise it",
		// The row is timestamped like the rest of the transcript.
		"Mjolnir · ",
	} {
		if !remoteTranscriptShows(chat, want) {
			t.Fatalf("%q is not in the transcript", want)
		}
	}
	// A newer projection rebuilds the entries; the notice is local until the
	// journal has it, so it has to outlive that.
	session := core.MaterializedSessionEmpty("1234567890")
	session.AppliedEventOrdinal = 9
	chat.ApplyMaterialized(&session, nil, nil)
	if !remoteTranscriptShows(chat, "Prompt was not sent: relay attach failed") {
		t.Fatal("the refusal vanished on the rebuild")
	}
}

// The screenshot case: a prompt refused while context is being cleared. The
// relay refuses the notice too, so it is sent again once the journal moves
// on, and then sits above the next prompt instead of below it.
func TestAPromptRefusedDuringAClearLandsInTimeOrderOnceTheClearEnds(t *testing.T) {
	// What ActiveChat.publishConversationNotices does for each notice it
	// hands to the relay. Returns how many it sent.
	publish := func(chat *ChatState) int {
		applied := chat.latestSeq
		sent := 0
		for i := range chat.conversationNotices {
			notice := &chat.conversationNotices[i]
			if !notice.Submitted {
				notice.Submitted = true
				notice.SentAt = applied
				sent++
			}
		}
		return sent
	}
	at := func(ordinal uint64) *core.MaterializedSession {
		session := core.MaterializedSessionEmpty("1234567890")
		session.AppliedEventOrdinal = ordinal
		return &session
	}
	item := func(stableID string, position uint64, body core.TranscriptBody) *core.TranscriptItem {
		return &core.TranscriptItem{StableID: stableID, Position: position, CreatedAtMs: 10, LastChangedAtMs: 10, Body: body}
	}
	clearing := "Context is being cleared; wait for the new conversation"
	chat := remoteChat()
	chat.ApplyMaterialized(at(4), nil, nil)
	applyChatRemoteResult(chat, chatRemoteResultPrompt{
		CommandID: "test-submit",
		Text:      "tweak the close tool description",
		Err:       errors.New(clearing),
	})
	id := chat.conversationNotices[0].ID
	text := chat.conversationNotices[0].Text
	if want := "Prompt was not sent: " + clearing + "\ntweak the close tool description"; text != want {
		t.Fatalf("notice = %q", text)
	}
	if sent := publish(chat); sent != 1 {
		t.Fatalf("sent %d", sent)
	}
	refused := func(id string) chatRemoteResult {
		return chatRemoteResultNoticeRecorded{ID: id, Err: errors.New(clearing)}
	}

	// Refused, and nothing has happened since: no resend, even when the
	// same projection is applied again.
	applyChatRemoteResult(chat, refused(id))
	chat.ApplyMaterialized(at(4), nil, nil)
	if sent := publish(chat); sent != 0 {
		t.Fatalf("resent %d without a journal change", sent)
	}

	// The journal moves on, so it is sent again.
	chat.ApplyMaterialized(at(6), nil, nil)
	if sent := publish(chat); sent != 1 {
		t.Fatalf("sent %d after the journal moved", sent)
	}

	// Refused again, but the journal moved before the reply arrived: it is
	// resent at once rather than waiting for yet another event.
	chat.ApplyMaterialized(at(7), nil, nil)
	applyChatRemoteResult(chat, refused(id))
	if sent := publish(chat); sent != 1 {
		t.Fatalf("sent %d after a late refusal", sent)
	}

	// Journaled, then a later prompt is sent: the notice appears once, above
	// that prompt, and the chat holds no local copy.
	journaled := at(9)
	journaled.Transcript = append(journaled.Transcript,
		item("system:notice:"+id, 8, core.TranscriptBodySystem{Text: text}),
		item("user:9", 9, core.TranscriptBodyUser{Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"a later prompt"}`)}}),
	)
	chat.ApplyMaterialized(journaled, nil, nil)
	if len(chat.conversationNotices) != 0 {
		t.Fatalf("the chat keeps %d local notices", len(chat.conversationNotices))
	}
	screen := transcriptText(chat, 100)
	notice := slices.IndexFunc(screen, func(line string) bool { return strings.Contains(line, "Prompt was not sent") })
	later := slices.IndexFunc(screen, func(line string) bool { return strings.Contains(line, "a later prompt") })
	if notice < 0 || notice >= later {
		t.Fatalf("the notice is not above the later prompt: %q", screen)
	}
	count := 0
	for _, line := range screen {
		if strings.Contains(line, "Prompt was not sent") {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("the notice appears %d times", count)
	}
}

func TestFailedFastUpdateRestoresTheUserFacingToggleCommand(t *testing.T) {
	chat := remoteChat()
	applyChatRemoteResult(chat, chatRemoteResultSetConfig{Key: "fast-mode", Value: "on", Err: errors.New("rejected")})
	if chat.input != "/fast" {
		t.Fatalf("input = %q", chat.input)
	}
	if !remoteTranscriptShows(chat, "Configuration was not changed: rejected") {
		t.Fatal("the failure is not in the transcript")
	}
}
