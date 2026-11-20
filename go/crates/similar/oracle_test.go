package similar

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// The differential tests ask the real crate (go/port/oracle, module
// `similar`) what it does with an input and hold this package to the same
// answer. Each test has a recorded corpus under testdata/oracle.
//
// MJ_ORACLE_SCALE multiplies the number of random cases. It is for a longer
// run against the live oracle (`MJ_ORACLE=live MJ_ORACLE_SCALE=50 ...`); the
// recorded corpora are made with the default of 1.
func oracleScale() int {
	scale, err := strconv.Atoi(os.Getenv("MJ_ORACLE_SCALE"))
	if err != nil || scale < 1 {
		return 1
	}
	return scale
}

// oracleRequest is every member a request to the `similar` module can carry.
type oracleRequest struct {
	Op                string          `json:"op"`
	Alg               string          `json:"alg"`
	Deadline          string          `json:"deadline"`
	TimeoutMs         *int            `json:"timeout_ms"`
	NewlineTerminated *bool           `json:"newline_terminated"`
	Old               json.RawMessage `json:"old"`
	New               json.RawMessage `json:"new"`
	Radius            *int            `json:"radius"`
	Header            []string        `json:"header"`
	Hint              *bool           `json:"hint"`
	Digest            bool            `json:"digest"`
	Kind              string          `json:"kind"`
	Text              string          `json:"text"`
	Hook              string          `json:"hook"`
	OldStart          *int            `json:"old_start"`
	OldEnd            *int            `json:"old_end"`
	NewStart          *int            `json:"new_start"`
	NewEnd            *int            `json:"new_end"`
	Ops               []oracleOp      `json:"ops"`
	OldLen            int             `json:"old_len"`
	NewLen            int             `json:"new_len"`
}

type oracleOp struct {
	Op       string `json:"op"`
	OldIndex int    `json:"old_index"`
	NewIndex int    `json:"new_index"`
	Len      int    `json:"len"`
	OldLen   int    `json:"old_len"`
	NewLen   int    `json:"new_len"`
}

func (request *oracleRequest) algorithm() Algorithm {
	switch request.Alg {
	case "patience":
		return Algorithm_Patience
	case "lcs":
		return Algorithm_Lcs
	default:
		return Algorithm_Myers
	}
}

func (request *oracleRequest) deadline() rs.Option[rstime.Instant] {
	switch request.Deadline {
	case "passed":
		return rs.Some(rstime.InstantNow().Sub(time.Second))
	case "far":
		return rs.Some(rstime.InstantNow().Add(time.Hour))
	default:
		return rs.None[rstime.Instant]()
	}
}

func (request *oracleRequest) radius() int {
	if request.Radius == nil {
		return 3
	}
	return *request.Radius
}

func oracleDecode[T any](t *testing.T, raw json.RawMessage) T {
	t.Helper()
	var value T
	if len(raw) == 0 {
		return value
	}
	if err := json.Unmarshal(raw, &value); err != nil {
		t.Fatalf("decode %s: %v", raw, err)
	}
	return value
}

func oracleOpJSON(op DiffOp) map[string]any {
	switch op := op.(type) {
	case *DiffOp_Equal:
		return map[string]any{"op": "equal", "old_index": op.OldIndex, "new_index": op.NewIndex, "len": op.Len}
	case *DiffOp_Delete:
		return map[string]any{"op": "delete", "old_index": op.OldIndex, "old_len": op.OldLen, "new_index": op.NewIndex}
	case *DiffOp_Insert:
		return map[string]any{"op": "insert", "old_index": op.OldIndex, "new_index": op.NewIndex, "new_len": op.NewLen}
	case *DiffOp_Replace:
		return map[string]any{"op": "replace", "old_index": op.OldIndex, "old_len": op.OldLen, "new_index": op.NewIndex, "new_len": op.NewLen}
	default:
		panic("unknown op")
	}
}

func oracleOpsJSON(ops []DiffOp) []any {
	out := []any{}
	for _, op := range ops {
		out = append(out, oracleOpJSON(op))
	}
	return out
}

func oracleGroupsJSON(groups [][]DiffOp) []any {
	out := []any{}
	for _, group := range groups {
		out = append(out, oracleOpsJSON(group))
	}
	return out
}

func oracleDiffTag(tag DiffTag) string {
	return [...]string{"equal", "delete", "insert", "replace"}[tag]
}

func oracleChangeTag(tag ChangeTag) string {
	return [...]string{"equal", "delete", "insert"}[tag]
}

func oracleTagTuplesJSON(ops []DiffOp) []any {
	out := []any{}
	for _, op := range ops {
		tag, old, new_ := op.AsTagTuple()
		if tag != op.Tag() || old != op.OldRange() || new_ != op.NewRange() {
			panic("as_tag_tuple disagrees with tag, old_range, new_range")
		}
		out = append(out, []any{oracleDiffTag(tag), old.Start, old.End, new_.Start, new_.End})
	}
	return out
}

func oracleOption(index rs.Option[int]) any {
	if value, ok := index.Get(); ok {
		return value
	}
	return nil
}

// oracleFnv is the FNV-1a hash the oracle module digests large replies with.
type oracleFnv uint64

func oracleFnvNew() *oracleFnv {
	hash := oracleFnv(0xcbf29ce484222325)
	return &hash
}

func (hash *oracleFnv) bytes(data []byte) {
	for _, b := range data {
		*hash ^= oracleFnv(b)
		*hash *= 0x00000100000001b3
	}
}

func (hash *oracleFnv) text(data string) { hash.bytes([]byte(data)) }

func (hash *oracleFnv) number(value uint64) {
	hash.bytes(binary.LittleEndian.AppendUint64(nil, value))
}

func (hash *oracleFnv) hex() string { return fmt.Sprintf("%016x", uint64(*hash)) }

func oracleOpsDigest(ops []DiffOp) string {
	hash := oracleFnvNew()
	for _, op := range ops {
		var fields [5]int
		switch op := op.(type) {
		case *DiffOp_Equal:
			fields = [5]int{0, op.OldIndex, op.NewIndex, op.Len, 0}
		case *DiffOp_Delete:
			fields = [5]int{1, op.OldIndex, op.OldLen, op.NewIndex, 0}
		case *DiffOp_Insert:
			fields = [5]int{2, op.OldIndex, op.NewIndex, op.NewLen, 0}
		case *DiffOp_Replace:
			fields = [5]int{3, op.OldIndex, op.OldLen, op.NewIndex, op.NewLen}
		}
		for _, field := range fields {
			hash.number(uint64(field))
		}
	}
	return hash.hex()
}

func oracleBool(value bool) uint64 {
	if value {
		return 1
	}
	return 0
}

func oracleAnswer(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		request := oracleDecode[oracleRequest](t, raw)
		return oracle.Catch(func() any {
			switch request.Op {
			case "lines", "words", "chars", "slices":
				return oracleTextDiff(t, &request)
			case "tokenize":
				return oracleTokenize(&request)
			case "capture":
				return oracleCapture(t, &request)
			case "raw":
				return oracleRaw(t, &request)
			case "identify":
				return oracleIdentify(t, &request)
			case "group":
				return oracleGroup(&request)
			default:
				t.Fatalf("unknown op %q", request.Op)
				return nil
			}
		})
	}
}

func oracleTextDiff(t *testing.T, request *oracleRequest) any {
	config := TextDiffConfigure()
	config.Algorithm(request.algorithm())
	if instant, ok := request.deadline().Get(); ok {
		config.Deadline(instant)
	}
	if request.TimeoutMs != nil {
		config.Timeout(time.Duration(*request.TimeoutMs) * time.Millisecond)
	}
	if request.NewlineTerminated != nil {
		config.NewlineTerminated(*request.NewlineTerminated)
	}
	var diff *TextDiff
	switch request.Op {
	case "lines":
		diff = config.DiffLines(oracleDecode[string](t, request.Old), oracleDecode[string](t, request.New))
	case "words":
		diff = config.DiffWords(oracleDecode[string](t, request.Old), oracleDecode[string](t, request.New))
	case "chars":
		diff = config.DiffChars(oracleDecode[string](t, request.Old), oracleDecode[string](t, request.New))
	default:
		diff = config.DiffSlices(oracleDecode[[]string](t, request.Old), oracleDecode[[]string](t, request.New))
	}

	radius := request.radius()
	unified := diff.UnifiedDiff()
	unified.ContextRadius(radius)
	if request.Header != nil {
		header := append(append([]string{}, request.Header...), "", "")
		unified.Header(header[0], header[1])
	}
	if request.Hint != nil {
		unified.MissingNewlineHint(*request.Hint)
	}
	unifiedText := unified.String()
	var written strings.Builder
	if err := unified.ToWriter(&written); err != nil {
		t.Fatal(err)
	}
	if written.String() != unifiedText {
		t.Errorf("UnifiedDiff.ToWriter wrote %q, String is %q", written.String(), unifiedText)
	}

	insertions, deletions := 0, 0
	for change := range diff.IterAllChanges().All() {
		switch change.Tag() {
		case ChangeTag_Insert:
			insertions++
		case ChangeTag_Delete:
			deletions++
		}
	}

	if request.Digest {
		changes := oracleFnvNew()
		count := 0
		iterator := diff.IterAllChanges()
		for {
			change, ok := iterator.Next().Get()
			if !ok {
				break
			}
			count++
			changes.number(uint64(change.Tag()))
			changes.number(uint64(rs.OptionMapOr(change.OldIndex(), 0, func(index int) int { return index + 1 })))
			changes.number(uint64(rs.OptionMapOr(change.NewIndex(), 0, func(index int) int { return index + 1 })))
			changes.text(change.Value())
			changes.number(oracleBool(change.MissingNewline()))
		}
		text := oracleFnvNew()
		text.text(unifiedText)
		hunks := oracleFnvNew()
		hunkCount := 0
		for hunk := range unified.IterHunks() {
			hunkCount++
			hunks.text(hunk.Header().String())
			hunks.text(oracleOpsDigest(hunk.Ops()))
			hunks.text(hunk.String())
		}
		return map[string]any{
			"old_len":     len(diff.OldSlices()),
			"new_len":     len(diff.NewSlices()),
			"ops_len":     len(diff.Ops()),
			"ops":         oracleOpsDigest(diff.Ops()),
			"changes_len": count,
			"changes":     changes.hex(),
			"insertions":  insertions,
			"deletions":   deletions,
			"unified_len": len(unifiedText),
			"unified":     text.hex(),
			"hunks_len":   hunkCount,
			"hunks":       hunks.hex(),
			"ratio_bits":  math.Float32bits(diff.Ratio()),
		}
	}

	changes := []any{}
	for _, change := range diff.IterAllChanges().Collect() {
		changes = append(changes, map[string]any{
			"tag":             oracleChangeTag(change.Tag()),
			"sign":            change.Tag().String(),
			"old_index":       oracleOption(change.OldIndex()),
			"new_index":       oracleOption(change.NewIndex()),
			"value":           change.Value(),
			"as_str":          change.AsStr().Unwrap(),
			"lossy":           change.ToStringLossy(),
			"missing_newline": change.MissingNewline(),
			"display":         change.String(),
		})
	}
	byOp := []any{}
	for _, op := range diff.Ops() {
		for change := range diff.IterChanges(op).All() {
			byOp = append(byOp, []any{oracleChangeTag(change.Tag()), oracleOption(change.OldIndex()), oracleOption(change.NewIndex()), change.Value()})
		}
	}
	hunks := []any{}
	for hunk := range unified.IterHunks() {
		var written strings.Builder
		if err := hunk.ToWriter(&written); err != nil {
			t.Fatal(err)
		}
		hunkChanges := []any{}
		for change := range hunk.IterChanges().All() {
			hunkChanges = append(hunkChanges, []any{oracleChangeTag(change.Tag()), change.Value()})
		}
		hunks = append(hunks, map[string]any{
			"header":  hunk.Header().String(),
			"ops":     oracleOpsJSON(hunk.Ops()),
			"hint":    hunk.MissingNewlineHint(),
			"text":    hunk.String(),
			"written": written.String(),
			"changes": hunkChanges,
		})
	}
	algorithm := [...]string{"Myers", "Patience", "Lcs"}[diff.Algorithm()]
	return map[string]any{
		"old":                diff.OldSlices(),
		"new":                diff.NewSlices(),
		"algorithm":          algorithm,
		"newline_terminated": diff.NewlineTerminated(),
		"ops":                oracleOpsJSON(diff.Ops()),
		"tuples":             oracleTagTuplesJSON(diff.Ops()),
		"changes":            changes,
		"by_op":              byOp,
		"insertions":         insertions,
		"deletions":          deletions,
		"grouped":            oracleGroupsJSON(diff.GroupedOps(radius)),
		"hunks":              hunks,
		"unified":            unifiedText,
		"ratio_bits":         math.Float32bits(diff.Ratio()),
	}
}

func oracleTokenize(request *oracleRequest) any {
	var tokens []string
	switch request.Kind {
	case "lines":
		tokens = DiffableStrTokenizeLines(request.Text)
	case "lines_and_newlines":
		tokens = DiffableStrTokenizeLinesAndNewlines(request.Text)
	case "words":
		tokens = DiffableStrTokenizeWords(request.Text)
	case "chars":
		tokens = DiffableStrTokenizeChars(request.Text)
	default:
		panic("unknown kind " + request.Kind)
	}
	return map[string]any{
		"tokens":            tokens,
		"ends_with_newline": DiffableStrEndsWithNewline(request.Text),
	}
}

func oracleCapture(t *testing.T, request *oracleRequest) any {
	old := oracleDecode[[]uint64](t, request.Old)
	new_ := oracleDecode[[]uint64](t, request.New)
	ops := CaptureDiffSlicesDeadline(request.algorithm(), old, new_, request.deadline())
	n := request.radius()
	grouped := GroupDiffOps(append([]DiffOp{}, ops...), n)
	ratio := math.Float32bits(GetDiffRatio(ops, len(old), len(new_)))
	if request.Digest {
		groups := oracleFnvNew()
		for _, group := range grouped {
			groups.text(oracleOpsDigest(group))
		}
		return map[string]any{
			"ops_len":     len(ops),
			"ops":         oracleOpsDigest(ops),
			"grouped_len": len(grouped),
			"grouped":     groups.hex(),
			"ratio_bits":  ratio,
		}
	}
	changes := []any{}
	slices := []any{}
	for _, op := range ops {
		for change := range DiffOpIterChanges(op, old, new_).All() {
			changes = append(changes, []any{oracleChangeTag(change.Tag()), oracleOption(change.OldIndex()), oracleOption(change.NewIndex()), change.Value()})
		}
		for _, slice := range DiffOpIterSlices(op, old, new_) {
			slices = append(slices, []any{oracleChangeTag(slice.F0), append([]uint64{}, slice.F1...)})
		}
	}
	return map[string]any{
		"ops":        oracleOpsJSON(ops),
		"tuples":     oracleTagTuplesJSON(ops),
		"changes":    changes,
		"slices":     slices,
		"grouped":    oracleGroupsJSON(grouped),
		"ratio_bits": ratio,
	}
}

func oracleRange(start *int, end *int, length int) Range {
	r := Range{Start: 0, End: length}
	if start != nil {
		r.Start = *start
	}
	if end != nil {
		r.End = *end
	}
	return r
}

func oracleRaw(t *testing.T, request *oracleRequest) any {
	old := oracleDecode[[]uint64](t, request.Old)
	new_ := oracleDecode[[]uint64](t, request.New)
	alg := request.algorithm()
	oldRange := oracleRange(request.OldStart, request.OldEnd, len(old))
	newRange := oracleRange(request.NewStart, request.NewEnd, len(new_))
	deadline := request.deadline()
	var ops []DiffOp
	switch request.Hook {
	case "replace":
		d := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
		rs.UnwrapUnit(AlgorithmsDiffDeadline(alg, d, old, oldRange, new_, newRange, deadline))
		ops = d.IntoInner().IntoOps()
	case "compact":
		d := AlgorithmsCompactNew(AlgorithmsReplaceNew(AlgorithmsCaptureNew()), old, new_)
		rs.UnwrapUnit(AlgorithmsDiffDeadline(alg, d, old, oldRange, new_, newRange, deadline))
		ops = d.IntoInner().IntoInner().IntoOps()
	case "compact_only":
		d := AlgorithmsCompactNew(AlgorithmsCaptureNew(), old, new_)
		rs.UnwrapUnit(AlgorithmsDiffDeadline(alg, d, old, oldRange, new_, newRange, deadline))
		ops = d.IntoInner().IntoOps()
	default:
		d := AlgorithmsCaptureNew()
		rs.UnwrapUnit(AlgorithmsDiffDeadline(alg, d, old, oldRange, new_, newRange, deadline))
		ops = d.IntoOps()
	}
	if request.Digest {
		return map[string]any{"ops_len": len(ops), "ops": oracleOpsDigest(ops)}
	}
	return map[string]any{"ops": oracleOpsJSON(ops)}
}

func oracleIdentify(t *testing.T, request *oracleRequest) any {
	old := oracleDecode[[]string](t, request.Old)
	new_ := oracleDecode[[]string](t, request.New)
	oldRange := oracleRange(request.OldStart, request.OldEnd, len(old))
	newRange := oracleRange(request.NewStart, request.NewEnd, len(new_))
	ih := AlgorithmsIdentifyDistinctNew[uint32](old, oldRange, new_, newRange)
	oldIds := []uint32{}
	for index := oldRange.Start; index < oldRange.End; index++ {
		oldIds = append(oldIds, ih.OldLookup()[index])
	}
	newIds := []uint32{}
	for index := newRange.Start; index < newRange.End; index++ {
		newIds = append(newIds, ih.NewLookup()[index])
	}
	return map[string]any{
		"old":       oldIds,
		"new":       newIds,
		"old_range": []int{ih.OldRange().Start, ih.OldRange().End},
		"new_range": []int{ih.NewRange().Start, ih.NewRange().End},
	}
}

func oracleOpFromJSON(op oracleOp) DiffOp {
	switch op.Op {
	case "delete":
		return &DiffOp_Delete{OldIndex: op.OldIndex, OldLen: op.OldLen, NewIndex: op.NewIndex}
	case "insert":
		return &DiffOp_Insert{OldIndex: op.OldIndex, NewIndex: op.NewIndex, NewLen: op.NewLen}
	case "replace":
		return &DiffOp_Replace{OldIndex: op.OldIndex, OldLen: op.OldLen, NewIndex: op.NewIndex, NewLen: op.NewLen}
	default:
		return &DiffOp_Equal{OldIndex: op.OldIndex, NewIndex: op.NewIndex, Len: op.Len}
	}
}

func oracleGroup(request *oracleRequest) any {
	ops := []DiffOp{}
	for _, op := range request.Ops {
		ops = append(ops, oracleOpFromJSON(op))
	}
	n := request.radius()
	grouped := GroupDiffOps(append([]DiffOp{}, ops...), n)
	headers := []string{}
	for _, group := range grouped {
		if len(group) > 0 {
			headers = append(headers, UdiffUnifiedHunkHeaderNew(group).String())
		}
	}
	// A script holding a replace op is not replayed through Replace: see the
	// oracle module.
	var replayed any
	hasReplace := false
	for _, op := range ops {
		hasReplace = hasReplace || op.Tag() == DiffTag_Replace
	}
	if !hasReplace {
		replaced := AlgorithmsReplaceNew(AlgorithmsCaptureNew())
		for _, op := range ops {
			rs.UnwrapUnit(op.ApplyToHook(replaced))
		}
		rs.UnwrapUnit(replaced.Finish())
		replayed = oracleOpsJSON(replaced.IntoInner().Ops())
	}
	return map[string]any{
		"grouped":    oracleGroupsJSON(grouped),
		"headers":    headers,
		"replaced":   replayed,
		"ratio_bits": math.Float32bits(GetDiffRatio(ops, request.OldLen, request.NewLen)),
	}
}

// The generators. Each is deterministic: fixed cases first, then cases from a
// seeded generator.

// oracleLineSets are the vocabularies lines are drawn from. A small one gives
// sequences full of repeated lines, which is where the algorithms have to
// choose between equally short scripts.
var oracleLineSets = [][]string{
	{"a", "b"},
	{"a", "b", "c", "d"},
	{"", "x", "y"},
	{"{", "}", "", "    return nil", "    x++", "}"},
	{"alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu"},
	{"é", "日本語", "🙂", "a\tb", " ", "  ", "x y", "\u2028", "\u0085", "\u00a0z"},
}

var oracleTerminators = []string{"\n", "\n", "\n", "\r\n", "\r"}

// oracleRandomLines returns count lines drawn from a vocabulary of the given
// size, without terminators.
func oracleRandomLines(random *rand.Rand, count int, vocabulary int) []string {
	lines := make([]string, count)
	if vocabulary <= 0 {
		set := oracleLineSets[random.IntN(len(oracleLineSets))]
		for i := range lines {
			lines[i] = set[random.IntN(len(set))]
		}
		return lines
	}
	for i := range lines {
		lines[i] = "l" + strconv.Itoa(random.IntN(vocabulary))
	}
	return lines
}

// oracleMutate returns a copy of lines after a few edits of the kinds people
// make to files: deleting, inserting, replacing, moving and duplicating
// blocks.
func oracleMutate(random *rand.Rand, lines []string, edits int, fresh func() string) []string {
	out := append([]string{}, lines...)
	for range edits {
		position := 0
		if len(out) > 0 {
			position = random.IntN(len(out) + 1)
		}
		span := 1 + random.IntN(4)
		if random.IntN(8) == 0 {
			span = 1 + random.IntN(40)
		}
		end := min(position+span, len(out))
		switch random.IntN(6) {
		case 0: // delete
			out = append(out[:position:position], out[end:]...)
		case 1: // insert fresh lines
			block := make([]string, span)
			for i := range block {
				block[i] = fresh()
			}
			out = append(out[:position:position], append(block, out[position:]...)...)
		case 2: // replace
			for i := position; i < end; i++ {
				out[i] = fresh()
			}
		case 3: // move a block
			block := append([]string{}, out[position:end]...)
			rest := append(out[:position:position], out[end:]...)
			target := 0
			if len(rest) > 0 {
				target = random.IntN(len(rest) + 1)
			}
			out = append(rest[:target:target], append(block, rest[target:]...)...)
		case 4: // duplicate a block
			block := append([]string{}, out[position:end]...)
			out = append(out[:end:end], append(block, out[end:]...)...)
		case 5: // swap two lines
			if len(out) >= 2 {
				i, j := random.IntN(len(out)), random.IntN(len(out))
				out[i], out[j] = out[j], out[i]
			}
		}
	}
	return out
}

// oracleJoin turns lines into text. style picks the terminators: 0 is `\n`
// everywhere, 1 is `\r\n`, 2 is a mixture; final says whether the last line
// is terminated.
func oracleJoin(random *rand.Rand, lines []string, style int, final bool) string {
	var text strings.Builder
	for index, line := range lines {
		text.WriteString(line)
		if index == len(lines)-1 && !final {
			break
		}
		switch style {
		case 0:
			text.WriteString("\n")
		case 1:
			text.WriteString("\r\n")
		default:
			text.WriteString(oracleTerminators[random.IntN(len(oracleTerminators))])
		}
	}
	return text.String()
}

// oracleTextPair returns two related texts.
func oracleTextPair(random *rand.Rand, maxLines int, vocabulary int) (string, string) {
	count := random.IntN(maxLines + 1)
	old := oracleRandomLines(random, count, vocabulary)
	fresh := func() string {
		if vocabulary <= 0 {
			set := oracleLineSets[random.IntN(len(oracleLineSets))]
			return set[random.IntN(len(set))]
		}
		if random.IntN(2) == 0 {
			return "n" + strconv.Itoa(random.IntN(vocabulary))
		}
		return "l" + strconv.Itoa(random.IntN(vocabulary))
	}
	var new_ []string
	switch random.IntN(10) {
	case 0: // unrelated
		new_ = oracleRandomLines(random, random.IntN(maxLines+1), vocabulary)
	case 1: // identical
		new_ = append([]string{}, old...)
	default:
		new_ = oracleMutate(random, old, 1+random.IntN(6), fresh)
	}
	style := random.IntN(4)
	if style == 3 {
		style = 0
	}
	oldText := oracleJoin(random, old, style, random.IntN(4) != 0)
	newText := oracleJoin(random, new_, style, random.IntN(4) != 0)
	return oldText, newText
}

var oracleFixedTexts = [][2]string{
	{"", ""},
	{"", "a"},
	{"a", ""},
	{"", "\n"},
	{"\n", ""},
	{"a", "a"},
	{"a\n", "a"},
	{"a", "a\n"},
	{"a\n", "b"},
	{"a\nb\nc", "a\nb\nC"},
	{"a\nb", "a\nc\n"},
	{"\n\n\n", "\n\n"},
	{"\r\n", "\n"},
	{"a\r\nb\r\n", "a\nb\n"},
	{"a\rb\rc", "a\rB\rc"},
	{"a\r", "a\r\n"},
	{"\r\r\n\n\r", "\r\n\r\n"},
	{"\u0018\n\n", "\n\n\r"},
	{"Hello World\nsome stuff here\nsome more stuff here\n", "Hello World\nsome amazing stuff here\nsome more stuff here\n"},
	{"Hello World\nsome stuff here\nsome more stuff here\n\nAha stuff here\nand more stuff", "Stuff\nHello World\nsome amazing stuff here\nsome more stuff here\n"},
	{"1\n2\n3\n", "1\n99\n3\n"},
	{"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\nz\nA\nB\nC\nD\nE\nF\nG\nH\nI\nJ\nK\nL\nM\nN\nO\nP\nQ\nR\nS\nT\nU\nV\nW\nX\nY\nZ", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\nS\nt\nu\nv\nw\nx\ny\nz\nA\nB\nC\nD\nE\nF\nG\nH\nI\nJ\nK\nL\nM\nN\no\nP\nQ\nR\nS\nT\nU\nV\nW\nX\nY\nZ"},
	{"a\na\na\na\n", "a\na\n"},
	{"a\nb\na\nb\na\nb\n", "b\na\nb\na\nb\na\n"},
	{"x\na\nb\nc\ny\n", "y\na\nb\nc\nx\n"},
	{"one\ntwo\nthree\nfour\nfive\n", "four\nfive\none\ntwo\nthree\n"},
	{"é\n日本語\n🙂\n", "é\n日本\n🙂"},
	{"a\n\n\n\nb\n", "a\n\n\nb\n\n"},
	{"}\n\n{\n}\n", "}\n\n{\n}\n\n{\n}\n"},
	{"A\nB\nC\nA\nB\nB\nA\n", "C\nB\nA\nB\nA\nC\n"},
}

func oracleRandom(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x6d6a6f6c6e6972))
}

// oracleSetPassedDeadline gives a request a deadline that has already
// passed, unless the request runs the LCS algorithm straight into the Replace
// hook. LCS then reports the same deletion twice (it does not advance past
// the range it gave up on), which trips a `debug_assert_eq!` in Replace: the
// crate panics in a debug build and carries on in a release build. The Go
// tests are a debug build and the oracle is a release build, so that one
// combination cannot be compared. Behind Compact, which merges the two
// deletions first or reads out of bounds trying, the two agree.
func oracleSetPassedDeadline(request map[string]any) {
	if request["alg"] == "lcs" && request["hook"] == "replace" {
		return
	}
	request["deadline"] = "passed"
}

func oracleAlgName(random *rand.Rand) string {
	switch random.IntN(6) {
	case 0:
		return "patience"
	case 1:
		return "lcs"
	default:
		return "myers"
	}
}

// TestDiffAgainstRust compares whole text diffs: tokens, ops, changes,
// grouping, every hunk and the unified text.
func TestDiffAgainstRust(t *testing.T) {
	oracle.Check(t, "similar", "diff", func(yield func(any)) {
		for _, pair := range oracleFixedTexts {
			yield(map[string]any{"op": "lines", "old": pair[0], "new": pair[1]})
			yield(map[string]any{"op": "lines", "old": pair[0], "new": pair[1], "header": []string{"expected", "actual"}})
			yield(map[string]any{"op": "lines", "old": pair[0], "new": pair[1], "radius": 0, "hint": false, "header": []string{"a/x\t2026", "b/x"}})
			yield(map[string]any{"op": "lines", "old": pair[0], "new": pair[1], "radius": 1, "deadline": "passed"})
			yield(map[string]any{"op": "lines", "old": pair[0], "new": pair[1], "alg": "patience"})
			yield(map[string]any{"op": "lines", "old": pair[0], "new": pair[1], "alg": "lcs", "newline_terminated": false})
			yield(map[string]any{"op": "words", "old": pair[0], "new": pair[1]})
			yield(map[string]any{"op": "chars", "old": pair[0], "new": pair[1], "radius": 2})
		}
		// What Mjolnir's callers do: `from_lines(..).unified_diff().header(..)`,
		// a timeout with context radius 3, and `from_slices`.
		yield(map[string]any{"op": "lines", "old": "a\nb\n", "new": "a\nc\n", "timeout_ms": 250, "radius": 3})
		yield(map[string]any{"op": "slices", "old": []string{"foo", "bar", "baz"}, "new": []string{"foo", "BAR", "baz"}})
		yield(map[string]any{"op": "slices", "old": []string{}, "new": []string{}})
		yield(map[string]any{"op": "slices", "old": []string{"a\n", "b"}, "new": []string{"a\n", "b\n"}, "newline_terminated": true})

		random := oracleRandom(1)
		for range 200 * oracleScale() {
			old, new_ := oracleTextPair(random, 22, -random.IntN(2))
			request := map[string]any{"op": "lines", "old": old, "new": new_, "alg": oracleAlgName(random)}
			if random.IntN(3) == 0 {
				request["radius"] = random.IntN(5)
			}
			if random.IntN(3) == 0 {
				request["header"] = []string{"a/file", "b/file"}
			}
			if random.IntN(6) == 0 {
				request["hint"] = false
			}
			if random.IntN(8) == 0 {
				oracleSetPassedDeadline(request)
			}
			if random.IntN(12) == 0 {
				request["deadline"] = "far"
			}
			if random.IntN(12) == 0 {
				request["newline_terminated"] = random.IntN(2) == 0
			}
			yield(request)
		}
		for range 40 * oracleScale() {
			old, new_ := oracleTextPair(random, 8, 0)
			op := "words"
			if random.IntN(2) == 0 {
				op = "chars"
			}
			yield(map[string]any{"op": op, "old": old, "new": new_, "alg": oracleAlgName(random), "radius": random.IntN(4)})
		}
		for range 40 * oracleScale() {
			old := oracleRandomLines(random, random.IntN(16), 5)
			new_ := oracleMutate(random, old, 1+random.IntN(4), func() string { return "n" + strconv.Itoa(random.IntN(5)) })
			yield(map[string]any{"op": "slices", "old": old, "new": new_, "alg": oracleAlgName(random), "radius": random.IntN(4)})
		}
	}, oracleAnswer(t))
}

// TestLargeDiffAgainstRust compares diffs of inputs past the 100-line point
// where the text differ first maps lines to integers, by digest.
func TestLargeDiffAgainstRust(t *testing.T) {
	oracle.Check(t, "similar", "large", func(yield func(any)) {
		random := oracleRandom(2)
		// Around the threshold: 99, 100 and 101 lines on either side.
		for _, counts := range [][2]int{{99, 99}, {100, 100}, {101, 100}, {100, 101}, {101, 101}, {0, 101}, {101, 0}, {1, 150}} {
			old := oracleRandomLines(random, counts[0], 7)
			new_ := oracleRandomLines(random, counts[1], 7)
			yield(map[string]any{"op": "lines", "old": oracleJoin(random, old, 0, true), "new": oracleJoin(random, new_, 0, true), "digest": true})
		}
		for index := range 70 * oracleScale() {
			vocabulary := []int{2, 3, 8, 40, 400, 5000}[index%6]
			count := 90 + random.IntN(700)
			old := oracleRandomLines(random, count, vocabulary)
			var new_ []string
			if index%7 == 0 {
				new_ = oracleRandomLines(random, 90+random.IntN(400), vocabulary)
			} else {
				new_ = oracleMutate(random, old, 1+random.IntN(30), func() string { return "n" + strconv.Itoa(random.IntN(vocabulary)) })
			}
			style := 0
			if index%5 == 0 {
				style = 2
			}
			request := map[string]any{
				"op":     "lines",
				"old":    oracleJoin(random, old, style, random.IntN(3) != 0),
				"new":    oracleJoin(random, new_, style, random.IntN(3) != 0),
				"digest": true,
				"header": []string{"a/large", "b/large"},
			}
			switch index % 9 {
			case 1:
				request["alg"] = "patience"
			case 2:
				request["deadline"] = "passed"
			case 3:
				request["radius"] = random.IntN(6)
			case 4:
				request["deadline"] = "far"
			}
			yield(request)
		}
	}, oracleAnswer(t))
}

func oracleNumbers(random *rand.Rand, count int, vocabulary int) []uint64 {
	numbers := make([]uint64, count)
	for i := range numbers {
		numbers[i] = uint64(random.IntN(vocabulary))
	}
	return numbers
}

func oracleMutateNumbers(random *rand.Rand, numbers []uint64, edits int, vocabulary int) []uint64 {
	lines := make([]string, len(numbers))
	for i, number := range numbers {
		lines[i] = strconv.FormatUint(number, 10)
	}
	lines = oracleMutate(random, lines, edits, func() string { return strconv.Itoa(random.IntN(vocabulary + 2)) })
	out := make([]uint64, len(lines))
	for i, line := range lines {
		out[i], _ = strconv.ParseUint(line, 10, 64)
	}
	return out
}

// TestAlgorithmsAgainstRust compares the three algorithms on sequences of
// numbers: bare, behind each clean-up hook, over sub-ranges, and with a
// deadline that has already passed.
func TestAlgorithmsAgainstRust(t *testing.T) {
	oracle.Check(t, "similar", "algorithms", func(yield func(any)) {
		fixed := [][2][]uint64{
			{{}, {}},
			{{1}, {}},
			{{}, {1}},
			{{1}, {1}},
			{{1}, {2}},
			{{0, 1, 2, 3, 4}, {0, 1, 2, 9, 4}},
			{{0, 1, 2, 3, 4, 4, 4, 5}, {0, 1, 2, 8, 9, 4, 4, 7}},
			{{0, 1, 3, 4, 5}, {0, 1, 4, 5, 8, 9}},
			{{0, 1, 2, 3, 4}, {0, 1, 2, 7, 8, 9}},
			{{11, 1, 2, 2, 3, 4, 4, 4, 5, 47, 19}, {10, 1, 2, 2, 8, 9, 4, 4, 7, 47, 18}},
			{{1, 2, 3, 4}, {1, 2, 3}},
			{{0}, {0, 0}},
			{{0, 0}, {0}},
			{{1, 2, 3}, {1, 2, 4}},
			{{1, 1, 1, 1}, {1, 1}},
			{{1, 2, 1, 2, 1, 2}, {2, 1, 2, 1, 2, 1}},
			{{1, 2, 3, 1, 2, 2, 1}, {3, 2, 1, 2, 1, 3}},
			{{1}, {2, 2, 2, 2, 2, 2, 2, 2, 2}},
			{{2, 2, 2, 2, 2, 2, 2, 2, 2}, {1}},
			{{1}, {2, 2, 2, 2, 1, 2, 2, 2, 2}},
		}
		for _, pair := range fixed {
			for _, alg := range []string{"myers", "patience", "lcs"} {
				yield(map[string]any{"op": "capture", "alg": alg, "old": pair[0], "new": pair[1]})
				passed := map[string]any{"op": "capture", "alg": alg, "old": pair[0], "new": pair[1], "radius": 1}
				oracleSetPassedDeadline(passed)
				yield(passed)
				for _, hook := range []string{"capture", "replace", "compact", "compact_only"} {
					yield(map[string]any{"op": "raw", "alg": alg, "hook": hook, "old": pair[0], "new": pair[1]})
					passed := map[string]any{"op": "raw", "alg": alg, "hook": hook, "old": pair[0], "new": pair[1]}
					oracleSetPassedDeadline(passed)
					yield(passed)
				}
			}
		}

		// Ranges the caller got wrong: past the end of a sequence, or with
		// the start after the end. Rust reads out of bounds and panics, or
		// treats the range as empty.
		for _, alg := range []string{"myers", "patience", "lcs"} {
			for _, hook := range []string{"capture", "replace", "compact"} {
				for _, ranges := range [][4]int{{0, 5, 0, 5}, {0, 3, 0, 5}, {0, 5, 0, 3}, {4, 6, 0, 3}, {0, 3, 4, 6}, {2, 1, 0, 3}, {0, 3, 3, 1}, {2, 1, 3, 2}, {1, 4, 1, 3}, {3, 3, 3, 3}} {
					yield(map[string]any{
						"op": "raw", "alg": alg, "hook": hook,
						"old": []uint64{1, 2, 3}, "new": []uint64{1, 2, 3},
						"old_start": ranges[0], "old_end": ranges[1], "new_start": ranges[2], "new_end": ranges[3],
					})
					yield(map[string]any{
						"op": "raw", "alg": alg, "hook": hook,
						"old": []uint64{1, 2, 3}, "new": []uint64{4, 5, 6},
						"old_start": ranges[0], "old_end": ranges[1], "new_start": ranges[2], "new_end": ranges[3],
					})
				}
			}
		}

		random := oracleRandom(3)
		for index := range 500 * oracleScale() {
			vocabulary := []int{1, 2, 2, 3, 4, 6, 12, 60}[index%8]
			old := oracleNumbers(random, random.IntN(30), vocabulary)
			var new_ []uint64
			if random.IntN(4) == 0 {
				new_ = oracleNumbers(random, random.IntN(30), vocabulary)
			} else {
				new_ = oracleMutateNumbers(random, old, 1+random.IntN(5), vocabulary)
			}
			request := map[string]any{"alg": oracleAlgName(random), "old": old, "new": new_}
			switch random.IntN(3) {
			case 0:
				request["op"] = "capture"
				request["radius"] = random.IntN(5)
			default:
				request["op"] = "raw"
				request["hook"] = []string{"capture", "replace", "compact", "compact_only"}[random.IntN(4)]
				if random.IntN(3) == 0 && len(old) > 0 && len(new_) > 0 {
					// A sub-range of each side.
					oldStart := random.IntN(len(old))
					newStart := random.IntN(len(new_))
					request["old_start"] = oldStart
					request["old_end"] = oldStart + random.IntN(len(old)-oldStart+1)
					request["new_start"] = newStart
					request["new_end"] = newStart + random.IntN(len(new_)-newStart+1)
				}
			}
			if random.IntN(8) == 0 {
				oracleSetPassedDeadline(request)
			}
			yield(request)
		}
		// Longer sequences, by digest.
		for index := range 200 * oracleScale() {
			vocabulary := []int{2, 3, 5, 20, 300}[index%5]
			count := 60 + random.IntN(400)
			alg := oracleAlgName(random)
			if alg == "lcs" {
				// The LCS table is quadratic in memory.
				count = 60 + random.IntN(120)
			}
			old := oracleNumbers(random, count, vocabulary)
			var new_ []uint64
			if index%6 == 0 {
				new_ = oracleNumbers(random, 60+random.IntN(count), vocabulary)
			} else {
				new_ = oracleMutateNumbers(random, old, 1+random.IntN(25), vocabulary)
			}
			request := map[string]any{"alg": alg, "old": old, "new": new_, "digest": true}
			if index%2 == 0 {
				request["op"] = "capture"
				request["radius"] = random.IntN(5)
			} else {
				request["op"] = "raw"
				request["hook"] = []string{"capture", "replace", "compact", "compact_only"}[random.IntN(4)]
			}
			if index%11 == 0 {
				oracleSetPassedDeadline(request)
			}
			yield(request)
		}
	}, oracleAnswer(t))
}

// TestTokenizeAgainstRust compares the tokenizers.
func TestTokenizeAgainstRust(t *testing.T) {
	oracle.Check(t, "similar", "tokenize", func(yield func(any)) {
		fixed := []string{
			"",
			"\n",
			"\r",
			"\r\n",
			"\n\r",
			"\r\r\n",
			"first\nsecond\rthird\r\nfourth\nlast",
			"\n\n",
			"foo    bar baz\n\n  aha",
			"abcfö❄\ufe0f",
			"a\u0085b\u00a0c\u1680d\u2000e\u200af\u2028g\u2029h\u202fi\u205fj\u3000k",
			"a\u200bb\u180ec\ufeffd\u2060e",
			"tab\tand\vvertical\fform feed",
			"trailing space \n",
			"💩🇦🇹🦠",
		}
		kinds := []string{"lines", "lines_and_newlines", "words", "chars"}
		for _, text := range fixed {
			for _, kind := range kinds {
				yield(map[string]any{"op": "tokenize", "kind": kind, "text": text})
			}
		}
		random := oracleRandom(4)
		pieces := []string{"a", "bc", " ", "  ", "\t", "\n", "\r", "\r\n", "é", "日", "🙂", "\u00a0", "\u2028", "\u0085", "\u200b", "x", "\v", "\f", "\u3000", "\u1680"}
		for range 300 * oracleScale() {
			var text strings.Builder
			for range random.IntN(14) {
				text.WriteString(pieces[random.IntN(len(pieces))])
			}
			yield(map[string]any{"op": "tokenize", "kind": kinds[random.IntN(len(kinds))], "text": text.String()})
		}
	}, oracleAnswer(t))
}

// TestGroupingAgainstRust compares `group_diff_ops`, the hunk header and the
// `Replace` hook on op lists written out by hand, and `IdentifyDistinct`.
func TestGroupingAgainstRust(t *testing.T) {
	oracle.Check(t, "similar", "grouping", func(yield func(any)) {
		yield(map[string]any{"op": "group", "ops": []any{}, "radius": 3})
		yield(map[string]any{"op": "identify", "old": []string{"", "foo", "bar", "baz"}, "new": []string{"", "foo", "blah", "baz"}, "old_start": 1, "old_end": 4, "new_start": 1, "new_end": 4})
		yield(map[string]any{"op": "identify", "old": []string{}, "new": []string{}})

		random := oracleRandom(5)
		for range 400 * oracleScale() {
			// A well-formed script: ops that follow one another in both
			// sequences, as every algorithm emits them.
			ops := []any{}
			oldIndex, newIndex := 0, 0
			if random.IntN(4) == 0 {
				oldIndex, newIndex = random.IntN(50), random.IntN(50)
			}
			for range random.IntN(9) {
				length := random.IntN(12)
				if random.IntN(5) == 0 {
					length = random.IntN(3)
				}
				other := random.IntN(5)
				switch random.IntN(4) {
				case 0:
					ops = append(ops, map[string]any{"op": "equal", "old_index": oldIndex, "new_index": newIndex, "len": length})
					oldIndex += length
					newIndex += length
				case 1:
					ops = append(ops, map[string]any{"op": "delete", "old_index": oldIndex, "old_len": length, "new_index": newIndex})
					oldIndex += length
				case 2:
					ops = append(ops, map[string]any{"op": "insert", "old_index": oldIndex, "new_index": newIndex, "new_len": length})
					newIndex += length
				case 3:
					ops = append(ops, map[string]any{"op": "replace", "old_index": oldIndex, "old_len": length, "new_index": newIndex, "new_len": other})
					oldIndex += length
					newIndex += other
				}
			}
			yield(map[string]any{"op": "group", "ops": ops, "radius": random.IntN(6), "old_len": oldIndex, "new_len": newIndex})
		}
		for range 100 * oracleScale() {
			old := oracleRandomLines(random, random.IntN(12), 4)
			new_ := oracleRandomLines(random, random.IntN(12), 6)
			request := map[string]any{"op": "identify", "old": old, "new": new_}
			if len(old) > 0 && len(new_) > 0 && random.IntN(2) == 0 {
				oldStart := random.IntN(len(old))
				newStart := random.IntN(len(new_))
				request["old_start"] = oldStart
				request["old_end"] = oldStart + random.IntN(len(old)-oldStart+1)
				request["new_start"] = newStart
				request["new_end"] = newStart + random.IntN(len(new_)-newStart+1)
			}
			yield(request)
		}
	}, oracleAnswer(t))
}
