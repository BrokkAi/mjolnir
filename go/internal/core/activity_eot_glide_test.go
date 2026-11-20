// Port of the tests in mj-core/src/activity/eot_glide.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func eotGlideFreshSample(t *testing.T) []any {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", "activity_eot_glide", "eot_glide_fresh_sample.json"))
	if err != nil {
		t.Fatal(err)
	}
	cases, err := jsonvalue.Parse(data)
	if err != nil {
		t.Fatal(err)
	}
	return cases.([]any)
}

func TestFrozenFreshTestProbabilitiesMatchResearch(t *testing.T) {
	for _, item := range eotGlideFreshSample(t) {
		testCase := item.(jsonvalue.Object)
		expectedValue, _ := testCase.Get("expected")
		expected, _ := expectedValue.(string)
		answers, _ := testCase.Get("answers")
		index, _ := testCase.Get("index")
		if got := Action(answers); got != AssessmentAction(expected) {
			t.Fatalf("%v: action = %q, want %q", index, got, expected)
		}
		hasBackgroundValue, _ := testCase.Get("has_background")
		hasBackground := hasBackgroundValue.(bool)
		wantValue, _ := testCase.Get("background_needed")
		var want *bool
		if wantValue != nil {
			needed := wantValue.(bool)
			want = &needed
		}
		if got := BackgroundNeeded(answers, hasBackground); !reflect.DeepEqual(got, want) {
			t.Fatalf("%v: background needed = %v, want %v", index, got, want)
		}
	}
}

func TestWeakQuestionsOnRequestedAdviceDemoteOnlyWithAnAttentionAnswer(t *testing.T) {
	// E0028 is a recorded needs-input turn; move its question signal between
	// the cut and the demote ceiling.
	var answers map[string]any
	for _, item := range eotGlideFreshSample(t) {
		testCase := item.(jsonvalue.Object)
		if index, _ := testCase.Get("index"); index == "E0028" {
			value, _ := testCase.Get("answers")
			encoded, err := jsonvalue.Marshal(value)
			if err != nil {
				t.Fatal(err)
			}
			if err := json.Unmarshal(encoded, &answers); err != nil {
				t.Fatal(err)
			}
		}
	}
	if answers == nil {
		t.Fatal("E0028 is missing")
	}
	setRequired := func(value float64) {
		answers["input"].(map[string]any)["probabilities"].(map[string]any)["required"] = value
	}
	setRequired(0.3)
	// A proxy that predates the attention question must not demote.
	if got := Action(answers); got != AssessmentActionAwaitInput {
		t.Fatalf("action = %q", got)
	}
	answers["attention"] = map[string]any{"probabilities": map[string]any{
		"needs_input": 0.1, "important": 0.8, "redundant_request": 0.0,
		"none": 0.1, "unclear": 0.0,
	}}
	if got := Action(answers); got == AssessmentActionAwaitInput {
		t.Fatalf("action = %q", got)
	}
	setRequired(0.6)
	if got := Action(answers); got != AssessmentActionAwaitInput {
		t.Fatalf("action = %q", got)
	}
}
