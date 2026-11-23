// Port of mj-github-watch/src/lib.rs.

// Package githubwatch is durable GitHub activity polling for live primary
// sessions. It is the Go port of the mj-github-watch crate
// (`brokk-mj-github-watch`, library `mj_github_watch`); the crate's only
// module, `service`, is service.go.
//
// Library replacements: reqwest is net/http, tokio's CancellationToken a
// context.Context, JoinSet and `buffer_unordered` goroutines, and
// serde_json::Value the values jsonvalue.Parse returns (see jsonIndex).
//
// Rust's test-only item (`enable_test_fixtures_on_startup!`) belongs to this
// package's tests (lib_test.go).
package githubwatch
