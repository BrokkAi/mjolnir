// Port of mj-targets/src/lib.rs.

// Package targets is Mjolnir's session targets: where a session's worker
// runs (a local directory, a container, an SSH host, an EC2 instance) and
// the commands that provision, control and clean it up. Nothing here knows
// about the store or about sessions' lifecycle; the packages above compose
// these plans.
//
// This is the lowest package of the daemon side, so the store, the session
// runtime and the lifecycle operations can each build on it without
// building on each other.
//
// The Rust crate's modules (aws_assume_role, aws_credentials,
// command_checks, image_pull_gate, signing_key, streamed_child, tailscale,
// target_storage, targets and its submodules, termination, test_isolation)
// all live in this one package, one file per Rust module. Rust's
// `pub use mj_core::targets::*` and `pub use mj_core::test_log` have no Go
// equivalent: callers use package core for those items directly.
package targets

import (
	"errors"
	"fmt"
	"runtime"
	"slices"
	"strings"
)

// cfgTest is Rust's `cfg!(test)`: true only in this package's own tests
// (lib_test.go sets it), not in the tests of packages that import it.
var cfgTest bool

// libSamePath compares paths by component, as Rust's Path equality does: a
// repeated separator, a `.` component and a trailing separator do not make
// two paths differ.
func libSamePath(left, right string) bool {
	components := func(path string) (bool, []string) {
		absolute := strings.HasPrefix(path, "/") || (runtime.GOOS == "windows" && strings.HasPrefix(path, `\`))
		parts := strings.FieldsFunc(path, func(r rune) bool {
			return r == '/' || (runtime.GOOS == "windows" && r == '\\')
		})
		kept := make([]string, 0, len(parts))
		for index, part := range parts {
			// Rust keeps a leading `.` of a relative path only.
			if part != "." || (index == 0 && !absolute && strings.HasPrefix(path, ".")) {
				kept = append(kept, part)
			}
		}
		return absolute, kept
	}
	leftAbsolute, leftParts := components(left)
	rightAbsolute, rightParts := components(right)
	return leftAbsolute == rightAbsolute && slices.Equal(leftParts, rightParts)
}

// lossyText is Rust's String::from_utf8_lossy for command output.
func lossyText(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}

// libLines is Rust's `str::lines`: lines split on `\n`, each without a
// trailing `\r`, and no empty last line after a final newline.
func libLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// libFromUTF8 is Rust's `std::str::from_utf8`, failing with the text of Rust's
// Utf8Error ("invalid utf-8 sequence of 1 bytes from index 3", "incomplete
// utf-8 byte sequence from index 3").
func libFromUTF8(text []byte) (string, error) {
	index := 0
	for index < len(text) {
		first := text[index]
		if first < 0x80 {
			index++
			continue
		}
		width := 0
		switch {
		case first >= 0xC2 && first <= 0xDF:
			width = 2
		case first >= 0xE0 && first <= 0xEF:
			width = 3
		case first >= 0xF0 && first <= 0xF4:
			width = 4
		}
		if width == 0 {
			return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		low, high := byte(0x80), byte(0xBF)
		switch first {
		case 0xE0:
			low = 0xA0
		case 0xED:
			high = 0x9F
		case 0xF0:
			low = 0x90
		case 0xF4:
			high = 0x8F
		}
		for offset := 1; offset < width; offset++ {
			if index+offset >= len(text) {
				return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
			}
			next := text[index+offset]
			if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
				return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", offset, index)
			}
		}
		index += width
	}
	return string(text), nil
}

// libASCIILower is Rust's `str::to_ascii_lowercase`.
func libASCIILower(text string) string {
	return strings.Map(func(r rune) rune {
		if 'A' <= r && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, text)
}

// libParseU64 is Rust's `str::parse::<u64>`: an optional leading '+' and
// decimal digits, failing with the text of Rust's ParseIntError.
func libParseU64(text string) (uint64, error) {
	digits := text
	if strings.HasPrefix(digits, "+") && len(digits) > 1 {
		digits = digits[1:]
	}
	if text == "" {
		return 0, errors.New("cannot parse integer from empty string")
	}
	var value uint64
	for i := 0; i < len(digits); i++ {
		digit := digits[i]
		if digit < '0' || digit > '9' {
			return 0, errors.New("invalid digit found in string")
		}
		next := value*10 + uint64(digit-'0')
		if value > (^uint64(0))/10 || next < value*10 {
			return 0, errors.New("number too large to fit in target type")
		}
		value = next
	}
	return value, nil
}

// libEqualFoldASCII is Rust's `str::eq_ignore_ascii_case`.
func libEqualFoldASCII(left, right string) bool {
	if len(left) != len(right) {
		return false
	}
	for i := 0; i < len(left); i++ {
		a, b := left[i], right[i]
		if 'A' <= a && a <= 'Z' {
			a += 'a' - 'A'
		}
		if 'A' <= b && b <= 'Z' {
			b += 'a' - 'A'
		}
		if a != b {
			return false
		}
	}
	return true
}
