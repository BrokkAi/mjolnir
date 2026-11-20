package unicodesegmentation

// Differential tests: the same inputs go to this package and, through
// go/port/oracle (module `unicode`), to the unicode-segmentation crate itself.
// The oracle's side of each operation is in go/port/oracle/src/unicode.rs.

import (
	"encoding/json"
	"fmt"
	"os"
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

// oracleProbeProperties checks that a probe test can see every property its
// tables give a character. properties names what the algorithm reads of a
// character (its category and the flags it tests); masks is the probe's
// answer for it. The answer for a character is a function of its properties
// alone, and for the comparison with Rust to prove the tables equal, it must
// also tell every two combinations of properties apart: then a character the
// two tables disagree on gets two different answers.
//
// Each combination that occurs among the scalar values is checked through
// three of its characters (the first, one in the middle and the last), which
// must all get the same answer, and against every other combination, whose
// answer must differ.
func oracleProbeProperties[P comparable](t *testing.T, properties func(c rune) P, masks func(c rune) [2]string) {
	t.Helper()
	type members struct{ first, middle, last rune }
	found := map[P]*members{}
	counts := map[P]int{}
	var order []P
	for cp := rune(0); cp < 0x110000; cp++ {
		if cp >= 0xD800 && cp <= 0xDFFF {
			continue
		}
		p := properties(cp)
		if m, ok := found[p]; ok {
			m.last = cp
		} else {
			found[p] = &members{cp, cp, cp}
			order = append(order, p)
		}
		counts[p]++
	}
	// The middle member: the character half way through the combination's
	// characters.
	seen := map[P]int{}
	for cp := rune(0); cp < 0x110000; cp++ {
		if cp >= 0xD800 && cp <= 0xDFFF {
			continue
		}
		p := properties(cp)
		if seen[p] == counts[p]/2 {
			found[p].middle = cp
		}
		seen[p]++
	}

	answers := map[[2]string]P{}
	for _, p := range order {
		m := found[p]
		answer := masks(m.first)
		for _, c := range []rune{m.middle, m.last} {
			if other := masks(c); other != answer {
				t.Errorf("U+%04X and U+%04X have the same properties %+v but different probe answers %q and %q",
					m.first, c, p, answer, other)
			}
		}
		if q, clash := answers[answer]; clash {
			t.Errorf("the probes do not tell %+v (U+%04X) from %+v (U+%04X): both answer %q",
				p, m.first, q, found[q].first, answer)
			continue
		}
		answers[answer] = p
	}
	t.Logf("%d combinations of properties, each with its own probe answer", len(order))
}

// graphemeProbeProperties is what the grapheme cursor reads of a character:
// its category (with the ASCII shortcut of grapheme_category), whether it is
// InCB=Extend and whether it is InCB=Linker.
type graphemeProbeProperties struct {
	Category tablesGraphemeGraphemeCat
	Extend   bool
	Linker   bool
}

// TestGraphemeProbesTellPropertiesApart checks that the probes of
// TestDiffAgainstRustGraphemeProbes see every property the grapheme tables
// give a character.
func TestGraphemeProbesTellPropertiesApart(t *testing.T) {
	cursor := GraphemeCursorNew(0, 0, true)
	var text, legacyText []byte
	oracleProbeProperties(t, func(c rune) graphemeProbeProperties {
		return graphemeProbeProperties{
			Category: cursor.graphemeCategory(c),
			Extend:   tablesDerivedPropertyInCBExtend(c),
			Linker:   tablesIsIncbLinker(c),
		}
	}, func(c rune) [2]string { return oracleGraphemeProbeMasks(&text, &legacyText, c) })
}

// wordProbeProperties is what the word iterators read of a character: its
// category, whether it is Extended_Pictographic, and whether it is
// alphanumeric (which decides whether `unicode_words` keeps a piece).
//
// WC_Format counts as WC_Extend. src/word.rs reads the two only together
// (`WC_Extend | WC_Format` in each of the six places it names either), so no
// output of the crate depends on which of the two a character has, and no
// probe can tell them apart.
type wordProbeProperties struct {
	Category     tablesWordWordCat
	Emoji        bool
	Alphanumeric bool
}

// TestWordProbesTellPropertiesApart checks that the probes of
// TestDiffAgainstRustWordProbes see every property the word tables give a
// character.
func TestWordProbesTellPropertiesApart(t *testing.T) {
	var text []byte
	oracleProbeProperties(t, func(c rune) wordProbeProperties {
		category := tablesWordWordCategoryOf(c)
		if category == tablesWordWordCat_WcFormat {
			category = tablesWordWordCat_WcExtend
		}
		return wordProbeProperties{
			Category:     category,
			Emoji:        wordIsEmoji(c),
			Alphanumeric: tablesUtilIsAlphanumeric(c),
		}
	}, func(c rune) [2]string { return oracleWordProbeMasks(&text, c) })
}

// oracleCursorAnswer is `cursor` of the oracle: it drives a GraphemeCursor
// over the string cut into chunks at the request's bounds, as the user of a
// rope does. It hands the cursor one chunk at a time, moves to the
// neighbouring chunk when asked, and provides the context asked for.
//
// The operations are `next`, `prev` and `is` (the three queries, on the chunk
// the driver is at), `next1`, `prev1` and `is1` (the same queries asked once,
// leaving any request for another chunk or for context unanswered), `seek`
// (go to the chunk holding the cursor), `set:N` (`set_cursor`), `chunk:K` (go
// to chunk K, whether or not it holds the cursor) and `ctx:K`
// (`provide_context` with chunk K, asked for or not). The reply is everything
// the cursor answered, in order; a panic ends the script and is recorded as
// the last entry.
func oracleCursorAnswer(t *testing.T, request json.RawMessage) any {
	var r struct {
		S        string   `json:"s"`
		Extended *bool    `json:"extended"`
		Offset   int      `json:"offset"`
		Bounds   []int    `json:"bounds"`
		Ops      []string `json:"ops"`
	}
	if err := json.Unmarshal(request, &r); err != nil {
		t.Fatal(err)
	}
	s := r.S
	extended := r.Extended == nil || *r.Extended
	bounds := append(append([]int{0}, r.Bounds...), len(s))
	type chunk struct {
		start int
		text  string
	}
	chunks := make([]chunk, 0, len(bounds)-1)
	for index := 0; index+1 < len(bounds); index++ {
		chunks = append(chunks, chunk{bounds[index], s[bounds[index]:bounds[index+1]]})
	}

	// The chunk that holds the character after `offset`; the last chunk at the end.
	chunkAt := func(offset int) int {
		for index := len(chunks) - 1; index >= 0; index-- {
			if chunks[index].start <= offset {
				return index
			}
		}
		return 0
	}
	number := func(text string) int {
		value, err := strconv.Atoi(text)
		if err != nil {
			t.Fatalf("operation argument %q: %v", text, err)
		}
		return value
	}

	trace := []any{}
	func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				trace = append(trace, map[string]string{"panic": rs.PanicInfoOf(recovered).Message})
			}
		}()
		cursor := GraphemeCursorNew(r.Offset, len(s), extended)
		at := chunkAt(r.Offset)
		for _, op := range r.Ops {
			if target, ok := strings.CutPrefix(op, "set:"); ok {
				cursor.SetCursor(number(target))
				trace = append(trace, []any{"set", cursor.CurCursor()})
				continue
			}
			if target, ok := strings.CutPrefix(op, "chunk:"); ok {
				at = number(target) % len(chunks)
				continue
			}
			if target, ok := strings.CutPrefix(op, "ctx:"); ok {
				index := number(target) % len(chunks)
				trace = append(trace, []any{"ctx", index})
				cursor.ProvideContext(chunks[index].text, chunks[index].start)
				continue
			}
			if op == "seek" {
				at = chunkAt(cursor.CurCursor())
				continue
			}
			// A query is repeated until it is answered, unless it is asked
			// once. Each round either moves to another chunk or provides
			// context, and both are bounded by the number of chunks.
			query, once := strings.CutSuffix(op, "1")
		rounds:
			for range 4*len(chunks) + 8 {
				start, text := chunks[at].start, chunks[at].text
				var found any
				var err error
				switch query {
				case "next":
					var boundary rs.Option[int]
					boundary, err = cursor.NextBoundary(text, start)
					if value, ok := boundary.Get(); ok {
						found = value
					}
				case "prev":
					var boundary rs.Option[int]
					boundary, err = cursor.PrevBoundary(text, start)
					if value, ok := boundary.Get(); ok {
						found = value
					}
				case "is":
					found, err = cursor.IsBoundary(text, start)
				default:
					rs.Panicf("unknown cursor operation {}", op)
				}
				if err == nil {
					trace = append(trace, []any{op, at, found, cursor.CurCursor()})
					break rounds
				}
				trace = append(trace, []any{op, at, oracleIncomplete(err), cursor.CurCursor()})
				if once {
					break rounds
				}
				switch e := err.(type) {
				case *GraphemeIncomplete_NextChunk:
					if at+1 >= len(chunks) {
						break rounds
					}
					at += 1
				case *GraphemeIncomplete_PrevChunk:
					if at == 0 {
						break rounds
					}
					at -= 1
				case *GraphemeIncomplete_PreContext:
					index := slices.IndexFunc(chunks, func(c chunk) bool {
						return c.start+len(c.text) == e.F0 && c.text != ""
					})
					if index < 0 {
						break rounds
					}
					cursor.ProvideContext(chunks[index].text, chunks[index].start)
				default:
					break rounds
				}
			}
		}
	}()
	return trace
}

// oracleIncomplete is `incomplete` of the oracle.
func oracleIncomplete(err error) any {
	switch e := err.(type) {
	case *GraphemeIncomplete_PreContext:
		return []any{"PreContext", e.F0}
	case *GraphemeIncomplete_PrevChunk:
		return "PrevChunk"
	case *GraphemeIncomplete_NextChunk:
		return "NextChunk"
	case *GraphemeIncomplete_InvalidOffset:
		return "InvalidOffset"
	default:
		panic(rs.Unreachable(err))
	}
}

const (
	// oracleCursorRecorded is the number of cursor scripts in the recorded
	// corpus.
	oracleCursorRecorded = 1500
	// oracleCursorLive is the number of further scripts, from another seed,
	// that a run with MJ_ORACLE=live compares as well.
	oracleCursorLive = 30000
)

// oracleCursorRequests yields count cursor scripts. The strings come from the
// corpus generator (corpus_test.go), whose units are the sequences that need
// pre-context across a chunk boundary: regional indicators, emoji ZWJ
// sequences, Indic conjuncts (GB9c), prepended marks, CR LF. Most scripts use
// the cursor as a rope would; some set it to any offset, move to a chunk that
// does not hold it, or provide context nobody asked for, where the crate
// returns InvalidOffset, gives a wrong answer or panics, and the port must do
// the same.
func oracleCursorRequests(yield func(any), seed uint64, count int) {
	r := &corpusRand{state: seed}
	fixed := []string{
		"", "a", "abcd", "\r\n", "\n\r\n\r",
		"\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4",
		"\U0001F1FA\U0001F1F8\U0001F1FA\U0001F1F8\U0001F1FA\U0001F1F8",
		"\U0001F469\u200D\U0001F52C", "\U0001F352\u200D\U0001F951", "\U0001F352\U0001F951",
		"\u0939\u093F\u0928\u094D\u0926\u0940", "\u0915\u094D\u200D\u0937", "\u0600\u0600 a",
	}
	for index := range count {
		var s string
		if index < len(fixed) {
			s = fixed[index]
		} else {
			s = r.corpusString(6)
		}
		// The char boundaries of s, which every bound and most offsets are.
		boundaries := []int{}
		for offset := range s {
			boundaries = append(boundaries, offset)
		}
		boundaries = append(boundaries, len(s))
		anyOffset := func() int {
			if r.chance(92) {
				return boundaries[r.intn(len(boundaries))]
			}
			return r.intn(len(s) + 2)
		}

		var bounds []int
		switch n := r.intn(10); {
		case n < 2 && len(boundaries) > 1:
			// Every character a chunk of its own.
			bounds = slices.Clone(boundaries[1 : len(boundaries)-1])
		case n < 9:
			for range r.intn(5) {
				bounds = append(bounds, boundaries[r.intn(len(boundaries))])
			}
			slices.Sort(bounds)
		}
		chunks := len(bounds) + 1

		ops := []string{}
		for range 1 + r.intn(10) {
			switch n := r.intn(100); {
			case n < 34:
				ops = append(ops, "next")
			case n < 56:
				ops = append(ops, "prev")
			case n < 68:
				ops = append(ops, "is")
			case n < 78:
				// A query asked once, often followed by context, the right
				// chunk or not.
				ops = append(ops, []string{"next1", "prev1", "is1"}[r.intn(3)])
				if r.chance(30) {
					ops = append(ops, "ctx:"+strconv.Itoa(r.intn(chunks)))
				}
			case n < 85:
				ops = append(ops, "seek")
			case n < 91:
				ops = append(ops, "set:"+strconv.Itoa(anyOffset()))
			case n < 98:
				ops = append(ops, "chunk:"+strconv.Itoa(r.intn(chunks)))
			default:
				ops = append(ops, "ctx:"+strconv.Itoa(r.intn(chunks)))
			}
		}

		yield(map[string]any{
			"op":       "cursor",
			"s":        s,
			"extended": r.chance(85),
			"offset":   anyOffset(),
			"bounds":   bounds,
			"ops":      ops,
		})
	}
}

// TestDiffAgainstRustCursor compares a GraphemeCursor driven over strings in
// chunks (oracleCursorAnswer) with the crate's, script for script: every
// answer, every request for another chunk or for context, and every panic.
func TestDiffAgainstRustCursor(t *testing.T) {
	oracle.Check(t, "unicode", "cursor", func(yield func(any)) {
		oracleCursorRequests(yield, 0x637572736F72, oracleCursorRecorded)
		if os.Getenv("MJ_ORACLE") == "live" {
			oracleCursorRequests(yield, 0x6C697665, oracleCursorLive)
		}
	}, func(request json.RawMessage) any {
		return oracleCursorAnswer(t, request)
	})
}
