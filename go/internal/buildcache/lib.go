// Port of mj-build-cache/src/lib.rs.

// Package buildcache is the caches Mjolnir keeps on the host a session runs
// on, so that a new session does not pay for work an earlier one already
// did: the mbx compiler cache and its installation (mbx.go,
// mbx_configuration.go, mbx_install.go, mbx_release.go), the shared clone
// cache (git_cache.go), the native build-tool caches (tool_cache.go), and the
// host they live on (cache_host.go). It builds on the targets and store
// packages and knows nothing of how a session is launched.
//
// The Rust crate's modules all live in this one package. Rust's
// `pub(crate) use mj_store::database` and `mj_targets::targets` re-exports
// have no Go equivalent: the files use packages store, targets and core
// directly (Rust's `targets::*` items that mj-targets re-exports from
// mj_core::targets are in package core, for example core.CommandSpec and
// core.TargetsTargetTemplate).
//
// Name collisions between the crate's modules are settled as in the core
// brief, the first module in path order keeping the name: git_cache's
// `prepare` is Prepare, mbx's is MbxPrepare and tool_cache's is
// ToolCachePrepare; git_cache's `checked` is checked, mbx's is mbxChecked and
// mbx/install's is installChecked; mbx's `LABEL` is label and mbx/release's
// is releaseLabel. The crate-private functions of mbx/configuration, which
// Rust always calls qualified as `configuration::...`, carry the
// `configuration` prefix.
package buildcache

import (
	"errors"
	"fmt"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// cfgTest is Rust's `cfg!(test)`: true only in this package's own tests
// (set by its test files), not in the tests of packages that import it.
var cfgTest bool

// The helpers below reproduce Rust std::path semantics on path text, so the
// ported code decides paths exactly as the Rust build does.

// pathComponentKind is Rust's `std::path::Component` variant, in its derived
// order.
type pathComponentKind int

const (
	pathComponentPrefix pathComponentKind = iota
	pathComponentRootDir
	pathComponentCurDir
	pathComponentParentDir
	pathComponentNormal
)

// pathComponent is one Rust path component and its byte span in the text.
type pathComponent struct {
	kind  pathComponentKind
	text  string
	start int
	end   int
}

func isPathSeparator(b byte) bool {
	return b == '/' || (runtime.GOOS == "windows" && b == '\\')
}

// pathComponents is Rust's `Path::components`: repeated separators and `.`
// after the first component are dropped, a leading `.` of a relative path is
// kept as CurDir, and `..` is ParentDir. On Windows a drive or UNC prefix
// comes first and both separators count.
func pathComponents(path string) []pathComponent {
	var components []pathComponent
	position := 0
	if runtime.GOOS == "windows" {
		if volume := filepath.VolumeName(path); volume != "" {
			components = append(components, pathComponent{kind: pathComponentPrefix, text: volume, end: len(volume)})
			position = len(volume)
		}
	}
	if position < len(path) && isPathSeparator(path[position]) {
		components = append(components, pathComponent{kind: pathComponentRootDir, text: path[position : position+1], start: position, end: position + 1})
		position++
	}
	first := true
	for position <= len(path) {
		end := position
		for end < len(path) && !isPathSeparator(path[end]) {
			end++
		}
		part := path[position:end]
		switch {
		case part == "":
		case part == ".":
			// Only a relative path without a prefix keeps a leading `.`.
			if first && len(components) == 0 {
				components = append(components, pathComponent{kind: pathComponentCurDir, text: part, start: position, end: end})
			}
		case part == "..":
			components = append(components, pathComponent{kind: pathComponentParentDir, text: part, start: position, end: end})
		default:
			components = append(components, pathComponent{kind: pathComponentNormal, text: part, start: position, end: end})
		}
		first = false
		position = end + 1
	}
	return components
}

// pathJoin is Rust's `Path::join`: an absolute part replaces the base,
// otherwise a separator is added unless the base already ends in one. It
// does not clean the result.
func pathJoin(base, part string) string {
	if runtime.GOOS == "windows" {
		if filepath.IsAbs(part) || filepath.VolumeName(part) != "" {
			return part
		}
		if part != "" && isPathSeparator(part[0]) {
			return filepath.VolumeName(base) + part
		}
		if base != "" && !isPathSeparator(base[len(base)-1]) {
			base += `\`
		}
		return base + part
	}
	if strings.HasPrefix(part, "/") {
		return part
	}
	if base != "" && !strings.HasSuffix(base, "/") {
		base += "/"
	}
	return base + part
}

// pathStartsWith is Rust's `Path::starts_with`: whether base is a component
// prefix of path.
func pathStartsWith(path, base string) bool {
	left, right := pathComponents(path), pathComponents(base)
	if len(right) > len(left) {
		return false
	}
	for i := range right {
		if left[i].kind != right[i].kind || left[i].text != right[i].text {
			return false
		}
	}
	return true
}

// pathStripPrefix is Rust's `Path::strip_prefix`: the rest of path after the
// components of base, or false when base is not a component prefix of path.
func pathStripPrefix(path, base string) (string, bool) {
	if !pathStartsWith(path, base) {
		return "", false
	}
	left, prefix := pathComponents(path), len(pathComponents(base))
	if prefix == len(left) {
		return "", true
	}
	return path[left[prefix].start:], true
}

// pathParent is Rust's `Path::parent`: the text before the last component,
// or false for a path that ends in its root or prefix, or is empty.
func pathParent(path string) (string, bool) {
	components := pathComponents(path)
	if len(components) == 0 {
		return "", false
	}
	last := components[len(components)-1]
	if last.kind == pathComponentPrefix || last.kind == pathComponentRootDir {
		return "", false
	}
	if len(components) == 1 {
		return "", true
	}
	return path[:components[len(components)-2].end], true
}

// pathEqual is Rust's `Path ==`, which compares by component: a trailing or
// repeated separator, or an inner `.`, does not make two paths differ.
func pathEqual(left, right string) bool {
	a, b := pathComponents(left), pathComponents(right)
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i].kind != b[i].kind || a[i].text != b[i].text {
			return false
		}
	}
	return true
}

// fromUTF8 is Rust's `std::str::from_utf8`, failing with the text of Rust's
// Utf8Error ("invalid utf-8 sequence of 1 bytes from index 3", "incomplete
// utf-8 byte sequence from index 3").
func fromUTF8(text []byte) (string, error) {
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

// lossy is Rust's `String::from_utf8_lossy`.
func lossy(bytes []byte) string {
	return strings.ToValidUTF8(string(bytes), "�")
}

// trimmedLossy is Rust's `String::from_utf8_lossy(bytes).trim()`.
func trimmedLossy(bytes []byte) string {
	return strings.TrimSpace(lossy(bytes))
}

// alternate is anyhow's `{:#}` for an error: a refusal attached as context
// over a failure (core.Refusal.Wrap) reads as the refusal, then the failure,
// as anyhow prints the chain. Other errors already carry their causes in
// their text, the way fmt.Errorf's `%w` builds them.
func alternate(err error) string {
	var refusal core.Refusal
	if errors.As(err, &refusal) && refusal.Unwrap() != nil && err.Error() == refusal.Message() {
		return refusal.Message() + ": " + alternate(refusal.Unwrap())
	}
	return err.Error()
}
