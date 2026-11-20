// Port of coretests 1.99.1 tests/pattern.rs

package strs

import (
	"fmt"
	"slices"
	"testing"
)

// searchAsserts is the search_asserts! macro: it runs the named searcher
// methods in order on a char searcher and compares the steps they return,
// written as the Rust test writes them.
func searchAsserts(t *testing.T, haystack string, needle rune, testname string, funcs []string, want []string) {
	t.Helper()
	searcher := newCharSearcher(haystack, needle)
	step := func(s searchStep) string {
		switch s.kind {
		case stepMatch:
			return fmt.Sprintf("Matches(%d, %d)", s.a, s.b)
		case stepReject:
			return fmt.Sprintf("Rejects(%d, %d)", s.a, s.b)
		default:
			return "Done"
		}
	}
	rangeOf := func(a, b int, ok bool) string {
		if !ok {
			return "Done"
		}
		return fmt.Sprintf("InRange(%d, %d)", a, b)
	}
	var got []string
	for _, f := range funcs {
		switch f {
		case "next":
			got = append(got, step(searcher.next()))
		case "next_back":
			got = append(got, step(searcher.nextBack()))
		case "next_match":
			got = append(got, rangeOf(searcher.nextMatch()))
		case "next_match_back":
			got = append(got, rangeOf(searcher.nextMatchBack()))
		case "next_reject":
			got = append(got, rangeOf(searcher.nextReject()))
		case "next_reject_back":
			got = append(got, rangeOf(searcher.nextRejectBack()))
		default:
			t.Fatalf("unknown searcher method %s", f)
		}
	}
	if !slices.Equal(got, want) {
		t.Errorf("%s:\n got  %v\n want %v", testname, got, want)
	}
}

const patternStress = "Áa🁀bÁꁁfg😁각กᘀ각aÁ각ꁁก😁a"

func TestPattern_test_simple_iteration(t *testing.T) {
	searchAsserts(t, "abcdeabcd", 'a', "forward iteration for ASCII string",
		[]string{"next", "next", "next", "next", "next", "next", "next", "next", "next", "next"},
		[]string{"Matches(0, 1)", "Rejects(1, 2)", "Rejects(2, 3)", "Rejects(3, 4)", "Rejects(4, 5)", "Matches(5, 6)", "Rejects(6, 7)", "Rejects(7, 8)", "Rejects(8, 9)", "Done"})
	searchAsserts(t, "abcdeabcd", 'a', "reverse iteration for ASCII string",
		[]string{"next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back"},
		[]string{"Rejects(8, 9)", "Rejects(7, 8)", "Rejects(6, 7)", "Matches(5, 6)", "Rejects(4, 5)", "Rejects(3, 4)", "Rejects(2, 3)", "Rejects(1, 2)", "Matches(0, 1)", "Done"})
	searchAsserts(t, "我爱我的猫", '我', "forward iteration for Chinese string",
		[]string{"next", "next", "next", "next", "next", "next"},
		[]string{"Matches(0, 3)", "Rejects(3, 6)", "Matches(6, 9)", "Rejects(9, 12)", "Rejects(12, 15)", "Done"})
	searchAsserts(t, "我的猫说meow", 'm', "forward iteration for mixed string",
		[]string{"next", "next", "next", "next", "next", "next", "next", "next", "next"},
		[]string{"Rejects(0, 3)", "Rejects(3, 6)", "Rejects(6, 9)", "Rejects(9, 12)", "Matches(12, 13)", "Rejects(13, 14)", "Rejects(14, 15)", "Rejects(15, 16)", "Done"})
	searchAsserts(t, "我的猫说meow", '猫', "reverse iteration for mixed string",
		[]string{"next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back", "next_back"},
		[]string{"Rejects(15, 16)", "Rejects(14, 15)", "Rejects(13, 14)", "Rejects(12, 13)", "Rejects(9, 12)", "Matches(6, 9)", "Rejects(3, 6)", "Rejects(0, 3)", "Done"})
}

func TestPattern_test_simple_search(t *testing.T) {
	searchAsserts(t, "abcdeabcdeabcde", 'a', "next_match for ASCII string",
		[]string{"next_match", "next_match", "next_match", "next_match"},
		[]string{"InRange(0, 1)", "InRange(5, 6)", "InRange(10, 11)", "Done"})
	searchAsserts(t, "abcdeabcdeabcde", 'a', "next_match_back for ASCII string",
		[]string{"next_match_back", "next_match_back", "next_match_back", "next_match_back"},
		[]string{"InRange(10, 11)", "InRange(5, 6)", "InRange(0, 1)", "Done"})
	searchAsserts(t, "abcdeab", 'a', "next_reject for ASCII string",
		[]string{"next_reject", "next_reject", "next_match", "next_reject", "next_reject"},
		[]string{"InRange(1, 2)", "InRange(2, 3)", "InRange(5, 6)", "InRange(6, 7)", "Done"})
	searchAsserts(t, "abcdeabcdeabcde", 'a', "next_reject_back for ASCII string",
		[]string{"next_reject_back", "next_reject_back", "next_match_back", "next_reject_back", "next_reject_back", "next_reject_back"},
		[]string{"InRange(14, 15)", "InRange(13, 14)", "InRange(10, 11)", "InRange(9, 10)", "InRange(8, 9)", "InRange(7, 8)"})
}

func TestPattern_test_stress_indices(t *testing.T) {
	searchAsserts(t, patternStress, 'x', "Indices of characters in stress test",
		[]string{"next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next", "next"},
		[]string{"Rejects(0, 2)", "Rejects(2, 3)", "Rejects(3, 7)", "Rejects(7, 8)", "Rejects(8, 10)", "Rejects(10, 13)", "Rejects(13, 14)", "Rejects(14, 15)", "Rejects(15, 19)", "Rejects(19, 22)", "Rejects(22, 25)", "Rejects(25, 28)", "Rejects(28, 31)", "Rejects(31, 32)", "Rejects(32, 34)", "Rejects(34, 37)", "Rejects(37, 40)", "Rejects(40, 43)", "Rejects(43, 47)", "Rejects(47, 48)", "Done"})
}

func TestPattern_test_forward_search_shared_bytes(t *testing.T) {
	searchAsserts(t, patternStress, 'Á', "Forward search for two-byte Latin character",
		[]string{"next_match", "next_match", "next_match", "next_match"},
		[]string{"InRange(0, 2)", "InRange(8, 10)", "InRange(32, 34)", "Done"})
	searchAsserts(t, patternStress, 'Á', "Forward search for two-byte Latin character; check if next() still works",
		[]string{"next_match", "next", "next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(0, 2)", "Rejects(2, 3)", "InRange(8, 10)", "Rejects(10, 13)", "InRange(32, 34)", "Rejects(34, 37)", "Done"})
	searchAsserts(t, patternStress, '각', "Forward search for three-byte Hangul character",
		[]string{"next_match", "next", "next_match", "next_match", "next_match"},
		[]string{"InRange(19, 22)", "Rejects(22, 25)", "InRange(28, 31)", "InRange(34, 37)", "Done"})
	searchAsserts(t, patternStress, '각', "Forward search for three-byte Hangul character; check if next() still works",
		[]string{"next_match", "next", "next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(19, 22)", "Rejects(22, 25)", "InRange(28, 31)", "Rejects(31, 32)", "InRange(34, 37)", "Rejects(37, 40)", "Done"})
	searchAsserts(t, patternStress, 'ก', "Forward search for three-byte Thai character",
		[]string{"next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(22, 25)", "Rejects(25, 28)", "InRange(40, 43)", "Rejects(43, 47)", "Done"})
	searchAsserts(t, patternStress, 'ก', "Forward search for three-byte Thai character; check if next() still works",
		[]string{"next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(22, 25)", "Rejects(25, 28)", "InRange(40, 43)", "Rejects(43, 47)", "Done"})
	searchAsserts(t, patternStress, '😁', "Forward search for four-byte emoji",
		[]string{"next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(15, 19)", "Rejects(19, 22)", "InRange(43, 47)", "Rejects(47, 48)", "Done"})
	searchAsserts(t, patternStress, '😁', "Forward search for four-byte emoji; check if next() still works",
		[]string{"next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(15, 19)", "Rejects(19, 22)", "InRange(43, 47)", "Rejects(47, 48)", "Done"})
	searchAsserts(t, patternStress, 'ꁁ', "Forward search for three-byte Yi character with repeated bytes",
		[]string{"next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(10, 13)", "Rejects(13, 14)", "InRange(37, 40)", "Rejects(40, 43)", "Done"})
	searchAsserts(t, patternStress, 'ꁁ', "Forward search for three-byte Yi character with repeated bytes; check if next() still works",
		[]string{"next_match", "next", "next_match", "next", "next_match"},
		[]string{"InRange(10, 13)", "Rejects(13, 14)", "InRange(37, 40)", "Rejects(40, 43)", "Done"})
}

func TestPattern_test_reverse_search_shared_bytes(t *testing.T) {
	searchAsserts(t, patternStress, 'Á', "Reverse search for two-byte Latin character",
		[]string{"next_match_back", "next_match_back", "next_match_back", "next_match_back"},
		[]string{"InRange(32, 34)", "InRange(8, 10)", "InRange(0, 2)", "Done"})
	searchAsserts(t, patternStress, 'Á', "Reverse search for two-byte Latin character; check if next_back() still works",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back", "next_back"},
		[]string{"InRange(32, 34)", "Rejects(31, 32)", "InRange(8, 10)", "Rejects(7, 8)", "InRange(0, 2)", "Done"})
	searchAsserts(t, patternStress, '각', "Reverse search for three-byte Hangul character",
		[]string{"next_match_back", "next_back", "next_match_back", "next_match_back", "next_match_back"},
		[]string{"InRange(34, 37)", "Rejects(32, 34)", "InRange(28, 31)", "InRange(19, 22)", "Done"})
	searchAsserts(t, patternStress, '각', "Reverse search for three-byte Hangul character; check if next_back() still works",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(34, 37)", "Rejects(32, 34)", "InRange(28, 31)", "Rejects(25, 28)", "InRange(19, 22)", "Rejects(15, 19)", "Done"})
	searchAsserts(t, patternStress, 'ก', "Reverse search for three-byte Thai character",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(40, 43)", "Rejects(37, 40)", "InRange(22, 25)", "Rejects(19, 22)", "Done"})
	searchAsserts(t, patternStress, 'ก', "Reverse search for three-byte Thai character; check if next_back() still works",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(40, 43)", "Rejects(37, 40)", "InRange(22, 25)", "Rejects(19, 22)", "Done"})
	searchAsserts(t, patternStress, '😁', "Reverse search for four-byte emoji",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(43, 47)", "Rejects(40, 43)", "InRange(15, 19)", "Rejects(14, 15)", "Done"})
	searchAsserts(t, patternStress, '😁', "Reverse search for four-byte emoji; check if next_back() still works",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(43, 47)", "Rejects(40, 43)", "InRange(15, 19)", "Rejects(14, 15)", "Done"})
	searchAsserts(t, patternStress, 'ꁁ', "Reverse search for three-byte Yi character with repeated bytes",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(37, 40)", "Rejects(34, 37)", "InRange(10, 13)", "Rejects(8, 10)", "Done"})
	searchAsserts(t, patternStress, 'ꁁ', "Reverse search for three-byte Yi character with repeated bytes; check if next_back() still works",
		[]string{"next_match_back", "next_back", "next_match_back", "next_back", "next_match_back"},
		[]string{"InRange(37, 40)", "Rejects(34, 37)", "InRange(10, 13)", "Rejects(8, 10)", "Done"})
}

func TestPattern_double_ended_regression_test(t *testing.T) {
	searchAsserts(t, "abcdeabcdeabcde", 'a', "alternating double ended search",
		[]string{"next_match", "next_match_back", "next_match", "next_match_back"},
		[]string{"InRange(0, 1)", "InRange(10, 11)", "InRange(5, 6)", "Done"})
	searchAsserts(t, "abcdeabcdeabcde", 'a', "triple double ended search for a",
		[]string{"next_match", "next_match_back", "next_match_back", "next_match_back"},
		[]string{"InRange(0, 1)", "InRange(10, 11)", "InRange(5, 6)", "Done"})
	searchAsserts(t, "abcdeabcdeabcde", 'd', "triple double ended search for d",
		[]string{"next_match", "next_match_back", "next_match_back", "next_match_back"},
		[]string{"InRange(3, 4)", "InRange(13, 14)", "InRange(8, 9)", "Done"})
	searchAsserts(t, patternStress, 'Á', "Double ended search for two-byte Latin character",
		[]string{"next_match", "next_match_back", "next_match", "next_match_back"},
		[]string{"InRange(0, 2)", "InRange(32, 34)", "InRange(8, 10)", "Done"})
	searchAsserts(t, patternStress, '각', "Reverse double ended search for three-byte Hangul character",
		[]string{"next_match_back", "next_back", "next_match", "next", "next_match_back", "next_match"},
		[]string{"InRange(34, 37)", "Rejects(32, 34)", "InRange(19, 22)", "Rejects(22, 25)", "InRange(28, 31)", "Done"})
	searchAsserts(t, patternStress, 'ก', "Double ended search for three-byte Thai character",
		[]string{"next_match", "next_back", "next", "next_match_back", "next_match"},
		[]string{"InRange(22, 25)", "Rejects(47, 48)", "Rejects(25, 28)", "InRange(40, 43)", "Done"})
	searchAsserts(t, patternStress, '😁', "Double ended search for four-byte emoji",
		[]string{"next_match_back", "next", "next_match", "next_back", "next_match"},
		[]string{"InRange(43, 47)", "Rejects(0, 2)", "InRange(15, 19)", "Rejects(40, 43)", "Done"})
	searchAsserts(t, patternStress, 'ꁁ', "Double ended search for three-byte Yi character with repeated bytes",
		[]string{"next_match", "next", "next_match_back", "next_back", "next_match"},
		[]string{"InRange(10, 13)", "Rejects(13, 14)", "InRange(37, 40)", "Rejects(34, 37)", "Done"})
}

// Port of alloctests 1.99.1 tests/str.rs, `mod pattern`: the whole sequence of
// steps of a searcher, forwards and backwards.

// cmpSearchToVec is cmp_search_to_vec: the steps of the searcher, taken from
// the front or (reversed) from the back, must be `right`, which must cover
// the haystack without gaps.
func cmpSearchToVec(t *testing.T, rev bool, searcher searcher, haystack string, right []searchStep) {
	t.Helper()
	var v []searchStep
	for {
		var step searchStep
		if !rev {
			step = searcher.next()
		} else {
			step = searcher.nextBack()
		}
		if step.kind == stepDone {
			break
		}
		v = append(v, step)
	}
	if rev {
		slices.Reverse(v)
	}

	firstIndex := 0
	for i, e := range right {
		if e.a <= e.b && e.a == firstIndex {
			firstIndex = e.b
		} else {
			t.Fatalf("Input skipped range at %d", i)
		}
	}
	if firstIndex != len(haystack) {
		t.Fatal("Did not cover whole input")
	}

	if !slices.Equal(v, right) {
		t.Errorf("rev=%v: got %v, want %v", rev, v, right)
	}
}

func TestStrPattern_searcher_steps(t *testing.T) {
	match := func(a, b int) searchStep { return searchStep{stepMatch, a, b} }
	reject := func(a, b int) searchStep { return searchStep{stepReject, a, b} }
	type makeTest struct {
		name     string
		searcher func(haystack string) searcher
		haystack string
		steps    []searchStep
	}
	str := func(needle string) func(string) searcher {
		return func(haystack string) searcher { return newStrSearcher(haystack, needle) }
	}
	chr := func(needle rune) func(string) searcher {
		return func(haystack string) searcher { return newCharSearcher(haystack, needle) }
	}
	for _, c := range []makeTest{
		{"str_searcher_ascii_haystack", str("bb"), "abbcbbd",
			[]searchStep{reject(0, 1), match(1, 3), reject(3, 4), match(4, 6), reject(6, 7)}},
		{"str_searcher_ascii_haystack_seq", str("bb"), "abbcbbbbd",
			[]searchStep{reject(0, 1), match(1, 3), reject(3, 4), match(4, 6), match(6, 8), reject(8, 9)}},
		{"str_searcher_empty_needle_ascii_haystack", str(""), "abbcbbd",
			[]searchStep{
				match(0, 0), reject(0, 1), match(1, 1), reject(1, 2), match(2, 2), reject(2, 3), match(3, 3), reject(3, 4),
				match(4, 4), reject(4, 5), match(5, 5), reject(5, 6), match(6, 6), reject(6, 7), match(7, 7),
			}},
		{"str_searcher_multibyte_haystack", str(" "), "├──", []searchStep{reject(0, 3), reject(3, 6), reject(6, 9)}},
		{"str_searcher_empty_needle_multibyte_haystack", str(""), "├──",
			[]searchStep{match(0, 0), reject(0, 3), match(3, 3), reject(3, 6), match(6, 6), reject(6, 9), match(9, 9)}},
		{"str_searcher_empty_needle_empty_haystack", str(""), "", []searchStep{match(0, 0)}},
		{"str_searcher_nonempty_needle_empty_haystack", str("├"), "", nil},
		{"char_searcher_ascii_haystack", chr('b'), "abbcbbd",
			[]searchStep{reject(0, 1), match(1, 2), match(2, 3), reject(3, 4), match(4, 5), match(5, 6), reject(6, 7)}},
		{"char_searcher_multibyte_haystack", chr(' '), "├──", []searchStep{reject(0, 3), reject(3, 6), reject(6, 9)}},
		{"char_searcher_short_haystack", chr('\U0001F4A9'), "* \t", []searchStep{reject(0, 1), reject(1, 2), reject(2, 3)}},
	} {
		t.Run(c.name+"/fwd", func(t *testing.T) { cmpSearchToVec(t, false, c.searcher(c.haystack), c.haystack, c.steps) })
		t.Run(c.name+"/bwd", func(t *testing.T) { cmpSearchToVec(t, true, c.searcher(c.haystack), c.haystack, c.steps) })
	}
}

func TestStrPattern_str_searcher_empty_needle_after_done(t *testing.T) {
	match := func(a, b int) searchStep { return searchStep{stepMatch, a, b} }
	reject := func(a, b int) searchStep { return searchStep{stepReject, a, b} }
	done := searchStep{kind: stepDone}

	// Empty needle and haystack
	{
		searcher := newStrSearcher("", "")

		eq(t, searcher.next(), match(0, 0))
		eq(t, searcher.next(), done)
		eq(t, searcher.next(), done)
		eq(t, searcher.next(), done)

		searcher = newStrSearcher("", "")

		eq(t, searcher.nextBack(), match(0, 0))
		eq(t, searcher.nextBack(), done)
		eq(t, searcher.nextBack(), done)
		eq(t, searcher.nextBack(), done)
	}
	// Empty needle and non-empty haystack
	{
		searcher := newStrSearcher("a", "")

		eq(t, searcher.next(), match(0, 0))
		eq(t, searcher.next(), reject(0, 1))
		eq(t, searcher.next(), match(1, 1))
		eq(t, searcher.next(), done)
		eq(t, searcher.next(), done)
		eq(t, searcher.next(), done)

		searcher = newStrSearcher("a", "")

		eq(t, searcher.nextBack(), match(1, 1))
		eq(t, searcher.nextBack(), reject(0, 1))
		eq(t, searcher.nextBack(), match(0, 0))
		eq(t, searcher.nextBack(), done)
		eq(t, searcher.nextBack(), done)
		eq(t, searcher.nextBack(), done)
	}
}
