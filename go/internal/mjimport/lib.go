// Port of mj-import/src/lib.rs.

// Package mjimport brings work into Mjolnir and carries it between sessions:
// importing another tool's sessions, the catalog of projects and their
// repositories, compacting a transcript with the utility model, and the
// handoff built from it. It builds on the targets, store and accounts
// packages and knows nothing of a running session. It is the Go port of the
// mj-import crate (`brokk-mj-import`, library `mj_import`); the package is
// not named `import` because that is a Go keyword.
//
// This is a separate package so the session runtime, the lifecycle
// operations and the daemon can each use it without rebuilding one another.
//
// The Rust crate's modules all live in this one package, one file per Rust
// module (handoff/claude.rs is handoff_claude.go). Rust's crate-private
// re-exports (`mj_accounts::{kimi_auth, quota}`, `mj_store::{database,
// upgrade}`, `mj_targets::targets`) have no Go equivalent: the files import
// package accounts, store, targets and core directly. The Anvil LLM client
// the utility model uses is package anvilclient.
//
// Library replacements: tokio's CancellationToken is a context.Context,
// serde_json::Value a JSON value in acp-go's jsonvalue representation
// (`any`), and tracing log/slog.
//
// Rust's test-only `enable_test_fixtures_on_startup!` belongs to this
// package's tests.
package mjimport
