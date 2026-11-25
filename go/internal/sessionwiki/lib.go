// Port of mj-sessionwiki/src/lib.rs.

// Package sessionwiki is the index of session history that searches, the
// resume picker and workers' history queries read: keeping it in sync with
// Mjolnir's own sessions and with other tools' stores, and answering queries
// over it. It builds on the targets, store and import packages and reads
// session records straight from the store, so the session runtime above can
// call it. It is the Go port of the mj-sessionwiki crate
// (`brokk-mj-sessionwiki`, library `mj_sessionwiki`).
//
// The crate's one public module, `sessionwiki`, and its submodules are files
// of this package, named as in mj-core (sessionwiki/history.rs is
// sessionwiki_history.go). Rust's crate-private re-exports
// (`mj_import::import`, `mj_store::database`, `mj_targets::targets`) have no
// Go equivalent: the files import packages mjimport, store and targets
// directly. SessionWiki itself, the external crate brokk-sessionwiki, is
// package brokksessionwiki.
//
// Library replacements: rusqlite's Connection is store.Connection (and
// store.Queryer for code that only runs statements), tokio's watch channel
// core.Watch, serde_json::Value a JSON value in acp-go's jsonvalue
// representation (`any`), BTreeSet<String> a map[string]struct{}, and
// tracing log/slog.
//
// Rust's test-only `enable_test_fixtures_on_startup!` belongs to this
// package's tests.
package sessionwiki
