// Port of std 1.99.1 src/ffi/os_str/tests.rs
//
// An OsString is a Go string here, so the tests of what a string does not
// have are not ported: its capacity (test_os_string_with_capacity,
// test_os_string_capacity, test_os_string_reserve,
// test_os_string_reserve_exact, test_os_str_clone_into), leaking and boxing
// (test_os_string_leak, into_boxed, boxed_default, into_rc, clone_to_uninit).
// OsStr::slice_encoded_bytes, split_at and split_at_checked are not provided
// (Mjolnir does not call them), so slice_encoded_bytes, slice_out_of_bounds,
// slice_mid_char, slice_invalid_data, slice_partial_utf8, slice_invalid_edge,
// slice_between_surrogates, slice_surrogate_edge, os_str_slice_at,
// os_str_slice_at_out_of_bounds, os_str_slice_at_mid_char,
// os_str_slice_at_unix and os_str_slice_at_windows are not ported either.

package rspath

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestFfiOsStr_test_os_string_clear(t *testing.T) {
	// OsString::clear is assigning the empty string.
	osString := "abc"
	if OsStrLen(osString) != 3 {
		t.Errorf("len = %d", OsStrLen(osString))
	}

	osString = ""
	if osString != "" {
		t.Errorf("%q", osString)
	}
	if OsStrLen(osString) != 0 {
		t.Errorf("len = %d", OsStrLen(osString))
	}
}

func TestFfiOsStr_test_os_string_join(t *testing.T) {
	for _, F := range bothFlavors {
		strs := []string{"hello", "dear", "world"}
		check := func(expected string, got string) {
			t.Helper()
			if expected != got {
				t.Errorf("%v: expected %q, got %q", F, expected, got)
			}
		}
		check("hello", F.OsStrJoin(strs[:1], " "))
		check("hello dear world", F.OsStrJoin(strs, " "))
		check("hellodearworld", F.OsStrJoin(strs, ""))
		check("hello.\n dear.\n world", F.OsStrJoin(strs, ".\n "))

		check("dear world", F.OsStrJoin(strs[1:], " "))

		stringsAbc := []string{"a", "b", "c"}
		check("a b c", F.OsStrJoin(stringsAbc, " "))
	}
}

func TestFfiOsStr_test_os_string_default(t *testing.T) {
	var osString string
	if osString != "" {
		t.Errorf("%q", osString)
	}
}

func TestFfiOsStr_test_os_str_is_empty(t *testing.T) {
	for _, F := range bothFlavors {
		osString := ""
		if !OsStrIsEmpty(osString) {
			t.Errorf("%v: a new OsString is not empty", F)
		}

		F.OsStringPush(&osString, "abc")
		if OsStrIsEmpty(osString) {
			t.Errorf("%v: %q is empty", F, osString)
		}

		osString = ""
		if !OsStrIsEmpty(osString) {
			t.Errorf("%v: a cleared OsString is not empty", F)
		}
	}
}

func TestFfiOsStr_test_os_str_len(t *testing.T) {
	for _, F := range bothFlavors {
		osString := ""
		if OsStrLen(osString) != 0 {
			t.Errorf("%v: len = %d", F, OsStrLen(osString))
		}

		F.OsStringPush(&osString, "abc")
		if OsStrLen(osString) != 3 {
			t.Errorf("%v: len = %d", F, OsStrLen(osString))
		}

		osString = ""
		if OsStrLen(osString) != 0 {
			t.Errorf("%v: len = %d", F, OsStrLen(osString))
		}
	}
}

func TestFfiOsStr_test_os_str_default(t *testing.T) {
	var osStr string
	if osStr != "" {
		t.Errorf("%q", osStr)
	}
}

func TestFfiOsStr_debug(t *testing.T) {
	// assert_eq!(format!("{:?}", OsStr::new(s)), format!("{:?}", s));
	s := "'single quotes'"
	for _, F := range bothFlavors {
		// A str does not escape a single quote, so its Debug form is this.
		if got := F.DebugString(s); got != `"'single quotes'"` {
			t.Errorf("%v: %s", F, got)
		}
		if formatterReady() {
			if got, want := rs.Format("{:?}", F.Debug(s)), rs.Format("{:?}", s); got != want {
				t.Errorf("%v: %s, and as a str %s", F, got, want)
			}
		}
	}
}
