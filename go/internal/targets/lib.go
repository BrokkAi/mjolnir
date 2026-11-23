// Port of mj-targets/src/lib.rs.

// Package targets is Mjolnir's session targets: where a session's worker
// runs (a local directory, a container, an SSH host, an EC2 instance) and
// the commands that provision, control and clean it up. Nothing here knows
// about the store or about sessions' lifecycle; the packages above compose
// these plans.
//
// This is the lowest package of the daemon side, so the store, the session
// runtime and the lifecycle operations can each build on it without
// building on each other.
//
// The Rust crate's modules (aws_assume_role, aws_credentials,
// command_checks, image_pull_gate, signing_key, streamed_child, tailscale,
// target_storage, targets and its submodules, termination, test_isolation)
// all live in this one package, one file per Rust module. Rust's
// `pub use mj_core::targets::*` and `pub use mj_core::test_log` have no Go
// equivalent: callers use package core for those items directly.
package targets

import "strings"

// lossyText is Rust's String::from_utf8_lossy for command output.
func lossyText(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}
