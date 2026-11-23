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

// cfgTest is Rust's `cfg!(test)`: true only in this package's own tests
// (set by its test files), not in the tests of packages that import it.
var cfgTest bool

// The helpers below reproduce Rust std::path semantics on path text, so the
// ported code decides paths exactly as the Rust build does.

// pathJoin is Rust's `Path::join`: an absolute part replaces the base,
// otherwise a separator is added unless the base already ends in one. It
// does not clean the result.
func pathJoin(base, part string) string {
	panic("unported: mj-build-cache std::path helper pathJoin")
}

// pathStartsWith is Rust's `Path::starts_with`: whether base is a component
// prefix of path.
func pathStartsWith(path, base string) bool {
	panic("unported: mj-build-cache std::path helper pathStartsWith")
}

// pathStripPrefix is Rust's `Path::strip_prefix`: the rest of path after the
// components of base, or false when base is not a component prefix of path.
func pathStripPrefix(path, base string) (string, bool) {
	panic("unported: mj-build-cache std::path helper pathStripPrefix")
}

// pathParent is Rust's `Path::parent`: the text before the last component,
// or false for a path that ends in its root or prefix, or is empty.
func pathParent(path string) (string, bool) {
	panic("unported: mj-build-cache std::path helper pathParent")
}

// pathEqual is Rust's `Path ==`, which compares by component: a trailing or
// repeated separator, or an inner `.`, does not make two paths differ.
func pathEqual(left, right string) bool {
	panic("unported: mj-build-cache std::path helper pathEqual")
}
