// Port of mj-worker/src/acp/session_config_tests.rs: its #[cfg(unix)]
// tests.

//go:build unix

package worker

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// Codex reads its model out of the bridge environment at startup, so the
// pin has to carry the value this session holds at *this* launch. A session
// that accepts its model after the worker started -- which every session
// created with an explicit model does -- resumed every later bridge on the
// profile default while the supervisor spec was written once and never
// updated.
//
// Hard-won: 21f785d: A resumed Codex session reverted an accepted model to the profile default.
func TestACodexBridgeRestartStartsOnAModelAcceptedAfterTheWorkerDid(t *testing.T) {
	root := t.TempDir()
	specPath := filepath.Join(root, "acp-supervisor.json")
	launches := filepath.Join(root, "launched-on.txt")
	script := sessionConfigScript(t, root, "codex_like.py", `
import json, os, sys
spec = json.load(open(sys.argv[1]))
pinned = spec['environment'].get('CODEX_CONFIG')
with open(sys.argv[2], 'a') as log:
    log.write((json.loads(pinned)['model'] if pinned else 'unpinned') + '\n')
model = 'default'
def options():
    return [{'id':'model','name':'Model','category':'model','type':'select',
             'currentValue':model,
             'options':[{'value':x,'name':x} for x in ['default','chosen']]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options(),
                  'modes':{'currentModeId':'agent',
                           'availableModes':[{'id':'agent','name':'Agent'}]}}
    elif method == 'session/set_config_option':
        model = params['value']
        result = {'configOptions':options()}
    elif method == 'session/prompt':
        if params['prompt'][0]['text'] == 'restart': os._exit(0)
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`)
	supervisor := AcpSupervisorSpec{
		Command:             "python3",
		Args:                []string{script},
		Environment:         map[string]string{},
		ExcludedEnvironment: []string{},
		Cwd:                 root,
	}
	if err := supervisor.writeSpec(specPath); err != nil {
		t.Fatal(err)
	}

	spec := sessionConfigLaunch(root, script, core.AcceptedSessionConfig{})
	spec.Harness = core.HarnessKindCodex
	spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
	spec.BridgeSpecPath = &specPath
	spec.Args = []string{script, specPath, launches}
	runtime := acpFeaturesStart(spec)
	sessionConfigConfigured(t, runtime.events)

	runtime.commands <- CommandRequestSetConfig{RequestID: "choose", Key: "model", Value: "chosen"}
	for {
		event := sessionConfigNext(t, runtime.events)
		if applied, ok := event.(core.RuntimeEventConfigApplied); ok && applied.RequestID == "choose" {
			break
		}
		if rejected, ok := event.(core.RuntimeEventCommandRejected); ok {
			t.Fatal(rejected.Message)
		}
	}

	sessionConfigPrompt(runtime.commands, "restart")
	sessionConfigConfigured(t, runtime.events)
	runtime.stop(t)

	launched, err := os.ReadFile(launches)
	if err != nil {
		t.Fatal(err)
	}
	if string(launched) != "unpinned\nchosen\n" {
		t.Fatalf("the replacement bridge must start on the accepted model: %q", launched)
	}
}

// Hard-won: ba25f84: adoption hid a model-recovery form still owned by the command loop.
func TestWorkerOwnedModelRecoverySurvivesAdoptionUntilAnswerOrBridgeExit(t *testing.T) {
	for _, answer := range []bool{true, false} {
		root := t.TempDir()
		relay, err := DurableRelayOpen(filepath.Join(root, "relay"), "model-recovery-cut", "test")
		if err != nil {
			t.Fatal(err)
		}
		spec := sessionConfigLaunch(root, sessionConfigDroppedModelHarness(t, root),
			core.AcceptedSessionConfig{Model: sessionConfigString("withdrawn-model")})
		spec.OperatorAway = relay.operatorAway
		spec.OperatorAway.set(core.OperatorPresence{Away: true, Revision: 1})
		saved := spec.AcceptedConfig
		runtime := acpFeaturesStart(spec)
		request := sessionConfigRecoveryQuestion(t, runtime.events)
		sessionConfigObserve(t, relay, core.RelayObservationElicitationRequested{Request: request})
		cut, err := relay.LiveSessionBaseline()
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
		if answer {
			sessionConfigAnswerRecovery(t, runtime.commands, request.ID, core.ElicitationResponse{
				Action:  core.ElicitationResponseAccept,
				Content: map[string]core.ElicitationValue{"model": core.ElicitationValueString("chosen")},
			})
			if model := sessionConfigModel(saved); model != "chosen" {
				t.Fatalf("model %s", model)
			}
			// The owner retires it before its resolved event needs replaying.
			baseline, err := relay.LiveSessionBaseline()
			if err != nil {
				t.Fatal(err)
			}
			if len(baseline.Questions) != 0 {
				t.Fatalf("questions %+v", baseline.Questions)
			}
		}
		close(runtime.commands)
		drained := make(chan struct{})
		go func() {
			defer close(drained)
			for {
				select {
				case <-runtime.events:
				case <-time.After(time.Second):
					return
				}
			}
		}()
		runtime.await(t)
		<-drained
		baseline, err := relay.LiveSessionBaseline()
		if err != nil {
			t.Fatal(err)
		}
		if len(baseline.Questions) != 0 {
			t.Fatalf("questions %+v", baseline.Questions)
		}
	}
}
