// Port of the tests of mj-worker/src/worker_runtime/unix/dispatch.rs: its
// startup_tests module and dispatch/verdict_tests.rs.

//go:build unix

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// manualCoordinatorClock is the startup deadline's clock under test: time
// moves only when the test advances it, as with tokio's paused clock.
type manualCoordinatorClock struct {
	mu      sync.Mutex
	current time.Time
	waiters []*manualClockWaiter
}

type manualClockWaiter struct {
	deadline time.Time
	fired    chan time.Time
}

func newManualCoordinatorClock() *manualCoordinatorClock {
	return &manualCoordinatorClock{current: time.Unix(1_000_000, 0)}
}

func (c *manualCoordinatorClock) now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.current
}

func (c *manualCoordinatorClock) at(deadline time.Time) (<-chan time.Time, func()) {
	c.mu.Lock()
	defer c.mu.Unlock()
	fired := make(chan time.Time, 1)
	if !deadline.After(c.current) {
		fired <- c.current
		return fired, func() {}
	}
	waiter := &manualClockWaiter{deadline: deadline, fired: fired}
	c.waiters = append(c.waiters, waiter)
	return fired, func() {
		c.mu.Lock()
		defer c.mu.Unlock()
		for index, registered := range c.waiters {
			if registered == waiter {
				c.waiters = append(c.waiters[:index], c.waiters[index+1:]...)
				return
			}
		}
	}
}

// advance moves the clock and fires every deadline it reaches.
func (c *manualCoordinatorClock) advance(duration time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.current = c.current.Add(duration)
	remaining := c.waiters[:0]
	for _, waiter := range c.waiters {
		if waiter.deadline.After(c.current) {
			remaining = append(remaining, waiter)
			continue
		}
		waiter.fired <- c.current
	}
	c.waiters = remaining
}

// waitForDeadline waits until the coordinator waits for deadline: it has
// processed what it was given and armed the timer, as a single-threaded
// tokio test sees before it resumes.
func (c *manualCoordinatorClock) waitForDeadline(t *testing.T, deadline time.Time, coordinator *core.JoinHandle[error]) {
	t.Helper()
	limit := time.Now().Add(ioTimeout)
	for time.Now().Before(limit) {
		c.mu.Lock()
		armed := false
		for _, waiter := range c.waiters {
			armed = armed || waiter.deadline.Equal(deadline)
		}
		c.mu.Unlock()
		if armed {
			return
		}
		select {
		case <-coordinator.Done():
			t.Fatalf("the coordinator stopped before arming %v", deadline)
		case <-time.After(time.Millisecond):
		}
	}
	t.Fatalf("the coordinator never armed %v", deadline)
}

func startupSubmit(t *testing.T, relay *RelayCell, commandID string, command core.RelayCommand) {
	t.Helper()
	guard := relay.Lock()
	response := guard.Handle(core.RelayRequestEnvelope{
		RequestID:       commandID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestSubmit{CommandID: commandID, Command: command},
	})
	guard.Unlock()
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("submit %s: %#v", commandID, response.Body)
	}
	if _, accepted := ok.Payload.(core.RelayResponsePayloadAccepted); !accepted {
		t.Fatalf("submit %s: %#v", commandID, ok.Payload)
	}
}

func startupReady(events chan<- core.RuntimeEvent) {
	events <- core.RuntimeEventSessionStarted{NativeSessionID: "native"}
	events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}
}

// startupConsumed waits until the coordinator has taken every queued event
// or stopped.
func startupConsumed(t *testing.T, events chan core.RuntimeEvent, coordinator *core.JoinHandle[error]) {
	t.Helper()
	limit := time.Now().Add(ioTimeout)
	for len(events) > 0 {
		select {
		case <-coordinator.Done():
			return
		case <-time.After(time.Millisecond):
		}
		if time.Now().After(limit) {
			t.Fatal("the coordinator never consumed its events")
		}
	}
}

func startupTextPrompt(text string) []acpschema.ContentBlock {
	return []acpschema.ContentBlock{{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}}
}

// Hard-won: 4cb21a5d8 review: the actor's startup deadline could fail a harness that became ready during its write
func TestHarnessStartupReadyAndExpiryHaveOneOutcome(t *testing.T) {
	type arrival string
	const (
		readyThenClear      arrival = "ReadyThenClear"
		readyThenRestart    arrival = "ReadyThenRestart"
		expiryFirst         arrival = "ExpiryFirst"
		readyQueuedAtExpiry arrival = "ReadyQueuedAtExpiry"
		batchedRestart      arrival = "BatchedRestart"
		checkpointExpiry    arrival = "CheckpointExpiry"
		checkpointReady     arrival = "CheckpointReady"
		closeAtReleasedCut  arrival = "CloseAtReleasedCut"
	)
	allowance := core.NativeSessionStartupTimeout
	for _, arrival := range []arrival{
		readyThenClear, readyThenRestart, expiryFirst, readyQueuedAtExpiry,
		batchedRestart, checkpointExpiry, checkpointReady, closeAtReleasedCut,
	} {
		t.Run(string(arrival), func(t *testing.T) {
			temp := t.TempDir()
			durable, err := DurableRelayOpen(temp, "startup-race", "1.0.0")
			if err != nil {
				t.Fatal(err)
			}
			relay := NewRelayCell(durable)
			guard := relay.Lock()
			guard.SetTurnVerdictHarness(core.HarnessKindCodex)
			guard.Unlock()
			var checkpoint *core.RelayCursor
			if arrival == checkpointExpiry || arrival == checkpointReady || arrival == closeAtReleasedCut {
				startupSubmit(t, relay, "startup-cut", core.RelayCommandBeginCheckpoint{})
				guard := relay.Lock()
				if err := guard.DispatchPreparationLifecycle(); err != nil {
					t.Fatal(err)
				}
				checkpoint = guard.OperationalState().CheckpointReady
				guard.Unlock()
				if checkpoint == nil {
					t.Fatal("the startup cut is not ready")
				}
			}
			if arrival == closeAtReleasedCut {
				// A Close sealed at the cut, whose barrier the controller then
				// released. The coordinator claims the Close only once the
				// session is configured, so it stays pending.
				// Hard-won: #1617: a pending Close kept the open deadline from ever firing.
				startupSubmit(t, relay, "close-at-cut", core.RelayCommandClose{
					BarrierCommandID: "startup-cut",
					Expected:         *checkpoint,
				})
				startupSubmit(t, relay, "release-close-cut", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "startup-cut"})
				guard := relay.Lock()
				pending, held := guard.ClosePending(), guard.WorkerWritesHeld()
				guard.Unlock()
				if !pending || held {
					t.Fatalf("close pending %v, writes held %v", pending, held)
				}
			}
			guard = relay.Lock()
			sealedFrontier := guard.LatestOrdinal()
			guard.Unlock()
			clock := newManualCoordinatorClock()
			startup := newHarnessStartupOn(clock)
			events := make(chan core.RuntimeEvent, 16)
			wakes := newDispatchWakeChannel()
			commands := newCommandChannel(1)
			shellEvents := make(chan core.RuntimeEvent, 1)
			shells := NewUserShellRegistry(temp, map[string]string{}, shellEvents)
			if arrival == checkpointReady {
				// Readiness arrived before expiry while the cut held writes;
				// the owner cannot process that queued prefix until release.
				startupReady(events)
			}
			switch arrival {
			case readyThenClear, readyThenRestart, batchedRestart:
				clock.advance(allowance - time.Second)
			case readyQueuedAtExpiry:
				clock.advance(allowance)
			default:
				clock.advance(allowance + time.Second)
			}
			if arrival == readyQueuedAtExpiry {
				startupSubmit(t, relay, "initial-prompt", core.RelayCommandPrompt{Prompt: startupTextPrompt("queued before startup")})
			}
			switch arrival {
			case readyThenClear, readyThenRestart, readyQueuedAtExpiry, batchedRestart:
				// Already received readiness wins an uncommitted expiry,
				// including a tie, to avoid failing a usable harness.
				startupReady(events)
			}
			if arrival == batchedRestart {
				events <- core.RuntimeEventHarnessRestarting{Message: "restart"}
			}
			if arrival == checkpointReady {
				startupSubmit(t, relay, "release-startup-cut", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "startup-cut"})
			}
			coordinatorCtx, abort := context.WithCancel(context.Background())
			defer abort()
			coordinator := core.Spawn(func() error {
				return runRelayCoordinator(coordinatorCtx, relay, events, wakes, commands, shells, &taskMonitors{}, nil, nil, &startup)
			})
			nativeReady := func() bool {
				guard := relay.Lock()
				defer guard.Unlock()
				state := guard.OperationalState()
				return state.NativeSessionIsReady()
			}
			switch arrival {
			case checkpointExpiry:
				events <- core.RuntimeEventConnected{}
				startupConsumed(t, events, coordinator)
				guard := relay.Lock()
				// Hard-won: 8d32fbf09 review: expiry wrote faults through a held checkpoint cut.
				if !guard.WorkerWritesHeld() {
					t.Error("the startup cut no longer holds worker writes")
				}
				if latest := guard.LatestOrdinal(); latest != checkpoint.Ordinal {
					t.Errorf("latest ordinal %d moved past the cut %d", latest, checkpoint.Ordinal)
				}
				guard.Unlock()
				select {
				case <-coordinator.Done():
					t.Fatal("the coordinator stopped while the cut held its writes")
				default:
				}
				startupSubmit(t, relay, "release-startup-cut", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "startup-cut"})
				startupSubmit(t, relay, "handoff-checkpoint", core.RelayCommandBeginCheckpoint{})
				if err := wakes.trySend(); err != nil {
					t.Fatal(err)
				}
			case batchedRestart:
				startupConsumed(t, events, coordinator)
				clock.waitForDeadline(t, clock.now().Add(allowance), coordinator)
				clock.advance(allowance - time.Second)
				time.Sleep(10 * time.Millisecond)
				select {
				case <-coordinator.Done():
					t.Fatal("batched replacement gets a fresh deadline")
				default:
				}
				clock.advance(time.Second)
			case expiryFirst, closeAtReleasedCut:
				// A pending Close does not hold the deadline: it would wait
				// forever for a session that never opens.
			default:
				limit := time.Now().Add(ioTimeout)
				for !nativeReady() {
					select {
					case <-coordinator.Done():
						t.Fatalf("queued readiness was lost: %s", arrival)
					case <-time.After(time.Millisecond):
					}
					if time.Now().After(limit) {
						t.Fatal("readiness never arrived")
					}
				}
				if arrival == readyQueuedAtExpiry {
					select {
					case command := <-commands.receiver():
						prompt, ok := command.(CommandRequestPrompt)
						if !ok || prompt.RequestID != "initial-prompt" {
							t.Fatalf("dispatched %#v", command)
						}
					case <-time.After(ioTimeout):
						t.Fatal("the queued prompt was not dispatched")
					}
				}
				clock.advance(allowance * 2)
				time.Sleep(10 * time.Millisecond)
				// Hard-won: a09de965f review: an overdue timer discarded readiness queued behind a cut.
				select {
				case <-coordinator.Done():
					result, _ := coordinator.Join(context.Background())
					t.Fatalf("%s: the coordinator stopped: %v", arrival, result)
				default:
				}
				if arrival == readyQueuedAtExpiry || arrival == checkpointReady {
					abort()
					result, _ := coordinator.Join(context.Background())
					if !errors.Is(result, context.Canceled) {
						t.Fatalf("an aborted coordinator ended with %v", result)
					}
					return
				}
				if arrival == readyThenClear {
					startupSubmit(t, relay, "clear-startup", core.RelayCommandClearContext{})
					if err := wakes.trySend(); err != nil {
						t.Fatal(err)
					}
					select {
					case command := <-commands.receiver():
						clear, ok := command.(CommandRequestClearContext)
						if !ok || clear.RequestID != "clear-startup" {
							t.Fatalf("dispatched %#v", command)
						}
					case <-time.After(ioTimeout):
						t.Fatal("the clear was not dispatched")
					}
				}
				if arrival == readyThenClear {
					events <- core.RuntimeEventContextClearing{RequestID: "clear-startup"}
				} else {
					events <- core.RuntimeEventHarnessRestarting{Message: "restart"}
				}
				startupConsumed(t, events, coordinator)
				clock.waitForDeadline(t, clock.now().Add(allowance), coordinator)
				clock.advance(allowance)
			}
			// Expiry ends the worker with the typed failure its exit record
			// carries; no relay stays served without a harness.
			result := testSupportWaitFor("the startup expiry", func() error {
				err, _ := coordinator.Join(context.Background())
				return err
			})
			var failure core.WorkerHarnessStartupFailure
			if !errors.As(result, &failure) {
				t.Fatalf("untyped startup expiry for %s: %v", arrival, result)
			}
			if !strings.Contains(failure.Message, "300s") || !strings.Contains(failure.Message, "Restart agent") {
				t.Errorf("%s: %#v", arrival, failure)
			}
			if nativeReady() {
				t.Error("the expired harness is still ready")
			}
			// Rust asserts the event channel refuses a send: the coordinator
			// is gone, which its Join above already proved.
			if arrival == closeAtReleasedCut {
				guard := relay.Lock()
				// Nothing is journaled behind the seal; the next worker
				// finishes the Close.
				if latest := guard.LatestOrdinal(); latest != sealedFrontier {
					t.Errorf("latest ordinal %d moved past the seal %d", latest, sealedFrontier)
				}
				if !guard.ClosePending() {
					t.Error("the Close is no longer pending")
				}
				guard.Unlock()
			}
			if arrival == readyThenClear {
				replay, err := DurableRelayOpen(temp, "startup-race", "1.0.0")
				if err != nil {
					t.Fatal(err)
				}
				replayed, err := replay.EventsAfter(0, core.RelayEventGenesisDigest)
				if err != nil {
					t.Fatal(err)
				}
				interrupted := 0
				for _, event := range replayed {
					if observation, ok := event.Observation.(core.RelayObservationCommandInterrupted); ok &&
						observation.CommandID == "clear-startup" && strings.Contains(observation.Message, "Restart agent") {
						interrupted++
					}
				}
				// Hard-won: da28ee0bd review: startup expiry stranded a dispatched clear InFlight.
				if interrupted != 1 {
					t.Errorf("the clear was interrupted %d times", interrupted)
				}
			}
		})
	}
}

const verdictTestsSessionID = "018f9dd2-a3b4-7c8d-9000-123456789abc"

// whileClassifying is what happens while the classifier is asked.
type whileClassifying int

const (
	whileClassifyingChild whileClassifying = iota
	whileClassifyingWait
	whileClassifyingStop
	whileClassifyingKeepCurrent
	// whileClassifyingCheckpoint: a checkpoint barrier becomes ready while
	// the classifier is asked.
	whileClassifyingCheckpoint
)

func completedTurnVerdict(t *testing.T, action whileClassifying) {
	completedTurnWithBackground(t, action, "background_work", 0)
}

func completedTurnWithBackground(t *testing.T, action whileClassifying, choice string, tasks int) {
	completedTurnResponse(t, action, choice, tasks, 0.95, 0.95, 200)
}

// verdictTestsCaptureLog sends every log record of the test to path, as the
// Rust test scopes a subscriber to the coordinator.
func verdictTestsCaptureLog(t *testing.T, path string) {
	t.Helper()
	log, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	previous := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(log, &slog.HandlerOptions{Level: slog.LevelInfo})))
	t.Cleanup(func() {
		slog.SetDefault(previous)
		_ = log.Close()
	})
}

func completedTurnResponse(t *testing.T, action whileClassifying, choice string, tasks int, confidence float32, workProbability float64, status int) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	endpoint := "http://" + listener.Addr().String()
	requested := make(chan struct{}, 1)
	release := make(chan struct{})
	var releaseOnce sync.Once
	dropRelease := func() { releaseOnce.Do(func() { close(release) }) }
	released := false
	server := make(chan error, 1)
	go func() {
		server <- func() error {
			stream, err := listener.Accept()
			if err != nil {
				return err
			}
			defer stream.Close()
			reader := bufio.NewReader(stream)
			length := 0
			for {
				line, err := reader.ReadString('\n')
				if err != nil {
					return err
				}
				if line == "\r\n" {
					break
				}
				if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
					if length, err = strconv.Atoi(strings.TrimSpace(value)); err != nil {
						return err
					}
				}
			}
			body := make([]byte, length)
			if _, err := io.ReadFull(reader, body); err != nil {
				return err
			}
			var request map[string]any
			if err := json.Unmarshal(body, &request); err != nil {
				return err
			}
			if request["phase"] != "replied" {
				return fmt.Errorf("phase %v", request["phase"])
			}
			if request["assistant_text_tail"] != "Waiting for my agent to finish." {
				return fmt.Errorf("assistant_text_tail %v", request["assistant_text_tail"])
			}
			if request["background_commands"] != float64(tasks) {
				return fmt.Errorf("background_commands %v", request["background_commands"])
			}
			requested <- struct{}{}
			<-release
			if !released {
				return nil
			}
			work := choice
			if choice == "user" || choice == "background_work" {
				work = "waiting"
			}
			input := "none"
			if choice == "user" {
				input = "required"
			}
			var questions map[string]struct {
				Criteria map[string]any `json:"criteria"`
			}
			if err := json.Unmarshal([]byte(core.Questions), &questions); err != nil {
				return err
			}
			answers := map[string]any{}
			for axis, question := range questions {
				var winner string
				switch axis {
				case "work":
					winner = work
				case "input":
					winner = input
				case "failure", "attention":
					winner = "none"
				case "background":
					winner = "unclear"
				case "permission_veto":
					winner = "unknown"
				case "turn_state":
					winner = "wait"
					if work == "finished" {
						winner = "finished"
					}
				default:
					return fmt.Errorf("unexpected axis %s", axis)
				}
				probability := 1.0
				if axis == "work" || axis == "turn_state" {
					probability = workProbability
				}
				scores := map[string]float64{}
				for name := range question.Criteria {
					if name == winner {
						scores[name] = probability
					} else {
						scores[name] = (1.0 - probability) / float64(len(question.Criteria)-1)
					}
				}
				answers[axis] = map[string]any{"probabilities": scores}
			}
			encoded, err := json.Marshal(map[string]any{"answers": answers})
			if err != nil {
				return err
			}
			_ = confidence
			_, err = fmt.Fprintf(stream, "HTTP/1.1 %d Result\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", status, len(encoded), encoded)
			return err
		}()
	}()
	defer listener.Close()
	defer dropRelease()

	temp := t.TempDir()
	durable, err := DurableRelayOpen(temp, verdictTestsSessionID, "test")
	if err != nil {
		t.Fatal(err)
	}
	if err := durable.setManagedSubagent(action == whileClassifyingChild); err != nil {
		t.Fatal(err)
	}
	durable.SetTurnVerdictHarness(core.HarnessKindClaude)
	durable.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	fixtures := make([]AirTaskFixture, 0, tasks)
	for id := range tasks {
		fixtures = append(fixtures, AirTaskFixture{TaskID: strconv.Itoa(id), Description: "Old provisioning wait"})
	}
	if err := durable.setAirTasks(fixtures); err != nil {
		t.Fatal(err)
	}
	durable.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	submitResponse := durable.Handle(core.RelayRequestEnvelope{
		RequestID:       "submit-prompt-1",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestSubmit{
			CommandID: "prompt-1",
			Command:   core.RelayCommandPrompt{Prompt: startupTextPrompt("Do the work")},
		},
	})
	if _, ok := submitResponse.Body.(core.RelayResponseBodyOk); !ok {
		t.Fatalf("submit: %#v", submitResponse.Body)
	}
	relay := NewRelayCell(durable)
	events := make(chan core.RuntimeEvent, 16)
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	userShells := NewUserShellRegistry(temp, map[string]string{}, events)
	client, err := newVerdictClient(VerdictSourceHosted{Endpoint: endpoint})
	if err != nil {
		t.Fatal(err)
	}
	scopedLogCaptureSeesEveryCallsite()
	logPath := filepath.Join(temp, "worker.log")
	verdictTestsCaptureLog(t, logPath)
	coordinatorCtx, abort := context.WithCancel(context.Background())
	defer abort()
	coordinator := core.Spawn(func() error {
		return runRelayCoordinatorWithVerdict(coordinatorCtx, relay, events, wakes, commands, userShells, nil, client, nil)
	})
	joinCoordinator := func(description string) {
		t.Helper()
		result := testSupportWaitFor(description, func() error {
			err, joinErr := coordinator.Join(context.Background())
			return errors.Join(err, joinErr)
		})
		if result != nil {
			t.Fatalf("%s: %v", description, result)
		}
	}
	state := func() core.RelayOperationalState {
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.OperationalState()
	}
	waitUntilState := func(description string, condition func(core.RelayOperationalState) bool) {
		t.Helper()
		testSupportWaitFor(description, func() struct{} {
			for !condition(state()) {
				time.Sleep(5 * time.Millisecond)
			}
			return struct{}{}
		})
	}
	requestedBy := func(wait time.Duration) bool {
		select {
		case <-requested:
			return true
		case <-time.After(wait):
			return false
		}
	}
	events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}
	select {
	case command := <-commands.receiver():
		if _, ok := command.(CommandRequestPrompt); !ok {
			t.Fatalf("dispatched %#v", command)
		}
	case <-time.After(ioTimeout):
		t.Fatal("the prompt was not dispatched")
	}
	if action == whileClassifyingChild {
		if requestedBy(1100 * time.Millisecond) {
			t.Fatal("a child's turn was classified")
		}
		if state().ActivePrompt == nil {
			t.Fatal("the child's prompt is no longer active")
		}
	}
	events <- core.RuntimeEventSessionUpdate{Update: json.RawMessage(`{"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"Waiting for my agent to finish."}}`)}
	events <- core.RuntimeEventPromptFinished{RequestID: "prompt-1", StopReason: "end_turn"}
	if action == whileClassifyingChild {
		waitUntilState("child completion", func(state core.RelayOperationalState) bool { return state.ActivePrompt == nil })
		if requestedBy(1100 * time.Millisecond) {
			t.Fatal("a child's turn was classified")
		}
		current := state()
		if current.RetryAssessmentPending {
			t.Error("a child's assessment is pending a retry")
		}
		if current.Assessment == nil || current.Assessment.Current() {
			t.Errorf("assessment %#v", current.Assessment)
		}
		if current.IsQuiet() != (tasks == 0) {
			t.Errorf("quiet %v with %d tasks", current.IsQuiet(), tasks)
		}
		guard := relay.Lock()
		handback := guard.handbackVerdictClient
		guard.Unlock()
		if handback == nil {
			t.Error("the handback check has no classifier")
		}
		events <- core.RuntimeEventStopped{}
		joinCoordinator("child coordinator shutdown")
		// The server never got a request: closing its listener ends it.
		_ = listener.Close()
		if err := <-server; err == nil {
			t.Error("the classifier server served a child")
		}
		return
	}
	testSupportWaitFor("the local classifier request", func() struct{} {
		<-requested
		return struct{}{}
	})
	// Completion is published even while the classifier HTTP response is
	// blocked.
	if state().ActivePrompt != nil {
		t.Error("completion waited for the classifier")
	}
	if activity := state(); activity.ActivityState().Kind == core.ActivityStateExpecting {
		t.Error("the turn is still expected to continue")
	}
	readLog := func() string {
		log, err := os.ReadFile(logPath)
		if err != nil {
			t.Fatal(err)
		}
		return string(log)
	}
	if action == whileClassifyingStop {
		events <- core.RuntimeEventStopped{}
		joinCoordinator("classifier coordinator shutdown")
		dropRelease()
		if err := <-server; err != nil {
			t.Fatal(err)
		}
		log := readLog()
		if !strings.Contains(log, "GLiDE classification requested") {
			t.Error(log)
		}
		if !strings.Contains(log, "cancelled") {
			t.Error(log)
		}
		return
	}
	barrier := func(id string, command core.RelayCommand) {
		t.Helper()
		guard := relay.Lock()
		response := guard.Handle(core.RelayRequestEnvelope{
			RequestID:       "request-" + id,
			ProtocolVersion: core.RelayProtocolVersion,
			Request:         core.RelayRequestSubmit{CommandID: id, Command: command},
		})
		guard.Unlock()
		if _, ok := response.Body.(core.RelayResponseBodyOk); !ok {
			t.Fatalf("%#v", response)
		}
	}
	var cut *core.RelayCursor
	if action == whileClassifyingCheckpoint {
		barrier("checkpoint-barrier", core.RelayCommandBeginCheckpoint{})
		if err := wakes.trySend(); err != nil {
			t.Fatal(err)
		}
		waitUntilState("the checkpoint cut", func(state core.RelayOperationalState) bool { return state.CheckpointReady != nil })
		cut = state().CheckpointReady
	}
	released = true
	dropRelease()
	if err := testSupportWaitFor("the local classifier response", func() error { return <-server }); err != nil {
		t.Fatal(err)
	}
	switch {
	case cut != nil:
		// The answer is back, but the barrier holds its cut: the answer
		// waits.
		time.Sleep(200 * time.Millisecond)
		current := state()
		if current.LatestOrdinal != cut.Ordinal {
			t.Errorf("latest ordinal %d moved past the cut %d", current.LatestOrdinal, cut.Ordinal)
		}
		if current.Assessment == nil || current.Assessment.Status != core.StatusPending {
			t.Errorf("assessment %#v", current.Assessment)
		}
		barrier("checkpoint-release", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "checkpoint-barrier"})
		if err := wakes.trySend(); err != nil {
			t.Fatal(err)
		}
		waitUntilState("the held classifier answer after checkpoint release", func(state core.RelayOperationalState) bool {
			return state.Assessment != nil && state.Assessment.Status != core.StatusPending
		})
		if state().LatestOrdinal <= cut.Ordinal {
			t.Error("the held answer was never journaled")
		}
	case action == whileClassifyingWait:
		waitUntilState("the classifier's activity decision", func(state core.RelayOperationalState) bool {
			if choice == "background_work" {
				return state.ExpectedContinuation != nil
			}
			return state.InferredIdleSinceMs != nil
		})
		current := state()
		if len(current.BackgroundCommands) != tasks {
			t.Errorf("%d background commands, want %d", len(current.BackgroundCommands), tasks)
		}
		activity := current.ActivityState()
		if choice == "background_work" {
			if activity.IsWorking() != (tasks > 0) {
				t.Errorf("working %v with %d tasks", activity.IsWorking(), tasks)
			}
		} else if !activity.IsIdle() {
			t.Errorf("activity %#v", activity)
		}
		if tasks > 0 && current.IsQuiet() {
			t.Error("a session with background tasks is quiet")
		}
		// Autonomous output starts a real harness turn and clears the guess.
		events <- core.RuntimeEventSessionUpdate{Update: json.RawMessage(`{"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"The agent finished."}}`)}
		waitUntilState("the new autonomous harness turn", func(state core.RelayOperationalState) bool { return state.HarnessTurn != nil })
	case action == whileClassifyingKeepCurrent:
		expected := core.StatusAssessed
		if status != 200 {
			expected = core.StatusFailed
		}
		waitUntilState("the classifier assessment result", func(state core.RelayOperationalState) bool {
			return state.Assessment != nil && state.Assessment.Status == expected
		})
		current := state()
		if kind := current.ActivityState().Kind; kind != core.ActivityStateBackground {
			t.Errorf("activity %v", kind)
		}
		if current.InferredIdleSinceMs != nil {
			t.Error("an uncertain verdict inferred idleness")
		}
	}
	if activity := state(); activity.ActivityState().Kind == core.ActivityStateExpecting {
		t.Error("the turn is still expected to continue")
	}
	events <- core.RuntimeEventStopped{}
	joinCoordinator("classifier coordinator shutdown")
	log := readLog()
	for _, expected := range []string{"GLiDE classification requested", "summary_bytes=", verdictTestsSessionID} {
		if !strings.Contains(log, expected) {
			t.Errorf("log lacks %q: %s", expected, log)
		}
	}
	if strings.Contains(log, "Waiting for my agent to finish.") {
		t.Errorf("payloads must not be logged: %s", log)
	}
	if strings.Contains(log, "fake-key") {
		t.Error(log)
	}
	if action == whileClassifyingWait {
		for _, expected := range []string{"GLiDE classification received", "probabilities=", "applied"} {
			if !strings.Contains(log, expected) {
				t.Errorf("log lacks %q: %s", expected, log)
			}
		}
	}
}

func TestRepliedVerdictPublishesWithoutDelayingCompletionAndClearsOnOutput(t *testing.T) {
	t.Run("wait", func(t *testing.T) { completedTurnVerdict(t, whileClassifyingWait) })
	t.Run("child", func(t *testing.T) { completedTurnVerdict(t, whileClassifyingChild) })
	t.Run("child-finished", func(t *testing.T) {
		completedTurnWithBackground(t, whileClassifyingChild, "finished", 1)
	})
}

// I2-3: the classifier's answer landed after a close's checkpoint cut and
// made the worker refuse the Close. The answer now waits for the barrier.
// Hard-won: 634af0a: a classifier journal write moved the checkpoint cut and made suspend fail.
func TestAClassifierAnswerWaitsForAReadyCheckpointBarrier(t *testing.T) {
	completedTurnWithBackground(t, whileClassifyingCheckpoint, "finished", 0)
}

func TestShutdownCancelsPendingRepliedVerdict(t *testing.T) {
	completedTurnVerdict(t, whileClassifyingStop)
}

func TestProbabilityQualifiedCompletionAndInputReachActivityAtLowConfidence(t *testing.T) {
	for _, choice := range []string{"finished", "user"} {
		t.Run(choice, func(t *testing.T) {
			completedTurnResponse(t, whileClassifyingWait, choice, 4, 0.2, 0.95, 200)
		})
	}
}

func TestUncertainVerdictsAreCachedAndTransportFailuresBackOff(t *testing.T) {
	t.Run("uncertain", func(t *testing.T) {
		completedTurnResponse(t, whileClassifyingKeepCurrent, "finished", 4, 0.5, 0.5, 200)
	})
	t.Run("unclear", func(t *testing.T) {
		completedTurnResponse(t, whileClassifyingKeepCurrent, "unclear", 4, 0.95, 0.95, 200)
	})
	t.Run("unavailable", func(t *testing.T) {
		completedTurnResponse(t, whileClassifyingKeepCurrent, "finished", 4, 0.95, 0.95, 503)
	})
}

// askOnceWithoutAClassifier sends one classification to a closed port,
// logging through whatever logger is current.
func askOnceWithoutAClassifier(t *testing.T) {
	t.Helper()
	client, err := newVerdictClient(VerdictSourceHosted{Endpoint: "http://127.0.0.1:1"})
	if err != nil {
		t.Fatal(err)
	}
	evidence := core.TurnEvidence{
		Harness:       core.HarnessKindCodex,
		Phase:         core.TurnPhaseReplied,
		ToolsInFlight: []core.ToolEvidence{},
	}
	attempt, _, _ := client.askGlideLogged(context.Background(), "s", 1, &evidence, &core.ActivityFacts{}, 0)
	if attempt != nil {
		attempt.Close()
	}
}

// The CI flake in order: this test's logger is the only one registered,
// then a goroutine with no logger reaches the Jev log call first.
// Hard-won: ccd520d: parallel callsite initialization made CI log captures miss Jev events.
func TestScopedLogCaptureKeepsACallsiteFirstReachedWithoutASubscriber(t *testing.T) {
	scopedLogCaptureSeesEveryCallsite()
	temp := t.TempDir()
	logPath := filepath.Join(temp, "worker.log")
	previous := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(io.Discard, nil)))
	done := make(chan struct{})
	go func() {
		defer close(done)
		askOnceWithoutAClassifier(t)
	}()
	<-done
	slog.SetDefault(previous)
	verdictTestsCaptureLog(t, logPath)
	askOnceWithoutAClassifier(t)
	log, err := os.ReadFile(logPath)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(log), "GLiDE classification requested") {
		t.Error(string(log))
	}
}

// verdictTestsReadGlideRequest reads one GLiDE HTTP request and returns its
// JSON body.
func verdictTestsReadGlideRequest(stream net.Conn) (map[string]any, error) {
	reader := bufio.NewReader(stream)
	length := 0
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			return nil, err
		}
		if line == "\r\n" {
			break
		}
		if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
			if length, err = strconv.Atoi(strings.TrimSpace(value)); err != nil {
				return nil, err
			}
		}
	}
	body := make([]byte, length)
	if _, err := io.ReadFull(reader, body); err != nil {
		return nil, err
	}
	var request map[string]any
	return request, json.Unmarshal(body, &request)
}

type quotaCase int

const (
	quotaCaseUntimedRecovery quotaCase = iota
	quotaCaseFailedBackoff
	quotaCaseProseOnly
	quotaCasePausedGoal
)

func completedQuotaTurn(t *testing.T, quota quotaCase) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	endpoint := "http://" + listener.Addr().String()
	var requests atomic.Int64
	go func() {
		for {
			stream, err := listener.Accept()
			if err != nil {
				return
			}
			if _, err := verdictTestsReadGlideRequest(stream); err == nil {
				requests.Add(1)
				_, _ = stream.Write([]byte("HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"))
			}
			stream.Close()
		}
	}()
	providerError := jsonvalue.Object{{Key: "message", Value: "Plan usage exhausted"}, {Key: "code", Value: "usageLimitExceeded"}}
	if quota == quotaCaseProseOnly {
		providerError = jsonvalue.Object{{Key: "message", Value: "RPM quota exceeded"}}
	}
	diagnostic := core.TurnDiagnosticFromProvider(providerError)
	temp := t.TempDir()
	durable, err := DurableRelayOpen(temp, verdictTestsSessionID, "test")
	if err != nil {
		t.Fatal(err)
	}
	submit := func(relay *DurableRelay, id string, command core.RelayCommand) {
		t.Helper()
		response := relay.Handle(core.RelayRequestEnvelope{
			RequestID:       "submit-" + id,
			ProtocolVersion: core.RelayProtocolVersion,
			Request:         core.RelayRequestSubmit{CommandID: id, Command: command},
		})
		if _, ok := response.Body.(core.RelayResponseBodyOk); !ok {
			t.Fatalf("%#v", response)
		}
	}
	submit(durable, "prompt-1", core.RelayCommandPrompt{Prompt: core.PromptBlocks()})
	var failed *core.TurnAssessment
	if quota == quotaCaseFailedBackoff {
		// Write and reopen the durable state left by a worker that asked
		// GLiDE. Install the harness on the reopened relay so only it
		// settles locally.
		if _, err := durable.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}); err != nil {
			t.Fatal(err)
		}
		claimed, err := durable.ClaimPendingCommands(true)
		if err != nil || len(claimed) != 1 {
			t.Fatalf("claimed %d: %v", len(claimed), err)
		}
		if _, err := durable.RecordCommandCompleted("prompt-1", core.RelayCommandOutcomePrompt{
			StopReason: core.QuotaStopReason,
			Diagnostic: diagnostic,
		}); err != nil {
			t.Fatal(err)
		}
		pending := durable.OperationalState().Assessment
		if err := durable.failTurnAssessment(pending.Revision, "classifier_failed"); err != nil {
			t.Fatal(err)
		}
		durable, err = DurableRelayOpen(temp, verdictTestsSessionID, "test")
		if err != nil {
			t.Fatal(err)
		}
		failed = durable.OperationalState().Assessment
		if failed.Status != core.StatusFailed {
			t.Fatalf("status = %v", failed.Status)
		}
		if failed.RetryAtMs == nil || *failed.RetryAtMs <= core.EpochMillis() {
			t.Fatalf("retry at %v", failed.RetryAtMs)
		}
	}
	durable.SetTurnVerdictHarness(core.HarnessKindCodex)
	if quota == quotaCaseUntimedRecovery {
		durable.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		if err := durable.setAirTasks([]AirTaskFixture{{TaskID: "background-task", Description: "Waiting for a build"}}); err != nil {
			t.Fatal(err)
		}
	}
	relay := NewRelayCell(durable)
	events := make(chan core.RuntimeEvent, 4)
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	userShells := NewUserShellRegistry(temp, map[string]string{}, events)
	client, err := newVerdictClient(VerdictSourceHosted{Endpoint: endpoint})
	if err != nil {
		t.Fatal(err)
	}
	coordinatorCtx, abort := context.WithCancel(context.Background())
	defer abort()
	coordinator := core.Spawn(func() error {
		return runRelayCoordinatorWithVerdict(coordinatorCtx, relay, events, wakes, commands, userShells, nil, client, nil)
	})
	if failed == nil {
		events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}
		select {
		case command := <-commands.receiver():
			if _, ok := command.(CommandRequestPrompt); !ok {
				t.Fatalf("dispatched %#v", command)
			}
		case <-time.After(ioTimeout):
			t.Fatal("the prompt was not dispatched")
		}
		if quota == quotaCasePausedGoal {
			events <- core.RuntimeEventSessionUpdate{Update: json.RawMessage(`{"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"Build it","createdAt":1,"status":"paused"}}}`)}
		}
		// The harness can use QuotaLimit even for a prose-only diagnosis.
		events <- core.RuntimeEventPromptFinished{RequestID: "prompt-1", StopReason: core.QuotaStopReason, Diagnostic: diagnostic}
	}
	expectedStatus := core.StatusDeferred
	switch quota {
	case quotaCaseProseOnly:
		expectedStatus = core.StatusFailed
	case quotaCasePausedGoal:
		expectedStatus = core.StatusSuperseded
	}
	settled := testSupportWaitFor("the completed quota turn assessment", func() core.TurnAssessment {
		for {
			guard := relay.Lock()
			assessment := guard.OperationalState().Assessment
			guard.Unlock()
			if assessment != nil && assessment.Status == expectedStatus &&
				(quota != quotaCaseProseOnly || requests.Load() == 1) {
				return *assessment
			}
			time.Sleep(time.Millisecond)
		}
	})
	if quota == quotaCaseProseOnly {
		if settled.Action != nil {
			t.Fatalf("action = %v", *settled.Action)
		}
	} else {
		if settled.Action == nil || *settled.Action != core.AssessmentActionRecoverQuota {
			t.Fatalf("action = %v", settled.Action)
		}
		if settled.Verdict != nil || settled.Glide != nil || settled.RetryAtMs != nil {
			t.Fatalf("settled = %#v", settled)
		}
		reason := "quota_resolution_required"
		if quota == quotaCasePausedGoal {
			reason = "automatic_action_suppressed"
		}
		if settled.Reason != reason {
			t.Fatalf("reason = %q, want %q", settled.Reason, reason)
		}
	}
	if failed != nil {
		if settled.TurnID != failed.TurnID || settled.CompletedOrdinal != failed.CompletedOrdinal ||
			settled.Revision != failed.Revision || !reflect.DeepEqual(settled.Completion, failed.Completion) ||
			settled.Failures != failed.Failures {
			t.Fatalf("settled %#v does not continue %#v", settled, failed)
		}
	}
	if quota == quotaCaseUntimedRecovery {
		guard := relay.Lock()
		state := guard.OperationalState()
		if len(state.BackgroundCommands) != 1 {
			t.Fatalf("background commands = %#v", state.BackgroundCommands)
		}
		submit(guard.DurableRelay, "record-quota-recovery", core.RelayCommandSetQuotaRecovery{
			Expected: core.RelayCursor{Ordinal: state.LatestOrdinal, Digest: state.LatestDigest},
			Recovery: &core.QuotaRecovery{
				UserCommandID:      "prompt-1",
				CompletedCommandID: "prompt-1",
				ProfileID:          "isolated-quota-test",
				Notice:             "Quota deadline unavailable",
			},
		})
		guard.Unlock()
		// Each wake waits for the previous one to be taken, so the third
		// proves the coordinator returned to its assessment loop after
		// processing the first.
		for range 3 {
			testSupportWaitFor("the coordinator to process recovery", func() struct{} {
				wakes.wakes <- struct{}{}
				return struct{}{}
			})
		}
		guard = relay.Lock()
		recovered := guard.OperationalState().Assessment
		guard.Unlock()
		if recovered.Status != core.StatusAssessed || recovered.Action == nil ||
			*recovered.Action != core.AssessmentActionRecoverQuota ||
			recovered.Reason != "quota_deadline_unavailable" ||
			recovered.Revision != settled.Revision || recovered.Glide != nil {
			t.Fatalf("recovered = %#v", recovered)
		}
	}
	events <- core.RuntimeEventStopped{}
	if err := testSupportWaitFor("quota coordinator shutdown", func() error {
		err, joinErr := coordinator.Join(context.Background())
		return errors.Join(err, joinErr)
	}); err != nil {
		t.Fatal(err)
	}
	want := int64(0)
	if quota == quotaCaseProseOnly {
		want = 1
	}
	if got := requests.Load(); got != want {
		t.Fatalf("GLiDE requests = %d, want %d", got, want)
	}
}

// Hard-won: 2026-11-22 live GLiDE 502 outage blocked top-level typed QuotaLimit automove
func TestTypedQuotaSettlesLocallyAndUntimedRecoveryDoesNotReaskGlide(t *testing.T) {
	completedQuotaTurn(t, quotaCaseUntimedRecovery)
}

func TestTypedQuotaSettlesDuringAPersistedGlideBackoff(t *testing.T) {
	completedQuotaTurn(t, quotaCaseFailedBackoff)
}

func TestProseOnlyQuotaStillAsksGlide(t *testing.T) {
	completedQuotaTurn(t, quotaCaseProseOnly)
}

func TestTypedQuotaRespectsAPausedGoalWithoutAskingGlide(t *testing.T) {
	completedQuotaTurn(t, quotaCasePausedGoal)
}
