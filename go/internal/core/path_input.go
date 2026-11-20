// Port of mj-core/src/path_input.rs.

// Interpretation of explicitly entered paths, without shell evaluation or I/O.

package core

import (
	"errors"
	"runtime"
	"unicode/utf8"
)

// UserHome is this user's home directory: the one owner of that answer in mj.
//
// On Windows, the shell's known-folder API ignores `USERPROFILE`. Node's
// `os.homedir()`, and so Claude Code, reads `USERPROFILE` first. Honouring
// it keeps mj on the same home as the agents it starts, and lets an isolated
// test instance relocate the home as `HOME` does on Unix.
func UserHome() (string, bool) {
	panic("unported: mj-core/src/path_input.rs user_home")
}

// NeedsHome is whether an input requests home expansion. Named users are not
// supported.
func NeedsHome(path string) (bool, error) {
	panic("unported: mj-core/src/path_input.rs needs_home")
}

// ExpandHome expands only a leading home component; leave all other
// components intact.
func ExpandHome(path string, home *string) (string, error) {
	panic("unported: mj-core/src/path_input.rs expand_home")
}

func ExpandLocal(path string) (string, error) {
	home, ok := UserHome()
	if !ok {
		return ExpandHome(path, nil)
	}
	return ExpandHome(path, &home)
}

// ValidateAbsoluteInput keeps target-specific validation after resolution,
// but rejects unsafe drafts early.
func ValidateAbsoluteInput(path string) error {
	panic("unported: mj-core/src/path_input.rs validate_absolute_input")
}

// FromGitBytes interprets one path exactly as Git emitted it in `-z` output.
//
// Git writes raw bytes, which are not UTF-8 on every filesystem, so the
// platform's own path encoding decides how they are read.
func FromGitBytes(raw []byte) (string, error) {
	if runtime.GOOS != "windows" {
		return string(raw), nil
	}
	if !utf8.Valid(raw) {
		return "", errors.New("Git path is not UTF-8")
	}
	return string(raw), nil
}
