// Port of the tests in mj-review/src/delta.rs.

package review

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestTreesUnchangedComparesTreeIDsNotTheDiff(t *testing.T) {
	delta := func(baseline, current string) core.RepoDelta {
		return core.RepoDelta{BaselineTree: &baseline, CurrentTree: current}
	}
	if !TreesUnchanged([]core.RepoDelta{delta("tree1", "tree1")}) {
		t.Fatal("equal trees are unchanged")
	}
	// An empty diff with different trees, as `diff.ignoreSubmodules=all`
	// gives for a submodule-only change, still needs a reviewer.
	if TreesUnchanged([]core.RepoDelta{delta("tree1", "tree2")}) {
		t.Fatal("different trees are a change")
	}
	if TreesUnchanged([]core.RepoDelta{{CurrentTree: "tree1"}}) {
		t.Fatal("a delta without a baseline is a change")
	}
}

const deltaTestPatch = "diff --git a/one.rs b/one.rs\n" +
	"--- a/one.rs\n" +
	"+++ b/one.rs\n" +
	"@@ -1,2 +1,3 @@\n" +
	"+added\n" +
	"+added again\n" +
	"-removed\n" +
	"context\n" +
	"diff --git a/two.rs b/two.rs\n" +
	"--- a/two.rs\n" +
	"+++ b/two.rs\n" +
	"@@ -1 +1 @@\n" +
	"+only\n"

func TestHeadersOutsideAHunkAreNotCountedAsChangedLines(t *testing.T) {
	summary := RawDiffSummaryFromPatch("diff --git a/one.rs b/one.rs\n--- a/one.rs\n+++ b/one.rs\n")
	if summary.ChangedLineCount() != 0 {
		t.Fatalf("%+v", summary)
	}
	if summary.Files != 1 {
		t.Fatalf("%+v", summary)
	}
}

func deltaTestDelta(root, patch string) core.RepoDelta {
	summary := RawDiffSummaryFromPatch(patch)
	return core.RepoDelta{
		Root:         root,
		CurrentTree:  "tree",
		Patch:        patch,
		Diffstat:     summary.Diffstat(),
		ChangedLines: summary.ChangedLineCount(),
		Files:        []core.FileLineChange{},
		MergedIn:     []string{},
	}
}

func TestChangedFilesTableListsChangedRepositoriesWithoutThePatch(t *testing.T) {
	app := deltaTestDelta("/w/app", deltaTestPatch)
	oldPath := "src/old.rs"
	app.Files = []core.FileLineChange{
		{Path: "src/lib.rs", Insertions: 12, Deletions: 3},
		{Path: "src/new.rs", OldPath: &oldPath, Insertions: 1, Deletions: 1},
		{Path: "logo.png", Binary: true},
	}

	oldWorker := deltaTestDelta("/w/lib", deltaTestPatch)
	changed := []core.RepoDelta{app, oldWorker, deltaTestDelta("/w/quiet", "")}
	table := ChangedFilesTable(changed)
	lines := strings.Split(table, "\n")
	if !strings.HasPrefix(lines[0], "Repository: /w/app -- ") {
		t.Fatal(table)
	}
	if !strings.HasSuffix(lines[1], "+12      -3  src/lib.rs") {
		t.Fatal(table)
	}
	if !strings.HasSuffix(lines[2], "src/old.rs -> src/new.rs") {
		t.Fatal(table)
	}
	if !strings.Contains(lines[3], "binary  logo.png") {
		t.Fatal(table)
	}
	if !(strings.Contains(table, "Repository: /w/lib -- ") &&
		strings.Contains(table, "did not report per-file counts")) {
		t.Fatal(table)
	}
	if strings.Contains(table, "/w/quiet") {
		t.Fatalf("an unchanged repository is left out:\n%s", table)
	}
	if strings.Contains(table, "+added") {
		t.Fatalf("the change packet does not embed the patch:\n%s", table)
	}
	if got := ChangedLineCount(changed); got != 8 {
		t.Fatalf("changed lines = %d", got)
	}
	if got := ChangedFilesTable([]core.RepoDelta{deltaTestDelta("/w/quiet", "")}); got != "No files changed." {
		t.Fatalf("%q", got)
	}
}
