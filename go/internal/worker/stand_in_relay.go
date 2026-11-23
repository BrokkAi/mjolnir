// Port of mj-worker/src/stand_in_relay.rs.
//
// Stand-in relays for the controller and daemon tests.
//
// A test that needs a worker relay without a worker starts its test binary
// again as the relay: the child runs one exact test that calls a function
// here, which serves the relay protocol on standard input and output from a
// real relay journal. The test binaries of the controller and daemon both
// define those tests, under the names in core's test hooks, so either can
// start the relay. The environment each relay reads is defined there.
//
// Always built, as Rust builds it (#[doc(hidden)]): only a test binary that
// sets the stand-in environment reaches it. JSON fixtures are jsonvalue
// values, so a rewritten fixture keeps the member order serde_json (with
// preserve_order) keeps.

package worker

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// standInUnix is Rust's #[cfg(unix)] for the stand-ins' Unix-only steps.
const standInUnix = runtime.GOOS != "windows"

// standInJournalOwner is the leased relay's journal owner lock, held from
// the first session request to this process's exit. A package variable
// keeps the file reachable, so no finalizer closes it and drops the lock.
var standInJournalOwner *os.File

// standInWithExtension is Rust's Path::with_extension: path with its file
// name's extension replaced by (or extended with) extension.
func standInWithExtension(path, extension string) string {
	name := filepath.Base(path)
	stem := name
	if dot := strings.LastIndexByte(name, '.'); dot > 0 {
		stem = name[:dot]
	}
	return filepath.Join(filepath.Dir(path), stem+"."+extension)
}

func standInExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

func standInMust(err error, what string) {
	if err != nil {
		panic(fmt.Sprintf("%s: %v", what, err))
	}
}

// standInAppendLine appends line and a newline to the file at path,
// creating it.
func standInAppendLine(path, line, what string) {
	log, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o666)
	standInMust(err, what)
	_, err = fmt.Fprintln(log, line)
	standInMust(err, what)
	standInMust(log.Close(), what)
}

// standInSessionUpdate decodes a harness notification from its ACP JSON.
func standInSessionUpdate(text string) acpschema.SessionUpdate {
	var update acpschema.SessionUpdate
	standInMust(json.Unmarshal([]byte(text), &update), "decode a session update")
	return update
}

func standInWrite(writer io.Writer, response core.RelayResponseEnvelope, what string) {
	standInMust(core.WriteRelayFrame(writer, response), what)
}

func standInSubmitted(request core.RelayRequest) (core.RelayCommand, string, bool) {
	submit, ok := request.(core.RelayRequestSubmit)
	if !ok {
		return nil, "", false
	}
	return submit.Command, submit.CommandID, true
}

func standInSubmits[T core.RelayCommand](request core.RelayRequest) bool {
	command, _, ok := standInSubmitted(request)
	if !ok {
		return false
	}
	_, ok = command.(T)
	return ok
}

// ServeLatchRelay is the relay server half of the checkpoint latch test.
//
// A durable relay only reports a checkpoint barrier ready once a dispatch
// driver claims it, so this also runs the one step the worker runtime
// performs for a barrier. It does nothing unless a parent test points it at
// a relay journal root.
func ServeLatchRelay() {
	root, ok := os.LookupEnv(core.LatchRelayRoot)
	if !ok {
		return
	}
	// With -test.v the test framework writes `=== RUN <name>` before the
	// body runs. End that line first so it cannot glue itself onto the first
	// protocol frame.
	fmt.Println()
	resumeDiscarded := false
	_, checkpointOnly := os.LookupEnv(core.LatchCheckpointOnly)
	// Queue-only connections never own or write the control journal.
	var relay *DurableRelay
	delayMs := uint64(0)
	if value, ok := os.LookupEnv(core.LatchRelayStartupDelayMs); ok {
		parsed, err := strconv.ParseUint(value, 10, 64)
		standInMust(err, "parse the startup delay")
		delayMs = parsed
	}
	readyAt := time.Now().Add(time.Duration(delayMs) * time.Millisecond)
	_, rejectRelease := os.LookupEnv(core.LatchRelayRejectRelease)
	lateWrite, journalsBeforeClose := os.LookupEnv(core.LatchRelayJournalsBeforeClose)
	_, running := os.LookupEnv(core.LatchRelayRunning)
	_, answerPrompts := os.LookupEnv(core.LatchRelayAnswerPrompts)
	reader := bufio.NewReader(os.Stdin)
	writer := os.Stdout
	configured := false
	for {
		request, err := core.ReadRelayFrame(reader)
		standInMust(err, "read a relay request")
		if request == nil {
			return
		}
		if _, ok := os.LookupEnv(core.LatchRejectProjectMemory); ok &&
			strings.Contains(core.RelayRequestMethodName(request.Request), "project_memory") {
			panic("idle worker upgrade must not require a project-memory transfer")
		}
		maxProtocol := core.RelayChangeWaitProtocol
		if value, err := strconv.ParseUint(os.Getenv(core.LatchRelayMaxProtocol), 10, 32); err == nil {
			maxProtocol = uint32(value)
		}
		if standInExists(filepath.Join(root, "upgrade-goal-peer")) {
			name := core.RelayRequestMethodName(request.Request)
			switch {
			case standInSubmits[core.RelayCommandBeginCheckpoint](request.Request):
				name = "begin_checkpoint"
			case standInSubmits[core.RelayCommandCancelTurn](request.Request):
				name = "cancel_turn"
			}
			standInAppendLine(filepath.Join(root, "upgrade-requests.log"), name, "log an upgrade request")
			if reserve, ok := request.Request.(core.RelayRequestReserveIdle); ok && maxProtocol < core.RelayIdleGoalIntentProtocol {
				accepted := false
				if relay != nil {
					barrier := relay.OperationalState().CheckpointBarrier
					accepted = barrier != nil && *barrier == reserve.CommandID
				}
				if !accepted {
					panic("old worker's initial ReserveIdle rejects goal intent; only accepted-barrier confirmation is valid")
				}
			}
		}
		if _, record := os.LookupEnv("MJ_TEST_RECORD_IN_PLACE_MOVE_REQUESTS"); record {
			switch request.Request.(type) {
			case core.RelayRequestHoldInPlaceMoveDispatch, core.RelayRequestRestartHarnessInPlace,
				core.RelayRequestDiscardHeldMoveCommands, core.RelayRequestSubmitFront,
				core.RelayRequestReleaseInPlaceMoveDispatch:
				standInAppendLine(filepath.Join(root, "in-place-move-requests.log"),
					core.RelayRequestMethodName(request.Request), "log an in-place move request")
			}
		}
		var payload core.RelayResponsePayload
		switch hello := request.Request.(type) {
		case core.RelayRequestHello:
			payload = core.RelayResponsePayloadHello{
				Negotiated: min(maxProtocol, hello.Supported.Max),
				// The stand-in always negotiates down to its configured
				// protocol, as the Rust one does.
				RelayVersion: "older-protocol-test",
				SessionID:    core.LatchRelaySession,
			}
		case core.RelayRequestPollSubagentQueue, core.RelayRequestWaitForChange, core.RelayRequestSubagentRequests:
			if response := subagentConnectionResponse(request); response != nil {
				payload = response.Body.(core.RelayResponseBodyOk).Payload
			}
		}
		if payload != nil {
			protocolVersion := request.ProtocolVersion
			if hello, ok := payload.(core.RelayResponsePayloadHello); ok {
				protocolVersion = hello.Negotiated
			}
			standInWrite(writer, core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: protocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: payload},
			}, "answer a queue or hello request")
			continue
		}
		// A checkpoint carries project memory on a second connection while
		// the first still owns the control journal.
		if refusal := projectMemoryRefusal(request); refusal != nil {
			standInWrite(writer, *refusal, "answer a project-memory request")
			continue
		}
		if relay == nil {
			relay = openLatchRelay(root, checkpointOnly, running)
		}
		if command, commandID, ok := standInSubmitted(request.Request); ok && commandID == "discard-before-ready" {
			if _, clear := command.(core.RelayCommandClearQueuedPrompts); clear {
				resumeDiscarded = true
			}
		}
		discardReady := !standInExists(filepath.Join(root, "resume-discard-before-ready")) || resumeDiscarded
		if !checkpointOnly && !configured && !time.Now().Before(readyAt) && discardReady {
			_, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
			standInMust(err, "record the configured session")
			configured = true
		}
		if standInSubmits[core.RelayCommandBeginCheckpoint](request.Request) {
			state := relay.OperationalState()
			if !checkpointOnly && !state.NativeSessionIsReady() {
				panic("checkpoint submitted before current ACP startup finished")
			}
		}
		if os.Getenv("MJ_TEST_QUOTA_BARRIER_RACE") == "1" {
			_, holding := request.Request.(core.RelayRequestHoldInPlaceMoveDispatch)
			if standInSubmits[core.RelayCommandBeginCheckpoint](request.Request) || holding {
				_, err := relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()})
				standInMust(err, "record a harness turn")
			}
			if standInSubmits[core.RelayCommandCancelTurn](request.Request) {
				panic("automatic quota Move cancelled new work")
			}
		}
		lateTurn := filepath.Join(root, "upgrade-late-turn")
		if standInExists(lateTurn) {
			switch request.Request.(type) {
			case core.RelayRequestStatus, core.RelayRequestAttach:
				standInMust(os.Remove(lateTurn), "remove the late turn marker")
				_, err := relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()})
				standInMust(err, "record a harness turn")
				origin := "test autonomous completion"
				_, err = relay.RecordObservation(core.RelayObservationHarnessTurnSettled{Origin: &origin, PromptInFlight: false})
				standInMust(err, "settle a harness turn")
			}
		}
		if standInUnix {
			holdNamedRelayCommand(request.Request)
		}
		// The harness keeps talking while a checkpoint barrier holds: this
		// lands one write of its own after the daemon's last look and before
		// its Close, through the same relay calls the worker runtime uses.
		if standInUnix && journalsBeforeClose && standInSubmits[core.RelayCommandClose](request.Request) {
			switch lateWrite {
			case core.LateHarnessNotification:
				_, err := relay.RecordSessionUpdate(standInSessionUpdate(
					`{"sessionUpdate":"session_info_update","title":"A title the harness chose after the turn"}`))
				standInMust(err, "record a harness notification")
			case core.LateHarnessQuestion:
				_, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: core.ElicitationRequest{
					ID:      "late-question",
					Message: "May I continue?",
					Fields:  []core.ElicitationField{},
				}})
				standInMust(err, "record a harness question")
			default:
				panic(fmt.Sprintf("unknown late write %s", strconv.Quote(lateWrite)))
			}
		}
		if _, record := os.LookupEnv(core.LatchRecordSubagentAdmission); record {
			if admission, ok := request.Request.(core.RelayRequestSetSubagentAdmission); ok {
				recordTestSubagentAdmission(root, admission.Open)
				marker := filepath.Join(root, "drop-admission-close-reply-once")
				if _, drop := os.LookupEnv(core.LatchDropAdmissionCloseReply); !admission.Open && drop && !standInExists(marker) {
					standInMust(os.WriteFile(marker, []byte("dropped"), 0o666), "record the dropped close reply")
					return
				}
			}
		}
		var response core.RelayResponseEnvelope
		_, reserving := request.Request.(core.RelayRequestReserveIdle)
		_, operatorAway := request.Request.(core.RelayRequestSetOperatorAway)
		restart, restarting := request.Request.(core.RelayRequestRestartHarnessInPlace)
		switch {
		case reserving && standInExists(filepath.Join(root, "upgrade-held-reviewer")):
			response = core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: request.ProtocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadIdleReservation{}},
			}
		case rejectRelease && requestsCheckpointRelease(request):
			response = unparseableRequestResponse(request)
		case operatorAway:
			response = core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: request.ProtocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadStatus{State: relay.OperationalState()}},
			}
		case restarting:
			var body core.RelayResponseBody
			restarted := core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadHarnessRestartedInPlace{}}
			if standInExists(filepath.Join(root, "reject-in-place-restart")) {
				body = core.RelayError(core.RelayErrorCodeInvalidState,
					"cannot restart harness in place while a turn or effectful command is in progress", true, nil)
			} else {
				decision, refusal := relay.admitInPlaceHarnessRestart(restart.OperationID, restart.ExpectedStoreID)
				switch {
				case refusal != nil:
					body = core.RelayResponseBodyError{Error: *refusal}
				case decision == InPlaceHarnessRestartAlreadyRestarted:
					body = restarted
				default:
					_ = restart.LaunchConfig.Write(filepath.Join(root, "launch.json"))
					if err := relay.markInPlaceHarnessRestarted(restart.OperationID, restart.ExpectedStoreID); err != nil {
						body = core.RelayError(core.RelayErrorCodeInternal,
							fmt.Sprintf("complete in-place harness restart: %v", err), false, nil)
					} else {
						body = restarted
					}
				}
			}
			response = core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: min(maxProtocol, core.RelayProtocolVersion),
				Body:            body,
			}
		default:
			if queued := subagentConnectionResponse(request); queued != nil {
				response = *queued
				break
			}
			// Run the current durable relay's behavior while speaking the
			// older version negotiated by this test worker.
			envelope := *request
			envelope.ProtocolVersion = core.RelayProtocolVersion
			response = relay.Handle(envelope)
			response.ProtocolVersion = min(maxProtocol, core.RelayProtocolVersion)
		}
		standInWrite(writer, response, "answer a relay request")
		if checkpointOnly {
			standInMust(relay.DispatchCheckpointOnly(), "dispatch the checkpoint-only relay")
		}
		claimed, err := relay.ClaimPendingCommands(true)
		standInMust(err, "claim relay commands")
		for _, command := range claimed {
			switch command.Command.(type) {
			case core.RelayCommandBeginCheckpoint:
				_, err := relay.RecordCheckpointReady(command.CommandID)
				standInMust(err, "report the checkpoint barrier ready")
			case core.RelayCommandPrompt:
				if !standInUnix || !answerPrompts {
					continue
				}
				_, err := relay.RecordSessionUpdate(standInSessionUpdate(fmt.Sprintf(
					`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":%s}}`, strconv.Quote(core.LatchRelayAnswer))))
				standInMust(err, "answer the prompt")
				_, err = relay.RecordCommandCompleted(command.CommandID, core.RelayCommandOutcomePrompt{StopReason: "end_turn"})
				standInMust(err, "complete the answered prompt")
			case core.RelayCommandClose:
				if !standInUnix || !journalsBeforeClose {
					continue
				}
				_, err := relay.RecordCommandCompleted(command.CommandID, core.RelayCommandOutcomeClosed{})
				standInMust(err, "close the relay")
			case core.RelayCommandCancelTurn:
				if !standInUnix {
					continue
				}
				active := relay.OperationalState().ActivePrompt
				if active == nil {
					panic("a prompt to cancel")
				}
				promptID := active.CommandID
				_, err := relay.RecordCommandCompleted(command.CommandID, core.RelayCommandOutcomeCancelled{})
				standInMust(err, "complete the cancellation")
				if standInExists(filepath.Join(root, "unsettled-cancellation")) {
					standInMust(os.WriteFile(filepath.Join(root, "cancellation-accepted"), []byte("accepted"), 0o666), "record the accepted cancellation")
					continue
				}
				_, err = relay.RecordCommandCompleted(promptID, core.RelayCommandOutcomePrompt{StopReason: "cancelled"})
				standInMust(err, "complete the cancelled prompt")
			}
		}
	}
}

// openLatchRelay opens the latch test's relay journal the first time a
// request needs it, recording what a worker would have recorded by then.
func openLatchRelay(root string, checkpointOnly, running bool) *DurableRelay {
	// Record this start so a parent test can tell a reconnect from a
	// reused connection.
	if starts, ok := os.LookupEnv(core.LatchRelayStarts); ok {
		standInAppendLine(starts, strconv.Itoa(os.Getpid()), "record this relay start")
	}
	var relay *DurableRelay
	var err error
	if checkpointOnly {
		relay, err = DurableRelayOpenForCheckpoint(root, core.LatchRelaySession, "1.0.0")
	} else {
		relay, err = DurableRelayOpen(root, core.LatchRelaySession, "1.0.0")
	}
	standInMust(err, "open the test relay journal")
	relay.SetTurnVerdictHarness(core.HarnessKindCodex)
	if relay.OperationalState().NativeSessionID == nil {
		// A launch that names no native session has nothing to load, so the
		// worker opens a new one and has nothing to report.
		launchNamesNative := func() bool {
			launch, err := core.WorkerLaunchConfigRead(filepath.Join(root, "launch.json"))
			standInMust(err, "read the stand-in worker's launch configuration")
			return launch.NativeSessionID != nil
		}
		fresh, hasFresh := os.LookupEnv(core.LatchRelayFreshNative)
		switch {
		case hasFresh && !launchNamesNative():
			_, err := relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: fresh})
			standInMust(err, "record the opened session")
		case hasFresh:
			var replaced *string
			if value, ok := os.LookupEnv(core.LatchRelayReplacedUnused); ok {
				replaced = &value
			}
			// What a worker restored from a checkpoint journals when the
			// harness cannot reload the archived native session and this
			// session never used it.
			_, err := relay.RecordObservation(core.RelayObservationSessionRestarted{})
			standInMust(err, "record the restarted session")
			_, err = relay.RecordObservation(core.RelayObservationWarning{
				Message: "Claude Code has no native session native-session and this session never used it; continuing in a new empty session",
			})
			standInMust(err, "record the warning")
			_, err = relay.RecordObservation(core.RelayObservationSessionOpened{
				NativeSessionID:               fresh,
				ReplacedUnusedNativeSessionID: replaced,
			})
			standInMust(err, "record the opened session")
		default:
			_, err := relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "native-session", Resumed: true})
			standInMust(err, "record the opened session")
		}
	}
	if state := relay.OperationalState(); !state.Goal.Synchronized() && !checkpointOnly {
		_, err := relay.RecordSessionUpdate(standInSessionUpdate(
			`{"sessionUpdate":"session_info_update","_meta":{"goal":null,"execution":{"version":1,"status":"idle"}}}`))
		standInMust(err, "record the goal")
	}
	if standInUnix && running && relay.OperationalState().ActivePrompt == nil &&
		!standInExists(filepath.Join(root, "cancellation-accepted")) {
		response := relay.Handle(core.RelayRequestEnvelope{
			RequestID:       "seed-running-request",
			ProtocolVersion: core.RelayProtocolVersion,
			Request: core.RelayRequestSubmit{
				CommandID: "seed-running-prompt",
				Command: core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{{
					Kind: acpschema.ContentBlockKindText,
					Text: &acpschema.TextContent{Text: "running"},
				}}},
			},
		})
		ok, isOk := response.Body.(core.RelayResponseBodyOk)
		if _, accepted := ok.Payload.(core.RelayResponsePayloadAccepted); !isOk || !accepted {
			panic(fmt.Sprintf("the running prompt was not accepted: %#v", response.Body))
		}
		claimed, err := relay.ClaimPendingCommands(true)
		standInMust(err, "seed the running prompt")
		if len(claimed) != 1 || claimed[0].CommandID != "seed-running-prompt" {
			panic(fmt.Sprintf("claimed %#v", claimed))
		}
	}
	return relay
}

// holdNamedRelayCommand: see core.LatchRelayHoldCommand. Only the first
// matching request waits. Rust runs it on Unix only.
func holdNamedRelayCommand(request core.RelayRequest) {
	command, hasCommand := os.LookupEnv(core.LatchRelayHoldCommand)
	path, hasPath := os.LookupEnv(core.LatchRelayHoldPath)
	if !hasCommand || !hasPath {
		return
	}
	matches := false
	switch {
	case standInSubmits[core.RelayCommandBeginCheckpoint](request):
		matches = command == "begin_checkpoint"
	case standInSubmits[core.RelayCommandClose](request):
		matches = command == "close"
	}
	reached := standInWithExtension(path, "reached")
	if !matches || standInExists(reached) {
		return
	}
	standInMust(os.WriteFile(reached, []byte("held"), 0o666), "record the held relay command")
	release := standInWithExtension(path, "release")
	deadline := time.Now().Add(30 * time.Second)
	for !standInExists(release) {
		if !time.Now().Before(deadline) {
			panic(fmt.Sprintf("the parent test never released the held %s", command))
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func testSubagentQueuePath(relayRoot string) string {
	return filepath.Join(filepath.Dir(relayRoot), core.LatchRelaySession, "subagents.json")
}

// emptyTestSubagentQueue is a fixture queue with no requests or results.
func emptyTestSubagentQueue() jsonvalue.Object {
	return subagentObject("requests", jsonvalue.Object{}, "results", jsonvalue.Object{})
}

func readTestSubagentQueue(path string) jsonvalue.Object {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return emptyTestSubagentQueue()
	}
	if err != nil {
		panic(fmt.Sprintf("read fixture sub-agent queue: %v", err))
	}
	queue, err := jsonvalue.Parse(data)
	if err != nil {
		panic(fmt.Sprintf("parse fixture sub-agent queue: %v", err))
	}
	object, ok := queue.(jsonvalue.Object)
	if !ok {
		panic("parse fixture sub-agent queue: not an object")
	}
	return object
}

func writeTestSubagentQueue(path string, queue jsonvalue.Object, what string) {
	body, err := jsonvalue.Marshal(queue)
	standInMust(err, what)
	standInMust(core.AtomicWrite(path, body), what)
}

func recordTestSubagentAdmission(relayRoot string, open bool) {
	queuePath := testSubagentQueuePath(relayRoot)
	queue := readTestSubagentQueue(queuePath)
	transitions, ok := subagentMCPGet(queue, "admission_transitions")
	if items, isArray := transitions.([]any); ok && isArray {
		queue.Set("admission_transitions", append(slices.Clone(items), open))
	} else {
		queue.Set("admission_transitions", []any{open})
	}
	queue.Set("mutating_admission_open", open)
	writeTestSubagentQueue(queuePath, queue, "persist fake worker sub-agent admission")
}

// projectMemoryRefusal: a project-memory connection runs beside the
// session's own, which owns the control journal. A worker serves memory at
// the connection layer, without the journal; these stand-ins have no memory
// endpoint, so they answer as DurableRelay does without opening a second
// journal writer. Opening one is not read-only: it saves the snapshot and
// settles interrupted work. Nil for any other request.
func projectMemoryRefusal(request *core.RelayRequestEnvelope) *core.RelayResponseEnvelope {
	switch request.Request.(type) {
	case core.RelayRequestProjectMemorySnapshot, core.RelayRequestProjectMemoryTransfer,
		core.RelayRequestInstallProjectMemorySnapshot, core.RelayRequestReplaceProjectMemoryTree:
	default:
		return nil
	}
	return &core.RelayResponseEnvelope{
		RequestID:       request.RequestID,
		ProtocolVersion: request.ProtocolVersion,
		Body:            core.RelayError(core.RelayErrorCodeInvalidState, "connection-only requests must be handled by the live relay transport", false, nil),
	}
}

// standInQueueValues decodes each member of the queue's section object.
func standInQueueValues[T any](queue jsonvalue.Object, section string) []T {
	values := []T{}
	members, _ := subagentMCPGet(queue, section)
	object, _ := members.(jsonvalue.Object)
	for _, member := range object {
		encoded, err := jsonvalue.Marshal(member.Value)
		standInMust(err, "encode a fixture queue entry")
		var value T
		standInMust(json.Unmarshal(encoded, &value), "decode a fixture queue entry")
		values = append(values, value)
	}
	return values
}

// subagentConnectionResponse answers the sub-agent queue requests from the
// fixture queue beside the relay root; nil for any other request.
func subagentConnectionResponse(request *core.RelayRequestEnvelope) *core.RelayResponseEnvelope {
	var queuePath *string
	if root, ok := os.LookupEnv(core.LatchRelayRoot); ok {
		path := testSubagentQueuePath(root)
		queuePath = &path
	}
	queue := emptyTestSubagentQueue()
	if queuePath != nil {
		queue = readTestSubagentQueue(*queuePath)
	}
	requests := func() []core.SubagentToolRequest {
		return standInQueueValues[core.SubagentToolRequest](queue, "requests")
	}
	results := func() []core.SubagentToolResult { return standInQueueValues[core.SubagentToolResult](queue, "results") }
	var payload core.RelayResponsePayload
	switch r := request.Request.(type) {
	case core.RelayRequestPollSubagentQueue:
		acknowledged := uint64(0)
		if r.Acknowledged != nil {
			acknowledged = *r.Acknowledged
		}
		changes := []core.SubagentQueueChange{}
		for _, queued := range requests() {
			changes = append(changes, core.SubagentQueueChangeRequest{Request: queued})
		}
		for _, result := range results() {
			changes = append(changes, core.SubagentQueueChangeCompleted{Completion: core.SubagentCompletionFrom(&result)})
		}
		payload = core.RelayResponsePayloadSubagentQueue{Page: core.SubagentQueuePage{
			Sequence: acknowledged + 1,
			Changes:  changes,
			HasMore:  false,
		}}
	case core.RelayRequestSubagentRequests:
		payload = core.RelayResponsePayloadSubagentRequests{Requests: requests(), Results: results()}
	case core.RelayRequestCompleteSubagentRequest:
		if queuePath == nil {
			panic("fixture queue root")
		}
		pending, _ := subagentMCPGet(queue, "requests")
		pendingObject, _ := pending.(jsonvalue.Object)
		pendingObject = pendingObject.Clone()
		// serde_json's Map::remove with preserve_order swaps the last member
		// into the removed one's place.
		pendingObject.SwapDelete(r.Result.RequestID)
		queue.Set("requests", pendingObject)
		finished, _ := subagentMCPGet(queue, "results")
		finishedObject, _ := finished.(jsonvalue.Object)
		finishedObject = finishedObject.Clone()
		encoded, err := json.Marshal(r.Result)
		standInMust(err, "encode the fixture result")
		value, err := jsonvalue.Parse(encoded)
		standInMust(err, "encode the fixture result")
		finishedObject.Set(r.Result.RequestID, value)
		queue.Set("results", finishedObject)
		writeTestSubagentQueue(*queuePath, queue, "persist the fixture queue")
		standInAppendLine(standInWithExtension(*queuePath, "deliveries"), r.Result.RequestID, "record the delivery")
		payload = core.RelayResponsePayloadSubagentRequestCompleted{}
	case core.RelayRequestSetSubagentAdmission:
		payload = core.RelayResponsePayloadSubagentAdmissionChanged{Open: r.Open}
	// This fake's queue never changes and its relay is served elsewhere:
	// hold briefly, as the worker would, and report nothing new.
	case core.RelayRequestWaitForChange:
		time.Sleep(200 * time.Millisecond)
		digest := ""
		if r.StateDigest != nil {
			digest = *r.StateDigest
		}
		payload = core.RelayResponsePayloadChangeObserved{
			LatestOrdinal:        r.AfterOrdinal,
			StateDigest:          digest,
			SubagentQueueChanged: len(requests()) > 0 || len(results()) > 0,
		}
	default:
		return nil
	}
	return &core.RelayResponseEnvelope{
		RequestID:       request.RequestID,
		ProtocolVersion: request.ProtocolVersion,
		Body:            core.RelayResponseBodyOk{Payload: payload},
	}
}

func requestsCheckpointRelease(request *core.RelayRequestEnvelope) bool {
	submit, ok := request.Request.(core.RelayRequestSubmit)
	if !ok {
		return false
	}
	_, ok = submit.Command.(core.RelayCommandReleaseCheckpoint)
	return ok
}

// unparseableRequestResponse is the answer a worker gives for a frame its
// own protocol cannot decode. An older RelayCommand has no
// release_checkpoint variant, and the enum denies unknown ones, so the
// request never reaches its relay.
func unparseableRequestResponse(request *core.RelayRequestEnvelope) core.RelayResponseEnvelope {
	return core.RelayResponseEnvelope{
		RequestID:       request.RequestID,
		ProtocolVersion: request.ProtocolVersion,
		Body: core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:      core.RelayErrorCodeInvalidRequest,
			Message:   "unknown variant `release_checkpoint`",
			Retryable: false,
		}},
	}
}

// standInReadJSON decodes the JSON file at path into a new T.
func standInReadJSON[T any](path string) T {
	data, err := os.ReadFile(path)
	standInMust(err, "read "+path)
	var value T
	standInMust(json.Unmarshal(data, &value), "decode "+path)
	return value
}

// ServeLeasedRelay is the relay server half of the leased-submission tests.
// It does nothing unless a parent test points it at a relay journal root.
func ServeLeasedRelay() {
	root, ok := os.LookupEnv(core.LeasedRelayRoot)
	if !ok {
		return
	}
	var memoryProtocol *uint32
	if path := filepath.Join(root, "memory-sync-protocol"); standInExists(path) {
		text, err := os.ReadFile(path)
		standInMust(err, "read the memory sync protocol")
		value, err := strconv.ParseUint(string(text), 10, 32)
		standInMust(err, "parse the memory sync protocol")
		protocol := uint32(value)
		memoryProtocol = &protocol
	}
	// With -test.v the test framework writes `=== RUN <name>` before the
	// body runs. End that line first so it cannot glue itself onto the first
	// protocol frame.
	fmt.Println()
	// The journal opens only together with its owner lock, which is held
	// from the first session request to this process's exit.
	var relay *DurableRelay
	marker, hasMarker := os.LookupEnv("MJ_TEST_BLOCKED_REVIEWER")
	input := bufio.NewReader(os.Stdin)
	output := os.Stdout
	subagentKnownIDs := map[string]struct{}{}
	// The queue files this connection last reported to a change wait.
	var reportedQueue *[3][]byte
	at := func(name string) string { return filepath.Join(root, name) }
	for {
		request, err := core.ReadRelayFrame(input)
		standInMust(err, "read a relay request")
		if request == nil {
			return
		}
		if _, poll := request.Request.(core.RelayRequestPollSubagentQueue); poll && standInExists(at("disconnect-subagent-queue")) {
			return
		}
		// One line per request, so a parent test can count round trips.
		standInAppendLine(at(core.RelayRequestLog), core.RelayRequestMethodName(request.Request), "log a relay request")
		if standInExists(at("worker-unreachable")) {
			return
		}
		if core.RelayRequestMethodName(request.Request) == "project_memory_transfer" && standInExists(at("drop-project-memory")) {
			return
		}
		if standInSubmits[core.RelayCommandSetQuotaRecovery](request.Request) {
			if blocked := at("block-quota-submit"); standInExists(blocked) {
				standInMust(os.WriteFile(at("quota-submit-waiting"), []byte("waiting"), 0o666), "record the waiting quota submit")
				for standInExists(blocked) {
					time.Sleep(5 * time.Millisecond)
				}
			}
		}
		var preparation core.HarnessPreparation
		if body, err := os.ReadFile(at("preparation.json")); err == nil {
			preparation, err = core.UnmarshalHarnessPreparationJSON(body)
			standInMust(err, "decode the harness preparation")
		}
		if _, admission := request.Request.(core.RelayRequestSetSubagentAdmission); preparation != nil && admission {
			standInWrite(output, core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: request.ProtocolVersion,
				Body: core.RelayResponseBodyError{Error: core.NewRelayProtocolError(
					core.RelayErrorCodeInvalidState,
					"harness preparation failed at harness-profile: missing staged Claude delegation server",
					false, nil)},
			}, "refuse the sub-agent admission")
			continue
		}
		if wait, ok := request.Request.(core.RelayRequestWaitForChange); ok {
			for standInExists(at("block-change-wait")) {
				if standInExists(at("worker-unreachable")) {
					return
				}
				time.Sleep(5 * time.Millisecond)
			}
			// Each connection to this fake is its own process, and only the
			// session's own connection may open the relay journal. So this
			// wait reports the relay as the controller named it and holds,
			// like the real worker, until the queue files it rereads change.
			queueFiles := func() [3][]byte {
				var files [3][]byte
				for index, name := range []string{"subagent-request.json", "subagent-result.json", "guardian-request.json"} {
					if body, err := os.ReadFile(at(name)); err == nil {
						files[index] = append([]byte{}, body...)
					}
				}
				return files
			}
			deadline := time.Now().Add(2 * time.Second)
			var queueChanged, guardianQueueChanged bool
			for {
				if standInExists(at("worker-unreachable")) {
					return
				}
				queue := queueFiles()
				subagent := reportedQueue == nil || !standInSameQueue(reportedQueue[:2], queue[:2])
				guardian := queue[2] != nil && (reportedQueue == nil || !standInSameQueue(reportedQueue[2:], queue[2:]))
				reportedQueue = &queue
				if subagent || guardian || !time.Now().Before(deadline) {
					queueChanged, guardianQueueChanged = subagent, guardian
					break
				}
				time.Sleep(20 * time.Millisecond)
			}
			digest := ""
			if wait.StateDigest != nil {
				digest = *wait.StateDigest
			}
			standInWrite(output, core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: request.ProtocolVersion,
				Body: core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadChangeObserved{
					LatestOrdinal:        wait.AfterOrdinal,
					StateDigest:          digest,
					SubagentQueueChanged: queueChanged,
					GuardianQueueChanged: guardianQueueChanged,
				}},
			}, "answer a change wait")
			continue
		}
		var history core.RelayResponsePayload
		switch r := request.Request.(type) {
		case core.RelayRequestHello:
			negotiated := core.RelayProtocolVersion
			if memoryProtocol != nil {
				negotiated = *memoryProtocol
			}
			history = core.RelayResponsePayloadHello{Negotiated: negotiated, RelayVersion: "1.0.0", SessionID: core.LeasedRelaySession}
		case core.RelayRequestCpuUsage:
			var usage *core.SessionCpuUsage
			if path := at("cpu-usage.json"); standInExists(path) {
				read := standInReadJSON[core.SessionCpuUsage](path)
				usage = &read
			}
			history = core.RelayResponsePayloadCpuUsage{Usage: usage}
		case core.RelayRequestSubagentRequests, core.RelayRequestPollSubagentQueue:
			requests := []core.SubagentToolRequest{}
			if path := at("subagent-request.json"); standInExists(path) {
				requests = append(requests, standInReadJSON[core.SubagentToolRequest](path))
			}
			results := []core.SubagentToolResult{}
			if path := at("subagent-result.json"); standInExists(path) {
				results = append(results, standInReadJSON[core.SubagentToolResult](path))
			}
			poll, polling := r.(core.RelayRequestPollSubagentQueue)
			if !polling {
				history = core.RelayResponsePayloadSubagentRequests{Requests: requests, Results: results}
				break
			}
			changes := []core.SubagentQueueChange{}
			for _, queued := range requests {
				changes = append(changes, core.SubagentQueueChangeRequest{Request: queued})
			}
			for _, result := range results {
				changes = append(changes, core.SubagentQueueChangeCompleted{Completion: core.SubagentCompletionFrom(&result)})
			}
			ids := map[string]struct{}{}
			for _, change := range changes {
				ids[change.RequestID()] = struct{}{}
			}
			var retired []string
			for id := range subagentKnownIDs {
				if _, still := ids[id]; !still {
					retired = append(retired, id)
				}
			}
			slices.Sort(retired)
			for _, id := range retired {
				changes = append(changes, core.SubagentQueueChangeRetired{RequestIDValue: id})
			}
			subagentKnownIDs = ids
			acknowledged := uint64(0)
			if poll.Acknowledged != nil {
				acknowledged = *poll.Acknowledged
			}
			history = core.RelayResponsePayloadSubagentQueue{Page: core.SubagentQueuePage{
				Sequence: acknowledged + 1,
				Changes:  changes,
				HasMore:  false,
			}}
		case core.RelayRequestCompleteSubagentRequest:
			if standInExists(at("slow-delegation-delivery")) {
				time.Sleep(6 * time.Second)
			}
			body, err := json.Marshal(r.Result)
			standInMust(err, "encode the sub-agent result")
			standInMust(os.WriteFile(at("subagent-result.json"), body, 0o666), "write the sub-agent result")
			if path := at("subagent-request.json"); standInExists(path) {
				standInMust(os.Remove(path), "remove the sub-agent request")
			}
			history = core.RelayResponsePayloadSubagentRequestCompleted{}
		case core.RelayRequestGuardianRequests:
			requests := []core.GuardianReviewRequest{}
			body, err := os.ReadFile(at("guardian-request.json"))
			switch {
			case err == nil:
				var pending core.GuardianReviewRequest
				standInMust(json.Unmarshal(body, &pending), "decode the stand-in guardian request")
				requests = append(requests, pending)
			case !errors.Is(err, fs.ErrNotExist):
				panic(fmt.Sprintf("read stand-in guardian queue: %v", err))
			}
			history = core.RelayResponsePayloadGuardianRequests{Requests: requests}
		case core.RelayRequestCompleteGuardianRequest:
			if path := at("guardian-request.json"); standInExists(path) {
				pending := standInReadJSON[core.GuardianReviewRequest](path)
				if pending.RequestID == r.Result.RequestID {
					body, err := json.Marshal(r.Result)
					standInMust(err, "encode the guardian result")
					standInMust(core.AtomicWrite(at("guardian-result.json"), body), "write the guardian result")
					standInMust(os.Remove(path), "remove the guardian request")
				}
			}
			history = core.RelayResponsePayloadGuardianRequestCompleted{}
		case core.RelayRequestHistoryRequests:
			requests := []core.HistoryRequest{}
			if path := at("history-request.json"); standInExists(path) {
				requests = append(requests, standInReadJSON[core.HistoryRequest](path))
			}
			history = core.RelayResponsePayloadHistoryRequests{Requests: requests}
		case core.RelayRequestSetSubagentAdmission:
			admission := at("subagent-admission.json")
			state := subagentObject("open", true, "calls", 0)
			if body, err := os.ReadFile(admission); err == nil {
				if parsed, err := jsonvalue.Parse(body); err == nil {
					if object, ok := parsed.(jsonvalue.Object); ok {
						state = object
					}
				}
			}
			calls := uint64(0)
			switch count, _ := subagentMCPGet(state, "calls"); count := count.(type) {
			case int64:
				if count >= 0 {
					calls = uint64(count)
				}
			case uint64:
				calls = count
			}
			state.Set("open", r.Open)
			state.Set("calls", calls+1)
			body, err := jsonvalue.Marshal(state)
			standInMust(err, "encode the sub-agent admission")
			standInMust(os.WriteFile(admission, body, 0o666), "write the sub-agent admission")
			marker := at("drop-admission-open-reply-once")
			if _, drop := os.LookupEnv(core.DropAdmissionOpenReply); r.Open && drop && !standInExists(marker) {
				standInMust(os.WriteFile(marker, []byte("dropped"), 0o666), "record the dropped open reply")
				return
			}
			history = core.RelayResponsePayloadSubagentAdmissionChanged{Open: r.Open}
		case core.RelayRequestCompleteHistoryRequest:
			body, err := json.Marshal(r.Result)
			standInMust(err, "encode the history result")
			standInMust(os.WriteFile(at("history-result.json"), body, 0o666), "write the history result")
			standInMust(os.Remove(at("history-request.json")), "remove the history request")
			history = core.RelayResponsePayloadHistoryRequestCompleted{}
		}
		if history != nil {
			protocolVersion := request.ProtocolVersion
			if hello, ok := history.(core.RelayResponsePayloadHello); ok {
				protocolVersion = hello.Negotiated
			}
			standInWrite(output, core.RelayResponseEnvelope{
				RequestID:       request.RequestID,
				ProtocolVersion: protocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: history},
			}, "answer a connection request")
			continue
		}
		if refusal := projectMemoryRefusal(request); refusal != nil {
			standInWrite(output, *refusal, "answer a project-memory request")
			continue
		}
		// Each connection to this fake is its own process with its own copy
		// of the relay, where the real worker is one process that holds
		// worker.lock. So the session's connection owns the journal the same
		// way, and its successor waits here until the connection it
		// replaced has finished its last request and exited: the client
		// abandons a connection that timed out while the fake is still
		// holding that request. Only the owner opens the journal, so it
		// reads everything earlier owners wrote.
		if relay == nil {
			relay = openLeasedRelay(root)
		}
		completeClear := at("complete-clear")
		_, clearControl := os.LookupEnv(core.ClearMailboxControl)
		if clearControl && standInExists(completeClear) {
			standInMust(os.Remove(completeClear), "remove the clear marker")
			_, err := relay.RecordCommandCompleted("clear-request", core.RelayCommandOutcomeContextCleared{NativeSessionID: "replacement"})
			standInMust(err, "complete the clear")
		}
		startsClear := standInSubmits[core.RelayCommandClearContext](request.Request)
		// This fake edits the state below, after the relay digested it, so
		// it cannot vouch for a digest: every attach gets the whole state.
		envelope := *request
		if attach, ok := envelope.Request.(core.RelayRequestAttach); ok && preparation != nil {
			attach.KnownStateDigest = nil
			envelope.Request = attach
		}
		var response core.RelayResponseEnvelope
		switch r := envelope.Request.(type) {
		case core.RelayRequestReviewer:
			if hasMarker && r.Role != nil && *r.Role == "slow" {
				standInMust(os.WriteFile(marker, []byte("started"), 0o666), "record the started reviewer")
				_, err := io.Copy(io.Discard, input)
				standInMust(err, "wait for the reviewer to disconnect")
				standInMust(os.WriteFile(marker, []byte("disconnected"), 0o666), "record the disconnected reviewer")
				return
			}
			response = core.RelayResponseEnvelope{
				RequestID:       envelope.RequestID,
				ProtocolVersion: envelope.ProtocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadReviewBaselineAdvanced{}},
			}
		case core.RelayRequestSetOperatorAway:
			standInAppendLine(at("operator-presence.log"), fmt.Sprintf("%d %t", r.Revision, r.Away), "log the operator presence")
			response = core.RelayResponseEnvelope{
				RequestID:       envelope.RequestID,
				ProtocolVersion: envelope.ProtocolVersion,
				Body:            core.RelayResponseBodyOk{Payload: core.RelayResponsePayloadStatus{State: relay.OperationalState()}},
			}
		default:
			wireProtocol := envelope.ProtocolVersion
			if memoryProtocol != nil {
				// This scoped peer speaks its advertised wire version, while
				// the in-process relay retains its strict current protocol.
				envelope.ProtocolVersion = core.RelayProtocolVersion
			}
			response = relay.Handle(envelope)
			if memoryProtocol != nil {
				response.ProtocolVersion = wireProtocol
			}
		}
		if startsClear && clearControl {
			_, err := relay.ClaimPendingCommands(true)
			standInMust(err, "claim the clear")
		}
		if preparation != nil {
			standInMust(relay.DispatchPreparationLifecycle(), "dispatch the preparation lifecycle")
			response.Body = withStandInPreparation(response.Body, preparation, at("active-user-shell.json"))
		}
		standInWrite(output, response, "answer a relay request")
	}
}

// standInSameQueue compares two readings of the queue files; a nil entry
// is a file that could not be read.
func standInSameQueue(a, b [][]byte) bool {
	for index := range a {
		if (a[index] == nil) != (b[index] == nil) || string(a[index]) != string(b[index]) {
			return false
		}
	}
	return true
}

// withStandInPreparation puts preparation, and an active user shell when
// the fixture names one, into a status or attach answer's state.
func withStandInPreparation(body core.RelayResponseBody, preparation core.HarnessPreparation, shellPath string) core.RelayResponseBody {
	ok, isOk := body.(core.RelayResponseBodyOk)
	if !isOk {
		return body
	}
	edit := func(state *core.RelayOperationalState) {
		state.HarnessPreparation = preparation
		if shell, err := os.ReadFile(shellPath); err == nil {
			var active core.ActiveUserShell
			standInMust(json.Unmarshal(shell, &active), "decode the active user shell")
			state.ActiveUserShells = []core.ActiveUserShell{active}
		}
	}
	switch payload := ok.Payload.(type) {
	case core.RelayResponsePayloadStatus:
		edit(&payload.State)
		ok.Payload = payload
	case core.RelayResponsePayloadAttached:
		payload.StateDigest = nil
		edit(&payload.State)
		ok.Payload = payload
	}
	return ok
}

// openLeasedRelay takes the journal owner lock and opens the leased test's
// relay journal, recording what the test asked a worker to have recorded.
func openLeasedRelay(root string) *DurableRelay {
	lock, err := os.OpenFile(filepath.Join(root, "journal-owner.lock"), os.O_CREATE|os.O_WRONLY, 0o666)
	standInMust(err, "open the test relay journal's owner lock")
	standInMust(standInLockExclusive(lock), "wait to own the test relay journal")
	standInJournalOwner = lock
	relay, err := DurableRelayOpen(root, core.LeasedRelaySession, "1.0.0")
	standInMust(err, "open the test relay journal")
	if _, ok := os.LookupEnv(core.ClearMailboxControl); ok {
		relay.SetTurnVerdictHarness(core.HarnessKindCodex)
		if relay.OperationalState().NativeSessionID == nil {
			_, err := relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "original"})
			standInMust(err, "record the opened session")
		}
	}
	if standInExists(filepath.Join(root, "disconnect-subagent-queue")) {
		relay.SetTurnVerdictHarness(core.HarnessKindClaude)
	}
	if _, ok := os.LookupEnv(core.RelaySessionConfigured); ok {
		// What a worker records once its harness session is open, so the
		// relay reports no work in flight.
		_, policy := os.LookupEnv("MJ_TEST_MODEL_POLICY")
		options := []acpschema.SessionConfigOption{}
		if policy {
			options = policyTestOptions()
		}
		_, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: options})
		standInMust(err, "record the configured harness session")
		if policy {
			_, err := relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "policy-native"})
			standInMust(err, "record the opened session")
		}
	}
	return relay
}

func policyTestOptions() []acpschema.SessionConfigOption {
	category := acpschema.SessionConfigOptionCategoryModel
	var choices []acpschema.SessionConfigSelectOption
	for _, value := range []string{"current", "allowed", "hidden"} {
		choices = append(choices, acpschema.SessionConfigSelectOption{Value: acpschema.SessionConfigValueId(value), Name: value})
	}
	return []acpschema.SessionConfigOption{{
		Kind:     acpschema.SessionConfigOptionKindSelect,
		ID:       "provider-model",
		Name:     "Model",
		Category: &category,
		Select: &acpschema.SessionConfigSelect{
			CurrentValue: "current",
			Options:      choices,
		},
	}}
}

// SeedSourceRelay is the source relay journal an in-place move starts from.
func SeedSourceRelay(workerRoot string) string {
	sourceRelay := filepath.Join(filepath.Dir(workerRoot), "source-relay")
	standInMust(os.MkdirAll(sourceRelay, 0o777), "create the source relay")
	seed, err := DurableRelayOpen(sourceRelay, core.LatchRelaySession, "1.0.0")
	standInMust(err, "open the source relay")
	_, err = seed.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "native-session", Resumed: true})
	standInMust(err, "record the opened session")
	_, err = seed.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	standInMust(err, "record the configured session")
	return sourceRelay
}

// KeepQuestionLive keeps a stand-in harness's real responder registration
// alive across relay requests. The result holds it (Rust: Box<dyn Send>).
func KeepQuestionLive(relay *DurableRelay, question core.ElicitationRequest) any {
	return relay.operatorAway.keepFixtureQuestionLive(question)
}
