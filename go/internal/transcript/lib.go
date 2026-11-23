// Port of mj-transcript/src/lib.rs.

// Package transcript is shared transcript interpretation, projection and
// presentation: the Go port of the mj-transcript crate. Its modules
// (transcript, projection, summary, turn_context, authorization, handoff,
// github_item, github_history) are files of this one package.
//
// Rust's test builds enable mj-core's shared test fixtures at startup
// (enable_test_fixtures_on_startup!); a Go test binary is recognised by
// testing.Testing() where core needs it.
package transcript

// ToolItemID is the stable transcript item identity for a raw ACP tool call
// ID.
func ToolItemID(callID string) string { return "tool:" + callID }
