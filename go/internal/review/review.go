// Port of mj-review/src/lib.rs.
//
// Cumulative review of a completed coding turn by independent harness
// sessions.
//
// A *turn review* runs after the primary agent finishes a prompt: Hel
// captures what the workspace's repositories changed since the last
// completed review, then asks independently running reviewer sessions to
// inspect that same captured change. The configured reviewer profile is
// resolved once for the group. The user can inspect each session and either
// forward the combined findings to the primary, dismiss them, or cancel the
// review.
//
// Each reviewer follows Codex's `/review` rubric and returns plain-text
// findings or exactly `No findings.` when there are none. The driver keeps
// reports in launch order, labels their source sessions, and declares the
// group clean only when every reviewer completed cleanly. It owns
// forwarding, cancellation and the durable state restored after a daemon
// restart.
//
// Terms used throughout:
//
//   - A *reviewer* reads the user messages, changed-file counts and captured
//     tree ids, then gets the diff through its own harness tools.
//   - A *baseline* is the Git tree id of a repository's working tree as of
//     the last completed review. It advances only after complete review
//     coverage; cancellation or an incomplete group leaves the changes for
//     the next review.
//
// The Rust modules re-export mj_core's review types (`verdict::*`,
// `driver::*`, `CODEX_REVIEW_RUBRIC`, `BRIEF_WRITER_INSTRUCTIONS`); Go
// callers use those from package core directly.

package review

// UserMessagesLimit is how much of the primary's user messages a review
// prompt embeds.
const UserMessagesLimit = 128 * 1024

// ChangedFilesLimit is how much of the per-file line-count table a prompt
// embeds. At roughly 60 bytes a row that is several hundred files; each
// repository's totals come first in its section, so a cut table still says
// how large the change is.
const ChangedFilesLimit = 32 * 1024

// SynthesisLimit is how much of each reviewer's findings is retained before
// aggregation.
const SynthesisLimit = 32 * 1024

// ReviewCaptureDiffLimit bounds the diff retained in a capture for existing
// review status consumers.
const ReviewCaptureDiffLimit = 96 * 1024

// BoundReviewSection bounds a captured Git patch retained for existing review
// status consumers: it keeps both ends of text and marks the gap.
func BoundReviewSection(text string, limit int, label string) string {
	if len(text) <= limit {
		return text
	}
	marker := "\n…[" + label + " omitted]…\n"
	available := max(limit-len(marker), 0)
	head := available * 3 / 4
	tail := available - head
	headEnd := floorCharBoundary(text, head)
	tailStart := ceilCharBoundary(text, max(len(text)-tail, 0))
	return text[:headEnd] + marker + text[tailStart:]
}

// BoundTail bounds model-authored prose, which puts its conclusions first,
// by keeping the head rather than both ends.
func BoundTail(text string, limit int, label string) string {
	if len(text) <= limit {
		return text
	}
	marker := "\n…[" + label + " truncated]…"
	head := floorCharBoundary(text, max(limit-len(marker), 0))
	return text[:head] + marker
}

// floorCharBoundary is Rust's str::floor_char_boundary: the largest UTF-8
// boundary of text at or below index.
func floorCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for index > 0 && !isCharBoundary(text, index) {
		index--
	}
	return index
}

// ceilCharBoundary is Rust's str::ceil_char_boundary: the smallest UTF-8
// boundary of text at or above index.
func ceilCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for !isCharBoundary(text, index) {
		index++
	}
	return index
}

// isCharBoundary reports whether index starts a UTF-8 sequence of text (or
// is its end), as Rust's str::is_char_boundary.
func isCharBoundary(text string, index int) bool {
	return index == len(text) || text[index]&0xC0 != 0x80
}
