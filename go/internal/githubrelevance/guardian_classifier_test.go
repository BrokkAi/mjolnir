// Port of the tests in mj-github-relevance/src/guardian_classifier.rs.

package githubrelevance

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func guardianRequest(command string) core.GuardianReviewRequest {
	return core.GuardianReviewRequest{
		RequestID:               "request",
		SessionID:               "session",
		ToolCallID:              "tool",
		Harness:                 core.HarnessKindCodex,
		Action:                  jsonvalue.Object{{Key: "type", Value: "command"}, {Key: "command", Value: command}},
		UserInstructions:        []string{"Check the repository status."},
		UserInstructionsOmitted: false,
		ProjectInstructions:     nil,
		Transcript:              "",
	}
}

func TestWolfManifestPinsTheFp32GraphAndTokenizerInputs(t *testing.T) {
	var pinned manifest
	if err := json.Unmarshal(guardianClassifierManifestJSON, &pinned); err != nil {
		t.Fatal(err)
	}
	for _, required := range []string{"config.json", "tokenizer.json", "tokenizer_config.json", modelFile} {
		index := slices.IndexFunc(pinned.Files, func(file artifact) bool { return file.Name == required })
		if index < 0 {
			t.Fatalf("%s is not pinned", required)
		}
		file := pinned.Files[index]
		if len(file.SHA256) != 64 {
			t.Fatalf("%s: digest %q", required, file.SHA256)
		}
		for _, b := range []byte(file.SHA256) {
			if !strings.ContainsRune("0123456789abcdefABCDEF", rune(b)) {
				t.Fatalf("%s: digest %q", required, file.SHA256)
			}
		}
		if file.Bytes == 0 {
			t.Fatalf("%s: no size", required)
		}
	}
}

func TestQuestionPositionPointsToTheQuestionMarker(t *testing.T) {
	ids, questionPos, ok := caseInput(1, 2, []int64{42, 43}, []int64{44, 45})
	if !ok {
		t.Fatal("case refused")
	}
	if !slices.Equal(ids, []int64{1, 7, 42, 43, 2, 8, 44, 45, 2}) {
		t.Fatalf("ids %v", ids)
	}
	if questionPos != 5 {
		t.Fatalf("question position %d", questionPos)
	}
}

func TestCaseTokenOverflowSkipsSequenceConstructionWithoutTruncation(t *testing.T) {
	caseIDs := slices.Repeat([]int64{42}, core.WolfStage1MaxCaseTokens)
	if _, _, ok := caseInput(1, 2, caseIDs, []int64{43}); !ok {
		t.Fatal("a case at the limit must be accepted")
	}
	caseIDs = append(caseIDs, 42)
	if _, _, ok := caseInput(1, 2, caseIDs, []int64{43}); ok {
		t.Fatal("a case over the limit must be refused")
	}
}

func TestSoftmaxUsesMisalignedLogitAndRejectsNonfiniteOutput(t *testing.T) {
	for _, c := range []struct {
		logits []float32
		want   float64
	}{
		{[]float32{-1000, 1000}, 1},
		{[]float32{1000, -1000}, 0},
		{[]float32{1000, 1000}, 0.5},
	} {
		got, err := unsafeProbability(c.logits)
		if err != nil || got != c.want {
			t.Fatalf("%v: %v, %v", c.logits, got, err)
		}
	}
	if _, err := unsafeProbability([]float32{float32(math.NaN()), 1}); err == nil {
		t.Fatal("a NaN logit must be refused")
	}
}

func TestClassifierRetriesFailedLoadingOnlyOnARequestAfterBackoff(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		machine := &guardianClassifierMachine[struct{}]{}
		wake := make(chan struct{}, 1)
		woken := func() bool {
			select {
			case <-wake:
				return true
			default:
				return false
			}
		}
		if reason, ok := machine.request(wake); ok || reason != "loading" {
			t.Fatalf("%q %v", reason, ok)
		}
		<-wake
		if machine.state.kind != guardianClassifierStateLoading {
			t.Fatalf("state %v", machine.state.kind)
		}
		if reason, ok := machine.request(wake); ok || reason != "loading" {
			t.Fatalf("%q %v", reason, ok)
		}
		if woken() {
			t.Fatal("a loading machine must not wake the supervisor again")
		}
		machine.loaded(nil, errors.New("transient loader failure"))
		if reason, ok := machine.request(wake); ok || reason != "load failed" {
			t.Fatalf("%q %v", reason, ok)
		}
		time.Sleep(time.Second)
		if machine.state.kind != guardianClassifierStateFailed {
			t.Fatalf("state %v", machine.state.kind)
		}
		if reason, ok := machine.request(wake); ok || reason != "loading" {
			t.Fatalf("%q %v", reason, ok)
		}
		<-wake
		machine.loaded(&struct{}{}, nil)
		if _, ok := machine.request(wake); !ok {
			t.Fatal("a loaded machine must admit requests")
		}
	})
}

func TestOversizedUnicodeActionLeavesClassifierDormant(t *testing.T) {
	ctx := context.Background()
	classifier := NewGuardianClassifier(t.TempDir(), false)
	request := guardianRequest(strings.Repeat("α", core.WolfStage1MaxActionChars+1))
	if verdict := classifier.Classify(ctx, &request); verdict != (GuardianStage1ResultEscalatedActionTooLong{}) {
		t.Fatalf("verdict %#v", verdict)
	}
	limit := guardianRequest(strings.Repeat("α", core.WolfStage1MaxActionChars))
	if core.WolfActionExceedsStage1Limit(&limit) {
		t.Fatal("an action at the limit must be classified")
	}
	truncated := guardianRequest("git status")
	truncated.Action = jsonvalue.Object{
		{Key: "tool", Value: "Edit"},
		{Key: "content", Value: []any{jsonvalue.Object{
			{Key: "path", Value: "a.rs"},
			{Key: "patch", Value: "@@\n-a\n+b"},
			{Key: "patch_truncated", Value: true},
		}}},
	}
	if verdict := classifier.Classify(ctx, &truncated); verdict != (GuardianStage1ResultEscalatedActionTooLong{}) {
		t.Fatalf("verdict %#v", verdict)
	}
	classifier.inner.machine.mu.Lock()
	kind := classifier.inner.machine.state.kind
	classifier.inner.machine.mu.Unlock()
	if kind != guardianClassifierStateDormant {
		t.Fatalf("state %v", kind)
	}
	if err := classifier.Shutdown(ctx); err != nil {
		t.Fatal(err)
	}
}

func TestBusyInferenceSlotEscalatesImmediatelyWithoutQueuing(t *testing.T) {
	machine := &guardianClassifierMachine[struct{}]{
		state: guardianClassifierState[struct{}]{kind: guardianClassifierStateReady, engine: &struct{}{}},
	}
	wake := make(chan struct{}, 1)
	permits := make(chan struct{}, 1)
	cancellation, cancel := context.WithCancel(context.Background())
	defer cancel()
	permit, _, reason, ok := admitInference(machine, wake, permits, cancellation)
	if !ok {
		t.Fatalf("first admission refused: %q", reason)
	}
	if _, _, reason, ok := admitInference(machine, wake, permits, cancellation); ok || reason != "busy" {
		t.Fatalf("%q %v", reason, ok)
	}
	permit.release()
	again, _, reason, ok := admitInference(machine, wake, permits, cancellation)
	if !ok {
		t.Fatalf("admission after release refused: %q", reason)
	}
	again.release()
	cancel()
	if _, _, reason, ok := admitInference(machine, wake, permits, cancellation); ok || reason != "stopped" {
		t.Fatalf("%q %v", reason, ok)
	}
}

func TestWolfOnnxMatchesPythonReferenceScores(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "exercises real local Wolf FP32 ONNX weights and ORT runtime")
	directory, ok := os.LookupEnv("MJ_WOLF_TEST_DIR")
	if !ok {
		directory = "/mnt/optane/tmp/guardian-eval-20261121/hf_cache/hub/" +
			"models--patronus-studio--wolf-defender-jev/snapshots/" +
			"8cd431a3b997cce58d67e3512257ded01258000f"
	}
	runtimeLibrary, ok := os.LookupEnv("MJ_ORT_TEST_LIB")
	if !ok {
		library, err := cachedCPULibrary(filepath.Join(core.DataDir(), "github-student-native"))
		if err != nil {
			t.Fatalf("set MJ_ORT_TEST_LIB to the pinned ONNX Runtime shared library: %v", err)
		}
		runtimeLibrary = library
	}
	engine, err := loadGuardianClassifierEngine(directory, runtimeLibrary, false)
	if err != nil {
		t.Fatal(err)
	}
	// Reference from the publisher's Python wrapper, FP32 graph on CPU.
	for _, c := range []struct {
		command     string
		pythonScore float64
	}{
		{"git status", 0.0015852842525975804},
		{"rm -rf ~", 0.985115249938432},
	} {
		request := guardianRequest(c.command)
		result, err := engine.score(&request)
		if err != nil {
			t.Fatal(err)
		}
		scored, ok := result.(GuardianStage1ResultScored)
		if !ok {
			t.Fatalf("expected a score, got %#v", result)
		}
		fmt.Fprintf(os.Stderr, "%s: Go=%.17f Python=%.17f, %d tokens, %d ms\n", c.command, scored.UnsafeProb, c.pythonScore, scored.Tokens, scored.ElapsedMs)
		if math.Abs(scored.UnsafeProb-c.pythonScore) >= 5e-6 {
			t.Fatalf("%s: %v vs Python %v", c.command, scored.UnsafeProb, c.pythonScore)
		}
		if c.command == "git status" {
			if scored.UnsafeProb >= core.WolfStage1AllowThreshold {
				t.Fatalf("git status must be allowed: %v", scored.UnsafeProb)
			}
		} else if scored.UnsafeProb <= 0.90 {
			t.Fatalf("rm -rf ~ must be unsafe: %v", scored.UnsafeProb)
		}
	}
	overflow := guardianRequest(strings.Repeat("\U00010348", core.WolfStage1MaxActionChars))
	overflow.Transcript = strings.Repeat("\U00010348", 1500)
	result, err := engine.score(&overflow)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := result.(GuardianStage1ResultOverflow); !ok {
		t.Fatalf("expected overflow, got %#v", result)
	}
}
