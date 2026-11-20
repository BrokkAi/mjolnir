// Port of std 1.99.1 tests/path.rs (test_unix_absolute)
//
// The test reads std::env::current_dir, which is go/rs/rsenv; rsenv imports
// this package, so the test is in the external test package.

//go:build unix

package rspath_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsenv"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

func TestItPath_test_unix_absolute(t *testing.T) {
	absolute := rspath.Absolute
	assertEq := func(left, right string) {
		t.Helper()
		if left != right {
			t.Errorf("assertion `left == right` failed\n  left: %q\n right: %q", left, right)
		}
	}

	if _, err := absolute(""); err == nil {
		t.Errorf("absolute(\"\") is not an error")
	}

	relative := "a/b"
	expected := rs.Unwrap(rsenv.CurrentDir())
	rspath.Push(&expected, relative)
	assertEq(rs.Unwrap(absolute(relative)), expected)

	// Test how components are collected.
	assertEq(rs.Unwrap(absolute("/a/b/c")), "/a/b/c")
	assertEq(rs.Unwrap(absolute("/a//b/c")), "/a/b/c")
	assertEq(rs.Unwrap(absolute("//a/b/c")), "//a/b/c")
	assertEq(rs.Unwrap(absolute("///a/b/c")), "/a/b/c")
	assertEq(rs.Unwrap(absolute("/a/b/c/")), "/a/b/c/")
	assertEq(
		rs.Unwrap(absolute("/a/./b/../c/.././..")),
		"/a/b/../c/../..",
	)

	// Test leading `.` and `..` components
	curdir := rs.Unwrap(rsenv.CurrentDir())
	assertEq(rs.Unwrap(absolute("./a")), rspath.Join(curdir, "a"))
	assertEq(rs.Unwrap(absolute("../a")), rspath.Join(curdir, "../a")) // return /pwd/../a
}
