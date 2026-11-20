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
//
// The line diff is the Rust `similar` crate's (2.7) Myers diff with its
// Compact and Replace hooks and unified-diff rendering, written out here so
// the patch text matches the Rust build byte for byte.

package core

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
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

// UnmarshalJSON requires the text and counts and reads the counts as
// unsigned, as serde does for `usize`.
func (p *DiffPatch) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "text", "insertions", "deletions"); err != nil {
		return err
	}
	var wire struct {
		Text       string `json:"text"`
		Insertions uint64 `json:"insertions"`
		Deletions  uint64 `json:"deletions"`
		Created    bool   `json:"created"`
		Truncated  bool   `json:"truncated"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*p = DiffPatch{
		Text:       wire.Text,
		Insertions: int(wire.Insertions),
		Deletions:  int(wire.Deletions),
		Created:    wire.Created,
		Truncated:  wire.Truncated,
	}
	return nil
}

// metaValue is the patch as the JSON value `_meta` holds.
func (p *DiffPatch) metaValue() (any, error) {
	encoded, err := marshalSerdeJSON(p)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Parse(encoded)
}

// setDiffPatch stores the patch under DiffPatchMetaKey without writing into
// a `_meta` another copy of the diff may share.
func setDiffPatch(diff *schema.Diff, value any) {
	meta := diff.Meta.Clone()
	if meta == nil {
		meta = schema.Meta{}
	}
	meta.Set(DiffPatchMetaKey, value)
	diff.Meta = meta
}

// CompactDiff replaces a diff's two file copies with a patch, in place.
//
// Returns whether anything changed, so a caller can tell a rewritten update
// from one that was already compact.
func CompactDiff(diff *schema.Diff) bool {
	if diff.OldText == nil && diff.NewText == "" {
		return false
	}
	patch := buildPatch(diff.Path, diff.OldText, diff.NewText)
	value, err := patch.metaValue()
	if err != nil {
		// Serializing a struct of strings and counts does not fail. If it
		// somehow does, keeping the original copies is the safe answer.
		return false
	}
	setDiffPatch(diff, value)
	diff.OldText = nil
	diff.NewText = ""
	return true
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
	changed := CompactDiff(diff)
	patch := StoredPatch(diff)
	if patch == nil {
		return changed
	}
	if patch.Text == "" && patch.Truncated {
		return changed
	}
	patch.Text = ""
	patch.Truncated = true
	value, err := patch.metaValue()
	if err != nil {
		return changed
	}
	setDiffPatch(diff, value)
	return true
}

// StoredPatch reads back the stored patch, if this diff has one.
func StoredPatch(diff *schema.Diff) *DiffPatch {
	value, ok := diff.Meta.Get(DiffPatchMetaKey)
	if !ok {
		return nil
	}
	encoded, err := jsonvalue.Marshal(value)
	if err == nil {
		var patch DiffPatch
		if err = json.Unmarshal(encoded, &patch); err == nil {
			return &patch
		}
	}
	slog.Warn("could not read a stored diff patch", "error", err)
	return nil
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
	created := oldText == nil
	relative := strings.TrimLeft(path, "/")
	oldHeader := "a/" + relative
	old := ""
	if created {
		oldHeader = "/dev/null"
	} else {
		old = *oldText
	}
	oldLines, newLines := tokenizeLines(old), tokenizeLines(newText)
	ops := captureLineDiff(oldLines, newLines, time.Now().Add(diffDeadline))
	insertions, deletions := 0, 0
	for _, op := range ops {
		if op.tag != diffTagEqual {
			insertions += op.newLength
			deletions += op.oldLength
		}
	}
	// Assemble hunk by hunk so the budget is spent on whole hunks. Half a
	// hunk reads as a corrupt patch; a short patch that says so does not.
	var text strings.Builder
	fmt.Fprintf(&text, "--- %s\n+++ b/%s\n", oldHeader, relative)
	truncated := false
	for _, hunk := range groupDiffOps(ops, contextRadius) {
		if len(hunk) == 0 {
			continue
		}
		rendered := renderUnifiedHunk(hunk, oldLines, newLines)
		if text.Len()+len(rendered) > DiffPatchBytes {
			truncated = true
			break
		}
		text.WriteString(rendered)
	}
	if truncated {
		text.WriteString("[mj dropped the remaining hunks]\n")
	}
	return DiffPatch{
		Text:       text.String(),
		Insertions: insertions,
		Deletions:  deletions,
		Created:    created,
		Truncated:  truncated,
	}
}

// tokenizeLines is similar's line tokenizer: each line keeps its `\n`,
// `\r\n` or `\r` terminator, and a last line without one is kept.
func tokenizeLines(text string) []string {
	var lines []string
	start := 0
	for i := 0; i < len(text); i++ {
		switch text[i] {
		case '\r':
			if i+1 < len(text) && text[i+1] == '\n' {
				lines = append(lines, text[start:i+2])
				i++
				start = i + 1
			} else {
				lines = append(lines, text[start:i+1])
				start = i + 1
			}
		case '\n':
			lines = append(lines, text[start:i+1])
			start = i + 1
		}
	}
	if start < len(text) {
		lines = append(lines, text[start:])
	}
	return lines
}

type diffTag int

const (
	diffTagEqual diffTag = iota
	diffTagDelete
	diffTagInsert
	diffTagReplace
)

// diffOp is similar's DiffOp. An Equal has the same length on both sides;
// a Delete has an empty new range and an Insert an empty old range.
type diffOp struct {
	tag                diffTag
	oldIndex, newIndex int
	oldLength          int
	newLength          int
}

func (o *diffOp) isEmpty() bool { return o.oldLength == 0 && o.newLength == 0 }

// adjust is similar's DiffOp::adjust: move both indexes by `offset` and
// change the op's length by `length`, on the side or sides it has.
func (o *diffOp) adjust(offset, length int) {
	o.oldIndex += offset
	o.newIndex += offset
	switch o.tag {
	case diffTagEqual:
		o.oldLength += length
		o.newLength += length
	case diffTagDelete:
		o.oldLength += length
	case diffTagInsert:
		o.newLength += length
	case diffTagReplace:
		o.oldLength += length
		o.newLength += length
	}
}

func (o *diffOp) shiftLeft(n int)   { o.adjust(-n, 0) }
func (o *diffOp) shiftRight(n int)  { o.adjust(n, 0) }
func (o *diffOp) growLeft(n int)    { o.adjust(-n, n) }
func (o *diffOp) growRight(n int)   { o.adjust(0, n) }
func (o *diffOp) shrinkLeft(n int)  { o.adjust(0, -n) }
func (o *diffOp) shrinkRight(n int) { o.adjust(n, -n) }

func commonPrefixLen(old []string, oldStart, oldEnd int, new []string, newStart, newEnd int) int {
	count := 0
	for oldStart+count < oldEnd && newStart+count < newEnd && old[oldStart+count] == new[newStart+count] {
		count++
	}
	return count
}

func commonSuffixLen(old []string, oldStart, oldEnd int, new []string, newStart, newEnd int) int {
	count := 0
	for oldEnd-count > oldStart && newEnd-count > newStart && old[oldEnd-count-1] == new[newEnd-count-1] {
		count++
	}
	return count
}

// captureLineDiff is similar's capture_diff_deadline with Myers: the raw
// ops are compacted, then adjacent deletes and inserts become replaces.
func captureLineDiff(old, new []string, deadline time.Time) []diffOp {
	differ := myersDiffer{old: old, new: new, deadline: deadline}
	maxD := myersMaxD(len(old), len(new))
	differ.vf = make([]int, 2*maxD)
	differ.vb = make([]int, 2*maxD)
	differ.offset = maxD
	differ.conquer(0, len(old), 0, len(new))
	ops := differ.ops
	cleanupDiffOps(old, new, &ops)
	var replace diffReplacer
	for _, op := range ops {
		switch op.tag {
		case diffTagEqual:
			replace.equal(op.oldIndex, op.newIndex, op.oldLength)
		case diffTagDelete:
			replace.delete(op.oldIndex, op.oldLength, op.newIndex)
		case diffTagInsert:
			replace.insert(op.oldIndex, op.newIndex, op.newLength)
		}
	}
	replace.finish()
	return replace.ops
}

func myersMaxD(len1, len2 int) int { return (len1+len2+1)/2 + 1 }

type myersDiffer struct {
	old, new []string
	deadline time.Time
	vf, vb   []int
	offset   int
	ops      []diffOp
}

func (m *myersDiffer) equal(oldIndex, newIndex, length int) {
	m.ops = append(m.ops, diffOp{tag: diffTagEqual, oldIndex: oldIndex, newIndex: newIndex, oldLength: length, newLength: length})
}

func (m *myersDiffer) delete(oldIndex, length, newIndex int) {
	m.ops = append(m.ops, diffOp{tag: diffTagDelete, oldIndex: oldIndex, newIndex: newIndex, oldLength: length})
}

func (m *myersDiffer) insert(oldIndex, newIndex, length int) {
	m.ops = append(m.ops, diffOp{tag: diffTagInsert, oldIndex: oldIndex, newIndex: newIndex, newLength: length})
}

func (m *myersDiffer) findMiddleSnake(oldStart, oldEnd, newStart, newEnd int) (int, int, bool) {
	n, mm := oldEnd-oldStart, newEnd-newStart
	delta := n - mm
	odd := delta&1 == 1
	vf := func(k int) *int { return &m.vf[k+m.offset] }
	vb := func(k int) *int { return &m.vb[k+m.offset] }
	*vf(1) = 0
	*vb(1) = 0
	dMax := myersMaxD(n, mm)
	for d := 0; d < dMax; d++ {
		if time.Now().After(m.deadline) {
			break
		}
		for k := d; k >= -d; k -= 2 {
			var x int
			if k == -d || (k != d && *vf(k - 1) < *vf(k + 1)) {
				x = *vf(k + 1)
			} else {
				x = *vf(k - 1) + 1
			}
			y := x - k
			x0, y0 := x, y
			if x < n && y < mm {
				x += commonPrefixLen(m.old, oldStart+x, oldEnd, m.new, newStart+y, newEnd)
			}
			*vf(k) = x
			if odd && absInt(k-delta) <= d-1 && *vf(k)+*vb(-(k - delta)) >= n {
				return x0 + oldStart, y0 + newStart, true
			}
		}
		for k := d; k >= -d; k -= 2 {
			var x int
			if k == -d || (k != d && *vb(k - 1) < *vb(k + 1)) {
				x = *vb(k + 1)
			} else {
				x = *vb(k - 1) + 1
			}
			y := x - k
			if x < n && y < mm {
				advance := commonSuffixLen(m.old, oldStart, oldStart+n-x, m.new, newStart, newStart+mm-y)
				x += advance
				y += advance
			}
			*vb(k) = x
			if !odd && absInt(k-delta) <= d && *vb(k)+*vf(-(k - delta)) >= n {
				return n - x + oldStart, mm - y + newStart, true
			}
		}
	}
	return 0, 0, false
}

func absInt(value int) int {
	if value < 0 {
		return -value
	}
	return value
}

func (m *myersDiffer) conquer(oldStart, oldEnd, newStart, newEnd int) {
	prefix := commonPrefixLen(m.old, oldStart, oldEnd, m.new, newStart, newEnd)
	if prefix > 0 {
		m.equal(oldStart, newStart, prefix)
	}
	oldStart += prefix
	newStart += prefix
	suffix := commonSuffixLen(m.old, oldStart, oldEnd, m.new, newStart, newEnd)
	suffixOld, suffixNew := oldEnd-suffix, newEnd-suffix
	oldEnd -= suffix
	newEnd -= suffix

	oldEmpty, newEmpty := oldStart >= oldEnd, newStart >= newEnd
	switch {
	case oldEmpty && newEmpty:
	case newEmpty:
		m.delete(oldStart, oldEnd-oldStart, newStart)
	case oldEmpty:
		m.insert(oldStart, newStart, newEnd-newStart)
	default:
		if x, y, ok := m.findMiddleSnake(oldStart, oldEnd, newStart, newEnd); ok {
			m.conquer(oldStart, x, newStart, y)
			m.conquer(x, oldEnd, y, newEnd)
		} else {
			m.delete(oldStart, oldEnd-oldStart, newStart)
			m.insert(oldStart, newStart, newEnd-newStart)
		}
	}
	if suffix > 0 {
		m.equal(suffixOld, suffixNew, suffix)
	}
}

// cleanupDiffOps is similar's Compact hook: deletes, then inserts, slide to
// a canonical position and merge with their neighbours.
func cleanupDiffOps(old, new []string, ops *[]diffOp) {
	for _, tag := range []diffTag{diffTagDelete, diffTagInsert} {
		for pointer := 0; pointer < len(*ops); pointer++ {
			if (*ops)[pointer].tag == tag {
				pointer = shiftDiffOpsUp(ops, old, new, pointer)
				pointer = shiftDiffOpsDown(ops, old, new, pointer)
			}
		}
	}
}

func shiftDiffOpsUp(ops *[]diffOp, old, new []string, pointer int) int {
	for pointer >= 1 {
		list := *ops
		prev, this := list[pointer-1], list[pointer]
		switch {
		case (this.tag == diffTagInsert || this.tag == diffTagDelete) && prev.tag == diffTagEqual:
			suffix := commonSuffixLen(old, prev.oldIndex, prev.oldIndex+prev.oldLength, new, this.newIndex, this.newIndex+this.newLength)
			if suffix > 0 {
				if pointer+1 < len(list) && list[pointer+1].tag == diffTagEqual {
					list[pointer+1].growLeft(suffix)
				} else {
					length := suffix
					if this.tag == diffTagDelete {
						length = prev.oldLength - suffix
					}
					inserted := diffOp{
						tag:       diffTagEqual,
						oldIndex:  prev.oldIndex + prev.oldLength - suffix,
						newIndex:  this.newIndex + this.newLength - suffix,
						oldLength: length,
						newLength: length,
					}
					*ops = slices.Insert(*ops, pointer+1, inserted)
					list = *ops
				}
				list[pointer].shiftLeft(suffix)
				list[pointer-1].shrinkLeft(suffix)
				if list[pointer-1].isEmpty() {
					*ops = slices.Delete(*ops, pointer-1, pointer)
					pointer--
				}
			} else if list[pointer-1].isEmpty() {
				*ops = slices.Delete(*ops, pointer-1, pointer)
				pointer--
			} else {
				return pointer
			}
		case (this.tag == diffTagInsert && prev.tag == diffTagDelete) || (this.tag == diffTagDelete && prev.tag == diffTagInsert):
			list[pointer-1], list[pointer] = list[pointer], list[pointer-1]
			pointer--
		case this.tag == diffTagInsert && prev.tag == diffTagInsert:
			list[pointer-1].growRight(this.newLength)
			*ops = slices.Delete(*ops, pointer, pointer+1)
			pointer--
		case this.tag == diffTagDelete && prev.tag == diffTagDelete:
			list[pointer-1].growRight(this.oldLength)
			*ops = slices.Delete(*ops, pointer, pointer+1)
			pointer--
		default:
			panic("unexpected tag")
		}
	}
	return pointer
}

func shiftDiffOpsDown(ops *[]diffOp, old, new []string, pointer int) int {
	for pointer+1 < len(*ops) {
		list := *ops
		next, this := list[pointer+1], list[pointer]
		switch {
		case (this.tag == diffTagInsert || this.tag == diffTagDelete) && next.tag == diffTagEqual:
			prefix := commonPrefixLen(old, next.oldIndex, next.oldIndex+next.oldLength, new, this.newIndex, this.newIndex+this.newLength)
			if prefix > 0 {
				if pointer >= 1 && list[pointer-1].tag == diffTagEqual {
					list[pointer-1].growRight(prefix)
				} else {
					inserted := diffOp{
						tag:       diffTagEqual,
						oldIndex:  next.oldIndex,
						newIndex:  this.newIndex,
						oldLength: prefix,
						newLength: prefix,
					}
					*ops = slices.Insert(*ops, pointer, inserted)
					list = *ops
					pointer++
				}
				list[pointer].shiftRight(prefix)
				list[pointer+1].shrinkRight(prefix)
				if list[pointer+1].isEmpty() {
					*ops = slices.Delete(*ops, pointer+1, pointer+2)
				}
			} else if list[pointer+1].isEmpty() {
				*ops = slices.Delete(*ops, pointer+1, pointer+2)
			} else {
				return pointer
			}
		case (this.tag == diffTagInsert && next.tag == diffTagDelete) || (this.tag == diffTagDelete && next.tag == diffTagInsert):
			list[pointer], list[pointer+1] = list[pointer+1], list[pointer]
			pointer++
		case this.tag == diffTagInsert && next.tag == diffTagInsert:
			list[pointer].growRight(next.newLength)
			*ops = slices.Delete(*ops, pointer+1, pointer+2)
		case this.tag == diffTagDelete && next.tag == diffTagDelete:
			list[pointer].growRight(next.oldLength)
			*ops = slices.Delete(*ops, pointer+1, pointer+2)
		default:
			panic("unexpected tag")
		}
	}
	return pointer
}

// diffReplacer is similar's Replace hook over a Capture: equal runs merge,
// and a run of deletes and inserts becomes one replace.
type diffReplacer struct {
	ops      []diffOp
	del, ins *diffOp
	equalRun *diffOp
}

func (r *diffReplacer) flushEqual() {
	if r.equalRun != nil {
		r.ops = append(r.ops, *r.equalRun)
		r.equalRun = nil
	}
}

func (r *diffReplacer) flushDeleteInsert() {
	switch {
	case r.del != nil && r.ins != nil:
		r.ops = append(r.ops, diffOp{
			tag:       diffTagReplace,
			oldIndex:  r.del.oldIndex,
			oldLength: r.del.oldLength,
			newIndex:  r.ins.newIndex,
			newLength: r.ins.newLength,
		})
	case r.del != nil:
		r.ops = append(r.ops, *r.del)
	case r.ins != nil:
		r.ops = append(r.ops, *r.ins)
	}
	r.del, r.ins = nil, nil
}

func (r *diffReplacer) equal(oldIndex, newIndex, length int) {
	r.flushDeleteInsert()
	if r.equalRun != nil {
		r.equalRun.oldLength += length
		r.equalRun.newLength += length
		return
	}
	r.equalRun = &diffOp{tag: diffTagEqual, oldIndex: oldIndex, newIndex: newIndex, oldLength: length, newLength: length}
}

func (r *diffReplacer) delete(oldIndex, length, newIndex int) {
	r.flushEqual()
	if r.del != nil {
		r.del.oldLength += length
		return
	}
	r.del = &diffOp{tag: diffTagDelete, oldIndex: oldIndex, newIndex: newIndex, oldLength: length}
}

func (r *diffReplacer) insert(oldIndex, newIndex, length int) {
	r.flushEqual()
	if r.ins != nil {
		r.ins.newLength += length
		return
	}
	r.ins = &diffOp{tag: diffTagInsert, oldIndex: oldIndex, newIndex: newIndex, newLength: length}
}

func (r *diffReplacer) finish() {
	r.flushEqual()
	r.flushDeleteInsert()
}

// groupDiffOps is similar's group_diff_ops: hunks with at most `n` lines of
// context, split where more than `2n` lines are unchanged.
func groupDiffOps(ops []diffOp, n int) [][]diffOp {
	if len(ops) == 0 {
		return nil
	}
	ops = slices.Clone(ops)
	if first := &ops[0]; first.tag == diffTagEqual {
		offset := max(first.oldLength-n, 0)
		first.oldIndex += offset
		first.newIndex += offset
		first.oldLength -= offset
		first.newLength -= offset
	}
	if last := &ops[len(ops)-1]; last.tag == diffTagEqual {
		trim := max(last.oldLength-n, 0)
		last.oldLength -= trim
		last.newLength -= trim
	}
	var groups [][]diffOp
	var pending []diffOp
	for _, op := range ops {
		if op.tag == diffTagEqual && op.oldLength > n*2 {
			pending = append(pending, diffOp{tag: diffTagEqual, oldIndex: op.oldIndex, newIndex: op.newIndex, oldLength: n, newLength: n})
			groups = append(groups, pending)
			offset := max(op.oldLength-n, 0)
			length := op.oldLength - offset
			pending = []diffOp{{
				tag:       diffTagEqual,
				oldIndex:  op.oldIndex + offset,
				newIndex:  op.newIndex + offset,
				oldLength: length,
				newLength: length,
			}}
			continue
		}
		pending = append(pending, op)
	}
	if !(len(pending) == 0 || (len(pending) == 1 && pending[0].tag == diffTagEqual)) {
		groups = append(groups, pending)
	}
	return groups
}

// unifiedHunkRange is similar's UnifiedDiffHunkRange display.
func unifiedHunkRange(start, end int) string {
	beginning := start + 1
	length := max(end-start, 0)
	if length == 1 {
		return fmt.Sprint(beginning)
	}
	if length == 0 {
		// Empty ranges begin at the line just before the range.
		beginning--
	}
	return fmt.Sprintf("%d,%d", beginning, length)
}

// renderUnifiedHunk is similar's UnifiedDiffHunk display for a
// newline-terminated line diff with the missing-newline hint.
func renderUnifiedHunk(ops []diffOp, old, new []string) string {
	var text strings.Builder
	first, last := ops[0], ops[len(ops)-1]
	fmt.Fprintf(&text, "@@ -%s +%s @@\n",
		unifiedHunkRange(first.oldIndex, last.oldIndex+last.oldLength),
		unifiedHunkRange(first.newIndex, last.newIndex+last.newLength))
	line := func(tag byte, value string) {
		text.WriteByte(tag)
		text.WriteString(value)
		if !strings.HasSuffix(value, "\n") && !strings.HasSuffix(value, "\r") {
			text.WriteString("\n\\ No newline at end of file\n")
		}
	}
	for _, op := range ops {
		switch op.tag {
		case diffTagEqual:
			for i := 0; i < op.oldLength; i++ {
				line(' ', old[op.oldIndex+i])
			}
		case diffTagDelete:
			for i := 0; i < op.oldLength; i++ {
				line('-', old[op.oldIndex+i])
			}
		case diffTagInsert:
			for i := 0; i < op.newLength; i++ {
				line('+', new[op.newIndex+i])
			}
		case diffTagReplace:
			for i := 0; i < op.oldLength; i++ {
				line('-', old[op.oldIndex+i])
			}
			for i := 0; i < op.newLength; i++ {
				line('+', new[op.newIndex+i])
			}
		}
	}
	return text.String()
}
