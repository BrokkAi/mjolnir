// Port of mj-accounts/src/lib.rs.

// Package accounts is Mjolnir's harness profile accounts: whether a profile
// is logged in, the terminal a login runs in, and each provider's quota and
// usage reader with the scheduling and health derived from them. It builds on
// the targets and store packages and knows nothing of sessions.
//
// This is a separate package so that the session runtime, the lifecycle
// operations and the daemon can each use it without rebuilding one another.
//
// The Rust crate's modules all live in this one package, one file per Rust
// module. Rust's crate-private re-exports (`mj_store::{database, upgrade}`,
// `mj_targets::{streamed_child, targets}`) have no Go equivalent: the files
// import package store and package targets directly.
package accounts
