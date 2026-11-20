// Port of mj-core/src/path_input.rs.

// Interpretation of explicitly entered paths, without shell evaluation or I/O.

package core

import (
	"errors"
	"os"
	"os/user"
	"path/filepath"
	"runtime"
	"strings"
	"unicode/utf8"
)

// UserHome is this user's home directory: the one owner of that answer in mj.
//
// On Windows, the shell's known-folder API ignores `USERPROFILE`. Node's
// `os.homedir()`, and so Claude Code, reads `USERPROFILE` first. Honouring
// it keeps mj on the same home as the agents it starts, and lets an isolated
// test instance relocate the home as `HOME` does on Unix.
func UserHome() (string, bool) {
	if runtime.GOOS == "windows" {
		if profile := os.Getenv("USERPROFILE"); profile != "" && filepath.IsAbs(profile) {
			return profile, true
		}
	}
	// Rust's `dirs::home_dir`: a non-empty `HOME`, then the account database.
	if home := os.Getenv("HOME"); home != "" {
		return home, true
	}
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		return home, true
	}
	if account, err := user.Current(); err == nil && account.HomeDir != "" {
		return account.HomeDir, true
	}
	return "", false
}

// NeedsHome is whether an input requests home expansion. Named users are not
// supported.
func NeedsHome(path string) (bool, error) {
	components := pathComponents(path)
	if len(components) == 0 {
		return false, nil
	}
	first := components[0]
	if first.text == "~" {
		return true, nil
	}
	if first.kind == pathComponentNormal && strings.HasPrefix(first.text, "~") {
		return false, errors.New("Named-user paths are unsupported; use ~/ or an explicit path.")
	}
	return false, nil
}

// ExpandHome expands only a leading home component; leave all other
// components intact.
func ExpandHome(path string, home *string) (string, error) {
	needs, err := NeedsHome(path)
	if err != nil {
		return "", err
	}
	if !needs {
		return path, nil
	}
	if home == nil {
		return "", errors.New("Cannot expand ~: the home directory is unavailable.")
	}
	if !IsAbsoluteOnHostOrTarget(*home) {
		return "", errors.New("Cannot expand ~: the home directory must be absolute.")
	}
	rest := ""
	for _, component := range pathComponents(path)[1:] {
		rest = rustPathPush(rest, component.text)
	}
	return Join(*home, rest), nil
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
	if !IsAbsoluteOnHostOrTarget(path) {
		needs, err := NeedsHome(path)
		if err != nil {
			return err
		}
		if !needs {
			return errors.New("Enter an absolute path or a path starting with ~/.")
		}
	}
	for _, component := range pathComponents(path) {
		if component.kind == pathComponentParentDir {
			return errors.New("Path must not contain '..'.")
		}
	}
	return nil
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
