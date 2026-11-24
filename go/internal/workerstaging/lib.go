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

import "github.com/BrokkAi/mjolnir/go/internal/core"

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

// pathJoin is Rust's `Path::join`: an absolute part replaces the base,
// otherwise a separator is added unless the base already ends in one. It
// does not clean the result.
func pathJoin(base string, parts ...string) string {
	panic("unported: mj-worker-staging std::path helper pathJoin")
}

// pathParent is Rust's `Path::parent`: the text before the last component,
// or false for a path that ends in its root or prefix, or is empty.
func pathParent(path string) (string, bool) {
	panic("unported: mj-worker-staging std::path helper pathParent")
}

// pathFileName is Rust's `Path::file_name`: the last normal component, or
// false when the path ends in `..`, a root or a prefix, or is empty.
func pathFileName(path string) (string, bool) {
	panic("unported: mj-worker-staging std::path helper pathFileName")
}

// pathStripPrefix is Rust's `Path::strip_prefix`: the rest of path after the
// components of base, or false when base is not a component prefix of path.
func pathStripPrefix(path, base string) (string, bool) {
	panic("unported: mj-worker-staging std::path helper pathStripPrefix")
}

// pathStartsWith is Rust's `Path::starts_with`: whether base is a component
// prefix of path.
func pathStartsWith(path, base string) bool {
	panic("unported: mj-worker-staging std::path helper pathStartsWith")
}

// pathAncestors is Rust's `Path::ancestors`: path itself, then each parent
// in turn, ending with the root or the first relative component.
func pathAncestors(path string) []string {
	panic("unported: mj-worker-staging std::path helper pathAncestors")
}

// pathComponents is Rust's `Path::components` as text: repeated separators
// and inner `.` components dropped.
func pathComponents(path string) []string {
	panic("unported: mj-worker-staging std::path helper pathComponents")
}
