// Port of mj-worker-staging/src/lib.rs.

// Package workerstaging puts what a session needs onto its target before its
// worker starts and keeps it current afterwards: selecting, verifying and
// installing the worker binary, installing the harness, transferring the
// profile, and the commands that probe, stop and restart a staged worker.
//
// It fills in the recovery plans of package sessiontypes and builds on the
// targets, store and accounts packages. The session runtime and the
// controller's lifecycle operations use it; it uses neither.
//
// The Rust crate's modules all live in this one package, one file per Rust
// module: worker_binary.rs is worker_binary.go and worker_binary/store.rs is
// worker_binary_store.go. Rust's crate-private re-exports
// (`mj_accounts::{harness_auth, opencode_logins}`,
// `mj_store::{database, worker_lifecycle}`,
// `mj_targets::{aws_credentials, target_storage, targets}`) have no Go
// equivalent: the files import packages accounts, store and targets
// directly. Rust's `targets::*` items that mj-targets re-exports from
// mj_core::targets are in package core (core.CommandSpec,
// core.TargetsTargetLocator, core.LocatorCommand, ...).
//
// Rust's `enable_test_fixtures_on_startup!` (under `cfg(test)`) belongs in
// an init function of this package's test files.
//
// Files the crate embeds or its tests read: worker_binary/profile_receiver.pl
// is data/worker_binary/profile_receiver.pl, and the crate's tests/golden and
// tests/fixtures files are under testdata/tests/, where core.AssertGolden
// finds them with the manifest directory "testdata".
package workerstaging

import (
	"path/filepath"
	"runtime"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ioError is an I/O error shown as Rust's io::Error Display shows it (`No
// such file or directory (os error 2)`), still matching errors.Is and
// errors.As on the error it wraps. Wrap os errors with rustIOError where
// Rust prints an io::Error.
type ioError struct{ err error }

func (e ioError) Error() string { return core.IOErrorDisplay(e.err) }
func (e ioError) Unwrap() error { return e.err }

// rustIOError wraps err as an ioError; nil stays nil.
func rustIOError(err error) error {
	if err == nil {
		return nil
	}
	return ioError{err: err}
}

// The helpers below reproduce Rust std::path semantics on host path text, so
// the ported code decides and renders paths exactly as the Rust build does.
// Target paths go through core.Join and core.Text instead.

// stagingComponentKind is Rust's `std::path::Component` variant.
type stagingComponentKind int

const (
	stagingComponentPrefix stagingComponentKind = iota
	stagingComponentRootDir
	stagingComponentCurDir
	stagingComponentParentDir
	stagingComponentNormal
)

// stagingComponent is one Rust path component and its byte span in the text.
type stagingComponent struct {
	kind       stagingComponentKind
	text       string
	start, end int
}

func isStagingSeparator(b byte) bool {
	return b == '/' || (runtime.GOOS == "windows" && b == '\\')
}

// stagingComponents is Rust's `Path::components`: repeated separators and
// `.` after the first component are dropped, a leading `.` of a relative
// path is kept as CurDir, and `..` is ParentDir. On Windows a drive or UNC
// prefix comes first and both separators count.
func stagingComponents(path string) []stagingComponent {
	var components []stagingComponent
	position := 0
	if runtime.GOOS == "windows" {
		if volume := filepath.VolumeName(path); volume != "" {
			components = append(components, stagingComponent{kind: stagingComponentPrefix, text: volume, end: len(volume)})
			position = len(volume)
		}
	}
	if position < len(path) && isStagingSeparator(path[position]) {
		components = append(components, stagingComponent{kind: stagingComponentRootDir, text: path[position : position+1], start: position, end: position + 1})
		position++
	}
	first := true
	for position <= len(path) {
		end := position
		for end < len(path) && !isStagingSeparator(path[end]) {
			end++
		}
		part := path[position:end]
		switch {
		case part == "":
		case part == ".":
			// Only a relative path without a prefix keeps a leading `.`.
			if first && len(components) == 0 {
				components = append(components, stagingComponent{kind: stagingComponentCurDir, text: part, start: position, end: end})
			}
		case part == "..":
			components = append(components, stagingComponent{kind: stagingComponentParentDir, text: part, start: position, end: end})
		default:
			components = append(components, stagingComponent{kind: stagingComponentNormal, text: part, start: position, end: end})
		}
		first = false
		position = end + 1
	}
	return components
}

// pathPush is Rust's `PathBuf::push` of one part.
func pathPush(base, part string) string {
	if runtime.GOOS == "windows" {
		if filepath.IsAbs(part) || filepath.VolumeName(part) != "" {
			return part
		}
		if part != "" && isStagingSeparator(part[0]) {
			return filepath.VolumeName(base) + part
		}
		if base != "" && !isStagingSeparator(base[len(base)-1]) {
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

// pathJoin is Rust's `Path::join`: an absolute part replaces the base,
// otherwise a separator is added unless the base already ends in one. It
// does not clean the result.
func pathJoin(base string, parts ...string) string {
	for _, part := range parts {
		base = pathPush(base, part)
	}
	return base
}

// pathParent is Rust's `Path::parent`: the text before the last component,
// or false for a path that ends in its root or prefix, or is empty.
func pathParent(path string) (string, bool) {
	components := stagingComponents(path)
	if len(components) == 0 {
		return "", false
	}
	last := components[len(components)-1]
	if last.kind == stagingComponentPrefix || last.kind == stagingComponentRootDir {
		return "", false
	}
	if len(components) == 1 {
		return "", true
	}
	return path[:components[len(components)-2].end], true
}

// pathFileName is Rust's `Path::file_name`: the last normal component, or
// false when the path ends in `..`, a root or a prefix, or is empty.
func pathFileName(path string) (string, bool) {
	components := stagingComponents(path)
	if len(components) == 0 || components[len(components)-1].kind != stagingComponentNormal {
		return "", false
	}
	return components[len(components)-1].text, true
}

// pathStripPrefix is Rust's `Path::strip_prefix`: the rest of path after the
// components of base, or false when base is not a component prefix of path.
func pathStripPrefix(path, base string) (string, bool) {
	if !pathStartsWith(path, base) {
		return "", false
	}
	left, prefix := stagingComponents(path), len(stagingComponents(base))
	if prefix == len(left) {
		return "", true
	}
	return path[left[prefix].start:], true
}

// pathStartsWith is Rust's `Path::starts_with`: whether base is a component
// prefix of path.
func pathStartsWith(path, base string) bool {
	left, right := stagingComponents(path), stagingComponents(base)
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

// pathAncestors is Rust's `Path::ancestors`: path itself, then each parent
// in turn, ending with the root or the first relative component.
func pathAncestors(path string) []string {
	ancestors := []string{path}
	for {
		parent, ok := pathParent(path)
		if !ok {
			return ancestors
		}
		ancestors = append(ancestors, parent)
		path = parent
	}
}

// pathComponents is Rust's `Path::components` as text: repeated separators
// and inner `.` components dropped.
func pathComponents(path string) []string {
	components := stagingComponents(path)
	texts := make([]string, len(components))
	for index, component := range components {
		texts[index] = component.text
	}
	return texts
}
