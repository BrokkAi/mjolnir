// Port of the tests in mj-core/src/continuation.rs.

package core

import (
	"fmt"
	"os"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// The mining script keeps its own copy of the rules, so the two sides
// can drift without either failing to compile.
// Hard-won: 7c195cfd8: hashed recovery IDs lost the carried command's identity.
func TestRestartIdentitySurvivesRepeatedRecoveryAndMaximumRootIDs(t *testing.T) {
	root := strings.Repeat("r", 128)
	id := root
	for generation := 1; generation <= 11; generation++ {
		next := RestartContinuationID(id)
		if next != RestartContinuationID(id) {
			t.Fatal("restart id is not deterministic")
		}
		if want := fmt.Sprintf("auto-continue-restart-%d-%s", generation, root); next != want {
			t.Fatalf("next = %q, want %q", next, want)
		}
		if CarriedCommand(next) != root {
			t.Fatalf("carried = %q", CarriedCommand(next))
		}
		if !ValidCommandID(next) {
			t.Fatalf("%q is not valid", next)
		}
		id = next
	}
	if ValidCommandID(strings.Repeat("r", 129)) {
		t.Fatal("129-byte id is valid")
	}
	if ValidCommandID("auto-continue-restart-01-original-turn") {
		t.Fatal("zero-padded generation is valid")
	}
}

func TestJevExtractScriptListsTheSameGeneratedPromptPrefixes(t *testing.T) {
	data, err := os.ReadFile("../../../scripts/jev-scenarios-extract.py")
	if err != nil {
		t.Fatal(err)
	}
	script := string(data)
	quotedList := func(name string) []string {
		_, rest, ok := strings.Cut(script, "\n"+name+" = (")
		if !ok {
			t.Fatalf("%s missing from the script", name)
		}
		body, _, _ := strings.Cut(rest, ")")
		parts := strings.Split(body, `"`)
		var out []string
		for i := 1; i < len(parts); i += 2 {
			out = append(out, parts[i])
		}
		return out
	}
	expected := func(keep func(GeneratedPrompt) bool) []string {
		var out []string
		for _, producer := range GeneratedPromptAll {
			if keep(producer) {
				out = append(out, producer.Prefix())
			}
		}
		return out
	}
	if got, want := quotedList("GENERATED_PROMPT_PREFIXES"), expected(func(GeneratedPrompt) bool { return true }); !reflect.DeepEqual(got, want) {
		t.Fatalf("GENERATED_PROMPT_PREFIXES = %q, want %q", got, want)
	}
	got := quotedList("ORDINAL_PREFIXES")
	want := expected(GeneratedPrompt.IsRetry)
	slices.Sort(got)
	got = slices.Compact(got)
	slices.Sort(want)
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("ORDINAL_PREFIXES = %q, want %q", got, want)
	}
}

func TestParentWaitNoticeIsGeneratedPromptText(t *testing.T) {
	if !IsGeneratedPromptText(ParentWaitPromptText) {
		t.Fatal("notice is not generated")
	}
	if !IsGeneratedPromptText(" " + ParentWaitPromptText + "\n") {
		t.Fatal("padded notice is not generated")
	}
	if IsGeneratedPromptText("One or more sub-agents finished.") {
		t.Fatal("partial notice is generated")
	}
}

func TestIncompleteAndOversizedEvidenceIsRejected(t *testing.T) {
	e := ContinuationEvidence{
		Messages: []EvidenceMessage{
			{ID: "u", Role: "user", Text: "Implement it and test it"},
			{ID: "a", Role: "assistant", Text: "Implemented. Shall I test?"},
		},
	}
	if err := e.Validate(); err != nil {
		t.Fatal(err)
	}
	e.Messages[0].Text = strings.Repeat("x", UserBytes+1)
	if e.Validate() == nil {
		t.Fatal("oversized user text accepted")
	}
	e.Messages = e.Messages[1:]
	if e.Validate() == nil {
		t.Fatal("evidence without a user message accepted")
	}
}
