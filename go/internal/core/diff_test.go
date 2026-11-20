// Port of mj-core/src/diff/tests.rs.

package core

import (
	"encoding/json"
	"fmt"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/schema"
)

func diffTestDiff(oldText *string, newText string) schema.Diff {
	return schema.Diff{Path: "src/main.rs", OldText: oldText, NewText: newText}
}

// The point of the change: what gets stored is the size of the edit, not the
// size of the file.
// Hard-won: b63924a3: real sessions accumulated 561 MB of duplicated edits and exceeded the IPC frame limit
func TestCompactingAnEditCostsTheEditRatherThanTheFile(t *testing.T) {
	var old strings.Builder
	for line := 0; line < 2_000; line++ {
		fmt.Fprintf(&old, "line %d\n", line)
	}
	oldText := old.String()
	newText := strings.Replace(oldText, "line 900\n", "line 900 changed\n", 1)
	compacted := diffTestDiff(&oldText, newText)
	encoded, err := json.Marshal(compacted)
	if err != nil {
		t.Fatal(err)
	}
	original := len(encoded)

	if !CompactDiff(&compacted) {
		t.Fatal("nothing was compacted")
	}

	encoded, err = json.Marshal(compacted)
	if err != nil {
		t.Fatal(err)
	}
	if stored := len(encoded); stored*20 >= original {
		t.Fatalf("storing a one-line edit to a %d-byte diff still cost %d bytes", original, stored)
	}
	if compacted.OldText != nil || compacted.NewText != "" {
		t.Fatalf("file copies kept: %#v", compacted)
	}
	patch := PatchOf(&compacted)
	if patch.Insertions != 1 || patch.Deletions != 1 {
		t.Fatalf("counts = +%d -%d", patch.Insertions, patch.Deletions)
	}
	for _, line := range []string{"-line 900\n", "+line 900 changed\n"} {
		if !strings.Contains(patch.Text, line) {
			t.Fatalf("patch lacks %q:\n%s", line, patch.Text)
		}
	}
	if strings.Contains(patch.Text, "line 100\n") {
		t.Fatalf("kept distant context:\n%s", patch.Text)
	}
	if !strings.HasPrefix(patch.Text, "--- a/src/main.rs\n+++ b/src/main.rs\n") {
		t.Fatalf("patch header:\n%s", patch.Text)
	}
	if patch.Created || patch.Truncated {
		t.Fatalf("patch = %#v", patch)
	}
}

// A patch is proportional to the edit, but an edit can still be enormous.
// The text is capped on hunk boundaries; the counts keep describing the
// whole edit, so the stat stays honest even when the text is short.
func TestAnEnormousEditKeepsWholeHunksAndSaysItDroppedTheRest(t *testing.T) {
	var old strings.Builder
	for line := 0; line < 20_000; line++ {
		fmt.Fprintf(&old, "line %d\n", line)
	}
	oldText := old.String()
	newText := strings.ReplaceAll(oldText, "line ", "changed ")
	huge := diffTestDiff(&oldText, newText)

	if !CompactDiff(&huge) {
		t.Fatal("nothing was compacted")
	}

	patch := PatchOf(&huge)
	if !patch.Truncated || len(patch.Text) > DiffPatchBytes+64 {
		t.Fatalf("truncated = %v, %d bytes", patch.Truncated, len(patch.Text))
	}
	if !strings.HasSuffix(patch.Text, "[mj dropped the remaining hunks]\n") {
		t.Fatal("the patch does not say it dropped hunks")
	}
	if patch.Insertions != 20_000 || patch.Deletions != 20_000 {
		t.Fatalf("counts = +%d -%d", patch.Insertions, patch.Deletions)
	}
	// Whole hunks only: every line is a patch line, never a severed one.
	for _, line := range rustLines(patch.Text)[2:] {
		if strings.HasPrefix(line, "[") {
			break
		}
		if line == "" || !strings.ContainsAny(line[:1], "@+- ") {
			t.Fatalf("not a patch line: %q", line)
		}
	}
}

// Compaction runs on every recorded update, so it has to be idempotent and
// it must not touch what the agent put in `_meta`.
func TestCompactionIsIdempotentAndLeavesOtherMetadataAlone(t *testing.T) {
	oldText := "alpha\n"
	compacted := diffTestDiff(&oldText, "beta\n")
	compacted.Meta = schema.Meta{{Key: "agent.note", Value: "keep me"}}

	if !CompactDiff(&compacted) {
		t.Fatal("nothing was compacted")
	}
	once := compacted
	once.Meta = compacted.Meta.Clone()
	if CompactDiff(&compacted) {
		t.Fatal("compacted twice")
	}

	if !reflect.DeepEqual(compacted, once) {
		t.Fatalf("second compaction changed the diff: %#v", compacted)
	}
	if note, ok := compacted.Meta.Get("agent.note"); !ok || note != "keep me" {
		t.Fatalf("agent note = %v, %v", note, ok)
	}
}
