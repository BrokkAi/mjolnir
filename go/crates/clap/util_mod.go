// Port of clap_builder 4.6.0 src/util/mod.rs, src/util/str_to_bool.rs and
// src/util/escape.rs

package clap

import "strings"

const utilSuccessCode = 0

// While sysexists.h defines EX_USAGE as 64, this doesn't seem to be used much in practice but
// instead 2 seems to be frequently used.
// Examples
// - GNU `ls` returns 2
// - Python's `argparse` returns 2
const utilUsageCode = 2

// utilEqIgnoreCase is eq_ignore_case without the crate's `unicode` feature,
// which is how Mjolnir builds clap: ASCII case only.
func utilEqIgnoreCase(left, right string) bool {
	return strEqIgnoreAsciiCase(left, right)
}

// True values are `y`, `yes`, `t`, `true`, `on`, and `1`.
var utilTrueLiterals = [6]string{"y", "yes", "t", "true", "on", "1"}

// False values are `n`, `no`, `f`, `false`, `off`, and `0`.
var utilFalseLiterals = [6]string{"n", "no", "f", "false", "off", "0"}

// utilStrToBool converts a string literal representation of truth to true or
// false.
//
// `false` values are `n`, `no`, `f`, `false`, `off`, and `0` (case
// insensitive).
//
// Any other value will be considered as `true`.
//
// Rust lower-cases the value with str::to_lowercase. No character outside
// ASCII lower-cases to a letter of these literals, so ASCII lower-casing
// selects the same strings.
func utilStrToBool(val string) (value bool, ok bool) {
	pat := val
	if !strings.ContainsFunc(val, func(r rune) bool { return r >= 0x80 }) {
		pat = strToAsciiLowercase(val)
	}
	for _, literal := range utilTrueLiterals {
		if literal == pat {
			return true, true
		}
	}
	for _, literal := range utilFalseLiterals {
		if literal == pat {
			return false, true
		}
	}
	return false, false
}

// utilEscapeNeedsEscaping is Escape::needs_escaping.
func utilEscapeNeedsEscaping(s string) bool {
	return len(s) == 0 || strContainsWhitespace(s)
}

// utilEscape is Escape's Display and to_cow: the value, in its Debug form
// when it is empty or holds whitespace.
func utilEscape(s string) string {
	if utilEscapeNeedsEscaping(s) {
		return strDebug(s)
	}
	return s
}
