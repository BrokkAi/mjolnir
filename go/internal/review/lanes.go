// Port of mj-review/src/lanes.rs.
//
// Turn-review prompt construction shared by each independent reviewer.
//
// The reviewer rubric and the brief writer's instructions are
// core.CodexReviewRubric and core.BriefWriterInstructions (Rust re-exports
// them from mj_core::session_instructions). The rubric is adapted from Codex
// commit 1b1835f751, codex-rs/prompts/templates/review/rubric.md; its output
// contract is Mjolnir's plain-text handback format.

package review

import (
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RepositoryCapture is one repository's immutable review target.
type RepositoryCapture struct {
	Root         string
	BaselineTree string
	CaptureTree  string
	// MergedIn is commits of other history the reviewed work merged, pulled
	// or switched to, whose changes the baseline already holds.
	MergedIn []string
	// Unattributed is why the diff may hold changes outside the reviewed
	// commits; nil when it cannot.
	Unattributed *string
}

// ReviewJob is everything about a review pass needed to construct its
// prompt.
type ReviewJob struct {
	// Task is the latest real user prompt; earlier requirements remain in
	// the history.
	Task string
	// UserMessages is all real user messages in chronological order,
	// excluding harness notes.
	UserMessages []core.UserMessage
	// Repositories is the captured tree range in every repository with
	// changes.
	Repositories []RepositoryCapture
	// ChangedFiles is per-file additions and deletions computed from the
	// captured trees.
	ChangedFiles string
	ChangedLines int
	// Declined is findings the author declined to fix, which reviewers must
	// not repeat.
	Declined []core.ReviewOverride
	// Brief is relations a brief writer computed from the repository, if one
	// ran.
	Brief *string
}

// DeclinedFindingsContext is the findings the author declined to fix, with
// the reasons they gave. False when there are none.
func DeclinedFindingsContext(job *ReviewJob) (string, bool) {
	if len(job.Declined) == 0 {
		return "", false
	}
	entries := make([]string, 0, len(job.Declined))
	for _, entry := range job.Declined {
		entries = append(entries, fmt.Sprintf("- Finding: %s\n  Reason: %s",
			indentContinuation(entry.Finding), indentContinuation(entry.Reason)))
	}
	return "The author declined to fix the findings below, with these reasons. Do not report them again, or findings that restate them.\n<declined_findings>\n" +
		strings.Join(entries, "\n") + "\n</declined_findings>", true
}

// briefSection is the writer's relations, labelled so reviewers treat them
// as unverified facts to check rather than findings. False when no writer
// ran or it wrote nothing.
func briefSection(job *ReviewJob) (string, bool) {
	if job.Brief == nil {
		return "", false
	}
	brief := strings.TrimSpace(*job.Brief)
	if brief == "" {
		return "", false
	}
	return "## Relations computed from the repository\n\nThese are relations computed from the repository at this commit, not findings. They may be incomplete.\n\n" + brief, true
}

// indentContinuation keeps a multi-line entry inside its list item.
func indentContinuation(text string) string {
	return strings.ReplaceAll(strings.TrimSpace(text), "\n", "\n    ")
}

// UserMessagesPacket is the chronological user messages, with the message
// that governs the reviewed turn marked so a model does not read an older
// one as current intent.
func UserMessagesPacket(messages []core.UserMessage, currentTask string) string {
	panic("unported: mj-review/src/lanes.rs user_messages_packet")
}

// repositoriesSection is each repository's root and the exact command that
// prints its captured change.
func repositoriesSection(job *ReviewJob) string {
	panic("unported: mj-review/src/lanes.rs repositories_section")
}

// ReviewPrompt builds the common plain-text rubric prompt without embedding
// a diff.
func ReviewPrompt(job *ReviewJob) string {
	panic("unported: mj-review/src/lanes.rs review_prompt")
}

// BriefWriterPrompt is the prompt of the session that writes the relation
// brief. It sees the same diff commands and the same user request as the
// reviewers, and no findings.
func BriefWriterPrompt(job *ReviewJob) string {
	return fmt.Sprintf(
		"%s\n\n## The change\n\nRead each repository's captured change with its exact command:\n\n%s\n\n<user_task>\n%s\n</user_task>",
		core.ReadOnlyTaskScope,
		repositoriesSection(job),
		BoundReviewSection(job.Task, UserMessagesLimit, "task"),
	)
}

// shellQuote quotes a path for the shell only when it needs it, so the
// common case stays the plain command a reviewer would type.
func shellQuote(text string) string {
	plain := text != ""
	for _, c := range text {
		if !(c < 0x80 && (c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.ContainsRune("/._-+:@,=", c))) {
			plain = false
			break
		}
	}
	if plain {
		return text
	}
	return "'" + strings.ReplaceAll(text, "'", `'\''`) + "'"
}

// changedFilesSection is every changed file with line counts computed over
// the whole captured tree change, even if an older worker truncated other
// capture data.
func changedFilesSection(job *ReviewJob) string {
	return fmt.Sprintf(
		"<changed_files source=\"git diff --numstat of the captured trees\" trust=\"deterministic\" changed_lines=\"%d\">\n%s\n</changed_files>",
		job.ChangedLines,
		BoundReviewSection(job.ChangedFiles, ChangedFilesLimit, "changed files"),
	)
}
