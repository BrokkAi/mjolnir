// Port of mj-core/src/golden.rs.

// Small helpers for checked-in, full-surface golden tests.

package core

import (
	"runtime"
	"strings"
	"testing"
)

// AssertGolden compares rendered output with
// <manifestDir>/tests/golden/<name>.txt and fails t with a unified diff when
// they differ.
//
// Set MJ_UPDATE_GOLDEN=1 to write the current output to that file.
func AssertGolden(t testing.TB, manifestDir, name, actual string) {
	panic("unported: mj-core/src/golden.rs assert_golden")
}

// AssertPlatformGolden is like AssertGolden for surfaces that show platform
// key names (Cmd-V on macOS): macOS compares with <name>-macos.txt.
func AssertPlatformGolden(t testing.TB, manifestDir, name, actual string) {
	t.Helper()
	if runtime.GOOS == "darwin" {
		AssertGolden(t, manifestDir, name+"-macos", actual)
	} else {
		AssertGolden(t, manifestDir, name, actual)
	}
}

func normalizeOneTrailingNewline(text string) string {
	return strings.TrimSuffix(text, "\n")
}
