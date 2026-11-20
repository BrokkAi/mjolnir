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
