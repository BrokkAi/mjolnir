// Port of the tests in mj-review/src/lanes.rs.

package review

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func lanesTestJob() ReviewJob {
	return ReviewJob{
		Task: "Fix the retry behavior",
		UserMessages: []core.UserMessage{
			core.UserMessagePrompt("Keep the public API stable"),
			core.UserMessagePrompt("Fix the retry behavior"),
		},
		Repositories: []RepositoryCapture{{
			Root:         "/workspace/app",
			BaselineTree: "baseline123",
			CaptureTree:  "capture456",
			MergedIn:     []string{},
		}},
		ChangedFiles: "Repository: /workspace/app -- 1 file changed\n  +1 -1 src/lib.rs",
		ChangedLines: 2,
		Declined:     []core.ReviewOverride{},
	}
}

func TestReviewPromptContainsTreeIDsAndDiffCommandWithoutStandingRubric(t *testing.T) {
	job := lanesTestJob()
	prompt := ReviewPrompt(&job)
	for _, wanted := range []string{
		"Inspect the code with git and read-only commands. Do not build, run tests, or modify files.",
		"git -C /workspace/app diff --no-ext-diff --ignore-submodules=none baseline123 capture456",
		"Repository: /workspace/app -- 1 file changed",
		"Keep the public API stable",
		"Fix the retry behavior",
	} {
		if !strings.Contains(prompt, wanted) {
			t.Fatalf("missing %q in:\n%s", wanted, prompt)
		}
	}
	if !strings.HasPrefix(prompt, "## Task") {
		t.Fatal(prompt)
	}
	for _, unwanted := range []string{
		"You are acting as a reviewer for a proposed code change",
		"diff --git",
		"<workspace_diff",
		"merged, pulled or switched to",
	} {
		if strings.Contains(prompt, unwanted) {
			t.Fatalf("unexpected %q in:\n%s", unwanted, prompt)
		}
	}

	merged := lanesTestJob()
	merged.Repositories[0].MergedIn = []string{"upstream1", "upstream2"}
	prompt = ReviewPrompt(&merged)
	if !strings.Contains(prompt, "diff --no-ext-diff --ignore-submodules=none baseline123 capture456`\nThe base already includes the changes of other history merged, pulled or switched to (upstream1, upstream2)") {
		t.Fatal(prompt)
	}

	unattributed := lanesTestJob()
	reason := "the reflog is gone"
	unattributed.Repositories[0].Unattributed = &reason
	prompt = ReviewPrompt(&unattributed)
	if !strings.Contains(prompt, "capture456`\nThe review could not tell the reviewed commits from other history here (the reflog is gone)") {
		t.Fatal(prompt)
	}
	if !strings.Contains(prompt, "confirm with `git -C /workspace/app log`") {
		t.Fatal(prompt)
	}
}

func TestDiffCommandQuotesARootWithSpaces(t *testing.T) {
	job := lanesTestJob()
	job.Repositories[0].Root = "/work space/it's"
	prompt := ReviewPrompt(&job)
	if !strings.Contains(prompt, `git -C '/work space/it'\''s' diff --no-ext-diff`) {
		t.Fatal(prompt)
	}
}

func TestDeclinedFindingsAreListedAfterTheUserMessagesAndNoCorrectivePassExists(t *testing.T) {
	plainJob := lanesTestJob()
	plain := ReviewPrompt(&plainJob)
	if strings.Contains(plain, "corrective pass") || strings.Contains(plain, "declined") {
		t.Fatal(plain)
	}

	job := lanesTestJob()
	job.Declined = []core.ReviewOverride{
		{
			Finding: "[P2] src/lib.rs:7 -- retry loses the request",
			Reason:  "The queue replays it\non reconnect",
		},
		{
			Finding: "[P3] src/main.rs:1 -- missing doc",
			Reason:  "Internal binary",
		},
	}
	prompt := ReviewPrompt(&job)
	if strings.Contains(prompt, "corrective pass") {
		t.Fatal(prompt)
	}
	if !strings.Contains(prompt, "</user_messages>\n\nThe author declined to fix the findings below, with these reasons. Do not report them again, or findings that restate them.\n<declined_findings>\n"+
		"- Finding: [P2] src/lib.rs:7 -- retry loses the request\n  Reason: The queue replays it\n    on reconnect\n"+
		"- Finding: [P3] src/main.rs:1 -- missing doc\n  Reason: Internal binary\n</declined_findings>\n\nRepository contents") {
		t.Fatal(prompt)
	}
}

// The whole prompt, as a reviewer receives it. Without a brief it is what
// reviewers received before briefs existed.
func TestGoldenReviewPromptWithoutAndWithABrief(t *testing.T) {
	job := lanesTestJob()
	core.AssertGolden(t, "testdata", "review-prompt", ReviewPrompt(&job))
	brief := "## Affected callers\n- `src/queue.rs:40` calls `retry` (src/lib.rs:12) and awaits the result\n\n## Paths\n- none\n\n## Value flow\n- none"
	job.Brief = &brief
	core.AssertGolden(t, "testdata", "review-prompt-with-brief", ReviewPrompt(&job))
	blank := "  \n"
	job.Brief = &blank
	withoutBrief := job
	withoutBrief.Brief = nil
	if ReviewPrompt(&job) != ReviewPrompt(&withoutBrief) {
		t.Fatal("a blank brief renders nothing")
	}
}
