// Port of mj-profile-catalog/src/lib.rs.

// Package profilecatalog is what each profile's harness offers.
// profile_config.go asks a harness for its models and options by starting it
// from a staged copy of the profile, and caches the answer;
// profile_catalog.go keeps those answers current for every profile and
// serves them to the daemon's surfaces.
//
// It builds on worker staging, the store and the accounts package, and is a
// sibling of the session runtime: neither uses the other.
//
// The Rust crate's modules all live in this one package, one file per Rust
// module. Rust's crate-private re-exports (`mj_accounts::{quota,
// review_selection}`, `mj_targets::{targets, test_isolation}`,
// `mj_store::database`) have no Go equivalent: the files import packages
// accounts, targets and store directly. Rust's `targets::*` items that
// mj-targets re-exports from mj_core::targets are in package core
// (core.CommandSpec, core.TargetsTargetLocator, core.CancellableProcessExecutor).
//
// Library replacements: tokio's CancellationToken is a context.Context (with
// its CancelFunc where the owner cancels it, and context.WithCancel for a
// child token); a `futures::Shared` boxed future is probeAttempt; tokio's
// watch channel of publication revisions is core.Watch; `Arc<AtomicBool>`
// cancellation flags are *atomic.Bool, as core.NewCancellableProcessExecutor
// takes them. Async functions take a context.Context first: ending it
// abandons the caller's wait, as dropping the Rust future does, and never
// cancels work the catalog or a probe supervisor owns.
//
// Rust's `enable_test_fixtures_on_startup!` (under `cfg(test)`) belongs in
// an init function of this package's test files.
package profilecatalog
