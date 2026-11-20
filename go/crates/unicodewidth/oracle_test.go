package unicodewidth

// Differential tests: the same inputs go to this package and, through
// go/port/oracle (module `unicode`), to the unicode-width crate itself.

import (
	"encoding/json"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// widthRuns run-length encodes one number for each scalar value in
// [start, end), as the oracle does: [first code point, number] wherever the
// number changes.
func widthRuns(start, end int, value func(c rune) int) [][2]int {
	runs := [][2]int{}
	for cp := start; cp < end; cp++ {
		if cp >= 0xD800 && cp <= 0xDFFF {
			continue
		}
		v := value(rune(cp))
		if len(runs) == 0 || runs[len(runs)-1][1] != v {
			runs = append(runs, [2]int{cp, v})
		}
	}
	return runs
}

func optionWidth(width rs.Option[int]) int {
	if w, ok := width.Get(); ok {
		return w
	}
	return -1
}

// TestDiffAgainstRustCharWidth compares the two character widths of every
// Unicode scalar value, and the two string widths of the string holding only
// that character.
func TestDiffAgainstRustCharWidth(t *testing.T) {
	oracle.Check(t, "unicode", "char_width", func(yield func(any)) {
		const block = 0x1000
		for start := 0; start < 0x110000; start += block {
			yield(map[string]any{"op": "char_width_range", "start": start, "end": start + block})
		}
	}, func(request json.RawMessage) any {
		var r struct {
			Start int `json:"start"`
			End   int `json:"end"`
		}
		if err := json.Unmarshal(request, &r); err != nil {
			t.Fatal(err)
		}
		return map[string]any{
			"width":         widthRuns(r.Start, r.End, func(c rune) int { return optionWidth(UnicodeWidthCharWidth(c)) }),
			"width_cjk":     widthRuns(r.Start, r.End, func(c rune) int { return optionWidth(UnicodeWidthCharWidthCjk(c)) }),
			"str_width":     widthRuns(r.Start, r.End, func(c rune) int { return UnicodeWidthStrWidth(string(c)) }),
			"str_width_cjk": widthRuns(r.Start, r.End, func(c rune) int { return UnicodeWidthStrWidthCjk(string(c)) }),
		}
	})
}

// TestDiffAgainstRustStrWidth compares the two string widths over the
// generated corpus (corpus_test.go).
func TestDiffAgainstRustStrWidth(t *testing.T) {
	oracle.Check(t, "unicode", "str_width", func(yield func(any)) {
		corpusBatches(func(batch []string) {
			yield(map[string]any{"op": "str_width", "strings": batch})
		})
	}, func(request json.RawMessage) any {
		var r struct {
			Strings []string `json:"strings"`
		}
		if err := json.Unmarshal(request, &r); err != nil {
			t.Fatal(err)
		}
		width := make([]int, len(r.Strings))
		widthCjk := make([]int, len(r.Strings))
		for index, s := range r.Strings {
			width[index] = UnicodeWidthStrWidth(s)
			widthCjk[index] = UnicodeWidthStrWidthCjk(s)
		}
		return map[string]any{"w": width, "c": widthCjk}
	})
}
