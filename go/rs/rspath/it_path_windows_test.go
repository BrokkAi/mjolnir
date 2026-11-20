// Port of std 1.99.1 tests/path.rs (test_windows_absolute)
//
// The test reads std::env::current_dir, which is go/rs/rsenv; rsenv imports
// this package, so the test is in the external test package. It asks
// GetFullPathNameW, so it runs on Windows only.

package rspath_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsenv"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

func TestItPath_test_windows_absolute(t *testing.T) {
	absolute := rspath.Absolute
	assertEq := func(left, right string) {
		t.Helper()
		if left != right {
			t.Errorf("assertion `left == right` failed\n  left: %q\n right: %q", left, right)
		}
	}

	// An empty path is an error.
	if _, err := absolute(""); err == nil {
		t.Errorf("absolute(\"\") is not an error")
	}

	relative := `a\b`
	expected := rs.Unwrap(rsenv.CurrentDir())
	rspath.Push(&expected, relative)
	assertEq(rs.Unwrap(absolute(relative)), expected)

	unchanged := func(path string) {
		t.Helper()
		assertEq(rs.Unwrap(absolute(path)), path)
	}

	unchanged(`C:\path\to\file`)
	unchanged(`C:\path\to\file\`)
	unchanged(`\\server\share\to\file`)
	unchanged(`\\server.\share.\to\file`)
	unchanged(`\\.\PIPE\name`)
	unchanged(`\\.\C:\path\to\COM1`)
	unchanged(`\\?\C:\path\to\file`)
	unchanged(`\\?\UNC\server\share\to\file`)
	unchanged(`\\?\PIPE\name`)
	// Verbatim paths are always unchanged, no matter what.
	unchanged(`\\?\path.\to/file..`)

	assertEq(
		rs.Unwrap(absolute(`C:\path..\to.\file.`)),
		`C:\path..\to\file`,
	)
	assertEq(rs.Unwrap(absolute(`COM1`)), `\\.\COM1`)
}
