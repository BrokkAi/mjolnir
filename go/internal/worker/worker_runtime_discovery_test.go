// Port of the tests in mj-worker/src/worker_runtime/discovery.rs.

//go:build unix

package worker

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestDiscoveryCleanupErrorsDoNotReplaceResults(t *testing.T) {
	config := discovered(core.HarnessKindKimi, nil, nil)
	if _, err := preserveDiscovery(config, nil, errors.New("cleanup failure")); err != nil {
		t.Fatal(err)
	}
	_, err := preserveDiscovery(core.ProfileConfig{}, errors.New("original discovery failure"), errors.New("cleanup failure"))
	if err == nil || err.Error() != "original discovery failure" {
		t.Fatalf("err = %v", err)
	}
}

const discoveryTestHarness = `
import json, sys, os, time
model = 'default'
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select', 'currentValue':model,
       'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':'high' if model == 'chosen' else 'low',
       'options':[{'value':x,'name':x} for x in (['high','max'] if model == 'chosen' else ['low'])]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    assert method != 'session/prompt', 'discovery must never spend a turn'
    if ident is None: continue
    if method == 'initialize': result = {'protocolVersion':1}
    elif method == 'session/new': result = {'sessionId':'probe','configOptions':options()}
    elif method == 'session/close' and os.environ.get('HANG_CLOSE'):
        for i in range(300):
            print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'probe','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'x'*1024}}}}), flush=True)
        time.sleep(60)
        continue
    elif method == 'session/set_config_option':
        if os.environ.get('FAIL_CONFIG'):
            print(json.dumps({'jsonrpc':'2.0','id':ident,'error':{'code':-32603,'message':'original model failure'}}), flush=True)
            continue
        assert request['params']['configId'] == 'model_id'
        model = request['params']['value']
        result = {'configOptions':options()}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`

func TestDiscoveryReadsModelSpecificEffortsWithoutSendingAPrompt(t *testing.T) {
	root := t.TempDir()
	script := filepath.Join(root, "harness.py")
	if err := os.WriteFile(script, []byte(discoveryTestHarness), 0o666); err != nil {
		t.Fatal(err)
	}
	launch := discoveryLaunchSpec(core.HarnessKindKimi, core.ExecutionPolicyConfiguredApprovals,
		"python3", []string{script}, map[string]string{}, root)
	ctx := context.Background()
	defaults, err := discoveryProbe(ctx, launch.Clone(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if defaults.Model == nil || *defaults.Model != "default" {
		t.Fatalf("model = %v", defaults.Model)
	}
	if defaults.Efforts[0].Value != "low" {
		t.Fatalf("efforts = %v", defaults.Efforts)
	}
	chosenModel := "chosen"
	chosen, err := discoveryProbe(ctx, launch.Clone(), &chosenModel)
	if err != nil {
		t.Fatal(err)
	}
	if chosen.Model == nil || *chosen.Model != "chosen" {
		t.Fatalf("model = %v", chosen.Model)
	}
	var efforts []string
	for _, choice := range chosen.Efforts {
		efforts = append(efforts, choice.Value)
	}
	if !slices.Equal(efforts, []string{"high", "max"}) {
		t.Fatalf("efforts = %q", efforts)
	}
	hanging := launch.Clone()
	hanging.AcceptedConfig = NewShared(core.AcceptedSessionConfig{})
	hanging.Environment["HANG_CLOSE"] = "1"
	result, err := discoveryWithin(t, 12*time.Second, func() (core.ProfileConfig, error) {
		return probeWithCloseTimeout(ctx, hanging.Clone(), nil, 200*time.Millisecond)
	})
	if err != nil {
		t.Fatal(err)
	}
	if result.Model == nil || *result.Model != "default" {
		t.Fatalf("model = %v", result.Model)
	}
	hanging.Environment["FAIL_CONFIG"] = "1"
	_, err = discoveryWithin(t, 12*time.Second, func() (core.ProfileConfig, error) {
		return probeWithCloseTimeout(ctx, hanging, &chosenModel, 200*time.Millisecond)
	})
	if err == nil || !strings.Contains(err.Error(), "original model failure") {
		t.Fatalf("err = %v", err)
	}
}

// discoveryWithin is Rust's `tokio::time::timeout(limit, future)
// .await.unwrap()`: the call's result, failing the test when it takes
// longer than limit.
func discoveryWithin[T any](t *testing.T, limit time.Duration, call func() (T, error)) (T, error) {
	t.Helper()
	type outcome struct {
		value T
		err   error
	}
	done := make(chan outcome, 1)
	go func() {
		value, err := call()
		done <- outcome{value, err}
	}()
	select {
	case result := <-done:
		return result.value, result.err
	case <-time.After(limit):
		t.Fatalf("timed out after %v", limit)
		panic("unreachable")
	}
}
