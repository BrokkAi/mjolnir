// Port of mj-review/src/delta.rs.
//
// Describe committed changes captured for push review.

package review

import (
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RawDiffSummary is line and file totals parsed straight from a unified
// diff.
//
// Computed from the untruncated patch so bounding capture data cannot make a
// change look smaller than it is.
type RawDiffSummary struct {
	Files      int
	Insertions int
	Deletions  int
}

// RawDiffSummaryFromPatch counts the files (`diff --git` headers) and the
// added and removed lines inside hunks of patch. A patch with changed lines
// but no file header counts as one file.
func RawDiffSummaryFromPatch(patch string) RawDiffSummary {
	var summary RawDiffSummary
	inHunk := false
	for _, line := range rustLines(patch) {
		switch {
		case strings.HasPrefix(line, "diff --git "):
			summary.Files++
			inHunk = false
		case strings.HasPrefix(line, "@@"):
			inHunk = true
		case inHunk && strings.HasPrefix(line, "+"):
			summary.Insertions++
		case inHunk && strings.HasPrefix(line, "-"):
			summary.Deletions++
		}
	}
	if summary.Files == 0 && summary.ChangedLineCount() > 0 {
		summary.Files = 1
	}
	return summary
}

// rustLines splits text as Rust's str::lines does: on "\n", with one
// trailing "\r" removed from each line and no empty line after a final
// newline.
func rustLines(text string) []string {
	lines := []string{}
	for text != "" {
		line := text
		if index := strings.IndexByte(text, '\n'); index >= 0 {
			line, text = text[:index], text[index+1:]
		} else {
			text = ""
		}
		lines = append(lines, strings.TrimSuffix(line, "\r"))
	}
	return lines
}

// ChangedLineCount is the insertions plus the deletions.
func (s RawDiffSummary) ChangedLineCount() int {
	return s.Insertions + s.Deletions
}

// Diffstat renders the totals as Git's diffstat summary line does, e.g.
// "2 files changed, 3 insertions(+), 1 deletion(-)".
func (s RawDiffSummary) Diffstat() string {
	summary := fmt.Sprintf("%d %s changed", s.Files, plural(s.Files, "file", "files"))
	if s.Insertions > 0 {
		summary += fmt.Sprintf(", %d %s(+)", s.Insertions, plural(s.Insertions, "insertion", "insertions"))
	}
	if s.Deletions > 0 {
		summary += fmt.Sprintf(", %d %s(-)", s.Deletions, plural(s.Deletions, "deletion", "deletions"))
	}
	return summary
}

// plural picks the singular form for exactly one.
func plural(count int, one, many string) string {
	if count == 1 {
		return one
	}
	return many
}

// TreesUnchanged reports whether every repository's captured tree is its
// baseline tree.
func TreesUnchanged(deltas []core.RepoDelta) bool {
	for _, delta := range deltas {
		if delta.BaselineTree == nil || *delta.BaselineTree != delta.CurrentTree {
			return false
		}
	}
	return true
}

// ChangedFilesTable is every changed file with its added and removed line
// counts, one section per repository headed by that repository's totals. Git
// counts the whole change, so the table lists every file even where the
// patch a prompt shows was cut short.
func ChangedFilesTable(deltas []core.RepoDelta) string {
	sections := []string{}
	for _, delta := range deltas {
		if strings.TrimSpace(delta.Patch) == "" {
			continue
		}
		lines := []string{fmt.Sprintf("Repository: %s -- %s", delta.Root, delta.Diffstat)}
		if len(delta.Files) == 0 {
			lines = append(lines, "  (this worker did not report per-file counts)")
		}
		for _, file := range delta.Files {
			var counts string
			if file.Binary {
				counts = fmt.Sprintf("%15s", "binary")
			} else {
				counts = fmt.Sprintf("%7s %7s",
					fmt.Sprintf("+%d", file.Insertions),
					fmt.Sprintf("-%d", file.Deletions))
			}
			path := file.Path
			if file.OldPath != nil {
				path = fmt.Sprintf("%s -> %s", *file.OldPath, file.Path)
			}
			lines = append(lines, fmt.Sprintf("  %s  %s", counts, path))
		}
		sections = append(sections, strings.Join(lines, "\n"))
	}
	if len(sections) == 0 {
		return "No files changed."
	}
	return strings.Join(sections, "\n\n")
}

// ChangedLineCount is the total changed lines across every repository in the
// delta.
func ChangedLineCount(deltas []core.RepoDelta) int {
	total := 0
	for _, delta := range deltas {
		total += delta.ChangedLines
	}
	return total
}
