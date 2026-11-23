// Port of the tests in mj-client/src/daemon.rs.

package client

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// daemonTestListener is a loopback listener standing in for the daemon.
func daemonTestListener(t *testing.T) (net.Listener, netip.AddrPort) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	return listener, listener.Addr().(*net.TCPAddr).AddrPort()
}

// daemonTestServer runs serve as the fake daemon; the returned wait reports
// when it is done, failing the test on anything serve reported.
func daemonTestServer(t *testing.T, serve func() error) (wait func()) {
	t.Helper()
	done := make(chan error, 1)
	go func() { done <- serve() }()
	return func() {
		t.Helper()
		if err := <-done; err != nil {
			t.Fatalf("fake daemon: %v", err)
		}
	}
}

func daemonTestMetadata(address netip.AddrPort, protocolVersion uint32, token string) DaemonMetadata {
	return DaemonMetadata{
		ProtocolVersion: protocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         address,
		Token:           token,
		StartedAt:       "test",
		BuildVersion:    "test-version",
		InstanceID:      "",
		Placement:       DaemonPlacement{Kind: DaemonPlacementKindHost},
	}
}

func daemonCancellationMetadata(address netip.AddrPort) DaemonMetadata {
	return daemonTestMetadata(address, ProtocolVersion, "cancelled-exchange")
}

func daemonTestConnect(t *testing.T, metadata DaemonMetadata) *DaemonClient {
	t.Helper()
	client, err := DaemonClientConnect(context.Background(), metadata)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(client.Close)
	return client
}

func daemonTestRespond(stream net.Conn, request RequestEnvelope, result DaemonResult[DaemonReply]) error {
	return WriteFrame(stream, ResponseEnvelope{
		ProtocolVersion: request.ProtocolVersion,
		RequestID:       request.RequestID,
		Result:          result,
	})
}

// daemonTestExpectEOF reads one byte and requires the peer to have closed
// the connection.
func daemonTestExpectEOF(stream net.Conn) error {
	var one [1]byte
	if n, err := stream.Read(one[:]); n != 0 || !errors.Is(err, io.EOF) {
		return fmt.Errorf("expected the client to close the connection, read %d bytes: %v", n, err)
	}
	return nil
}

// daemonJSONValue is v as a generic JSON value, for comparing wire forms as
// Rust compares serde_json::Values. Safe in the fake daemon's goroutine.
func daemonJSONValue(v any) (any, error) {
	encoded, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	var value any
	err = json.Unmarshal(encoded, &value)
	return value, err
}

// daemonTestJSONValue is daemonJSONValue on the test's own goroutine.
func daemonTestJSONValue(t *testing.T, v any) any {
	t.Helper()
	value, err := daemonJSONValue(v)
	if err != nil {
		t.Fatal(err)
	}
	return value
}

// daemonSameJSON reports whether v encodes as want, for the fake daemon.
func daemonSameJSON(v, want any) error {
	got, err := daemonJSONValue(v)
	if err != nil {
		return err
	}
	if !reflect.DeepEqual(got, want) {
		return fmt.Errorf("wire form %v, want %v", got, want)
	}
	return nil
}

func TestContainerOwnerToleratesFutureFieldsWhileLegacyMetadataStaysStrict(t *testing.T) {
	directory := t.TempDir()
	owner := filepath.Join(directory, "daemon-container.json")
	err := os.WriteFile(owner, []byte(`{
		"runtime": "podman",
		"container_id": "full-id",
		"container_name": "mj-daemon",
		"address": "127.0.0.1:8787",
		"token": "fixture-token",
		"started_at": "2026-11-23T00:00:00Z",
		"future_field": {"extra": true}
	}`), 0o600)
	if err != nil {
		t.Fatal(err)
	}
	record, err := readContainerOwnerAt(owner)
	if err != nil || record == nil {
		t.Fatalf("read owner: %v, %v", record, err)
	}
	if record.Schema != ContainerOwnerSchema {
		t.Fatalf("schema %d", record.Schema)
	}
	if record.ContainerID != "full-id" {
		t.Fatalf("container id %q", record.ContainerID)
	}
	otherAddress := netip.MustParseAddrPort("127.0.0.1:8788")
	if err := removeContainerOwnerAt(owner, otherAddress, record.Token); err != nil {
		t.Fatal(err)
	}
	if err := removeContainerOwnerAt(owner, record.Address, "replacement-token"); err != nil {
		t.Fatal(err)
	}
	kept, err := readContainerOwnerAt(owner)
	if err != nil || kept == nil || *kept != *record {
		t.Fatalf("a different daemon's removal changed the owner: %v, %v", kept, err)
	}
	if err := removeContainerOwnerAt(owner, record.Address, record.Token); err != nil {
		t.Fatal(err)
	}
	if gone, err := readContainerOwnerAt(owner); err != nil || gone != nil {
		t.Fatalf("owner after its removal: %v, %v", gone, err)
	}

	metadata := filepath.Join(directory, "daemon.json")
	fixture := map[string]any{
		"protocol_version": ProtocolVersion,
		"pid":              0,
		"address":          record.Address.String(),
		"token":            record.Token,
		"started_at":       record.StartedAt,
		"build_version":    "2.73.0",
	}
	write := func() {
		body, err := json.Marshal(fixture)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(metadata, body, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write()
	read, err := readMetadataAt(metadata)
	if err != nil {
		t.Fatal(err)
	}
	if !record.Matches(&read) {
		t.Fatal("the owner does not match its daemon's metadata")
	}
	fixture["container_id"] = record.ContainerID
	write()
	if _, err := readMetadataAt(metadata); err == nil || !strings.Contains(err.Error(), "unknown field `container_id`") {
		t.Fatalf("legacy metadata accepted a container field: %v", err)
	}
}

// Hard-won: 372572b0b7b6: stopped daemon endpoint removal broke status and session refresh
func TestAMissingEndpointFileReadsAsAStoppedDaemon(t *testing.T) {
	path := filepath.Join(t.TempDir(), "daemon.json")
	_, err := readMetadataAt(path)
	stopped, ok := DaemonNotRunningOf(err)
	if !ok {
		t.Fatalf("stopped, not an I/O failure: %v", err)
	}
	if stopped.MetadataPath != path {
		t.Fatalf("metadata path %q, want %q", stopped.MetadataPath, path)
	}
	if got := err.Error(); got != "the Mjolnir daemon is not running" {
		t.Fatalf("error %q", got)
	}

	if err := os.WriteFile(path, []byte("not json"), 0o600); err != nil {
		t.Fatal(err)
	}
	_, err = readMetadataAt(path)
	if _, ok := DaemonNotRunningOf(err); ok || err == nil {
		t.Fatalf("an unreadable endpoint is not a stopped daemon: %v", err)
	}
}

// Hard-won: #1566: a cancelled request left its reply for the next request on a reused daemon connection.
func TestCancelledRequestCannotReuseACompleteOrPartialResponse(t *testing.T) {
	// Cancel before the reply, within its length header, within its body,
	// and between fragments of a chunked response.
	const betweenFragments = -1
	for _, prefix := range []int{0, 2, 8, betweenFragments} {
		listener, address := daemonTestListener(t)
		metadata := daemonCancellationMetadata(address)
		received := make(chan struct{})
		release := make(chan struct{})
		wait := daemonTestServer(t, func() error {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			defer stream.Close()
			ping, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			if err := daemonTestRespond(stream, ping, DaemonOk[DaemonReply](DaemonReplyDone{})); err != nil {
				return err
			}
			request, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			if _, ok := request.Action.(DaemonActionIdentity); !ok {
				return fmt.Errorf("expected an identity request, got %T", request.Action)
			}
			body, err := json.Marshal(ResponseEnvelope{
				ProtocolVersion: request.ProtocolVersion,
				RequestID:       request.RequestID,
				Result:          DaemonOk[DaemonReply](DaemonReplyDone{}),
			})
			if err != nil {
				return err
			}
			frame := binary.BigEndian.AppendUint32(nil, uint32(len(body)))
			frame = append(frame, body...)
			written := prefix
			if prefix == betweenFragments {
				err := daemonTestRespond(stream, request, DaemonOk[DaemonReply](DaemonReplyReplyChunk{
					Bytes:    []byte{'{'},
					Finished: false,
				}))
				if err != nil {
					return err
				}
				written = 0
			} else if _, err := stream.Write(frame[:prefix]); err != nil {
				return err
			}
			close(received)
			<-release
			// Cancellation may already have closed the socket. Either result
			// is valid; the client must never consume this abandoned reply.
			_, _ = stream.Write(frame[written:])
			return nil
		})
		client := daemonTestConnect(t, metadata)
		if _, err := client.Request(context.Background(), DaemonActionPing{}); err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithCancel(context.Background())
		result := make(chan error, 1)
		go func() {
			_, err := client.RequestSingle(ctx, DaemonActionIdentity{})
			result <- err
		}()
		select {
		case err := <-result:
			t.Fatalf("reply was not released: %v", err)
		case <-received:
		}
		cancel()
		<-result
		close(release)
		wait()
		_, err := client.Request(context.Background(), DaemonActionPing{})
		if err == nil || !strings.Contains(err.Error(), "connection lost to an interrupted request") {
			t.Fatalf("prefix %d: %v", prefix, err)
		}
		if !errors.As(err, new(DaemonTransportLost)) {
			t.Fatalf("prefix %d: not a lost transport: %v", prefix, err)
		}
	}
}

// daemonTestBlockingReader returns its data, then blocks like an upload
// source that remains open, until unblock closes.
type daemonTestBlockingReader struct {
	data    *bytes.Reader
	unblock chan struct{}
}

func (r *daemonTestBlockingReader) Read(buffer []byte) (int, error) {
	if r.data.Len() > 0 {
		return r.data.Read(buffer)
	}
	<-r.unblock
	return 0, io.ErrClosedPipe
}

func TestCancelledUploadCannotReuseItsConnection(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonCancellationMetadata(address)
	received := make(chan struct{})
	wait := daemonTestServer(t, func() error {
		stream, err := listener.Accept()
		if err != nil {
			return err
		}
		defer stream.Close()
		total := 0
		for total < 128*1024 {
			request, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			chunk, ok := request.Action.(DaemonActionUploadChunk)
			if !ok {
				return errors.New("expected upload chunk")
			}
			if chunk.Sha256 != nil {
				return errors.New("an open source must not finish its upload")
			}
			total += len(chunk.Bytes)
		}
		close(received)
		return daemonTestExpectEOF(stream)
	})
	reader := &daemonTestBlockingReader{
		data:    bytes.NewReader(bytes.Repeat([]byte{42}, 128*1024)),
		unblock: make(chan struct{}),
	}
	client := daemonTestConnect(t, metadata)
	ctx, cancel := context.WithCancel(context.Background())
	result := make(chan error, 1)
	go func() {
		_, err := client.Upload(ctx, reader)
		result <- err
	}()
	select {
	case err := <-result:
		t.Fatalf("upload source remains open: %v", err)
	case <-received:
	}
	cancel()
	wait()
	close(reader.unblock)
	<-result
	_, err := client.Request(context.Background(), DaemonActionPing{})
	if err == nil || !strings.Contains(err.Error(), "connection lost to an interrupted request") {
		t.Fatalf("%v", err)
	}
}

func TestAdoptionProgressKeepsResponseIdentityAndCancelClosesItsConnection(t *testing.T) {
	for _, cancelAdoption := range []bool{false, true} {
		listener, address := daemonTestListener(t)
		metadata := daemonCancellationMetadata(address)
		release := make(chan struct{})
		wait := daemonTestServer(t, func() error {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			defer stream.Close()
			request, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			if _, ok := request.Action.(DaemonActionAdoptRecovery); !ok {
				return fmt.Errorf("expected an adoption, got %T", request.Action)
			}
			destroy := "mj recover destroy"
			err = daemonTestRespond(stream, request, DaemonOk[DaemonReply](DaemonReplyAdoptionWaiting{
				Blockers:       []string{"current turn"},
				DestroyCommand: &destroy,
			}))
			if err != nil {
				return err
			}
			if cancelAdoption {
				return daemonTestExpectEOF(stream)
			}
			<-release
			err = daemonTestRespond(stream, request, DaemonOk[DaemonReply](DaemonReplyAdoptionInstalled{SessionID: "orphan"}))
			if err != nil {
				return err
			}
			return daemonTestRespond(stream, request, DaemonOk[DaemonReply](DaemonReplyDone{}))
		})
		client := daemonTestConnect(t, metadata)
		var installedNotice *string
		type waiting struct {
			blockers []string
			destroy  *string
		}
		observed := make(chan waiting, 4)
		ctx, cancel := context.WithCancel(context.Background())
		result := make(chan error, 1)
		go func() {
			result <- client.AdoptRecovery(ctx, RecoveryAdoptionRequest{
				SessionID: "orphan",
				TargetID:  "podman",
			}, func(event RecoveryAdoptionProgress) {
				switch event := event.(type) {
				case RecoveryAdoptionProgressWaiting:
					observed <- waiting{blockers: event.Blockers, destroy: event.DestroyCommand}
				case RecoveryAdoptionProgressInstalled:
					sessionID := event.SessionID
					installedNotice = &sessionID
				}
			})
		}()
		select {
		case err := <-result:
			t.Fatalf("adoption ended before its boundary: %v", err)
		case message := <-observed:
			if !reflect.DeepEqual(message.blockers, []string{"current turn"}) ||
				message.destroy == nil || *message.destroy != "mj recover destroy" {
				t.Fatalf("progress %+v", message)
			}
		}
		if cancelAdoption {
			cancel()
			<-result
		} else {
			close(release)
			if err := <-result; err != nil {
				t.Fatal(err)
			}
		}
		cancel()
		if cancelAdoption != (installedNotice == nil) {
			t.Fatalf("cancel %v: installed notice %v", cancelAdoption, installedNotice)
		}
		if installedNotice != nil && *installedNotice != "orphan" {
			t.Fatalf("installed %q", *installedNotice)
		}
		if (client.stream == nil) != cancelAdoption {
			t.Fatalf("cancel %v: connection kept %v", cancelAdoption, client.stream != nil)
		}
		wait()
	}
}

// Only a connection that dropped after the request was sent is marked as a
// lost transport; a daemon's answer, even an error, is not.
func TestOnlyAConnectionDroppedAfterTheRequestIsMarkedAsLost(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion, "transport-lost-test")
	wait := daemonTestServer(t, func() error {
		stream, err := listener.Accept()
		if err != nil {
			return err
		}
		defer stream.Close()
		request, err := ReadFrame[RequestEnvelope](stream)
		if err != nil {
			return err
		}
		if err := daemonTestRespond(stream, request, DaemonErr[DaemonReply]("create failed: clone refused")); err != nil {
			return err
		}
		// The second request is read and then the connection is dropped.
		_, err = ReadFrame[RequestEnvelope](stream)
		return err
	})
	client := daemonTestConnect(t, metadata)
	refused := client.WaitCreateSession(context.Background(), "session-1")
	if !errors.As(refused, new(DaemonRefusal)) || errors.As(refused, new(DaemonTransportLost)) {
		t.Fatalf("refusal %v", refused)
	}
	dropped := client.WaitCreateSession(context.Background(), "session-1")
	if !errors.As(dropped, new(DaemonTransportLost)) || errors.As(dropped, new(DaemonRefusal)) {
		t.Fatalf("drop %v", dropped)
	}
	wait()
}

func TestRestartSessionRejectsAnOldDaemonBeforeSendingTheAction(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion-1, "old-restart-daemon")
	wait := daemonTestServer(t, func() error {
		stream, err := listener.Accept()
		if err != nil {
			return err
		}
		defer stream.Close()
		return daemonTestExpectEOF(stream)
	})
	client := daemonTestConnect(t, metadata)

	err := client.RestartSession(context.Background(), "session-1")
	if err == nil {
		t.Fatal("an old daemon accepted Restart")
	}
	message := err.Error()
	for _, want := range []string{
		fmt.Sprintf("daemon protocol %d", ProtocolVersion-1),
		fmt.Sprintf("client protocol %d", ProtocolVersion),
		"restart the daemon",
	} {
		if !strings.Contains(message, want) {
			t.Fatalf("%q lacks %q", message, want)
		}
	}
	client.Close()
	wait()
}

func TestAbandonedPromptHistoryUsesTheDaemonProtocolAction(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion, "abandoned-prompt-test")
	want := daemonTestJSONValue(t, json.RawMessage(`{
		"action": "record_abandoned_prompt",
		"arguments": {
			"session_id": "session-1",
			"bundle_id": "project-1",
			"text": "unfinished draft"
		}
	}`))
	wait := daemonTestServer(t, func() error {
		stream, err := listener.Accept()
		if err != nil {
			return err
		}
		defer stream.Close()
		request, err := ReadFrame[RequestEnvelope](stream)
		if err != nil {
			return err
		}
		if request.ProtocolVersion != ProtocolVersion || request.Token != "abandoned-prompt-test" {
			return fmt.Errorf("envelope %d %q", request.ProtocolVersion, request.Token)
		}
		if err := daemonSameJSON(request.Action, want); err != nil {
			return err
		}
		return daemonTestRespond(stream, request, DaemonOk[DaemonReply](DaemonReplyDone{}))
	})
	client := daemonTestConnect(t, metadata)

	err := client.RecordAbandonedPrompt(context.Background(), "session-1", "project-1", "unfinished draft")
	if err != nil {
		t.Fatal(err)
	}
	wait()
}

// Hard-won: c502e9bdf662: cold-cache discovery failed to save and left Codex profiles out of model choices
func TestSubagentDiscoveryUsesTheDaemonAndPreservesChoicesAndFailures(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion, "subagent-discovery-test")
	var options core.SubagentOptions
	err := json.Unmarshal([]byte(`{
		"models": [{"value": "gpt-6-luna", "name": "Luna", "description": null}],
		"efforts": [{"value": "high", "name": "High", "description": null}],
		"unavailable": ["another-profile: authentication failed"]
	}`), &options)
	if err != nil {
		t.Fatal(err)
	}
	expected := daemonTestJSONValue(t, options)
	draft := core.DefaultConfig()
	var profile core.HarnessProfile
	err = json.Unmarshal([]byte(`{
		"kind": "codex", "home": "/unsaved/profile",
		"subagents": {"mode": "single_model", "model": "gpt-6-luna", "effort": "high"}
	}`), &profile)
	if err != nil {
		t.Fatal(err)
	}
	if draft.Profiles == nil {
		draft.Profiles = map[string]core.HarnessProfile{}
	}
	draft.Profiles["codex"] = profile
	expectedDraft := daemonTestJSONValue(t, draft)
	luna := "gpt-6-luna"
	wait := daemonTestServer(t, func() error {
		stream, err := listener.Accept()
		if err != nil {
			return err
		}
		defer stream.Close()
		for index, model := range []*string{nil, &luna, &luna} {
			request, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			if request.Token != "subagent-discovery-test" {
				return fmt.Errorf("token %q", request.Token)
			}
			action, ok := request.Action.(DaemonActionSubagentOptions)
			if !ok {
				return errors.New("unexpected discovery request")
			}
			if action.Profile != "codex" || !reflect.DeepEqual(action.Model, model) {
				return fmt.Errorf("request %d: profile %q model %v", index, action.Profile, action.Model)
			}
			var wantConfig any
			if index == 1 {
				wantConfig = expectedDraft
			}
			if err := daemonSameJSON(action.Config, wantConfig); err != nil {
				return fmt.Errorf("request %d: config: %w", index, err)
			}
			result := DaemonOk[DaemonReply](DaemonReplySubagentOptions{Options: options})
			if index == 2 {
				result = DaemonErr[DaemonReply]("profile discovery cancelled")
			}
			if err := daemonTestRespond(stream, request, result); err != nil {
				return err
			}
		}
		return nil
	})
	client := daemonTestConnect(t, metadata)
	discovered, err := client.SubagentOptions(context.Background(), "codex", nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := daemonTestJSONValue(t, discovered); !reflect.DeepEqual(got, expected) {
		t.Fatalf("options %v, want %v", got, expected)
	}
	discovered, err = client.SubagentOptions(context.Background(), "codex", &luna, &draft)
	if err != nil {
		t.Fatal(err)
	}
	if got := daemonTestJSONValue(t, discovered); !reflect.DeepEqual(got, expected) {
		t.Fatalf("options %v, want %v", got, expected)
	}
	_, err = client.SubagentOptions(context.Background(), "codex", &luna, nil)
	if err == nil || err.Error() != "profile discovery cancelled" {
		t.Fatalf("error %v", err)
	}
	wait()
}

func TestUpgradeRefusalRetriesTheIdenticalCommandButLostAcknowledgementsDoNot(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion, "test-token")
	action := DaemonActionSubmitSessionCommand{
		InheritedDraft: nil,
		SessionID:      "test-session",
		CommandID:      "steer-command",
		Command: core.RelayCommandSteer{
			ActivePromptID: "active-command",
			QueuedPromptID: "queued-command",
		},
	}
	expected := daemonTestJSONValue(t, action)
	wait := daemonTestServer(t, func() error {
		for _, reply := range []DaemonReply{DaemonReplyUpgradePending{}, DaemonReplyDone{}, nil} {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			request, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			if err := daemonSameJSON(request.Action, expected); err != nil {
				return err
			}
			if reply != nil {
				if err := daemonTestRespond(stream, request, DaemonOk(reply)); err != nil {
					return err
				}
			}
			_ = stream.Close()
		}
		return nil
	})
	client := daemonTestConnect(t, metadata)
	reply, err := client.requestWithReconnect(context.Background(), action, func(ctx context.Context) (*DaemonClient, error) {
		return DaemonClientConnect(ctx, metadata)
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, done := reply.(DaemonReplyDone); !done {
		t.Fatalf("reply %s", daemonReplyDebug(reply))
	}
	client = daemonTestConnect(t, metadata)
	_, err = client.requestWithReconnect(context.Background(), action, func(context.Context) (*DaemonClient, error) {
		t.Fatal("an ambiguous acknowledgement must not replay a mutation")
		return nil, nil
	})
	if err == nil {
		t.Fatal("a lost acknowledgement was reported as success")
	}
	wait()
}

// Hard-won: #1514: a refused consumer reused a blob from the old connection.
func TestRefusedBlobConsumerReuploadsBeforeSingleAcceptance(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion, "test")
	payload := bytes.Repeat([]byte{42}, 192*1024)
	wait := daemonTestServer(t, func() error {
		accepted := 0
		for attempt := range 4 {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			var received []byte
			var upload RequestEnvelope
			for {
				request, err := ReadFrame[RequestEnvelope](stream)
				if err != nil {
					return err
				}
				chunk, ok := request.Action.(DaemonActionUploadChunk)
				if !ok {
					return errors.New("expected upload before consuming action")
				}
				received = append(received, chunk.Bytes...)
				if chunk.Sha256 != nil {
					upload = request
					break
				}
			}
			if !bytes.Equal(received, payload) {
				return fmt.Errorf("attempt %d received %d bytes", attempt, len(received))
			}
			blob := fmt.Sprintf("connection-%d", attempt)
			if err := daemonTestRespond(stream, upload, DaemonOk[DaemonReply](DaemonReplyBlobUploaded{Blob: blob})); err != nil {
				return err
			}
			consume, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			install, ok := consume.Action.(DaemonActionInstallAttachment)
			if !ok {
				return errors.New("expected consumer")
			}
			if install.Blob != blob {
				return fmt.Errorf("consumed %q, uploaded %q", install.Blob, blob)
			}
			if attempt == 2 {
				// Lost ACK must not reconnect or replay.
				_ = stream.Close()
				continue
			}
			var reply DaemonReply = DaemonReplyUpgradePending{}
			if attempt != 0 && attempt != 3 {
				accepted++
				reply = DaemonReplyDone{}
			}
			if err := daemonTestRespond(stream, consume, DaemonOk(reply)); err != nil {
				return err
			}
			_ = stream.Close()
		}
		if accepted != 1 {
			return fmt.Errorf("accepted %d times", accepted)
		}
		return nil
	})
	reference := core.AttachmentRef{
		Sha256:   strings.Repeat("a", 64),
		MimeType: "image/png",
		Size:     len(payload),
		Width:    1,
		Height:   1,
	}
	action := func(blob string) DaemonAction {
		return DaemonActionInstallAttachment{SessionID: "session", Reference: reference, Blob: blob}
	}
	reader := bytes.NewReader(payload)
	client := daemonTestConnect(t, metadata)
	reply, err := client.uploadAndConsumeWithReconnect(context.Background(), reader, action, func(ctx context.Context) (*DaemonClient, error) {
		return DaemonClientConnect(ctx, metadata)
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, done := reply.(DaemonReplyDone); !done {
		t.Fatalf("reply %s", daemonReplyDebug(reply))
	}
	if _, err := reader.Seek(0, io.SeekStart); err != nil {
		t.Fatal(err)
	}
	client = daemonTestConnect(t, metadata)
	_, err = client.uploadAndConsumeWithReconnect(context.Background(), reader, action, func(context.Context) (*DaemonClient, error) {
		t.Fatal("lost acknowledgement cannot authorize replay")
		return nil, nil
	})
	if err == nil {
		t.Fatal("a lost acknowledgement was reported as success")
	}
	// Hard-won: af709d6e2: cancellation was ignored during refused-upload reconnection.
	if _, err := reader.Seek(0, io.SeekStart); err != nil {
		t.Fatal(err)
	}
	client = daemonTestConnect(t, metadata)
	reconnecting := make(chan struct{})
	var notify sync.Once
	result := make(chan error, 1)
	go func() {
		_, err := client.uploadAndConsumeObservedWithReconnect(
			context.Background(),
			reader,
			action,
			func(ctx context.Context) (*DaemonClient, error) {
				notify.Do(func() { close(reconnecting) })
				<-ctx.Done()
				return nil, ctx.Err()
			},
			func(context.Context, bool) error { return nil },
			reconnecting,
		)
		result <- err
	}()
	select {
	case err := <-result:
		if err == nil || !strings.Contains(err.Error(), "upload cancelled") {
			t.Fatalf("error %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("cancellation must interrupt reconnection")
	}
	wait()
}

// Hard-won: 25fee3db784d: protocol-mismatch advice omitted which installed mj the user ran
func TestUnsupportedProtocolMessageNamesBothBinariesAndVersions(t *testing.T) {
	daemonExecutable := "/home/dev/mj/target/release/mj"
	clientExecutable := "/home/dev/.cargo/bin/mj"
	message := unsupportedDaemonProtocolMessage(
		ProtocolVersion+5,
		DescribeDaemonAndClientBuilds(
			4242,
			BuildDescription{Executable: &daemonExecutable, Version: "2.14.0"},
			BuildDescription{Executable: &clientExecutable, Version: "2.9.0"},
		),
	)
	want := fmt.Sprintf("the daemon uses a newer protocol (%d) than this client (%d). "+
		"Daemon 4242 runs /home/dev/mj/target/release/mj (version 2.14.0), "+
		"while this client runs /home/dev/.cargo/bin/mj (version 2.9.0). "+
		"Put the daemon's directory first on PATH, or reinstall this client from that build.",
		ProtocolVersion+5, ProtocolVersion)
	if message != want {
		t.Fatalf("message\n%s\nwant\n%s", message, want)
	}
}

func TestUnsupportedProtocolMessageStillNamesTheClientWhenTheDaemonFileIsUnknown(t *testing.T) {
	clientExecutable := "/home/dev/.cargo/bin/mj"
	message := unsupportedDaemonProtocolMessage(
		ProtocolVersion+1,
		DescribeDaemonAndClientBuilds(
			4242,
			BuildDescription{Executable: nil, Version: "2.14.0"},
			BuildDescription{Executable: &clientExecutable, Version: "2.9.0"},
		),
	)
	if !strings.Contains(message, "Daemon 4242 runs an unknown file (version 2.14.0)") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "this client runs /home/dev/.cargo/bin/mj (version 2.9.0)") {
		t.Fatal(message)
	}
}

func TestUnavailableLifecycleObservationDistinguishesDisconnectFromOperationFailure(t *testing.T) {
	listener, address := daemonTestListener(t)
	metadata := daemonTestMetadata(address, ProtocolVersion, "lifecycle-observation")
	receipt := LifecycleReceipt{SessionID: "observed-session", OperationID: "accepted-operation"}
	wait := daemonTestServer(t, func() error {
		for phase := range 5 {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			request, err := ReadFrame[RequestEnvelope](stream)
			if err != nil {
				return err
			}
			_, waitAction := request.Action.(DaemonActionWaitSessionLifecycle)
			_, startAction := request.Action.(DaemonActionStartSessionLifecycle)
			switch phase {
			case 0, 1, 4:
				if !waitAction {
					return fmt.Errorf("phase %d: %T", phase, request.Action)
				}
			default:
				if !startAction {
					return fmt.Errorf("phase %d: %T", phase, request.Action)
				}
			}
			if phase == 1 || phase == 3 || phase == 4 {
				result := DaemonErr[DaemonReply]("daemon is shutting down; retry to reach a fresh daemon")
				if phase == 4 {
					result = DaemonOk[DaemonReply](DaemonReplyLifecycleFailed{Detail: "checkpoint verification failed"})
				}
				if err := daemonTestRespond(stream, request, result); err != nil {
					return err
				}
			}
			_ = stream.Close()
		}
		return nil
	})
	ctx := context.Background()
	client := daemonTestConnect(t, metadata)
	_, err := client.WaitSessionLifecycle(ctx, receipt)
	var unavailable LifecycleObservationUnavailable
	if !errors.As(err, &unavailable) || unavailable.Receipt != receipt {
		t.Fatalf("disconnect: %v", err)
	}
	client.Close()
	client = daemonTestConnect(t, metadata)
	_, err = client.WaitSessionLifecycle(ctx, receipt)
	if !errors.As(err, new(LifecycleObservationUnavailable)) {
		t.Fatalf("refused observation: %v", err)
	}
	if !strings.Contains(err.Error(), "daemon is shutting down") {
		t.Fatalf("refused observation: %v", err)
	}
	client.Close()
	for _, disconnected := range []bool{true, false} {
		client := daemonTestConnect(t, metadata)
		_, err := client.StartSessionLifecycle(ctx, SessionLifecycleRequestSuspend{
			SessionID:           "observed-session",
			DiscardUntracked:    core.UntrackedConsent{Kind: core.ConsentAny},
			AbortOperations:     core.OperationConsent{Kind: core.ConsentNone},
			WithoutCheckpoint:   false,
			ConfirmNoCheckpoint: false,
		})
		if disconnected {
			var unconfirmed LifecycleAdmissionUnconfirmed
			if !errors.As(err, &unconfirmed) || unconfirmed.SessionID != "observed-session" {
				t.Fatalf("disconnected start: %v", err)
			}
			if errors.As(err, new(DaemonRefusal)) {
				t.Fatalf("disconnected start is a refusal: %v", err)
			}
		} else {
			if !errors.As(err, new(DaemonRefusal)) {
				t.Fatalf("refused start: %v", err)
			}
			if errors.As(err, new(LifecycleAdmissionUnconfirmed)) {
				t.Fatalf("refused start is unconfirmed: %v", err)
			}
		}
		client.Close()
	}
	client = daemonTestConnect(t, metadata)
	_, err = client.WaitSessionLifecycle(ctx, receipt)
	if !errors.As(err, new(DaemonRefusal)) || errors.As(err, new(LifecycleObservationUnavailable)) {
		t.Fatalf("operation failure: %v", err)
	}
	if !strings.Contains(err.Error(), "checkpoint verification failed") {
		t.Fatalf("operation failure: %v", err)
	}
	client.Close()
	wait()
}
