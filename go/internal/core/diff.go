// Port of mj-core/src/diff.rs.

// Store an edit as a patch instead of two copies of the file.
//
// ACP v1 describes a file edit as `oldText` plus `newText` — the whole file
// before and the whole file after. A three-line change to a 200 KB source
// file therefore costs 400 KB, every time the agent touches it, forever: the
// journal keeps the event, the projection keeps the tool call, and both are
// read back on every resume. Measured on one real session, tool-call content
// reached 561 MB and the largest single transcript item was 2,087,180 bytes.
//
// Nothing in Mjolnir ever reconstructs a file from those two copies. The
// only reader is the diffstat — `path  +12 −3`. A unified patch answers that
// and is proportional to the edit rather than to the file, so that is what
// gets written. It also leaves the change renderable, which two discarded
// file copies did not.
//
// The patch travels in the diff's `_meta`, which ACP reserves for exactly
// this kind of extension, under DiffPatchMetaKey. `oldText` and `newText` are
// cleared once it is there. Records written before this existed still carry
// both copies, so every reader here falls back to them.

package core

import (
	"time"

	"github.com/BrokkAi/acp-go/schema"
)

// DiffPatchMetaKey is the `_meta` key holding a diff's patch. Namespaced
// because `_meta` is shared with whatever the agent puts there.
const DiffPatchMetaKey = "dev.mj.diffPatch"

// DiffPatchBytes is how much patch text one edit may record.
//
// A patch is proportional to the edit, so this only binds when an edit
// really is enormous — a generated file replaced wholesale, say. It matches
// the cap terminal output already gets in TerminalJournalOutputBytes in
// spirit: keep the informative part, say plainly that the rest was dropped.
const DiffPatchBytes = 128 * 1024

// contextRadius is the lines of unchanged context kept around each hunk.
const contextRadius = 3

// diffDeadline is how long the line diff may run before it settles for a
// coarser answer.
//
// Myers is O(N·D), so a whole-file rewrite of a large generated file can
// take arbitrarily long. This runs on the relay's recording path, where a
// stall is a stalled session, so the diff has a deadline: past it the diff
// returns a valid but blunter diff instead of the minimal one.
const diffDeadline = 250 * time.Millisecond

// DiffPatch is a stored edit, as a patch.
type DiffPatch struct {
	// Unified diff text, headed `--- a/path` / `+++ b/path` (`/dev/null` for
	// a created file). Same shape mjolnir's `trajectory::unified_diff` emits,
	// which is what the diff highlighter in `src/web/tool-output.js`
	// recognizes.
	Text string `json:"text"`
	// Lines the patch adds.
	Insertions int `json:"insertions"`
	// Lines the patch removes.
	Deletions int `json:"deletions"`
	// Whether the file had no previous content.
	Created bool `json:"created,omitempty"`
	// Whether hunks were dropped to fit DiffPatchBytes. The counts above
	// still describe the whole edit; only the text is short.
	Truncated bool `json:"truncated,omitempty"`
}

// CompactDiff replaces a diff's two file copies with a patch, in place.
//
// Returns whether anything changed, so a caller can tell a rewritten update
// from one that was already compact.
func CompactDiff(diff *schema.Diff) bool {
	panic("unported: mj-core/src/diff.rs compact_diff")
}

// CompactToolCallContent replaces every file copy in a tool call's content
// with a patch.
func CompactToolCallContent(content []schema.ToolCallContent) bool {
	compacted := false
	for i := range content {
		if content[i].Kind == schema.ToolCallContentKindDiff && content[i].Diff != nil {
			compacted = CompactDiff(content[i].Diff) || compacted
		}
	}
	return compacted
}

// DropPatchText reduces a diff to the counts a diffstat reads, dropping the
// patch text.
//
// This is what a retention pass leaves behind once a checkpoint holds the
// whole edit. A diff recorded before patches were stored is compacted first,
// so its counts come from the file copies before they go — otherwise the
// diffstat the transcript shows would drop to `+0 −0`.
//
// Returns whether anything changed.
func DropPatchText(diff *schema.Diff) bool {
	panic("unported: mj-core/src/diff.rs drop_patch_text")
}

// StoredPatch reads back the stored patch, if this diff has one.
func StoredPatch(diff *schema.Diff) *DiffPatch {
	panic("unported: mj-core/src/diff.rs stored_patch")
}

// PatchOf is the patch for a diff, whichever form it was stored in.
//
// A record written before diffs were compacted still holds both file
// copies, so this diffs them on demand rather than reporting nothing.
func PatchOf(diff *schema.Diff) DiffPatch {
	if patch := StoredPatch(diff); patch != nil {
		return *patch
	}
	return buildPatch(diff.Path, diff.OldText, diff.NewText)
}

func buildPatch(path string, oldText *string, newText string) DiffPatch {
	panic("unported: mj-core/src/diff.rs build_patch")
}
