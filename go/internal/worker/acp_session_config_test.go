// Port of mj-worker/src/acp/session_config_tests.rs.

// Exercise real ACP startup against a harness that forgets all selectors on
// load. The #[cfg(unix)] tests are in acp_session_config_unix_test.go.

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// acpFeaturesSpec is a launch of command with the defaults these tests'
// LaunchSpec literals share.
func acpFeaturesSpec(command string, args []string, environment map[string]string, cwd string) LaunchSpec {
	if environment == nil {
		environment = map[string]string{}
	}
	return LaunchSpec{
		OperatorAway:         &OperatorAway{},
		GoalRecovery:         NewShared(core.GoalRecoveryContext{}),
		Command:              command,
		Args:                 args,
		Environment:          environment,
		Cwd:                  cwd,
		SubagentPolicy:       core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		NativeSessionHistory: NativeSessionHistoryLoadBeforeReplace,
		AcceptedConfig:       NewShared(core.AcceptedSessionConfig{}),
		Harness:              core.HarnessKindKimi,
		ExecutionPolicy:      core.ExecutionPolicyConfiguredApprovals,
		ApprovalChoices:      core.DefaultApprovalChoices(),
		ACPActivity:          core.NewAcpActivityClock(),
		StepClock:            &core.StepClock{},
		ToolsInFlight:        &core.ToolsInFlight{},
		TurnContext:          &transcript.TurnContext{},
		Verdict:              VerdictSourceDisabled{},
	}
}

// acpFeaturesText is a text content block.
func acpFeaturesText(text string) schema.ContentBlock {
	return schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}
}

// acpFeaturesRuntime is a running ACP runtime: the command sender, the
// event receiver, and the runtime's result.
type acpFeaturesRuntime struct {
	commands chan CommandRequest
	events   chan core.RuntimeEvent
	done     chan error
}

// acpFeaturesStart starts spec as Rust spawns `run(spec, requests,
// events)`, with an 8-command and 64-event channel.
func acpFeaturesStart(spec LaunchSpec) *acpFeaturesRuntime {
	runtime := &acpFeaturesRuntime{
		commands: make(chan CommandRequest, 8),
		events:   make(chan core.RuntimeEvent, 64),
		done:     make(chan error, 1),
	}
	runtime.startWith(spec)
	return runtime
}

func (r *acpFeaturesRuntime) startWith(spec LaunchSpec) {
	go func() { r.done <- Run(spec, r.commands, r.events) }()
}

// stop closes the command channel and waits for the runtime to end
// cleanly.
func (r *acpFeaturesRuntime) stop(t *testing.T) {
	t.Helper()
	close(r.commands)
	r.await(t)
}

func (r *acpFeaturesRuntime) await(t *testing.T) {
	t.Helper()
	select {
	case err := <-r.done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("the runtime did not stop")
	}
}

func sessionConfigResetOnLoadHarness(t *testing.T, root string) string {
	return sessionConfigScript(t, root, "reset_config.py", `
import json, os, sys
model, effort, report_raw_model = 'default', 'low', False
def options():
    efforts = ['medium', 'high'] if model == 'chosen' else ['low']
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model + '-transcript-id' if report_raw_model else model,'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in efforts]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options(),
                  'modes':{'currentModeId':'default','availableModes':[
                      {'id':'default','name':'Default'},{'id':'auto','name':'Auto'}]}}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'model_id':
            assert value in ['default','chosen'], 'a transcript id is not a selectable model'
            model = value
            report_raw_model = False
            effort = 'high' if model == 'chosen' else 'low'
        elif key == 'thinking':
            assert model == 'chosen', 'effort was applied before model'
            assert value in ['medium','high']
            effort = value
            report_raw_model = 'raw' in sys.argv[1:]
        else: raise AssertionError(key)
        result = {'configOptions':options()}
    elif method == 'session/prompt':
        text = params['prompt'][0]['text']
        if text == 'restart': os._exit(0)
        assert (model,effort) == ('chosen','medium'), (model,effort)
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`)
}

// sessionConfigDroppedModelHarness is a harness whose model catalogue never
// contains the value this session saved, and which rejects one model it
// does list.
func sessionConfigDroppedModelHarness(t *testing.T, root string) string {
	return sessionConfigScript(t, root, "dropped_model.py", `
import json, sys
MODELS = ['default', 'chosen', 'broken']
model, effort = 'default', 'low'
def efforts():
    return ['medium', 'high'] if model == 'chosen' else ['low']
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':x,'name':x.title()} for x in MODELS]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in efforts()]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params, error, result = request.get('params', {}), None, {}
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options()}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'model_id' and value == 'broken':
            error = {'code':-32603,'message':'this model is listed but unusable'}
        elif key == 'model_id':
            model = value
            effort = efforts()[0]
            result = {'configOptions':options()}
        elif key == 'thinking':
            assert value in efforts(), value
            effort = value
            result = {'configOptions':options()}
        else: raise AssertionError(key)
    elif method == 'session/prompt':
        # A real harness answers; a turn with no output at all is
        # reported as unanswered (#970).
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    reply = {'jsonrpc':'2.0','id':ident}
    reply['error' if error else 'result'] = error or result
    print(json.dumps(reply), flush=True)
`)
}

// sessionConfigStaleAnswerHarness answers a successful model change with the
// configuration it held before the change, the way Codex and Kimi do, and
// caps an effort request to a lower value it reports straight away.
func sessionConfigStaleAnswerHarness(t *testing.T, root string) string {
	return sessionConfigScript(t, root, "stale_answer.py", `
import json, sys
model, effort = 'default', 'low'
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in ['low','high','max']]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options()}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'model_id':
            # Applied, but answered with the configuration from before it.
            answer = options()
            model = value
        elif key == 'thinking':
            effort = 'high' if value == 'max' else value
            answer = options()
        else: raise AssertionError(key)
        result = {'configOptions':answer}
    elif method == 'session/prompt':
        # A real harness answers; a turn with no output at all is
        # reported as unanswered (#970).
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`)
}

// sessionConfigSilentAnswerHarness applies a selector change and answers
// with no configuration at all, saying nothing about what the session now
// uses.
func sessionConfigSilentAnswerHarness(t *testing.T, root string) string {
	return sessionConfigScript(t, root, "silent_answer.py", `
import json, sys
model, effort = 'default', 'low'
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in ['low','high','max']]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options()}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'model_id': model = value
        elif key == 'thinking': effort = value
        else: raise AssertionError(key)
        # Applied, and answered with nothing.
        result = {'configOptions':[]}
    elif method == 'session/prompt':
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`)
}

func sessionConfigScript(t *testing.T, root, name, source string) string {
	t.Helper()
	script := filepath.Join(root, name)
	if err := os.WriteFile(script, []byte(source), 0o644); err != nil {
		t.Fatal(err)
	}
	return script
}

func sessionConfigLaunch(root, script string, saved core.AcceptedSessionConfig) LaunchSpec {
	spec := acpFeaturesSpec("python3", []string{script}, map[string]string{}, root)
	resume := "native"
	spec.ResumeSession = &resume
	spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
	spec.AcceptedConfig = NewShared(saved)
	spec.Harness = core.HarnessKindKimi
	return spec
}

func sessionConfigString(value string) *string { return &value }

// Muse's review follows the runtime's policy alone. A launch written by an
// older controller for a profile that opted back into review
// (`MUSE_APPROVAL_MODE=promptUnmatched`, 2.60 to 2.71) still runs an
// unconstrained session without permission checks, and guardian Muse always
// reviews.
func TestMuseSessionEnforcementIgnoresALegacyLaunchApprovalMode(t *testing.T) {
	root := os.TempDir()
	spec := sessionConfigLaunch(root, filepath.Join(root, "bridge.py"), core.AcceptedSessionConfig{})
	spec.Harness = core.HarnessKindMuse
	spec.ExecutionPolicy = core.ExecutionPolicyUnconstrained
	spec.Environment["MUSE_APPROVAL_MODE"] = "promptUnmatched"
	enforcement := sessionEnforcement(&spec)
	if enforcement == nil {
		t.Fatal("Muse enforces a policy")
	}
	if mode, _ := enforcement.ACPMode(); mode != "allowAll" {
		t.Fatalf("mode %s", mode)
	}
	if key, value, _ := enforcement.ACPSetting(); key != "auto_review" || value != "off" {
		t.Fatalf("setting %s %s", key, value)
	}

	spec.ExecutionPolicy = core.ExecutionPolicyConfiguredApprovals
	enforcement = sessionEnforcement(&spec)
	if enforcement == nil {
		t.Fatal("guardian enforces a policy")
	}
	if mode, _ := enforcement.ACPMode(); mode != "promptUnmatched" {
		t.Fatalf("mode %s", mode)
	}
	if key, value, _ := enforcement.ACPSetting(); key != "auto_review" || value != "on" {
		t.Fatalf("setting %s %s", key, value)
	}
}

func sessionConfigNext(t *testing.T, events <-chan core.RuntimeEvent) core.RuntimeEvent {
	t.Helper()
	select {
	case event, ok := <-events:
		if !ok {
			t.Fatal("runtime remains alive")
		}
		return event
	case <-time.After(10 * time.Second):
		t.Fatal("the fake adapter must make progress")
		return nil
	}
}

func sessionConfigConfigured(t *testing.T, events <-chan core.RuntimeEvent) {
	t.Helper()
	for {
		switch sessionConfigNext(t, events).(type) {
		case core.RuntimeEventSessionConfigured:
			return
		case core.RuntimeEventStopped:
			t.Fatal("adapter stopped before configuration completed")
		}
	}
}

func sessionConfigPrompt(commands chan<- CommandRequest, text string) {
	commands <- CommandRequestPrompt{RequestID: text, Prompt: []schema.ContentBlock{acpFeaturesText(text)}}
}

func sessionConfigOpenRelay(t *testing.T, journal string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(journal, "0123456789abcdef0123456789abcdef", "test")
	if err != nil {
		t.Fatal(err)
	}
	return relay
}

func sessionConfigObserve(t *testing.T, relay *DurableRelay, observation core.RelayObservation) {
	t.Helper()
	if _, err := relay.RecordObservation(observation); err != nil {
		t.Fatal(err)
	}
}

func sessionConfigAccepted(saved *Shared[core.AcceptedSessionConfig]) core.AcceptedSessionConfig {
	accepted, unlock := saved.Lock()
	defer unlock()
	return *accepted
}

func sessionConfigModel(saved *Shared[core.AcceptedSessionConfig]) string {
	if model := sessionConfigAccepted(saved).Model; model != nil {
		return *model
	}
	return "<none>"
}

// Hard-won: 21f785d: an effort response replaced the accepted picker model with a transcript ID.
func TestAcceptedSelectorsSurviveBridgeAndWorkerRestartsBeforeTheNextPrompt(t *testing.T) {
	root := t.TempDir()
	script := sessionConfigResetOnLoadHarness(t, root)
	journal := filepath.Join(root, "relay")
	relay := sessionConfigOpenRelay(t, journal)
	spec := sessionConfigLaunch(root, script, core.AcceptedSessionConfig{})
	spec.Args = append(spec.Args, "raw")
	saved := spec.AcceptedConfig
	runtime := acpFeaturesStart(spec)
	sessionConfigConfigured(t, runtime.events)
	for index, change := range [][2]string{
		{"model", "default"},
		{"model_id", "chosen"},
		{"effort", "high"},
		{"thinking", "medium"},
	} {
		requestID := fmt.Sprintf("config-%d", index)
		submitRelay(relay, requestID, core.RelayCommandSetConfig{Key: change[0], Value: change[1]})
		claimed, err := relay.ClaimPendingCommands(true)
		if err != nil || len(claimed) != 1 {
			t.Fatalf("claimed %v %v", claimed, err)
		}
		runtime.commands <- CommandRequestSetConfig{RequestID: requestID, Key: change[0], Value: change[1]}
	applied:
		for {
			switch event := sessionConfigNext(t, runtime.events).(type) {
			case core.RuntimeEventConfigApplied:
				// These are the same accepted observations the worker records,
				// including adapter-specific selector ids.
				sessionConfigObserve(t, relay, core.RelayObservationConfigurationUpdated{Key: event.Key, Value: event.Value})
				sessionConfigObserve(t, relay, core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions})
				if _, err := relay.RecordCommandCompleted(event.RequestID, core.RelayCommandOutcomeConfigured{}); err != nil {
					t.Fatal(err)
				}
				break applied
			case core.RuntimeEventCommandRejected:
				t.Fatal(event.Message)
			}
		}
	}
	accepted := sessionConfigAccepted(saved)
	if accepted.Model == nil || *accepted.Model != "chosen" {
		t.Fatalf("accepted %+v", accepted)
	}
	runtime.commands <- CommandRequestSetConfig{RequestID: "rejected", Key: "model", Value: "unavailable"}
	for {
		if rejected, ok := sessionConfigNext(t, runtime.events).(core.RuntimeEventCommandRejected); ok && rejected.RequestID == "rejected" {
			break
		}
	}
	if !reflect.DeepEqual(sessionConfigAccepted(saved), accepted) {
		t.Fatal("a rejected change must not replace accepted choices")
	}

	sessionConfigPrompt(runtime.commands, "restart")
	sessionConfigConfigured(t, runtime.events)
	sessionConfigPrompt(runtime.commands, "check-after-bridge")
	for {
		if finished, ok := sessionConfigNext(t, runtime.events).(core.RuntimeEventPromptFinished); ok && finished.RequestID == "check-after-bridge" {
			break
		}
	}
	runtime.stop(t)
	relay = nil

	relay = sessionConfigOpenRelay(t, journal)
	state := relay.OperationalState()
	restored := core.AcceptedSessionConfigFromConfiguration(state.Config, state.ConfigOptions)
	if !reflect.DeepEqual(restored, accepted) {
		t.Fatalf("worker startup recovers accepted selectors from its journal: %+v", restored)
	}
	next := &acpFeaturesRuntime{
		commands: make(chan CommandRequest, 8),
		events:   make(chan core.RuntimeEvent, 64),
		done:     make(chan error, 1),
	}
	// Queue before startup. The fake fails if this prompt sees profile
	// defaults.
	sessionConfigPrompt(next.commands, "check-after-worker")
	next.startWith(sessionConfigLaunch(root, script, restored))
	ready := false
	for {
		switch sessionConfigNext(t, next.events).(type) {
		case core.RuntimeEventSessionConfigured:
			ready = true
			continue
		case core.RuntimeEventPromptFinished:
			if !ready {
				t.Fatal("the prompt ran before the session was configured")
			}
		case core.RuntimeEventStopped:
			t.Fatal("restored worker must run the queued prompt")
		default:
			continue
		}
		break
	}
	next.stop(t)
}

// Hard-won: f822699: API validation refused a listed saved model and killed the worker before users could change it.
func TestARefusedSavedModelKeepsTheWorkerLiveForModelChanges(t *testing.T) {
	root := t.TempDir()
	spec := sessionConfigLaunch(root, sessionConfigDroppedModelHarness(t, root),
		core.AcceptedSessionConfig{Model: sessionConfigString("broken")})
	saved := spec.AcceptedConfig
	runtime := acpFeaturesStart(spec)
	configured, warned := false, false
	var request core.ElicitationRequest
startup:
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventSessionConfigured:
			if reported := sessionConfigReported(t, event.ConfigOptions, "model"); reported != "default" {
				t.Fatalf("model %s", reported)
			}
			configured = true
		case core.RuntimeEventWarning:
			if !strings.Contains(event.Message, "broken") || !strings.Contains(event.Message, "this model is listed but unusable") {
				t.Fatal(event.Message)
			}
			warned = true
		case core.RuntimeEventElicitationRequested:
			if !configured || !warned {
				t.Fatal("asked before the session was configured and warned")
			}
			request = event.Request
			break startup
		case core.RuntimeEventStopped:
			t.Fatal("a refused model must not stop the worker")
		}
	}
	sessionConfigAnswerRecovery(t, runtime.commands, request.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline})
	runtime.commands <- CommandRequestSetConfig{RequestID: "refused-live", Key: "model", Value: "broken"}
	for {
		event := sessionConfigNext(t, runtime.events)
		if rejected, ok := event.(core.RuntimeEventCommandRejected); ok && rejected.RequestID == "refused-live" {
			if !strings.Contains(rejected.Message, "this model is listed but unusable") {
				t.Fatal(rejected.Message)
			}
			break
		}
		if _, stopped := event.(core.RuntimeEventStopped); stopped {
			t.Fatal("a live model refusal must not stop the worker")
		}
	}
	if model := sessionConfigModel(saved); model != "broken" {
		t.Fatalf("model %s", model)
	}
	options := sessionConfigSet(t, runtime, "model", "chosen")
	if reported := sessionConfigReported(t, options, "model"); reported != "chosen" {
		t.Fatalf("model %s", reported)
	}
	if model := sessionConfigModel(saved); model != "chosen" {
		t.Fatalf("model %s", model)
	}
	sessionConfigPrompt(runtime.commands, "after-model-change")
	for {
		event := sessionConfigNext(t, runtime.events)
		if finished, ok := event.(core.RuntimeEventPromptFinished); ok && finished.RequestID == "after-model-change" {
			break
		}
		if _, stopped := event.(core.RuntimeEventStopped); stopped {
			t.Fatal("the repaired session must run its prompt")
		}
	}
	runtime.stop(t)
}

// sessionConfigRecoveryQuestion drives startup until it raises the recovery
// question, checking on the way that it warned about the dropped value and
// reached a configured session.
func sessionConfigRecoveryQuestion(t *testing.T, events <-chan core.RuntimeEvent) core.ElicitationRequest {
	t.Helper()
	warned, configured := false, false
	for {
		switch event := sessionConfigNext(t, events).(type) {
		case core.RuntimeEventWarning:
			message := event.Message
			if !strings.Contains(message, "withdrawn-model") || !strings.Contains(message, "Could not restore") ||
				!strings.Contains(message, `currently reports model "default"`) || strings.Contains(message, "harness default") {
				t.Fatal(message)
			}
			warned = true
		case core.RuntimeEventSessionConfigured:
			configured = true
		case core.RuntimeEventElicitationRequested:
			if !warned {
				t.Fatal("the dropped value must reach the transcript")
			}
			if !configured {
				t.Fatal("the session must be usable before it is asked")
			}
			return event.Request
		case core.RuntimeEventStopped:
			t.Fatal("startup must survive a withdrawn model")
		}
	}
}

func sessionConfigAnswerRecovery(t *testing.T, commands chan<- CommandRequest, elicitationID string, response core.ElicitationResponse) {
	t.Helper()
	resolved := make(chan error, 1)
	commands <- CommandRequestResolveElicitation{ElicitationID: elicitationID, Response: response, Resolved: resolved}
	select {
	case err := <-resolved:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("the answer was not confirmed")
	}
}

// Hard-won: f822699: A withdrawn saved model left sessions suspended and unreachable instead of using the default.
func TestAWithdrawnSavedModelStartsOnTheDefaultAndItsReplacementSurvivesARestart(t *testing.T) {
	root := t.TempDir()
	script := sessionConfigDroppedModelHarness(t, root)
	journal := filepath.Join(root, "relay")
	relay := sessionConfigOpenRelay(t, journal)
	spec := sessionConfigLaunch(root, script, core.AcceptedSessionConfig{Model: sessionConfigString("withdrawn-model")})
	saved := spec.AcceptedConfig
	runtime := acpFeaturesStart(spec)

	request := sessionConfigRecoveryQuestion(t, runtime.events)
	if request.ID != core.SessionConfigRecoveryID {
		t.Fatalf("question %s", request.ID)
	}
	if len(request.Fields) != 1 {
		t.Fatalf("one dropped selector asks one question: %+v", request.Fields)
	}
	field := request.Fields[0]
	if field.ID != "model" {
		t.Fatalf("field %s", field.ID)
	}
	selectKind, ok := field.Kind.(core.ElicitationFieldKindSingleSelect)
	if !ok {
		t.Fatal("a model choice is a single select")
	}
	var values []string
	for _, option := range selectKind.Options {
		values = append(values, option.Value)
	}
	if !slices.Equal(values, []string{"default", "chosen", "broken"}) {
		t.Fatalf("the question carries what the harness offers now: %v", values)
	}
	if selectKind.Default == nil || *selectKind.Default != "default" {
		t.Fatalf("default %v", selectKind.Default)
	}

	sessionConfigAnswerRecovery(t, runtime.commands, request.ID, core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"model": core.ElicitationValueString("chosen")},
	})
	resolved := false
answered:
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventElicitationResolved:
			if event.ElicitationID != core.SessionConfigRecoveryID || event.Action != "accept" {
				t.Fatalf("resolved %+v", event)
			}
			resolved = true
		case core.RuntimeEventConfigApplied:
			if event.RequestID != "" {
				t.Fatal("a recovered selector completes no relay command")
			}
			if event.Key != "model" || event.Value != "chosen" {
				t.Fatalf("applied %s %s", event.Key, event.Value)
			}
			// The same observations the worker records for a change no relay
			// command asked for.
			sessionConfigObserve(t, relay, core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions})
			sessionConfigObserve(t, relay, core.RelayObservationConfigurationUpdated{Key: event.Key, Value: event.Value})
			break answered
		case core.RuntimeEventStopped:
			t.Fatal("answering must not stop the session")
		}
	}
	if !resolved {
		t.Fatal("the question was not resolved")
	}
	accepted := sessionConfigAccepted(saved)
	if !reflect.DeepEqual(accepted, core.AcceptedSessionConfig{Model: sessionConfigString("chosen"), Effort: sessionConfigString("medium")}) {
		t.Fatalf("the chosen model brings its own effort catalogue with it: %+v", accepted)
	}
	runtime.stop(t)

	relay = sessionConfigOpenRelay(t, journal)
	state := relay.OperationalState()
	restored := core.AcceptedSessionConfigFromConfiguration(state.Config, state.ConfigOptions)
	if !reflect.DeepEqual(restored, accepted) {
		t.Fatalf("the answer must outlive the worker: %+v", restored)
	}

	next := &acpFeaturesRuntime{
		commands: make(chan CommandRequest, 8),
		events:   make(chan core.RuntimeEvent, 64),
		done:     make(chan error, 1),
	}
	sessionConfigPrompt(next.commands, "after-recovery")
	next.startWith(sessionConfigLaunch(root, script, restored))
	for {
		event := sessionConfigNext(t, next.events)
		if _, asked := event.(core.RuntimeEventElicitationRequested); asked {
			t.Fatal("the withdrawn model must not be replayed and asked about again")
		}
		if _, finished := event.(core.RuntimeEventPromptFinished); finished {
			break
		}
		if _, stopped := event.(core.RuntimeEventStopped); stopped {
			t.Fatal("the recovered session must run its prompt")
		}
	}
	next.stop(t)
}

func TestDecliningTheRecoveryQuestionKeepsTheHarnessDefaultAndClearsTheQuestion(t *testing.T) {
	root := t.TempDir()
	spec := sessionConfigLaunch(root, sessionConfigDroppedModelHarness(t, root),
		core.AcceptedSessionConfig{Model: sessionConfigString("withdrawn-model")})
	saved := spec.AcceptedConfig
	runtime := acpFeaturesStart(spec)

	request := sessionConfigRecoveryQuestion(t, runtime.events)
	sessionConfigAnswerRecovery(t, runtime.commands, request.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline})
	resolved := false
	sessionConfigPrompt(runtime.commands, "after-decline")
declined:
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventElicitationResolved:
			if event.ElicitationID != core.SessionConfigRecoveryID || event.Action != "decline" {
				t.Fatalf("resolved %+v", event)
			}
			resolved = true
		case core.RuntimeEventConfigApplied:
			t.Fatal("declining changes nothing")
		case core.RuntimeEventPromptFinished:
			break declined
		case core.RuntimeEventStopped:
			t.Fatal("declining must leave the session usable")
		}
	}
	if !resolved {
		t.Fatal("the question must stop asking once it is answered")
	}
	if model := sessionConfigModel(saved); model != "withdrawn-model" {
		t.Fatalf("declining defers the choice rather than discarding what was stored: %s", model)
	}
	runtime.stop(t)
}

// sessionConfigSet sends one selector change and returns the configuration
// the worker reports with the ConfigApplied event that answers it.
func sessionConfigSet(t *testing.T, runtime *acpFeaturesRuntime, key, value string) []schema.SessionConfigOption {
	t.Helper()
	runtime.commands <- CommandRequestSetConfig{RequestID: key + "-" + value, Key: key, Value: value}
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventConfigApplied:
			return event.ConfigOptions
		case core.RuntimeEventCommandRejected:
			t.Fatal(event.Message)
		case core.RuntimeEventStopped:
			t.Fatal("the session must survive a selector change")
		}
	}
}

func sessionConfigReported(t *testing.T, options []schema.SessionConfigOption, key string) string {
	t.Helper()
	option := core.FindSessionConfigOption(options, key)
	if option == nil {
		t.Fatal("the selector stays advertised")
	}
	if option.Select == nil {
		t.Fatalf("%s is a select", key)
	}
	return string(option.Select.CurrentValue)
}

// Hard-won: 1c3e8e3: Codex and Kimi reported a successful config change while retaining the old value.
func TestAChangeTheHarnessAnswersWithItsOldConfigurationIsReportedAsApplied(t *testing.T) {
	root := t.TempDir()
	runtime := acpFeaturesStart(sessionConfigLaunch(root, sessionConfigStaleAnswerHarness(t, root), core.AcceptedSessionConfig{}))
	sessionConfigConfigured(t, runtime.events)

	options := sessionConfigSet(t, runtime, "model", "chosen")
	if reported := sessionConfigReported(t, options, "model"); reported != "chosen" {
		t.Fatalf("a successful change is reported as applied, not as the value it replaced: %s", reported)
	}

	// The harness answered this one with a value of its own choosing. That
	// is a real mismatch and it has to stay visible.
	options = sessionConfigSet(t, runtime, "effort", "max")
	if reported := sessionConfigReported(t, options, "effort"); reported != "high" {
		t.Fatalf("effort %s", reported)
	}
	runtime.stop(t)
}

// Hard-won: 582d09d: An empty config response erased the accepted selectors after a successful change.
func TestAChangeTheHarnessAnswersWithNoConfigurationKeepsTheSelectorsAndTheNewValue(t *testing.T) {
	root := t.TempDir()
	runtime := acpFeaturesStart(sessionConfigLaunch(root, sessionConfigSilentAnswerHarness(t, root), core.AcceptedSessionConfig{}))
	sessionConfigConfigured(t, runtime.events)

	options := sessionConfigSet(t, runtime, "model", "chosen")
	if reported := sessionConfigReported(t, options, "model"); reported != "chosen" {
		t.Fatalf("an empty answer must not hide the value the call applied: %s", reported)
	}
	if reported := sessionConfigReported(t, options, "effort"); reported != "low" {
		t.Fatalf("an empty answer must not drop the selectors the session still has: %s", reported)
	}

	options = sessionConfigSet(t, runtime, "effort", "max")
	if reported := sessionConfigReported(t, options, "effort"); reported != "max" {
		t.Fatalf("effort %s", reported)
	}
	if reported := sessionConfigReported(t, options, "model"); reported != "chosen" {
		t.Fatalf("model %s", reported)
	}
	runtime.stop(t)
}

func TestPermissionAndPlanConfigurationAreNotRestoredAsSessionSelectors(t *testing.T) {
	saved := core.AcceptedSessionConfigFromConfiguration(map[string]string{
		"mode":               "bypassPermissions",
		"collaboration_mode": "plan",
	}, nil)
	if !reflect.DeepEqual(saved, core.AcceptedSessionConfig{}) {
		t.Fatalf("saved %+v", saved)
	}
}

// Model menus under Claude setup-token auth retain the 1M alias only when
// the native process receives the accepted model before resuming history.
func TestClaudeResumePinsTheSavedModelBeforeCatalogueAndQueuedPrompt(t *testing.T) {
	root := t.TempDir()
	script := sessionConfigResetOnLoadHarness(t, root)
	source, err := os.ReadFile(script)
	if err != nil {
		t.Fatal(err)
	}
	rewritten := strings.ReplaceAll(string(source), "'chosen'", "'opus[1m]'")
	rewritten = strings.ReplaceAll(rewritten, "model, effort = 'default', 'low'", "model, effort = 'default', 'low'\npinned = False")
	rewritten = strings.ReplaceAll(rewritten, "['default','opus[1m]']", "(['default','opus[1m]'] if pinned else ['default','opus'])")
	rewritten = strings.ReplaceAll(rewritten, "('session/new','session/load'):",
		"('session/new','session/load'):\n        pinned = params.get('_meta',{}).get('claudeCode',{}).get('options',{}).get('model') == 'opus[1m]'")
	if err := os.WriteFile(script, []byte(rewritten), 0o644); err != nil {
		t.Fatal(err)
	}
	spec := sessionConfigLaunch(root, script, core.AcceptedSessionConfig{
		Model:  sessionConfigString("opus[1m]"),
		Effort: sessionConfigString("medium"),
	})
	spec.Harness = core.HarnessKindClaude
	runtime := &acpFeaturesRuntime{
		commands: make(chan CommandRequest, 8),
		events:   make(chan core.RuntimeEvent, 64),
		done:     make(chan error, 1),
	}
	sessionConfigPrompt(runtime.commands, "queued-before-startup")
	runtime.startWith(spec)
	for turn := 0; turn < 3; turn++ {
		ready := false
	turns:
		for {
			switch event := sessionConfigNext(t, runtime.events).(type) {
			case core.RuntimeEventSessionConfigured:
				if reported := sessionConfigReported(t, event.ConfigOptions, "model"); reported != "opus[1m]" {
					t.Fatalf("model %s", reported)
				}
				ready = true
				if turn > 0 {
					sessionConfigPrompt(runtime.commands, "after-restart")
				}
			case core.RuntimeEventPromptFinished:
				if event.StopReason == "EndTurn" {
					if !ready {
						t.Fatal("prompts wait for restored model and effort")
					}
					break turns
				}
			case core.RuntimeEventElicitationRequested:
				t.Fatalf("unexpected recovery: %+v", event.Request)
			case core.RuntimeEventStopped:
				t.Fatal("Claude startup failed")
			}
		}
		if turn < 2 {
			sessionConfigPrompt(runtime.commands, "restart")
		}
	}
	runtime.stop(t)
}

func TestClaudeStartupErrorsAreNotReportedAsModelReplacement(t *testing.T) {
	for _, cause := range []string{"authentication expired", "requested startup model is unavailable"} {
		root := t.TempDir()
		script := sessionConfigDroppedModelHarness(t, root)
		source, err := os.ReadFile(script)
		if err != nil {
			t.Fatal(err)
		}
		rewritten := strings.Replace(string(source),
			"result = {'sessionId':'native','configOptions':options()}",
			fmt.Sprintf("error = {'code':-32603,'message':%s}", sessionConfigDebugString(cause)), 1)
		if err := os.WriteFile(script, []byte(rewritten), 0o644); err != nil {
			t.Fatal(err)
		}
		spec := sessionConfigLaunch(root, script, core.AcceptedSessionConfig{Model: sessionConfigString("opus[1m]")})
		spec.Harness = core.HarnessKindClaude
		commands := make(chan CommandRequest, 8)
		events := make(chan core.RuntimeEvent, 64)
		sessionConfigPrompt(commands, "must-not-run")
		done := make(chan error, 1)
		go func() { done <- Run(spec, commands, events) }()
		var runErr error
		select {
		case runErr = <-done:
		case <-time.After(10 * time.Second):
			t.Fatal("startup did not fail")
		}
		if runErr == nil || !strings.Contains(runErr.Error(), cause) {
			t.Fatalf("error %v", runErr)
		}
		for len(events) > 0 {
			switch event := (<-events).(type) {
			case core.RuntimeEventSessionConfigured, core.RuntimeEventElicitationRequested, core.RuntimeEventPromptFinished:
				t.Fatalf("event %#v", event)
			}
		}
	}
}

// sessionConfigClaudeAliasHarness is a harness shaped like claude-agent-acp
// 0.81.0: it advertises short model values with display names, and resolves
// a full model id sent to `session/set_config_option` to the advertised
// value that runs it, the way the bridge's `resolveModelPreference` does.
// Its fuzzy last resort places text it cannot resolve on the catch-all
// `default` entry, as R4 saw for `not-a-model` (the title became "default ·
// high"). Anything else is refused with the bridge's own error text. Every
// requested value is appended to `claude_alias.log`.
func sessionConfigClaudeAliasHarness(t *testing.T, root string) string {
	return sessionConfigScript(t, root, "claude_alias.py", `
import json, os, sys
log = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'claude_alias.log')
model = 'default'
choices = [('default','Default (recommended)'),('opus[1m]','Opus 5.5 (1M context)'),('sonnet','Sonnet 5')]
resolved = {'claude-opus-5-5':'opus[1m]','claude-opus-5-5[1m]':'opus[1m]','claude-sonnet-5':'sonnet',
            'not-a-model':'default'}
def options():
    return [{'id':'model','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':v,'name':n} for v,n in choices]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    error = None
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load','session/resume'):
        result = {'sessionId':'native','configOptions':options(),
                  'modes':{'currentModeId':'default','availableModes':[
                      {'id':'default','name':'Default'},{'id':'auto','name':'Auto'}]}}
    elif method == 'session/set_config_option':
        value = params['value']
        with open(log, 'a') as output:
            output.write(value + '\n')
        value = resolved.get(value, value)
        if value in [v for v,_ in choices]:
            model = value
            result = {'configOptions':options()}
        else:
            result = None
            error = {'code':-32603,'message':'Invalid value for config option model: ' + params['value']}
    else: result = {}
    reply = {'jsonrpc':'2.0','id':ident}
    reply['error' if error else 'result'] = error or result
    print(json.dumps(reply), flush=True)
`)
}

// sessionConfigSetModelOutcome sends a model change and returns the value
// applied with the configuration, or the refusal.
func sessionConfigSetModelOutcome(t *testing.T, runtime *acpFeaturesRuntime, value string) (string, []schema.SessionConfigOption, *string) {
	t.Helper()
	runtime.commands <- CommandRequestSetConfig{RequestID: "model-" + value, Key: "model", Value: value}
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventConfigApplied:
			return event.Value, event.ConfigOptions, nil
		case core.RuntimeEventCommandRejected:
			message := event.Message
			return "", nil, &message
		case core.RuntimeEventStopped:
			t.Fatal("the session must survive a selector change")
		}
	}
}

// Hard-won: bc7495e: Claude rejected a valid full model id and gave no useful accepted values.
func TestClaudeModelAcceptsFullModelIDsAndDisplayNamesAndListsValuesOnRefusal(t *testing.T) {
	root := t.TempDir()
	spec := sessionConfigLaunch(root, sessionConfigClaudeAliasHarness(t, root),
		core.AcceptedSessionConfig{Model: sessionConfigString("claude-opus-5-5")})
	spec.Harness = core.HarnessKindClaude
	accepted := spec.AcceptedConfig
	runtime := acpFeaturesStart(spec)
	journal := filepath.Join(root, "relay")
	relay := sessionConfigOpenRelay(t, journal)
	ready := false
normalized:
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventSessionConfigured:
			ready = true
		case core.RuntimeEventConfigApplied:
			if event.RequestID != "" {
				continue
			}
			if !ready {
				t.Fatal("normalization must not publish readiness before startup completes")
			}
			if event.Key != "model" || event.Value != "opus[1m]" {
				t.Fatalf("applied %s %s", event.Key, event.Value)
			}
			sessionConfigObserve(t, relay, core.RelayObservationSessionConfigured{ConfigOptions: event.ConfigOptions})
			sessionConfigObserve(t, relay, core.RelayObservationConfigurationUpdated{Key: event.Key, Value: event.Value})
			break normalized
		case core.RuntimeEventStopped:
			t.Fatal("restoring a full model ID must stay usable")
		}
	}
	if model := sessionConfigModel(accepted); model != "opus[1m]" {
		t.Fatalf("model %s", model)
	}
	relay = sessionConfigOpenRelay(t, journal)
	state := relay.OperationalState()
	if model := core.AcceptedSessionConfigFromConfiguration(state.Config, state.ConfigOptions).Model; model == nil || *model != "opus[1m]" {
		t.Fatalf("journalled model %v", model)
	}
	relay = nil

	// I1-3: the id commit 44957c9f says can be selected.
	value, options, refusal := sessionConfigSetModelOutcome(t, runtime, "claude-opus-5-5")
	if refusal != nil {
		t.Fatalf("the bridge resolves a full model id: %s", *refusal)
	}
	if value != "opus[1m]" {
		t.Fatalf("the applied value is the advertised one: %s", value)
	}
	if reported := sessionConfigReported(t, options, "model"); reported != "opus[1m]" {
		t.Fatalf("model %s", reported)
	}
	if model := sessionConfigModel(accepted); model != "opus[1m]" {
		t.Fatalf("restarts replay the advertised value, not the alias: %s", model)
	}

	// The display name the picker shows selects its value.
	value, _, refusal = sessionConfigSetModelOutcome(t, runtime, "sonnet 5")
	if refusal != nil || value != "sonnet" {
		t.Fatalf("a display name selects its value: %s %v", value, refusal)
	}

	_, _, refusal = sessionConfigSetModelOutcome(t, runtime, "gpt-9")
	if refusal == nil {
		t.Fatal("an unknown model is refused")
	}
	if !strings.Contains(*refusal, `"default"`) || !strings.Contains(*refusal, `"opus[1m]"`) || !strings.Contains(*refusal, `"sonnet"`) {
		t.Fatalf("the refusal lists the accepted values: %s", *refusal)
	}
	runtime.stop(t)
}

// R4-1: `/model not-a-model` answered "model set to not-a-model" and the
// session silently ran the default model, because the bridge places text it
// cannot resolve on `default`. The value is refused as before bc7495e4 and
// the model the session had stays selected; full ids still resolve.
//
// Hard-won: 9fb470e: The worker reported a Claude model selection that the bridge silently placed on its default.
func TestAClaudeModelTheBridgeCannotPlaceIsRefusedAndTheModelKept(t *testing.T) {
	root := t.TempDir()
	spec := sessionConfigLaunch(root, sessionConfigClaudeAliasHarness(t, root), core.AcceptedSessionConfig{})
	spec.Harness = core.HarnessKindClaude
	accepted := spec.AcceptedConfig
	runtime := acpFeaturesStart(spec)
	sessionConfigConfigured(t, runtime.events)

	value, _, refusal := sessionConfigSetModelOutcome(t, runtime, "claude-opus-5-5")
	if refusal != nil || value != "opus[1m]" {
		t.Fatalf("the bridge resolves a full model id: %s %v", value, refusal)
	}

	_, _, refusal = sessionConfigSetModelOutcome(t, runtime, "not-a-model")
	if refusal == nil {
		t.Fatal("a value the bridge can only place on its default is refused")
	}
	if !strings.Contains(*refusal, `"not-a-model" is not an available model value`) || !strings.Contains(*refusal, `"opus[1m]"`) {
		t.Fatal(*refusal)
	}
	if model := sessionConfigModel(accepted); model != "opus[1m]" {
		t.Fatalf("restarts keep replaying the model the session had: %s", model)
	}
	requested, err := os.ReadFile(filepath.Join(root, "claude_alias.log"))
	if err != nil {
		t.Fatal(err)
	}
	if lines := strings.Split(strings.TrimSuffix(string(requested), "\n"), "\n"); !slices.Equal(lines, []string{"claude-opus-5-5", "not-a-model", "opus[1m]"}) {
		t.Fatalf("the previous model is selected again on the bridge: %v", lines)
	}

	// Asking for the default by name is still a model change.
	value, _, refusal = sessionConfigSetModelOutcome(t, runtime, "default")
	if refusal != nil || value != "default" {
		t.Fatalf("the default is a listed value: %s %v", value, refusal)
	}
	runtime.stop(t)
}

func TestAnUnlistedValueIsRefusedWithTheAcceptedValues(t *testing.T) {
	root := t.TempDir()
	runtime := acpFeaturesStart(sessionConfigLaunch(root, sessionConfigStaleAnswerHarness(t, root), core.AcceptedSessionConfig{}))
	sessionConfigConfigured(t, runtime.events)
	_, _, refusal := sessionConfigSetModelOutcome(t, runtime, "claude-opus-5-5")
	if refusal == nil {
		t.Fatal("harnesses without id resolution only take listed values")
	}
	if !strings.Contains(*refusal, `"default"`) || !strings.Contains(*refusal, `"chosen"`) {
		t.Fatal(*refusal)
	}
	runtime.stop(t)
}

// sessionConfigClaudeModeHarness is a harness shaped like claude-agent-acp
// 0.81.0 around Auto mode, for a session whose recorded native session is
// gone. `session/load` reports the session missing, so the worker opens a
// new one (R7-5). On `session/new` Claude Code starts on the model the
// request names, but the adapter reports its default model until one is
// selected (`getAvailableModels` reads only the environment, settings and a
// resumed transcript). A request for Auto is then judged against the
// reported model: the adapter maps it to Accept edits with its notice when
// that model has no Auto mode (`SessionModeManager.selectMode`), and
// otherwise passes it to Claude Code, which refuses it for haiku. A model
// change away from Auto mode does the same mapping (`reconcileForModel`).
// With refuseAuto set, every request for Auto is refused.
func sessionConfigClaudeAutoModeHarness(t *testing.T, root string, refuseAuto bool) string {
	return sessionConfigClaudeModeHarness(t, root, refuseAuto, false)
}

func sessionConfigClaudeModeHarness(t *testing.T, root string, refuseAuto, resumePlan bool) string {
	python := func(value bool) string {
		if value {
			return "True"
		}
		return "False"
	}
	source := strings.ReplaceAll(sessionConfigClaudeModeSource, "__REFUSE_AUTO__", python(refuseAuto))
	source = strings.ReplaceAll(source, "__RESUME_PLAN__", python(resumePlan))
	return sessionConfigScript(t, root, "claude_auto_mode.py", source)
}

const sessionConfigClaudeModeSource = `
import json, sys
REFUSE_AUTO = __REFUSE_AUTO__
RESUME_PLAN = __RESUME_PLAN__
NO_AUTO = ['haiku']
session, running, model, mode = 'replacement', 'default', 'default', 'default'
NOTICE = '**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.'
def options():
    return [
      {'id':'mode','name':'Mode','category':'mode','type':'select','currentValue':mode,
       'options':[{'value':x,'name':x} for x in ['default','acceptEdits','plan','auto']]},
      {'id':'model','name':'Model','category':'model','type':'select','currentValue':model,
       'options':[{'value':x,'name':x} for x in ['default','sonnet','haiku']]}]
def modes():
    return {'currentModeId':mode,'availableModes':[
        {'id':x,'name':x} for x in ['default','acceptEdits','plan','auto']]}
def notify(update):
    print(json.dumps({'jsonrpc':'2.0','method':'session/update',
                      'params':{'sessionId':session,'update':update}}), flush=True)
def fall_back_from_auto():
    global mode
    mode = 'acceptEdits'
    notify({'sessionUpdate':'current_mode_update','currentModeId':mode})
    notify({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':NOTICE}})
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params, error, result = request.get('params', {}), None, {}
    if method == 'initialize':
        result = {'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
    elif method == 'session/load':
        if RESUME_PLAN:
            model = running = 'sonnet'
            mode = 'plan'
            result = {'configOptions':options(),'modes':modes()}
        else:
            error = {'code':-32002,'message':'Resource not found: ' + params['sessionId'],
                     'data':{'uri':params['sessionId']}}
    elif method == 'session/new':
        running = params.get('_meta',{}).get('claudeCode',{}).get('options',{}).get('model') or 'default'
        model, mode = 'default', 'default'
        result = {'sessionId':session,'configOptions':options(),'modes':modes()}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'mode' and value == 'auto' and REFUSE_AUTO:
            error = {'code':-32603,'message':'Internal error',
                     'data':{'details':'Cannot set permission mode to auto: this bridge refuses it'}}
        elif key == 'mode' and value == 'auto' and model in NO_AUTO:
            fall_back_from_auto()
        elif key == 'mode' and value == 'auto' and running in NO_AUTO:
            error = {'code':-32603,'message':'Internal error',
                     'data':{'details':'Cannot set permission mode to auto: auto mode unavailable for this model'}}
        elif key == 'mode':
            mode = value
        elif key == 'model':
            model = running = value
            if mode == 'auto' and value in NO_AUTO:
                fall_back_from_auto()
        else: raise AssertionError(key)
        if error is None:
            result = {'configOptions':options()}
    elif method == 'session/prompt':
        notify({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}})
        result = {'stopReason':'end_turn'}
    reply = {'jsonrpc':'2.0','id':ident}
    reply['error' if error else 'result'] = error or result
    print(json.dumps(reply), flush=True)
`

// haikuFallbackResume is what a Claude session whose native session is gone
// reports while it resumes on haiku: every warning, the mode it settled in,
// and whether the queued prompt was answered.
type haikuFallbackResume struct {
	warnings []string
	mode     *string
	answered bool
}

func resumeAMissingHaikuSession(t *testing.T, refuseAuto bool) haikuFallbackResume {
	return resumeHaikuThenSwitch(t, refuseAuto, false)
}

func resumeHaikuThenSwitch(t *testing.T, refuseAuto, switchModel bool) haikuFallbackResume {
	t.Helper()
	root := t.TempDir()
	spec := sessionConfigLaunch(root, sessionConfigClaudeAutoModeHarness(t, root, refuseAuto),
		core.AcceptedSessionConfig{Model: sessionConfigString("haiku")})
	spec.Harness = core.HarnessKindClaude
	spec.ResumeSession = sessionConfigString("missing-session")
	spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
	runtime := &acpFeaturesRuntime{
		commands: make(chan CommandRequest, 8),
		events:   make(chan core.RuntimeEvent, 64),
		done:     make(chan error, 1),
	}
	sessionConfigPrompt(runtime.commands, "queued-before-startup")
	runtime.startWith(spec)
	var resumed haikuFallbackResume
startup:
	for {
		switch event := sessionConfigNext(t, runtime.events).(type) {
		case core.RuntimeEventWarning:
			resumed.warnings = append(resumed.warnings, event.Message)
		case core.RuntimeEventSessionModesConfigured:
			resumed.mode = nil
			if event.Modes != nil {
				resumed.mode = sessionConfigString(string(event.Modes.CurrentModeID))
			}
		case core.RuntimeEventPromptFinished:
			resumed.answered = event.StopReason == "EndTurn"
			break startup
		case core.RuntimeEventStopped:
			break startup
		}
	}
	if switchModel {
		want := "acceptEdits"
		if refuseAuto {
			want = "default"
		}
		if resumed.mode == nil || *resumed.mode != want {
			t.Fatalf("mode %v, want %s", resumed.mode, want)
		}
		runtime.commands <- CommandRequestSetConfig{RequestID: "switch-to-sonnet", Key: "model", Value: "sonnet"}
	switched:
		for {
			switch event := sessionConfigNext(t, runtime.events).(type) {
			case core.RuntimeEventConfigApplied:
				resumed.mode = core.ConfigCurrentValue(event.ConfigOptions, "mode")
				break switched
			case core.RuntimeEventCommandRejected:
				t.Fatal(event.Message)
			case core.RuntimeEventWarning:
				resumed.warnings = append(resumed.warnings, event.Message)
			case core.RuntimeEventStopped:
				t.Fatal("the model change ended the session")
			}
		}
	}
	runtime.stop(t)
	return resumed
}

// Hard-won: aaa4c484: a model switch left the Auto fallback (acceptEdits) in place
func TestSwitchingFromHaikuToSonnetReassertsGuardianAutoMode(t *testing.T) {
	resumed := resumeHaikuThenSwitch(t, false, true)
	if resumed.mode == nil || *resumed.mode != "auto" {
		t.Fatalf("mode %v", resumed.mode)
	}
}

// A bridge that refuses Auto keeps its own mode after a model change too:
// the reassertion warns again instead of ending the session.
//
// Hard-won: 60f44f8: reasserting a refused policy mode after a model change ended the session
func TestAModelChangeKeepsASessionWhoseBridgeRefusesThePolicyMode(t *testing.T) {
	resumed := resumeHaikuThenSwitch(t, true, true)
	if resumed.mode == nil || *resumed.mode != "default" {
		t.Fatalf("mode %v", resumed.mode)
	}
	refusals := 0
	for _, warning := range resumed.warnings {
		if strings.Contains(warning, "this bridge refuses it") {
			refusals++
		}
	}
	if refusals != 2 {
		t.Fatalf("%d refusals: %v", refusals, resumed.warnings)
	}
}

// R8-2: a never-prompted Claude session on haiku could not be resumed. Its
// new native session was asked for Auto while the adapter still described
// its default model, so the adapter passed Auto on and Claude Code refused
// it for haiku; the worker exited on every retry. Selecting the model first
// lets the adapter apply its own Accept edits fallback, as it does on load.
//
// Hard-won: e794b25: Never-prompted Haiku sessions entered a permanent retry loop when the bridge refused Auto.
func TestANewHaikuSessionTakesTheAdaptersAutoFallbackInsteadOfFailing(t *testing.T) {
	resumed := resumeAMissingHaikuSession(t, false)
	if !resumed.answered {
		t.Fatalf("the resumed session must take its queued prompt: %v", resumed.warnings)
	}
	if resumed.mode == nil || *resumed.mode != "acceptEdits" {
		t.Fatalf("mode %v", resumed.mode)
	}
	for _, warning := range resumed.warnings {
		if strings.Contains(warning, "refused") {
			t.Fatalf("%v", resumed.warnings)
		}
	}
}

// R8-2: when the harness refuses the policy's mode for a new session, the
// session keeps the harness's own mode and says so once, rather than the
// worker exiting and the session staying suspended.
//
// Hard-won: e794b25: A refused mode change stopped session startup instead of retaining the harness default.
func TestAModeTheHarnessRefusesForANewSessionLeavesItsDefaultModeAndOneWarning(t *testing.T) {
	resumed := resumeAMissingHaikuSession(t, true)
	if !resumed.answered {
		t.Fatalf("the session must keep serving: %v", resumed.warnings)
	}
	if resumed.mode == nil || *resumed.mode != "default" {
		t.Fatalf("mode %v", resumed.mode)
	}
	var refusals []string
	for _, warning := range resumed.warnings {
		if strings.Contains(warning, "auto") {
			refusals = append(refusals, warning)
		}
	}
	if len(refusals) != 1 {
		t.Fatalf("%v", resumed.warnings)
	}
	if !strings.Contains(refusals[0], "Cannot set permission mode to auto: this bridge refuses it") || !strings.Contains(refusals[0], "default") {
		t.Fatal(refusals[0])
	}
}

func TestResumeAndContextRestorePreserveOnlyActivePlanUnderGuardian(t *testing.T) {
	// A bridge that offers Plan only on its `mode` selector saves it among
	// the selectors, not as the context's mode. A bridge that refuses Auto
	// would leave a restored bypassPermissions behind; the session takes
	// Claude's default mode instead.
	selector := func(value string) [][2]string { return [][2]string{{"mode", value}} }
	for _, tc := range []struct {
		resumePlan     bool
		savedMode      *string
		savedSelectors [][2]string
		refuseAuto     bool
		expected       string
	}{
		{true, nil, nil, false, "plan"},
		{false, sessionConfigString("plan"), nil, false, "plan"},
		{false, nil, selector("plan"), false, "plan"},
		{false, sessionConfigString("acceptEdits"), nil, false, "auto"},
		{false, nil, selector("bypassPermissions"), true, "default"},
	} {
		root := t.TempDir()
		spec := sessionConfigLaunch(root, sessionConfigClaudeModeHarness(t, root, tc.refuseAuto, tc.resumePlan),
			core.AcceptedSessionConfig{Model: sessionConfigString("sonnet")})
		spec.Harness = core.HarnessKindClaude
		if tc.resumePlan {
			spec.ResumeSession = sessionConfigString("native-plan")
		}
		if tc.savedMode != nil || len(tc.savedSelectors) > 0 {
			selectors := tc.savedSelectors
			if selectors == nil {
				selectors = [][2]string{}
			}
			spec.ContextRestore = &ContextReset{RequestID: "restore", Selectors: selectors, Mode: tc.savedMode}
		}
		runtime := acpFeaturesStart(spec)
		for {
			if configured, ok := sessionConfigNext(t, runtime.events).(core.RuntimeEventSessionConfigured); ok {
				if mode := core.ConfigCurrentValue(configured.ConfigOptions, "mode"); mode == nil || *mode != tc.expected {
					t.Fatalf("%+v: mode %v", tc, mode)
				}
				break
			}
		}
		runtime.stop(t)
	}
}
