// Port of the tests of mj-worker/src/worker_runtime/unix.rs: its
// history_tests and preparation_tests modules.

//go:build unix

package worker

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// runtimeUnixStreamPair is tokio's UnixStream::pair.
func runtimeUnixStreamPair(t *testing.T) (*net.UnixConn, *net.UnixConn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatal(err)
	}
	connection := func(fd int, name string) *net.UnixConn {
		file := os.NewFile(uintptr(fd), name)
		defer file.Close()
		conn, err := net.FileConn(file)
		if err != nil {
			t.Fatal(err)
		}
		return conn.(*net.UnixConn)
	}
	return connection(fds[0], "server"), connection(fds[1], "client")
}

// runtimeUnixSend writes one request frame.
func runtimeUnixSend(t *testing.T, conn net.Conn, requestID string, request core.RelayRequest) {
	t.Helper()
	encoded, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := conn.Write(append(encoded, '\n')); err != nil {
		t.Fatal(err)
	}
}

// runtimeUnixReceive reads one response frame.
func runtimeUnixReceive(t *testing.T, reader *bufio.Reader, conn net.Conn) core.RelayResponseEnvelope {
	t.Helper()
	if err := conn.SetReadDeadline(time.Now().Add(ioTimeout)); err != nil {
		t.Fatal(err)
	}
	line, err := reader.ReadString('\n')
	if err != nil {
		t.Fatalf("read a response: %v", err)
	}
	var response core.RelayResponseEnvelope
	if err := json.Unmarshal([]byte(line), &response); err != nil {
		t.Fatalf("decode %s: %v", line, err)
	}
	return response
}

// runtimeUnixPayload is the payload of an Ok response.
func runtimeUnixPayload(t *testing.T, response core.RelayResponseEnvelope) core.RelayResponsePayload {
	t.Helper()
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("%#v", response.Body)
	}
	return ok.Payload
}

func TestHistoryRoundTripsLargeRepliesWithoutDelegationOrDurableEvents(t *testing.T) {
	temp := t.TempDir()
	durable, err := DurableRelayOpen(temp, "history-test", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	relay := NewRelayCell(durable)
	historySocket := filepath.Join(temp, "control.sock")
	memory := newProjectMemoryEndpoint(&core.ProjectMemoryLaunchConfig{
		ProjectKey:    "key",
		Root:          filepath.Join(temp, "memory"),
		BaselineRoot:  filepath.Join(temp, "baseline"),
		MCPDelivery:   core.ProjectMemoryMcpDeliveryAcp,
		HistorySocket: &historySocket,
	})
	wake := newDispatchWakeChannel()
	fatal := make(chan error, 1)
	var tasks []*core.JoinHandle[error]
	var clients []*net.UnixConn
	var readers []*bufio.Reader
	for range 2 {
		server, client := runtimeUnixStreamPair(t)
		runtime := defaultConnectionRuntime()
		runtime.projectMemory = memory
		tasks = append(tasks, core.Spawn(func() error {
			return serveClientWithMemory(context.Background(), server, relay, wake,
				credentialEndpointResult{err: errors.New("no credentials")}, runtime, fatal)
		}))
		clients = append(clients, client)
		readers = append(readers, bufio.NewReader(client))
	}
	runtimeUnixSend(t, clients[0], "request", core.RelayRequestHistoryQuery{
		Query: core.HistoryQuerySearchSessions{Query: "needle", Limit: 20},
	})
	var requests []core.HistoryRequest
	for {
		runtimeUnixSend(t, clients[1], "request", core.RelayRequestHistoryRequests{})
		payload, ok := runtimeUnixPayload(t, runtimeUnixReceive(t, readers[1], clients[1])).(core.RelayResponsePayloadHistoryRequests)
		if !ok {
			t.Fatal("history requests")
		}
		if len(payload.Requests) > 0 {
			requests = payload.Requests
			break
		}
		time.Sleep(time.Millisecond)
	}
	value, err := json.Marshal(map[string]string{"text": strings.Repeat("é🙂", 30_000)})
	if err != nil {
		t.Fatal(err)
	}
	runtimeUnixSend(t, clients[1], "request", core.RelayRequestCompleteHistoryRequest{
		Result: core.HistoryResult{RequestID: requests[0].RequestID, Value: value},
	})
	if _, ok := runtimeUnixPayload(t, runtimeUnixReceive(t, readers[1], clients[1])).(core.RelayResponsePayloadHistoryRequestCompleted); !ok {
		t.Fatal("the completion was not acknowledged")
	}
	result, ok := runtimeUnixPayload(t, runtimeUnixReceive(t, readers[0], clients[0])).(core.RelayResponsePayloadHistoryResult)
	if !ok {
		t.Fatal("history result")
	}
	var got, want any
	if err := json.Unmarshal(result.Result.Value, &got); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(value, &want); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatal("the history reply changed on its way back")
	}
	// Disconnect while another request is waiting for the controller.
	runtimeUnixSend(t, clients[0], "request", core.RelayRequestHistoryQuery{
		Query: core.HistoryQueryTraceFile{Path: "a.rs", Limit: 20},
	})
	for len(memory.history.Requests()) == 0 {
		time.Sleep(time.Millisecond)
	}
	for _, client := range clients {
		_ = client.Close()
	}
	for _, task := range tasks {
		err := testSupportWaitFor("a closed history connection", func() error {
			result, joinErr := task.Join(context.Background())
			return errors.Join(result, joinErr)
		})
		if err != nil {
			t.Fatal(err)
		}
	}
	if pending := memory.history.Requests(); len(pending) != 0 {
		t.Fatalf("abandoned history requests remain: %#v", pending)
	}
	guard := relay.Lock()
	sessionID := guard.OperationalState().SessionID
	guard.Unlock()
	if sessionID != "history-test" {
		t.Fatal(sessionID)
	}
}

func preparationTestRelay(t *testing.T, root string) *RelayCell {
	t.Helper()
	durable, err := DurableRelayOpen(root, "preparation-test", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	return NewRelayCell(durable)
}

// preparationControlServer is a published control socket and one client
// connection to it.
type preparationControlServer struct {
	accept *core.JoinHandle[error]
	abort  context.CancelFunc
	reader *bufio.Reader
	conn   *net.UnixConn
}

func preparationStartControlServer(t *testing.T, root string, relay *RelayCell, dispatchWake *dispatchWakeChannel, preparation *core.Watch[preparationSnapshot], preparationCancel context.CancelFunc) preparationControlServer {
	t.Helper()
	socket := filepath.Join(root, "control.sock")
	listener, guard, err := publishControlSocket(root, socket)
	if err != nil {
		t.Fatal(err)
	}
	fatal := make(chan error, 1)
	runtime := defaultConnectionRuntime()
	runtime.preparation = preparation
	runtime.preparationCancel = preparationCancel
	ctx, abort := context.WithCancel(context.Background())
	accept := core.Spawn(func() error {
		return acceptWorkerClients(ctx, listener, guard, relay, dispatchWake,
			credentialEndpointResult{err: errors.New("fixture has no credentials")}, runtime, fatal)
	})
	conn, err := core.ConnectUnixStream(socket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		abort()
		_ = conn.Close()
	})
	return preparationControlServer{accept: accept, abort: abort, reader: bufio.NewReader(conn), conn: conn}
}

func (s preparationControlServer) request(t *testing.T, requestID string, request core.RelayRequest) core.RelayResponseEnvelope {
	t.Helper()
	runtimeUnixSend(t, s.conn, requestID, request)
	return runtimeUnixReceive(t, s.reader, s.conn)
}

func preparationHello() core.RelayRequest {
	return core.RelayRequestHello{ControllerVersion: "test", Supported: core.RelayVersionRangeCurrent}
}

func preparationPrompt(text string) []acpschema.ContentBlock {
	return []acpschema.ContentBlock{{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}}
}

func TestAdoptionWaitCapturesBetweenTurnsAndDisconnectReleasesQueuedWork(t *testing.T) {
	for _, mode := range []string{"served", "disconnect", "another-waiter"} {
		t.Run(mode, func(t *testing.T) {
			temp := t.TempDir()
			relay := preparationTestRelay(t, temp)
			wake := newDispatchWakeChannel()
			preparation := core.NewWatch(preparationSnapshot{})
			server := preparationStartControlServer(t, temp, relay, wake, preparation, func() {})
			server.request(t, "hello", preparationHello())
			for _, submitted := range []struct {
				id      string
				command core.RelayCommand
			}{
				{"running-prompt", core.RelayCommandPrompt{Prompt: preparationPrompt("first")}},
				{"next-prompt", core.RelayCommandPrompt{Prompt: preparationPrompt("second")}},
				{"queued-shell", core.RelayCommandRunUserShell{Command: "printf next"}},
			} {
				response := server.request(t, submitted.id, core.RelayRequestSubmit{CommandID: submitted.id, Command: submitted.command})
				if _, ok := response.Body.(core.RelayResponseBodyOk); !ok {
					t.Fatalf("%#v", response)
				}
			}
			guard := relay.Lock()
			if _, err := guard.ClaimPendingCommands(true); err != nil {
				t.Fatal(err)
			}
			guard.Unlock()
			response := server.request(t, "cut", core.RelayRequestBeginLiveBaseline{})
			if _, ok := runtimeUnixPayload(t, response).(core.RelayResponsePayloadLiveBaselineWaiting); !ok {
				t.Fatalf("%#v", response)
			}
			claimShells := func() []core.ClaimedRelayCommand {
				guard := relay.Lock()
				defer guard.Unlock()
				claimed, err := guard.ClaimPendingUserShellCommandsUpTo(1)
				if err != nil {
					t.Fatal(err)
				}
				return claimed
			}
			if claimed := claimShells(); len(claimed) != 0 {
				t.Fatalf("queued work ran during the adoption wait: %#v", claimed)
			}
			reader, conn := server.reader, server.conn
			if mode == "another-waiter" {
				peer, err := core.ConnectUnixStream(filepath.Join(temp, "control.sock"))
				if err != nil {
					t.Fatal(err)
				}
				t.Cleanup(func() { _ = peer.Close() })
				other := preparationControlServer{reader: bufio.NewReader(peer), conn: peer}
				other.request(t, "hello-other", core.RelayRequestHello{ControllerVersion: "other", Supported: core.RelayVersionRangeCurrent})
				waiting := other.request(t, "cut-other", core.RelayRequestBeginLiveBaseline{})
				if _, ok := runtimeUnixPayload(t, waiting).(core.RelayResponsePayloadLiveBaselineWaiting); !ok {
					t.Fatalf("%#v", waiting)
				}
				_ = conn.Close()
				reader, conn = other.reader, other.conn
				if claimed := claimShells(); len(claimed) != 0 {
					t.Fatalf("queued work ran while another waiter held the queue: %#v", claimed)
				}
			}
			if mode == "disconnect" {
				_ = conn.Close()
				// The current turn still runs: EOF alone must remove the hold.
				testSupportWaitFor("the released adoption hold", func() struct{} {
					for len(claimShells()) == 0 {
						time.Sleep(time.Millisecond)
					}
					return struct{}{}
				})
			} else {
				guard := relay.Lock()
				if _, err := guard.RecordCommandCompleted("running-prompt", core.RelayCommandOutcomePrompt{StopReason: "end_turn"}); err != nil {
					t.Fatal(err)
				}
				guard.Unlock()
				started := runtimeUnixReceive(t, reader, conn)
				if _, ok := runtimeUnixPayload(t, started).(core.RelayResponsePayloadLiveBaselineStarted); !ok {
					t.Fatalf("%#v", started)
				}
				var bytes []byte
				offset := uint64(0)
				for {
					runtimeUnixSend(t, conn, "page", core.RelayRequestReadLiveBaselinePage{Offset: offset})
					response := runtimeUnixReceive(t, reader, conn)
					page, ok := runtimeUnixPayload(t, response).(core.RelayResponsePayloadLiveBaselinePage)
					if !ok {
						t.Fatalf("%#v", response)
					}
					chunk, err := base64.StdEncoding.DecodeString(page.Page.Chunk)
					if err != nil {
						t.Fatal(err)
					}
					bytes = append(bytes, chunk...)
					offset = page.Page.NextOffset
					if page.Page.Complete {
						break
					}
				}
				var cut core.LiveSessionBaseline
				if err := json.Unmarshal(bytes, &cut); err != nil {
					t.Fatal(err)
				}
				if cut.State.ActivePrompt != nil {
					t.Error("the cut was taken inside a turn")
				}
				if len(cut.Queue) == 0 || cut.Queue[0].CommandID != "next-prompt" {
					t.Errorf("queue %#v", cut.Queue)
				}
				if len(cut.State.ActiveUserShells) == 0 || cut.State.ActiveUserShells[0].CommandID != "queued-shell" ||
					cut.State.ActiveUserShells[0].StartedAtMs != nil {
					t.Errorf("shells %#v", cut.State.ActiveUserShells)
				}
				guard = relay.Lock()
				if _, err := guard.ClaimPendingCommands(true); err != nil {
					t.Fatal(err)
				}
				state := guard.OperationalState()
				latest := guard.LatestOrdinal()
				guard.Unlock()
				if state.ActivePrompt == nil || state.ActivePrompt.CommandID != "next-prompt" {
					t.Errorf("active prompt %#v", state.ActivePrompt)
				}
				if latest <= cut.State.LatestOrdinal {
					t.Error("work did not resume after the cut")
				}
				_ = conn.Close()
			}
			server.abort()
			_, _ = server.accept.Join(context.Background())
		})
	}
}

func TestFailingProjectRecipeRecordsNoticeAndContinuesPreparation(t *testing.T) {
	root := t.TempDir()
	checkout := filepath.Join(root, "checkout")
	bin := filepath.Join(root, "bin")
	for _, directory := range []string{checkout, bin} {
		if err := os.MkdirAll(directory, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(checkout, "mise.toml"), []byte("[tools]\nmissing = '1'\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	mise := filepath.Join(bin, "mise")
	// More than 64KB catches pipe truncation/deadlocks; both streams belong
	// in the log even when installation exits unsuccessfully.
	if err := os.WriteFile(mise, []byte("#!/bin/sh\n/usr/bin/head -c 131072 /dev/zero\nprintf 'recipe failed\\n' >&2\nexit 42\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	relay := preparationTestRelay(t, root)
	status := core.NewWatch(preparationSnapshot{})
	config := launchConfig(root)
	if config.Environment == nil {
		config.Environment = map[string]string{}
	}
	config.Environment["PATH"] = bin
	config.Environment["MISE_DATA_DIR"] = filepath.Join(root, "mise")
	_, install, err := prepareProjectTools(context.Background(), root, []string{checkout}, &config, map[string]string{}, status, relay)
	if err != nil {
		t.Fatal(err)
	}
	// mise failing its status commands falls back to naming the recipe
	// files; the installation then runs after preparation returns.
	if install == nil {
		t.Fatal("installation planned")
	}
	install.run(context.Background(), root, relay)
	guard := relay.Lock()
	defer guard.Unlock()
	payload, err := guard.DrainMailbox("PostToolBatch")
	if err != nil {
		t.Fatal(err)
	}
	drained, ok := payload.(core.RelayResponsePayloadMailboxDrained)
	if !ok || drained.Text == nil {
		t.Fatalf("drain returned another payload: %#v", payload)
	}
	text := *drained.Text
	if drained.Count != 2 {
		t.Fatalf("%d items: %s", drained.Count, text)
	}
	started := strings.Index(text, "being installed in the background")
	finished := strings.Index(text, "did not finish")
	if started < 0 || finished < 0 || started >= finished {
		t.Fatal(text)
	}
	if !strings.Contains(text, "mise.toml") || !strings.Contains(text, "second message") {
		t.Fatal(text)
	}
	if !strings.Contains(text, "42") || !strings.Contains(text, "project-tools.log") || !strings.Contains(text, "rerun `mise install`") {
		t.Fatal(text)
	}
	events, err := guard.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		t.Fatal(err)
	}
	var notices []string
	for _, event := range events {
		if notice, ok := event.Observation.(core.RelayObservationNotice); ok {
			notices = append(notices, notice.Message)
		}
	}
	if len(notices) != 2 {
		t.Fatalf("notices %#v", notices)
	}
	found := false
	for _, message := range notices {
		found = found || (strings.Contains(message, "mise install") && strings.Contains(message, "42") && strings.Contains(message, "project-tools.log"))
	}
	if !found {
		t.Fatalf("notices %#v", notices)
	}
	log, err := os.ReadFile(filepath.Join(root, projectToolsLogFile))
	if err != nil {
		t.Fatal(err)
	}
	if len(log) <= 262144 {
		t.Fatalf("the log holds %d bytes", len(log))
	}
	if !strings.Contains(string(log), "recipe failed") {
		t.Fatal("the log lacks the recipe's stderr")
	}
}

func preparationStartedServices(t *testing.T, root string, relay *RelayCell, commands *commandChannel) preparedConnectionServices {
	return preparedConnectionServices{
		commands: commands,
		reviewer: &reviewCapture{
			SessionID:             "preparation-test",
			Cwd:                   root,
			AdditionalDirectories: []string{},
			PrimaryRelay:          relay,
		},
	}
}

func TestHelloAttachAndPromptQueueWorkWhileFakePreparationIsBlocked(t *testing.T) {
	root := t.TempDir()
	relay := preparationTestRelay(t, root)
	status := core.NewWatch(preparationSnapshot{})
	cancellation, cancel := context.WithCancel(context.Background())
	defer cancel()
	dispatchWake := newDispatchWakeChannel()
	commands := newCommandChannel(8)
	services := preparationStartedServices(t, root, relay, commands)
	prepared := preparationSnapshot{state: core.HarnessPreparationStarted{}, services: &services}
	preparationStarted := make(chan struct{}, 1)
	releasePreparation := make(chan struct{})
	prep := core.Spawn(func() preparationStop {
		_, stop := supervisePreparation(cancellation, root, status, func(ctx context.Context) (struct{}, error) {
			if _, err := preparationStep(ctx, root, status, "harness-resolve", localPreparationStepTimeout); err != nil {
				return struct{}{}, err
			}
			preparationStarted <- struct{}{}
			<-releasePreparation
			status.SendReplace(prepared)
			return struct{}{}, nil
		})
		return stop
	})
	<-preparationStarted
	server := preparationStartControlServer(t, root, relay, dispatchWake, status, cancel)

	hello := server.request(t, "hello", preparationHello())
	if _, ok := runtimeUnixPayload(t, hello).(core.RelayResponsePayloadHello); !ok {
		t.Fatalf("%#v", hello)
	}
	attached := server.request(t, "attach", core.RelayRequestAttach{AfterDigest: core.RelayEventGenesisDigest})
	attach, ok := attached.Body.(core.RelayResponseBodyOk)
	if !ok {
		t.Fatalf("attach failed before preparation completed: %#v", attached.Body)
	}
	attachedState, ok := attach.Payload.(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("attach failed before preparation completed: %#v", attach.Payload)
	}
	if preparing, ok := attachedState.State.HarnessPreparation.(core.HarnessPreparationPreparing); !ok || preparing.Step != "harness-resolve" {
		t.Fatalf("preparation %#v", attachedState.State.HarnessPreparation)
	}

	submitted := server.request(t, "prompt-submit", core.RelayRequestSubmit{
		CommandID: "prompt-during-preparation",
		Command:   core.RelayCommandPrompt{Prompt: preparationPrompt("queued prompt")},
	})
	if _, ok := submitted.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("%#v", submitted)
	}
	statusResponse := server.request(t, "status-queued", core.RelayRequestStatus{})
	state, ok := runtimeUnixPayload(t, statusResponse).(core.RelayResponsePayloadStatus)
	if !ok {
		t.Fatal("status failed while preparation was blocked")
	}
	represented := state.State.ActivePrompt != nil && state.State.ActivePrompt.CommandID == "prompt-during-preparation"
	for _, prompt := range state.State.QueuedPrompts {
		represented = represented || prompt.CommandID == "prompt-during-preparation"
	}
	if !represented {
		t.Fatalf("accepted prompt was not represented in relay state: %#v", state.State)
	}
	select {
	case command := <-commands.receiver():
		t.Fatalf("a command was dispatched before preparation completed: %#v", command)
	default:
	}

	close(releasePreparation)
	if stop, err := prep.Join(context.Background()); err != nil || stop != nil {
		t.Fatalf("preparation stopped with %#v, %v", stop, err)
	}
	events := make(chan core.RuntimeEvent, 16)
	shellEvents := make(chan core.RuntimeEvent, 1)
	shells := NewUserShellRegistry(root, map[string]string{}, shellEvents)
	coordinatorCtx, abort := context.WithCancel(context.Background())
	coordinator := core.Spawn(func() error {
		return runRelayCoordinatorWithVerdict(coordinatorCtx, relay, events, dispatchWake, commands, shells, nil, nil, nil)
	})
	events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}
	select {
	case command := <-commands.receiver():
		prompt, ok := command.(CommandRequestPrompt)
		if !ok || prompt.RequestID != "prompt-during-preparation" {
			t.Fatalf("dispatched %#v", command)
		}
	case <-time.After(ioTimeout):
		t.Fatal("the queued prompt was not dispatched")
	}

	abort()
	_, _ = coordinator.Join(context.Background())
	_ = server.conn.CloseWrite()
	server.abort()
}

// preparationStartupFailure is the typed failure a stopped preparation ends with, which
// becomes the worker's exit record.
func preparationStartupFailure(t *testing.T, stop preparationStop) core.WorkerHarnessStartupFailure {
	t.Helper()
	failed, ok := stop.(preparationStopFailed)
	if !ok {
		t.Fatalf("preparation was cancelled instead of failing: %#v", stop)
	}
	var failure core.WorkerHarnessStartupFailure
	if !errors.As(failed.Value, &failure) {
		t.Fatalf("untyped preparation failure: %v", failed.Value)
	}
	return failure
}

func TestPanickingPreparationEndsWithATypedStartupFailure(t *testing.T) {
	panicFakePreparation := func() error {
		panic("fake profile step panicked")
	}
	temp := t.TempDir()
	status := core.NewWatch(preparationSnapshot{
		state: core.HarnessPreparationPreparing{Step: "subagent-profile", SinceMs: time.Now().UnixMilli()},
	})
	cancellation, cancel := context.WithCancel(context.Background())
	defer cancel()
	_, stop := supervisePreparation(cancellation, temp, status, func(context.Context) (struct{}, error) {
		return struct{}{}, panicFakePreparation()
	})
	failure := preparationStartupFailure(t, stop)
	if !strings.HasPrefix(failure.Message, "harness preparation failed at subagent-profile: ") ||
		!strings.Contains(failure.Message, "fake profile step panicked") {
		t.Fatalf("%#v", failure)
	}
	if _, preparing := status.Borrow().state.(core.HarnessPreparationPreparing); !preparing {
		t.Fatalf("%#v", status.Borrow().state)
	}
}

// A step whose thread blocks past its deadline fails the preparation at
// once: the worker exits, which is the only way to end that thread.
func TestBlockingPreparationDeadlineFailsWithoutWaitingForItsThread(t *testing.T) {
	root := t.TempDir()
	status := core.NewWatch(preparationSnapshot{})
	cancellation, cancel := context.WithCancel(context.Background())
	defer cancel()
	releaseBlocking := make(chan struct{})
	blockingFinished := make(chan struct{}, 1)
	type supervised struct{ stop preparationStop }
	result := core.Spawn(func() supervised {
		_, stop := supervisePreparation(cancellation, root, status, func(ctx context.Context) (struct{}, error) {
			budget, err := preparationStep(ctx, root, status, "acp-setup", 200*time.Millisecond)
			if err != nil {
				return struct{}{}, err
			}
			return boundedBlockingPreparationStep(ctx, &budget, func(context.Context) (struct{}, error) {
				<-releaseBlocking
				blockingFinished <- struct{}{}
				return struct{}{}, nil
			})
		})
		return supervised{stop}
	})
	var outcome supervised
	select {
	case <-result.Done():
		outcome, _ = result.Join(context.Background())
	case <-time.After(5 * time.Second):
		t.Fatal("an expired step must not wait for its blocked thread")
	}
	failure := preparationStartupFailure(t, outcome.stop)
	if !strings.Contains(failure.Message, "acp-setup") || !strings.Contains(failure.Message, "200ms deadline") {
		t.Fatalf("%#v", failure)
	}
	select {
	case <-blockingFinished:
		t.Fatal("the blocked thread finished before it was released")
	default:
	}
	startup, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
	if err != nil {
		t.Fatal(err)
	}
	var recorded struct {
		Failure struct {
			Step string `json:"step"`
		} `json:"failure"`
	}
	if err := json.Unmarshal(startup, &recorded); err != nil {
		t.Fatal(err)
	}
	if recorded.Failure.Step != "acp-setup" {
		t.Fatalf("startup record %s", startup)
	}
	close(releaseBlocking)
	testSupportWaitFor("the released blocking step", func() struct{} {
		<-blockingFinished
		return struct{}{}
	})
}

// A Close accepted during preparation and the Started publication are
// decided under the relay lock, in one order or the other: a sealed relay
// never gets a bridge, and a started bridge's coordinator owns lifecycle
// work the handler then leaves alone. A seal that publishStarted finds
// before the handler has cancelled preparation is still a cancellation, not
// a startup failure that would end the worker mid-Close.
// Hard-won: #1617: a Close racing Started cancelled a coordinator that held it InFlight.
func TestACloseDuringPreparationAndStartedHaveOneOrder(t *testing.T) {
	type order string
	const (
		closeThroughHandler        order = "CloseThroughHandler"
		startedFirst               order = "StartedFirst"
		sealedBeforeHandlerCancels order = "SealedBeforeHandlerCancels"
	)
	for _, order := range []order{closeThroughHandler, startedFirst, sealedBeforeHandlerCancels} {
		t.Run(string(order), func(t *testing.T) {
			root := t.TempDir()
			relay := preparationTestRelay(t, root)
			status := core.NewWatch(preparationSnapshot{
				state: core.HarnessPreparationPreparing{Step: "acp-setup", SinceMs: 1},
			})
			cancellation, cancel := context.WithCancel(context.Background())
			defer cancel()
			dispatchWake := newDispatchWakeChannel()
			dispatchWakes := dispatchWake
			commands := newCommandChannel(1)
			services := func() preparedConnectionServices {
				return preparationStartedServices(t, root, relay, commands)
			}
			server := preparationStartControlServer(t, root, relay, dispatchWake, status, cancel)
			// Held as the coordinator would hold it.
			if order == startedFirst {
				if _, err := publishStarted(cancellation, cancel, relay, status, &dispatchWakes, services()); err != nil {
					t.Fatal(err)
				}
			}
			checkpoint := server.request(t, "checkpoint-before-close", core.RelayRequestSubmit{
				CommandID: "close-barrier",
				Command:   core.RelayCommandBeginCheckpoint{},
			})
			if _, ok := checkpoint.Body.(core.RelayResponseBodyOk); !ok {
				t.Fatalf("%#v", checkpoint)
			}
			cursor, ok := runtimeUnixPayload(t, server.request(t, "cursor", core.RelayRequestStatus{})).(core.RelayResponsePayloadStatus)
			if !ok {
				t.Fatal("status unavailable during preparation")
			}
			if order == startedFirst {
				if cursor.State.CheckpointReady != nil {
					t.Fatal("the handler completed a checkpoint the coordinator owns")
				}
				if cancellation.Err() != nil {
					t.Fatal("a started preparation was cancelled")
				}
				_ = server.conn.CloseWrite()
				server.abort()
				return
			}
			if cursor.State.CheckpointReady == nil {
				t.Fatal("the handler did not complete the checkpoint during preparation")
			}
			closeRequest := core.RelayRequestSubmit{
				CommandID: "close-during-preparation",
				Command: core.RelayCommandClose{
					BarrierCommandID: "close-barrier",
					Expected:         *cursor.State.CheckpointReady,
				},
			}
			if order == closeThroughHandler {
				closed := server.request(t, "close", closeRequest)
				if _, ok := closed.Body.(core.RelayResponseBodyOk); !ok {
					t.Fatalf("%#v", closed)
				}
				if cancellation.Err() == nil {
					t.Fatal("the Close did not cancel preparation")
				}
			} else {
				// Accepted, as by a handler that has not yet taken the lock
				// again to cancel preparation.
				guard := relay.Lock()
				closed := guard.Handle(core.RelayRequestEnvelope{
					ProtocolVersion: core.RelayProtocolVersion,
					RequestID:       "close",
					Request:         closeRequest,
				})
				guard.Unlock()
				if _, ok := closed.Body.(core.RelayResponseBodyOk); !ok {
					t.Fatalf("%#v", closed)
				}
				if cancellation.Err() != nil {
					t.Fatal("preparation was cancelled before the handler took the lock again")
				}
			}
			fakeServices := services()
			_, stop := supervisePreparation(cancellation, root, status, func(context.Context) (struct{}, error) {
				_, err := publishStarted(cancellation, cancel, relay, status, &dispatchWakes, fakeServices)
				return struct{}{}, err
			})
			if stop == nil {
				t.Fatal("a bridge started for a relay a Close had sealed")
			}
			if _, cancelled := stop.(preparationStopCancelled); !cancelled {
				t.Fatalf("%s: %#v", order, stop)
			}
			snapshot := status.Borrow()
			if !snapshot.isPending() {
				t.Fatal("a sealed relay published Started")
			}
			if dispatchWakes == nil {
				t.Fatal("the sealed relay lost its dispatch wakes")
			}
			_ = server.conn.CloseWrite()
			server.abort()
		})
	}
}

func TestCloseDuringPreparationKillsTheFakeHarnessProcessGroup(t *testing.T) {
	root := t.TempDir()
	relay := preparationTestRelay(t, root)
	status := core.NewWatch(preparationSnapshot{})
	cancellation, cancel := context.WithCancel(context.Background())
	defer cancel()
	dispatchWake := newDispatchWakeChannel()
	server := preparationStartControlServer(t, root, relay, dispatchWake, status, cancel)
	pidPath := filepath.Join(root, "fake-harness.pid")
	type supervised struct{ stop preparationStop }
	prep := core.Spawn(func() supervised {
		_, stop := supervisePreparation(cancellation, root, status, func(ctx context.Context) (struct{}, error) {
			if _, err := preparationStep(ctx, root, status, "harness-resolve", localPreparationStepTimeout); err != nil {
				return struct{}{}, err
			}
			child := exec.Command("sh", "-c", "printf '%s\\n' \"$$\" > \"$FAKE_PID_FILE\"; exec sleep 60")
			child.Env = append(os.Environ(), "FAKE_PID_FILE="+pidPath)
			output, err := core.RunBounded(ctx, child, 1024, 120*time.Second)
			if err != nil {
				return struct{}{}, err
			}
			return struct{}{}, errors.New("fake harness unexpectedly exited: " + workFactsExitStatus(output.Status))
		})
		return supervised{stop}
	})
	deadline := time.Now().Add(2 * time.Second)
	for {
		if _, err := os.Stat(pidPath); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("fake harness process started")
		}
		time.Sleep(10 * time.Millisecond)
	}
	var pid int
	testSupportWaitFor("the fake harness pid", func() struct{} {
		for {
			text, err := os.ReadFile(pidPath)
			if err == nil {
				if parsed, err := strconv.Atoi(strings.TrimSpace(string(text))); err == nil {
					pid = parsed
					return struct{}{}
				}
			}
			time.Sleep(10 * time.Millisecond)
		}
	})

	checkpoint := server.request(t, "checkpoint-before-close", core.RelayRequestSubmit{
		CommandID: "close-barrier",
		Command:   core.RelayCommandBeginCheckpoint{},
	})
	if _, ok := checkpoint.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("%#v", checkpoint)
	}
	state, ok := runtimeUnixPayload(t, server.request(t, "checkpoint-cursor", core.RelayRequestStatus{})).(core.RelayResponsePayloadStatus)
	if !ok || state.State.CheckpointReady == nil {
		t.Fatalf("checkpoint status unavailable: %#v", state)
	}
	closed := server.request(t, "close-during-preparation", core.RelayRequestSubmit{
		CommandID: "close-during-preparation",
		Command: core.RelayCommandClose{
			BarrierCommandID: "close-barrier",
			Expected:         *state.State.CheckpointReady,
		},
	})
	if _, ok := closed.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("close did not complete during preparation: %#v", closed.Body)
	}
	completed := server.request(t, "complete-checkpoint", core.RelayRequestSubmit{
		CommandID: "checkpoint-complete",
		Command:   core.RelayCommandCompleteCheckpoint{BarrierCommandID: "close-barrier"},
	})
	if _, ok := completed.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("%#v", completed)
	}
	var outcome supervised
	select {
	case <-prep.Done():
		outcome, _ = prep.Join(context.Background())
	case <-time.After(2 * time.Second):
		t.Fatal("close cancels the preparation owner")
	}
	if _, cancelled := outcome.stop.(preparationStopCancelled); !cancelled {
		t.Fatalf("%#v", outcome.stop)
	}
	if preparing, ok := status.Borrow().state.(core.HarnessPreparationPreparing); !ok || preparing.Step != "harness-resolve" {
		t.Fatalf("%#v", status.Borrow().state)
	}
	killed := time.Now().Add(2 * time.Second)
	for syscall.Kill(pid, 0) == nil {
		if time.Now().After(killed) {
			t.Fatal("preparation cancellation terminates its process group")
		}
		time.Sleep(10 * time.Millisecond)
	}
	final, ok := runtimeUnixPayload(t, server.request(t, "closed-status", core.RelayRequestStatus{})).(core.RelayResponsePayloadStatus)
	if !ok {
		t.Fatal("closed relay stopped serving")
	}
	if final.State.Execution != core.RelayExecutionStateClosed {
		t.Fatalf("execution %v", final.State.Execution)
	}
	// A sealed relay is not a startup failure; Closed is the fact.
	if _, failed := final.State.HarnessPreparation.(core.HarnessPreparationFailed); failed {
		t.Fatal("a sealed relay reported a startup failure")
	}
	_ = server.conn.CloseWrite()
	server.abort()
}
