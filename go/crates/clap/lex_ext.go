// Port of clap_lex 1.1.0 src/ext.rs

package clap

import "strings"

// The methods of clap_lex::OsStrExt, on an OsStr held as a Go string. They
// work on bytes: a needle is UTF-8, so a match always falls on a boundary.

// lexFind is OsStrExt::find: the byte index of the first match.
func lexFind(s string, needle string) (int, bool) {
	if len(s) < len(needle) {
		return 0, false
	}
	index := strings.Index(s, needle)
	if index < 0 {
		return 0, false
	}
	return index, true
}

// lexContains is OsStrExt::contains.
func lexContains(s string, needle string) bool {
	_, ok := lexFind(s, needle)
	return ok
}

// lexStripPrefix is OsStrExt::strip_prefix.
func lexStripPrefix(s string, prefix string) (string, bool) {
	if strings.HasPrefix(s, prefix) {
		return s[len(prefix):], true
	}
	return "", false
}

// lexStartsWith is OsStrExt::starts_with.
func lexStartsWith(s string, prefix string) bool {
	return strings.HasPrefix(s, prefix)
}

// lexSplit is OsStrExt::split: an iterator over substrings of this string
// slice, separated by characters matched by a pattern.
func lexSplit(s string, needle string) []string {
	if needle == "" {
		panic("assertion `left != right` failed\n  left: \"\"\n right: \"\"")
	}
	var parts []string
	haystack := s
	for {
		first, second, ok := lexSplitOnce(haystack, needle)
		if !ok {
			parts = append(parts, haystack)
			return parts
		}
		parts = append(parts, first)
		haystack = second
	}
}

// lexSplitOnce is OsStrExt::split_once: splits the string on the first
// occurrence of the specified delimiter and returns prefix before delimiter
// and suffix after delimiter.
func lexSplitOnce(s string, needle string) (string, string, bool) {
	start, ok := lexFind(s, needle)
	if !ok {
		return "", "", false
	}
	end := start + len(needle)
	return s[:start], s[end:], true
}
