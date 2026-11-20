package unicodesegmentation

// Differential tests: the same inputs go to this package and, through
// go/port/oracle (module `unicode`), to the unicode-segmentation crate itself.
// The oracle's side of each operation is in go/port/oracle/src/unicode.rs.

import (
	"encoding/json"
	"fmt"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// oraclePiece is one item of an iterator with offsets: `(usize, &str)`.
type oraclePiece struct {
	offset int
	text   string
}

// oracleIndices and oraclePlain are the two shapes the iterators of this
// package have.
type oracleIndices interface {
	Next() rs.Option[rs.Tuple2[int, string]]
	NextBack() rs.Option[rs.Tuple2[int, string]]
}

type oraclePlain interface {
	Next() rs.Option[string]
	NextBack() rs.Option[string]
}

func oracleForward(iter oracleIndices) []oraclePiece {
	pieces := []oraclePiece{}
	for {
		item, ok := iter.Next().Get()
		if !ok {
			return pieces
		}
		pieces = append(pieces, oraclePiece{item.F0, item.F1})
	}
}

func oracleBackward(iter oracleIndices) []oraclePiece {
	pieces := []oraclePiece{}
	for {
		item, ok := iter.NextBack().Get()
		if !ok {
			return pieces
		}
		pieces = append(pieces, oraclePiece{item.F0, item.F1})
	}
}

func oraclePlainForward(iter oraclePlain) []string {
	pieces := []string{}
	for {
		item, ok := iter.Next().Get()
		if !ok {
			return pieces
		}
		pieces = append(pieces, item)
	}
}

func oraclePlainBackward(iter oraclePlain) []string {
	pieces := []string{}
	for {
		item, ok := iter.NextBack().Get()
		if !ok {
			return pieces
		}
		pieces = append(pieces, item)
	}
}

// oracleBothEnds is one item taken by oracleFromBothEnds, with the end it
// came from.
type oracleBothEnds struct {
	front bool
	piece oraclePiece
}

// oracleFromBothEnds takes pieces from both ends of an iterator in turn, the
// front first, and returns them in the order taken.
func oracleFromBothEnds(iter oracleIndices) []oracleBothEnds {
	taken := []oracleBothEnds{}
	front := true
	for {
		var next rs.Option[rs.Tuple2[int, string]]
		if front {
			next = iter.Next()
		} else {
			next = iter.NextBack()
		}
		item, ok := next.Get()
		if !ok {
			return taken
		}
		taken = append(taken, oracleBothEnds{front, oraclePiece{item.F0, item.F1}})
		front = !front
	}
}

func oracleLengths(pieces []oraclePiece) []int {
	lengths := make([]int, len(pieces))
	for index, piece := range pieces {
		lengths[index] = len(piece.text)
	}
	return lengths
}

func oracleOffsetsAndLengths(pieces []oraclePiece) []int {
	flat := make([]int, 0, 2*len(pieces))
	for _, piece := range pieces {
		flat = append(flat, piece.offset, len(piece.text))
	}
	return flat
}

func oracleBothEndsValue(taken []oracleBothEnds) []int {
	flat := make([]int, 0, 3*len(taken))
	for _, item := range taken {
		side := 0
		if !item.front {
			side = 1
		}
		flat = append(flat, side, item.piece.offset, len(item.piece.text))
	}
	return flat
}

func oracleContiguous(pieces []oraclePiece) bool {
	offset := 0
	for _, piece := range pieces {
		if piece.offset != offset {
			return false
		}
		offset += len(piece.text)
	}
	return true
}

func oracleTexts(pieces []oraclePiece) []string {
	texts := make([]string, len(pieces))
	for index, piece := range pieces {
		texts[index] = piece.text
	}
	return texts
}

func oracleSorted(taken []oracleBothEnds) []oraclePiece {
	sorted := make([]oraclePiece, len(taken))
	for index, item := range taken {
		sorted[index] = item.piece
	}
	slices.SortFunc(sorted, func(a, b oraclePiece) int {
		if a.offset != b.offset {
			return a.offset - b.offset
		}
		return strings.Compare(a.text, b.text)
	})
	return sorted
}

// oracleRecordSplit is `record_split` of the oracle: the directions other
// than forward are recorded only when they do not yield the same pieces.
func oracleRecordSplit(extras map[string]any, key string, forward, backward []oraclePiece, both []oracleBothEnds, plain, plainBackward []string) {
	if !oracleContiguous(forward) {
		extras[key+"_offsets"] = oracleOffsetsAndLengths(forward)
	}
	reversed := slices.Clone(backward)
	slices.Reverse(reversed)
	if !slices.Equal(reversed, forward) {
		extras[key+"_back"] = oracleOffsetsAndLengths(backward)
	}
	if !slices.Equal(oracleSorted(both), forward) {
		extras[key+"_both"] = oracleBothEndsValue(both)
	}
	if !slices.Equal(plain, oracleTexts(forward)) {
		extras[key+"_plain"] = plain
	}
	if !slices.Equal(plainBackward, oracleTexts(backward)) {
		extras[key+"_plain_back"] = plainBackward
	}
}

// oracleSegmentOne is `segment_one` of the oracle.
func oracleSegmentOne(s string) any {
	extras := map[string]any{}

	extended := oracleForward(GraphemeIndices(s, true))
	oracleRecordSplit(extras, "g", extended,
		oracleBackward(GraphemeIndices(s, true)),
		oracleFromBothEnds(GraphemeIndices(s, true)),
		oraclePlainForward(Graphemes(s, true)),
		oraclePlainBackward(Graphemes(s, true)))

	legacy := oracleForward(GraphemeIndices(s, false))
	if !slices.Equal(legacy, extended) {
		extras["l"] = oracleLengths(legacy)
	}
	oracleRecordSplit(extras, "l", legacy,
		oracleBackward(GraphemeIndices(s, false)),
		oracleFromBothEnds(GraphemeIndices(s, false)),
		oraclePlainForward(Graphemes(s, false)),
		oraclePlainBackward(Graphemes(s, false)))

	bounds := oracleForward(SplitWordBoundIndices(s))
	oracleRecordSplit(extras, "w", bounds,
		oracleBackward(SplitWordBoundIndices(s)),
		oracleFromBothEnds(SplitWordBoundIndices(s)),
		oraclePlainForward(SplitWordBounds(s)),
		oraclePlainBackward(SplitWordBounds(s)))

	words := oracleForward(UnicodeWordIndices(s))
	wordsBackward := oracleBackward(UnicodeWordIndices(s))
	reversed := slices.Clone(wordsBackward)
	slices.Reverse(reversed)
	if !slices.Equal(reversed, words) {
		extras["u_back"] = oracleOffsetsAndLengths(wordsBackward)
	}
	both := oracleFromBothEnds(UnicodeWordIndices(s))
	if !slices.Equal(oracleSorted(both), words) {
		extras["u_both"] = oracleBothEndsValue(both)
	}
	if plain := oraclePlainForward(UnicodeWords(s)); !slices.Equal(plain, oracleTexts(words)) {
		extras["u_plain"] = plain
	}
	if plainBackward := oraclePlainBackward(UnicodeWords(s)); !slices.Equal(plainBackward, oracleTexts(wordsBackward)) {
		extras["u_plain_back"] = plainBackward
	}

	reply := []any{oracleLengths(extended), oracleLengths(bounds), oracleOffsetsAndLengths(words)}
	if len(extras) > 0 {
		reply = append(reply, extras)
	}
	return reply
}

// TestDiffAgainstRustSegment compares graphemes (extended and legacy), word
// bounds and Unicode words over the generated corpus (corpus_test.go): each
// iterator forwards, backwards and from both ends in turn, with and without
// offsets.
func TestDiffAgainstRustSegment(t *testing.T) {
	oracle.Check(t, "unicode", "segment", func(yield func(any)) {
		corpusBatches(func(batch []string) {
			yield(map[string]any{"op": "segment", "strings": batch})
		})
	}, func(request json.RawMessage) any {
		var r struct {
			Strings []string `json:"strings"`
		}
		if err := json.Unmarshal(request, &r); err != nil {
			t.Fatal(err)
		}
		replies := make([]any, len(r.Strings))
		for index, s := range r.Strings {
			replies[index] = oracle.Catch(func() any { return oracleSegmentOne(s) })
		}
		return replies
	})
}

// oracleProbeC stands for the character under test in a probe.
const oracleProbeC = rune(-1)

// The probes of the oracle's `grapheme_probe_range`: GRAPHEME_PROBES,
// GRAPHEME_LEGACY_PROBES and their separator.
var (
	oracleGraphemeProbes = [][]rune{
		{0x0D, oracleProbeC},                // CR x LF
		{oracleProbeC, 0x0A},                // CR x LF
		{0x61, oracleProbeC},                // x (Extend | ZWJ | SpacingMark)
		{oracleProbeC, 0x61},                // Prepend x
		{oracleProbeC, 0x300},               // Control / (Extend)
		{0x1100, oracleProbeC},              // L x (L | V | LV | LVT)
		{oracleProbeC, 0x11A8},              // (LV | V | LVT | T) x T
		{0xAC00, oracleProbeC},              // LV x (V | T)
		{0xAC01, oracleProbeC},              // LVT x T
		{oracleProbeC, 0x1160},              // (L | LV | V) x V
		{0x1F1E6, oracleProbeC},             // RI x RI
		{0x1F352, 0x200D, oracleProbeC},     // ExtPict ZWJ x ExtPict
		{0x1F352, oracleProbeC, 0x1F951},    // ExtPict (Extend* ZWJ) x ExtPict
		{0x915, 0x94D, oracleProbeC},        // Consonant Linker x Consonant
		{0x915, oracleProbeC, 0x915},        // Consonant (Linker) x Consonant
		{0x915, 0x94D, oracleProbeC, 0x915}, // Consonant Linker (Extend) x Consonant
		{oracleProbeC, 0x200D, 0x1F352},     // (ExtPict) ZWJ x ExtPict
		{oracleProbeC, 0x94D, 0x915},        // (Consonant) Linker x Consonant
		{oracleProbeC, oracleProbeC},
	}
	oracleGraphemeLegacyProbes = [][]rune{
		{0x61, oracleProbeC},         // x SpacingMark (GB9a)
		{oracleProbeC, 0x61},         // Prepend x (GB9b)
		{0x915, 0x94D, oracleProbeC}, // GB9c
	}
	oracleGraphemeProbeSeparator = rune(0x1)
)

// The probes of the oracle's `word_probe_range`: WORD_PROBES and their
// separator.
var (
	oracleWordProbes = [][]rune{
		{0x61, oracleProbeC, 0x61},    // AHLetter (MidLetter | MidNumLetQ) AHLetter
		{0x31, oracleProbeC, 0x31},    // Numeric (MidNum | MidNumLetQ) Numeric
		{0x5D0, oracleProbeC, 0x5D0},  // Hebrew_Letter Double_Quote Hebrew_Letter
		{0x5D0, oracleProbeC},         // Hebrew_Letter x Single_Quote
		{0x30A2, oracleProbeC},        // Katakana x Katakana
		{oracleProbeC, 0x30A2},        //
		{0x5F, oracleProbeC},          // ExtendNumLet x (AHLetter | Numeric | Katakana)
		{oracleProbeC, 0x5F},          //
		{0x1F1E6, oracleProbeC},       // RI x RI
		{0x0D, oracleProbeC},          // CR x LF
		{0x20, oracleProbeC},          // WSegSpace x WSegSpace
		{0x200D, oracleProbeC},        // ZWJ x ExtPict
		{0x21, oracleProbeC},          // x (Extend | Format | ZWJ)
		{oracleProbeC, 0x300},         // (Newline | CR | LF) /
		{oracleProbeC, 0x22, 0x5D0},   // (Hebrew_Letter) Double_Quote Hebrew_Letter
		{oracleProbeC, 0x2C, 0x31},    // (Numeric) MidNum Numeric
		{oracleProbeC, 0x3A, 0x61},    // (AHLetter) MidLetter AHLetter
		{0x21, oracleProbeC, 0x1F352}, // (ZWJ) x ExtPict
		{oracleProbeC, oracleProbeC},  //
		{oracleProbeC},                //
	}
	oracleWordProbeSeparator = rune('\n')
)

// oracleProbeString is `probe_string` of the oracle: the probes with c in
// place of the character under test, joined by the separator.
func oracleProbeString(buffer []byte, probes [][]rune, separator rune, c rune) []byte {
	buffer = buffer[:0]
	for index, probe := range probes {
		if index > 0 {
			buffer = utf8.AppendRune(buffer, separator)
		}
		for _, code := range probe {
			if code == oracleProbeC {
				buffer = utf8.AppendRune(buffer, c)
			} else {
				buffer = utf8.AppendRune(buffer, code)
			}
		}
	}
	return buffer
}

// oracleStartMask is `start_mask` of the oracle: which characters of text
// start a piece, four characters to a hexadecimal digit, the first character
// in the highest bit. starts holds the byte offset of each piece, ascending.
func oracleStartMask(text string, starts []int) string {
	var mask strings.Builder
	digit, bits := 0, 0
	next := 0
	for offset := range text {
		for next < len(starts) && starts[next] < offset {
			next++
		}
		digit <<= 1
		if next < len(starts) && starts[next] == offset {
			digit |= 1
		}
		bits++
		if bits == 4 {
			mask.WriteString(strconv.FormatInt(int64(digit), 16))
			digit, bits = 0, 0
		}
	}
	if bits > 0 {
		digit <<= 4 - bits
		mask.WriteString(strconv.FormatInt(int64(digit), 16))
	}
	return mask.String()
}

func oracleOffsets(iter oracleIndices) []int {
	offsets := []int{}
	for {
		item, ok := iter.Next().Get()
		if !ok {
			return offsets
		}
		offsets = append(offsets, item.F0)
	}
}

// oracleProbeBlock is the number of code points in one probe request.
const oracleProbeBlock = 0x1000

// oracleProbeRanges yields one probe request for each block of code points.
func oracleProbeRanges(op string, yield func(any)) {
	for start := 0; start < 0x110000; start += oracleProbeBlock {
		yield(map[string]any{"op": op, "start": start, "end": start + oracleProbeBlock})
	}
}

// oracleProbeRuns answers a probe request for the scalar values in
// [start, end): one answer for each, run-length encoded as the oracle's `Runs`
// does it. masks returns a function that answers for one character; each call
// of oracleProbeRuns gets its own, so that it can keep buffers.
func oracleProbeRuns(start, end int, masks func() func(c rune) [2]string) any {
	answer := masks()
	runs := []any{}
	var last [2]string
	for cp := start; cp < end; cp++ {
		if cp >= 0xD800 && cp <= 0xDFFF {
			continue
		}
		masks := answer(rune(cp))
		if len(runs) == 0 || masks != last {
			runs = append(runs, []any{cp, masks})
			last = masks
		}
	}
	return map[string]any{"runs": runs}
}

// oracleProbes answers the requests of a probe test. Segmenting a probe
// string for each of 1,114,112 code points is seconds of work, so the first
// request has every block of oracleProbeRanges computed, on all processors at
// once, and the requests are then answered from that.
type oracleProbes struct {
	masks  func() func(c rune) [2]string
	once   sync.Once
	blocks map[int]any
	failed any
}

func (p *oracleProbes) answer(t *testing.T, request json.RawMessage) any {
	var r struct {
		Start int `json:"start"`
		End   int `json:"end"`
	}
	if err := json.Unmarshal(request, &r); err != nil {
		t.Fatal(err)
	}
	p.once.Do(func() {
		p.blocks = map[int]any{}
		var lock sync.Mutex
		var group sync.WaitGroup
		starts := make(chan int)
		for range runtime.GOMAXPROCS(0) {
			group.Add(1)
			go func() {
				defer group.Done()
				for start := range starts {
					func() {
						defer func() {
							if recovered := recover(); recovered != nil {
								lock.Lock()
								p.failed = fmt.Sprintf("probes of U+%04X..U+%04X: panic: %v", start, start+oracleProbeBlock, recovered)
								lock.Unlock()
							}
						}()
						runs := oracleProbeRuns(start, start+oracleProbeBlock, p.masks)
						lock.Lock()
						p.blocks[start] = runs
						lock.Unlock()
					}()
				}
			}()
		}
		for start := 0; start < 0x110000; start += oracleProbeBlock {
			starts <- start
		}
		close(starts)
		group.Wait()
	})
	if p.failed != nil {
		t.Fatal(p.failed)
	}
	if runs, ok := p.blocks[r.Start]; ok && r.End == r.Start+oracleProbeBlock {
		return runs
	}
	return oracleProbeRuns(r.Start, r.End, p.masks)
}

// TestDiffAgainstRustGraphemeProbes compares, for every Unicode scalar value,
// where the grapheme boundaries fall in a string of probes built around it.
// The probes tell apart every property the grapheme tables give a character
// (TestGraphemeProbesTellPropertiesApart), so agreement on every code point
// means the tables are the crate's.
func TestDiffAgainstRustGraphemeProbes(t *testing.T) {
	probes := &oracleProbes{masks: func() func(c rune) [2]string {
		var text, legacyText []byte
		return func(c rune) [2]string { return oracleGraphemeProbeMasks(&text, &legacyText, c) }
	}}
	oracle.Check(t, "unicode", "grapheme_probes", func(yield func(any)) {
		oracleProbeRanges("grapheme_probe_range", yield)
	}, func(request json.RawMessage) any {
		return probes.answer(t, request)
	})
}

func oracleGraphemeProbeMasks(text, legacyText *[]byte, c rune) [2]string {
	*text = oracleProbeString(*text, oracleGraphemeProbes, oracleGraphemeProbeSeparator, c)
	*legacyText = oracleProbeString(*legacyText, oracleGraphemeLegacyProbes, oracleGraphemeProbeSeparator, c)
	extended, legacy := string(*text), string(*legacyText)
	return [2]string{
		oracleStartMask(extended, oracleOffsets(GraphemeIndices(extended, true))),
		oracleStartMask(legacy, oracleOffsets(GraphemeIndices(legacy, false))),
	}
}

// TestDiffAgainstRustWordProbes is the same comparison for word boundaries
// and for which pieces `unicode_words` keeps, which is where the Alphabetic
// and Numeric tables show (TestWordProbesTellPropertiesApart).
func TestDiffAgainstRustWordProbes(t *testing.T) {
	probes := &oracleProbes{masks: func() func(c rune) [2]string {
		var text []byte
		return func(c rune) [2]string { return oracleWordProbeMasks(&text, c) }
	}}
	oracle.Check(t, "unicode", "word_probes", func(yield func(any)) {
		oracleProbeRanges("word_probe_range", yield)
	}, func(request json.RawMessage) any {
		return probes.answer(t, request)
	})
}

func oracleWordProbeMasks(text *[]byte, c rune) [2]string {
	*text = oracleProbeString(*text, oracleWordProbes, oracleWordProbeSeparator, c)
	probe := string(*text)
	return [2]string{
		oracleStartMask(probe, oracleOffsets(SplitWordBoundIndices(probe))),
		oracleStartMask(probe, oracleOffsets(UnicodeWordIndices(probe))),
	}
}
