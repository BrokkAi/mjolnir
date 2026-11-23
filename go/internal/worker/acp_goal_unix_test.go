// Port of mj-worker/src/acp/goal.rs: its tests module (cfg(all(test,
// unix))).

//go:build unix

package worker

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
	"weak"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

const goalRecoveryScript = `
import json, os, sys, time
asking = os.environ['ASKING'] == 'true'
running = os.environ['RUNNING'] == 'true'
def emit(x): print(json.dumps(x), flush=True)
def update(meta): emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'session_info_update','_meta':meta}}})
goal = {'objective':'finish campaign','status':'active','createdAt':123,'tokenBudget':900,'tokensUsed':100,'controlMethod':'_session/goal'}
for line in sys.stdin:
    r = json.loads(line); method=r.get('method'); ident=r.get('id'); p=r.get('params',{})
    if ident is None: continue
    if method == 'initialize':
        result={'protocolVersion':1,'agentCapabilities':{'sessionCapabilities':{'resume':{}}},'_meta':{'jetbrains':{'air':{'goal':{'version':1,'controlMethod':'_session/goal','actions':['resume','pause'],'resumePolicies':['pause','preserve']}}},'execution':{'version':1}}}
    elif method == 'session/resume':
        assert p['_meta']['jetbrains']['air']['goal']['resumePolicy'] == ('pause' if asking else 'preserve')
        goal['status']='paused' if asking and os.environ['GOAL_STATUS']=='active' else os.environ['GOAL_STATUS']
        result={'modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Guardian'},{'id':'auto','name':'Auto'}]},'_meta':{'jetbrains':{'air':{'goal':goal}},'execution':{'version':1,'status':'running' if running else 'idle','turnId':'native-turn' if running else None}}}
        # Live output is legal before the resume response and exceeds a pipe buffer.
        if running:
            emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'x'*80000}}}})
    elif method == '_session/goal':
        if p['action']=='resume':
            assert p['expectedGoal']=={'objective':'finish campaign','createdAt':123}
            with open('resumes','a') as f: f.write('resume\n')
            if os.environ.get('WAIT_FOR_CUT') == 'true':
                while not os.path.exists('release-resume'): time.sleep(0.01)
            if os.environ.get('REJECT_CODE'):
                emit({'jsonrpc':'2.0','id':ident,'error':{'code':int(os.environ['REJECT_CODE']),'message':'recorded rejection'}})
                continue
            goal['status']='active'; update({'jetbrains':{'air':{'goal':goal}},'execution':{'version':1,'status':'running','turnId':'resumed'}})
        else:
            goal['status']='paused'; update({'jetbrains':{'air':{'goal':goal}}})
        result={}
    else: result={}
    emit({'jsonrpc':'2.0','id':ident,'result':result})
`

const goalControlsScript = `
import json, os, sys
def emit(x): print(json.dumps(x), flush=True)
def result(i): emit({'jsonrpc':'2.0','id':i,'result':{}})
def update(x): emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':x}})
def goal(status): update({'sessionUpdate':'session_info_update','_meta':{'jetbrains':{'air':{'goal':None if status is None else {'objective':'finish','status':status,'createdAt':123,'tokensUsed':42,'tokenBudget':900}}}}})
def text(s): update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':s}})
codex = os.environ['CODEX'] == 'true'
held = None
clears = 0
for line in sys.stdin:
    r=json.loads(line); m=r.get('method'); i=r.get('id'); p=r.get('params',{})
    if i is None: continue
    if m=='initialize':
        emit({'jsonrpc':'2.0','id':i,'result':{'protocolVersion':1,'agentCapabilities':{},'_meta':{'jetbrains':{'air':{'goal':{'version':1,'controlMethod':'_session/goal','actions':['pause','resume','clear'] if codex else ['set','clear']}}}}}})
    elif m=='session/new':
        emit({'jsonrpc':'2.0','id':i,'result':{'sessionId':'native','modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Guardian'},{'id':'auto','name':'Auto'}]},'_meta':{'jetbrains':{'air':{'goal':{'objective':'finish','status':'active','createdAt':123,'tokensUsed':42,'tokenBudget':900}}}}}})
    elif m=='session/prompt':
        assert p['prompt'][0]['text']=='work'
        text('prompt-started')
        # Leave the foreground prompt open until close.
    elif m=='_session/goal':
        assert 'expectedGoal' not in p
        action=p['action']
        if action=='resume':
            assert codex
            held=i
            goal('active')
            text('resume-pending')
        elif action=='pause':
            assert codex
            goal('paused')
            text('x'*100000)
            result(i)
        elif action=='clear':
            clears += 1
            if clears > 1:
                emit({'jsonrpc':'2.0','id':i,'error':{'code':-32603,'message':'control denied by test adapter'}})
                continue
            goal(None)
            text('x'*100000)
            result(i)
            if held is not None: result(held); held=None
        else: raise AssertionError(action)
    else: result(i)
`

// goalUpdateText is `update.pointer("/content/text")` of a session update's
// text, "" when it has none.
func goalUpdateText(update json.RawMessage) string {
	var decoded struct {
		Content struct {
			Text string `json:"text"`
		} `json:"content"`
	}
	_ = json.Unmarshal(update, &decoded)
	return decoded.Content.Text
}

// goalNext waits up to limit for the next runtime event; ok is false when
// the channel closed.
func goalNext(t *testing.T, events <-chan core.RuntimeEvent, limit time.Duration) (core.RuntimeEvent, bool) {
	t.Helper()
	select {
	case event, ok := <-events:
		return event, ok
	case <-time.After(limit):
		t.Fatal("the runtime must make progress")
		return nil, false
	}
}

func goalRecord(t *testing.T, relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent) {
	t.Helper()
	if _, err := recordRuntimeEvent(relay, inFlight, event); err != nil {
		t.Fatal(err)
	}
}

func goalFileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// goalAwaitStopped is Rust's `drop(tx); while receive.recv().await.is_some()
// {}` followed by the bounded wait for the runtime task: Run never closes
// events, so the drain ends when Run returns rather than outliving the test.
func goalAwaitStopped(t *testing.T, requests chan<- CommandRequest, events <-chan core.RuntimeEvent, runtime <-chan error) {
	t.Helper()
	close(requests)
	deadline := time.After(10 * time.Second)
	for {
		select {
		case <-events:
		case err := <-runtime:
			if err != nil {
				t.Fatal(err)
			}
			return
		case <-deadline:
			t.Fatal("the runtime did not stop")
		}
	}
}

// Hard-won: 5c9e20e: guarded Codex identity rejection uses invalidRequest.
func TestGoalRecoveryResumesOnlyWhenNeededAndExplicitOpenAsks(t *testing.T) {
	_ = harnessinstall.InitializeFromParent()
	type goalCase struct {
		asking, running bool
		answer          *bool
		expectedResumes int
		status          string
		decision        bool
		timedOut        bool
		rejectCode      *int
	}
	yes, no := true, false
	code := func(value int) *int { return &value }
	for _, tc := range []goalCase{
		{false, false, nil, 1, "active", false, false, nil},
		{false, true, nil, 0, "active", false, false, nil},
		{true, false, &yes, 1, "active", false, false, nil},
		{true, false, &no, 0, "active", false, false, nil},
		// Hard-won: ba25f84: an unanswered command-loop goal form must survive adoption until bridge exit.
		{true, false, nil, 0, "active", false, false, nil},
		{false, false, nil, 0, "paused", false, false, nil},
		{false, false, nil, 0, "blocked", false, false, nil},
		{false, false, nil, 0, "limited", false, false, nil},
		{false, false, nil, 0, "complete", false, false, nil},
		{true, false, nil, 0, "complete", false, false, nil},
		{false, false, nil, 1, "paused", true, false, nil},
		{false, false, nil, 1, "active", false, true, nil},
		// Hard-won: 655be2b: only proven peer rejection releases its own resume.
		{false, false, nil, 1, "active", false, false, code(-32600)},
		{false, false, nil, 1, "active", false, false, code(-32602)},
		{false, false, nil, 1, "active", false, false, code(-32601)},
		{false, false, nil, 1, "active", false, false, code(-32603)},
	} {
		root := t.TempDir()
		script := filepath.Join(root, "goal.py")
		if err := os.WriteFile(script, []byte(goalRecoveryScript), 0o644); err != nil {
			t.Fatal(err)
		}
		recovery := core.GoalRecoveryContext{}
		if tc.asking {
			request := "explicit-1"
			recovery.Request = &request
		}
		if tc.decision {
			var goal core.GoalSnapshot
			if err := json.Unmarshal([]byte(`{"objective":"finish campaign","status":"paused","createdAt":123,"controlMethod":"_session/goal"}`), &goal); err != nil {
				t.Fatal(err)
			}
			recovery.State.Decision = &core.GoalDecision{Goal: goal, Resume: true}
		}
		context := NewShared(recovery)
		trackCut := !tc.asking && !tc.running && tc.status == "active"
		durable, err := DurableRelayOpen(filepath.Join(root, "relay"), session, "test")
		if err != nil {
			t.Fatal(err)
		}
		durable.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
		relay := NewRelayCell(durable)
		observed := relay.Observe()
		operatorAway := observed.operatorAway
		observed.Unlock()
		rejectCode := ""
		if tc.rejectCode != nil {
			rejectCode = strconv.Itoa(*tc.rejectCode)
		}
		resume := "native"
		spec := acpFeaturesSpec("python3", []string{script}, map[string]string{
			"ASKING":       strconv.FormatBool(tc.asking),
			"WAIT_FOR_CUT": strconv.FormatBool(trackCut),
			"REJECT_CODE":  rejectCode,
			"RUNNING":      strconv.FormatBool(tc.running),
			"GOAL_STATUS":  tc.status,
		}, root)
		spec.OperatorAway = operatorAway
		spec.GoalRecovery = context
		spec.ResumeSession = &resume
		spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
		spec.Harness = core.HarnessKindCodex
		if trackCut {
			owner := weak.Make(relay)
			spec.TerminalOwner = &owner
		}
		requests := make(chan CommandRequest, 8)
		events := make(chan core.RuntimeEvent, 8)
		runtime := make(chan error, 1)
		go func() { runtime <- Run(spec, requests, events) }()
		if trackCut {
			inFlight := map[string]core.RelayCommand{}
			// Reopen's automatic resume is not a relay command. Apply every
			// preceding runtime event in FIFO order until the reservation
			// marker arrives, then prove `_session/goal` cannot be sent before
			// the relay records and acknowledges that reservation.
			var reservation core.RuntimeEvent
			deadline := time.Now().Add(10 * time.Second)
			for reservation == nil {
				event, _ := goalNext(t, events, time.Until(deadline))
				if _, ok := event.(core.RuntimeEventNativeGoalResumeSent); ok {
					reservation = event
					break
				}
				goalRecord(t, relay, inFlight, event)
			}
			time.Sleep(50 * time.Millisecond)
			if goalFileExists(filepath.Join(root, "resumes")) {
				t.Fatal("_session/goal must not be sent before the FIFO reservation is recorded on the relay")
			}
			goalRecord(t, relay, inFlight, reservation)
			guard := relay.Lock()
			cut := guard.ReserveLiveBaseline()
			guard.Unlock()
			if _, ok := cut.TryRecv(); ok {
				t.Fatal("quiescent adoption cut must be blocked as soon as the reservation applies")
			}
			observed := relay.Observe()
			blockers := observed.adoptionBlockers()
			observed.Unlock()
			if !slices.ContainsFunc(blockers, func(blocker string) bool { return strings.Contains(blocker, "goal resume") }) {
				t.Fatal("relay must report the goal resume blocker before _session/goal reaches the peer")
			}
			deadline = time.Now().Add(10 * time.Second)
			for !goalFileExists(filepath.Join(root, "resumes")) {
				if time.Now().After(deadline) {
					t.Fatal("the resume must reach the peer")
				}
				select {
				case event := <-events:
					goalRecord(t, relay, inFlight, event)
				case <-time.After(10 * time.Millisecond):
				}
			}
			awaitQuestion := func(limit time.Duration) {
				deadline := time.Now().Add(limit)
				for {
					event, _ := goalNext(t, events, time.Until(deadline))
					_, question := event.(core.RuntimeEventElicitationRequested)
					goalRecord(t, relay, inFlight, event)
					if question {
						return
					}
				}
			}
			if tc.timedOut {
				// Rust advances a paused clock past the 30-second
				// acknowledgement timeout; Go waits it out.
				awaitQuestion(45 * time.Second)
				if _, ok := cut.TryRecv(); ok {
					t.Fatal("an ambiguous timeout must not withdraw the resume reservation")
				}
			}
			if err := os.WriteFile(filepath.Join(root, "release-resume"), nil, 0o644); err != nil {
				t.Fatal(err)
			}
			if tc.rejectCode != nil {
				awaitQuestion(10 * time.Second)
				if *tc.rejectCode == -32603 {
					if _, ok := cut.TryRecv(); ok {
						t.Fatal("an internal error remains ambiguous")
					}
				} else if result, ok := cut.TryRecv(); !ok || result.Err != nil {
					t.Fatal("peer rejected resume without a turn update")
				}
			} else {
				deadline := time.Now().Add(10 * time.Second)
				for {
					observed := relay.Observe()
					inProgress := observed.turnInProgress()
					observed.Unlock()
					if inProgress {
						break
					}
					event, _ := goalNext(t, events, time.Until(deadline))
					goalRecord(t, relay, inFlight, event)
				}
				if _, ok := cut.TryRecv(); ok {
					t.Fatal("the cut was taken while the resumed turn ran")
				}
				guard := relay.Lock()
				_, err := guard.RecordObservation(core.RelayObservationHarnessTurnSettled{})
				guard.Unlock()
				if err != nil {
					t.Fatal(err)
				}
				if result, ok := cut.TryRecv(); !ok || result.Err != nil {
					t.Fatal("the cut was not taken after the turn settled")
				}
			}
		}
		bytes := 0
		if tc.answer != nil || (tc.asking && tc.status == "active") {
			for {
				event, _ := goalNext(t, events, 10*time.Second)
				requested, ok := event.(core.RuntimeEventElicitationRequested)
				if !ok {
					continue
				}
				request := requested.Request
				if request.ID != core.RecoveryID {
					t.Fatalf("question %s", request.ID)
				}
				if goalFileExists(filepath.Join(root, "resumes")) {
					t.Fatal("a resume was sent before the person answered")
				}
				{
					state := relay.Lock()
					state.operatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
					if _, err := state.RecordObservation(core.RelayObservationElicitationRequested{Request: request}); err != nil {
						t.Fatal(err)
					}
					cut, err := state.LiveSessionBaseline()
					state.Unlock()
					if err != nil {
						t.Fatal(err)
					}
					if len(cut.Questions) != 1 || cut.Questions[0].ID != request.ID {
						t.Fatalf("questions %+v", cut.Questions)
					}
					adopted, err := transcript.MaterializedSessionFromLiveBaseline(&cut)
					if err != nil {
						t.Fatal(err)
					}
					if len(adopted.PendingElicitations) != 1 || adopted.PendingElicitations[0].ID != request.ID {
						t.Fatalf("pending %+v", adopted.PendingElicitations)
					}
				}
				if tc.answer == nil {
					break
				}
				response := core.ElicitationResponse{Action: core.ElicitationResponseDecline}
				if *tc.answer {
					response = core.ElicitationResponse{
						Action:  core.ElicitationResponseAccept,
						Content: map[string]core.ElicitationValue{"action": core.ElicitationValueString("resume")},
					}
				}
				resolved := make(chan error, 1)
				requests <- CommandRequestResolveElicitation{ElicitationID: core.RecoveryID, Response: response, Resolved: resolved}
				// Continue draining while the control response is in flight.
				// Rust's ack and event receivers both fail once the runtime
				// drops them; Run never closes events, so watch it return.
			drain:
				for {
					select {
					case err := <-resolved:
						if err != nil {
							t.Fatal(err)
						}
						break drain
					case <-events:
					case err := <-runtime:
						t.Fatalf("the runtime stopped before answering: %v", err)
					}
				}
				observed := relay.Observe()
				live := observed.operatorAway.liveQuestions()
				observed.Unlock()
				if len(live) != 0 {
					t.Fatalf("live questions %v", live)
				}
				recovery, unlock := context.Lock()
				answered := recovery.State.AnsweredResume
				unlock()
				if answered == nil || *answered != "explicit-1" {
					t.Fatalf("answered resume %v", answered)
				}
				break
			}
		}
		requests <- CommandRequestClose{RequestID: "close"}
	closing:
		for {
			event, ok := goalNext(t, events, 10*time.Second)
			if !ok {
				break
			}
			switch event := event.(type) {
			case core.RuntimeEventSessionUpdate:
				bytes += len(goalUpdateText(event.Update))
			case core.RuntimeEventCloseApplied:
				break closing
			case core.RuntimeEventElicitationRequested:
				t.Fatal("automatic recovery must not ask")
			}
		}
		goalAwaitStopped(t, requests, events, runtime)
		observed = relay.Observe()
		live := observed.operatorAway.liveQuestions()
		observed.Unlock()
		if len(live) != 0 {
			t.Fatalf("live questions %v", live)
		}
		resumes, _ := os.ReadFile(filepath.Join(root, "resumes"))
		if count := strings.Count(string(resumes), "\n"); count != tc.expectedResumes {
			t.Fatalf("%+v: %d resumes, want %d", tc, count, tc.expectedResumes)
		}
		if tc.running && bytes != 80000 {
			t.Fatalf("%d bytes", bytes)
		}
	}
}

func TestGoalControlsWorkDuringPromptsAndPendingResumeWithLargeOutput(t *testing.T) {
	_ = harnessinstall.InitializeFromParent()
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude} {
		for _, foreground := range []bool{false, true} {
			root := t.TempDir()
			script := filepath.Join(root, "controls.py")
			if err := os.WriteFile(script, []byte(goalControlsScript), 0o644); err != nil {
				t.Fatal(err)
			}
			context := NewShared(core.GoalRecoveryContext{})
			spec := acpFeaturesSpec("python3", []string{script}, map[string]string{
				"CODEX": strconv.FormatBool(harness == core.HarnessKindCodex),
			}, root)
			spec.GoalRecovery = context
			spec.Harness = harness
			requests := make(chan CommandRequest, 8)
			events := make(chan core.RuntimeEvent, 2)
			runtime := make(chan error, 1)
			go func() { runtime <- Run(spec, requests, events) }()
			next := func() core.RuntimeEvent {
				event, ok := goalNext(t, events, 10*time.Second)
				if !ok {
					t.Fatal("runtime remains alive")
				}
				return event
			}
			for {
				if _, ok := next().(core.RuntimeEventSessionStarted); ok {
					break
				}
			}
			recovery, unlock := context.Lock()
			supportsClear := recovery.State.Supports(core.GoalControlActionClear)
			active := recovery.State.Active()
			unlock()
			if !supportsClear {
				t.Fatal("clear is not supported")
			}
			if !active {
				t.Fatal("both adapters' session-open AIR goal snapshot is ingested")
			}
			awaitText := func(text string) {
				for {
					if update, ok := next().(core.RuntimeEventSessionUpdate); ok && goalUpdateText(update.Update) == text {
						return
					}
				}
			}
			if foreground {
				requests <- CommandRequestPrompt{
					RequestID: "work",
					Prompt:    []schema.ContentBlock{acpFeaturesText("work")},
				}
				awaitText("prompt-started")
			}
			if harness == core.HarnessKindCodex {
				requests <- CommandRequestGoalControl{RequestID: "resume", Action: core.GoalControlActionResume}
				awaitText("resume-pending")
				requests <- CommandRequestGoalControl{RequestID: "pause", Action: core.GoalControlActionPause}
			} else {
				requests <- CommandRequestGoalControl{RequestID: "unsupported", Action: core.GoalControlActionPause}
			}
			bytes := 0
		controls:
			for {
				switch event := next().(type) {
				case core.RuntimeEventGoalControlApplied:
					if event.RequestID != "pause" {
						t.Fatalf("applied %s", event.RequestID)
					}
					break controls
				case core.RuntimeEventCommandRejected:
					if harness != core.HarnessKindClaude || event.RequestID != "unsupported" ||
						!strings.Contains(event.Message, "not supported") {
						t.Fatalf("rejected %+v", event)
					}
					break controls
				case core.RuntimeEventSessionUpdate:
					bytes += len(goalUpdateText(event.Update))
				}
			}
			requests <- CommandRequestGoalControl{RequestID: "clear", Action: core.GoalControlActionClear}
			want := 1
			if harness == core.HarnessKindCodex {
				want = 2
			}
			for completed := 0; completed < want; {
				switch event := next().(type) {
				case core.RuntimeEventGoalControlApplied:
					if event.RequestID != "clear" && event.RequestID != "resume" {
						t.Fatalf("applied %s", event.RequestID)
					}
					completed++
				case core.RuntimeEventSessionUpdate:
					bytes += len(goalUpdateText(event.Update))
				case core.RuntimeEventCommandRejected:
					t.Fatal(event.Message)
				}
			}
			wantBytes := 100000
			if harness == core.HarnessKindCodex {
				wantBytes = 200000
			}
			if bytes != wantBytes {
				t.Fatalf("%d bytes, want %d", bytes, wantBytes)
			}
			recovery, unlock = context.Lock()
			snapshot := recovery.State.Snapshot
			unlock()
			if snapshot != nil {
				t.Fatal("late resume acknowledgement must not reactivate a cleared goal")
			}
			requests <- CommandRequestGoalControl{RequestID: "denied", Action: core.GoalControlActionClear}
			for {
				if rejected, ok := next().(core.RuntimeEventCommandRejected); ok {
					if rejected.RequestID != "denied" || !strings.Contains(rejected.Message, "control denied by test adapter") {
						t.Fatalf("rejected %+v", rejected)
					}
					break
				}
			}
			if harness == core.HarnessKindCodex {
				requests <- CommandRequestGoalControl{RequestID: "unfinished-resume", Action: core.GoalControlActionResume}
				awaitText("resume-pending")
				requests <- CommandRequestCancel{RequestID: "cancel"}
				for {
					if _, ok := next().(core.RuntimeEventCancelApplied); ok {
						break
					}
				}
			}
			requests <- CommandRequestClose{RequestID: "close"}
			for {
				if _, ok := next().(core.RuntimeEventCloseApplied); ok {
					break
				}
			}
			goalAwaitStopped(t, requests, events, runtime)
		}
	}
}
