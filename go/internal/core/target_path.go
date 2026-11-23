// Port of mj-core/src/target_path.rs.

// Paths on the machine a session runs on.
//
// Every session target is a POSIX machine: a Linux container, an SSH or EC2
// host, or this machine when it is Unix (a Windows controller runs no local
// sessions). The controller keeps those paths as strings like its own, but
// they mean POSIX paths on every controller. On Windows, filepath.IsAbs
// requires a drive or UNC prefix and joining inserts `\`, so neither may
// decide or render a target path; these functions do.

package core

import (
	"path/filepath"
	"runtime"
	"strings"
)

// IsAbsolute is whether `path` starts at the POSIX root of its target. On
// Unix this is filepath.IsAbs.
func IsAbsolute(path string) bool {
	if runtime.GOOS != "windows" {
		return strings.HasPrefix(path, "/")
	}
	// Rust's first component is RootDir only without a drive or UNC prefix.
	return filepath.VolumeName(path) == "" && (strings.HasPrefix(path, "/") || strings.HasPrefix(path, `\`))
}

// IsAbsoluteOnHostOrTarget is whether `path` is absolute on this controller
// or on a session target. For records and drafts that may name either side,
// such as a mount source on a local or an SSH container host; the side that
// uses the path checks it again.
func IsAbsoluteOnHostOrTarget(path string) bool {
	return filepath.IsAbs(path) || IsAbsolute(path)
}

// Join joins `relative` onto `base`. When `base` is a target path, absolute
// or relative to the target user's home (as SSH and EC2 worker roots are),
// the result keeps POSIX separators on a Windows controller too, so it reads
// the same wherever it is shown or sent. Only a base this controller would
// call absolute, with a Windows drive or UNC prefix, is joined as a host
// path. Like Rust's Path::join, it does not clean the result.
func Join(base string, relative string) string {
	if runtime.GOOS != "windows" {
		return rustPathPush(base, relative)
	}
	if filepath.IsAbs(base) {
		// Pushed part by part: `relative` is often Git's `/`-separated text,
		// and a verbatim `\\?\` base would keep a `/` as a file name
		// character.
		joined := base
		for _, component := range pathComponents(relative) {
			joined = rustPathPush(joined, component.text)
		}
		return joined
	}
	// As with Path::join, an absolute `relative` replaces the base.
	if IsAbsolute(relative) {
		return Text(relative)
	}
	joined := Text(base)
	for _, component := range pathComponents(relative) {
		if joined != "" && !strings.HasSuffix(joined, "/") {
			joined += "/"
		}
		joined += component.text
	}
	return joined
}

// Text is the POSIX text of a target path, for an argv element or a remote
// command. A path with a Windows drive or UNC prefix is this controller's
// own, as in Join, and keeps its spelling.
func Text(path string) string {
	// Only Windows joins components with `\`; on POSIX a backslash is an
	// ordinary file name character and must survive.
	if runtime.GOOS == "windows" && !filepath.IsAbs(path) {
		return strings.ReplaceAll(path, `\`, "/")
	}
	return path
}

// The helpers below reproduce Rust std::path semantics on path text, so
// ported code decides and renders paths exactly as the Rust build does.

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
	kind       pathComponentKind
	text       string
	start, end int
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
			components = append(components, pathComponent{kind: pathComponentPrefix, text: volume, start: 0, end: len(volume)})
			position = len(volume)
		}
	}
	hasRoot := position < len(path) && isPathSeparator(path[position])
	if hasRoot {
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

// rustPathPush is Rust's `PathBuf::push` (and so `Path::join`): an absolute
// part replaces the base, otherwise a separator is added unless the base is
// empty or already ends in one. It does not clean the result.
func rustPathPush(base, part string) string {
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

// pathFileName is Rust's `Path::file_name`: the last component when it is a
// normal name.
func pathFileName(path string) (string, bool) {
	components := pathComponents(path)
	if len(components) == 0 || components[len(components)-1].kind != pathComponentNormal {
		return "", false
	}
	return components[len(components)-1].text, true
}

// comparePaths is Rust's `Path` ordering, which compares by component.
// Paths that compare 0 are what Rust's `Path ==` calls equal: a trailing or
// repeated separator, or an inner `.`, does not make two paths differ.
func comparePaths(a, b string) int {
	left, right := pathComponents(a), pathComponents(b)
	for i := 0; i < len(left) && i < len(right); i++ {
		if left[i].kind != right[i].kind {
			return int(left[i].kind) - int(right[i].kind)
		}
		if order := strings.Compare(left[i].text, right[i].text); order != 0 {
			return order
		}
	}
	return len(left) - len(right)
}

// pathStripPrefix is Rust's `Path::strip_prefix`: the rest of `path`, as it
// is spelled there, after the components of `base`; false when `base` is not
// a component prefix of `path`.
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

// pathStartsWith is Rust's `Path::starts_with`: whether `base` is a
// component prefix of `path`.
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
