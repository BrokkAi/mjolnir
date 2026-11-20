// Port of mj-transcript/src/lib.rs.

// Package transcript is shared transcript interpretation, projection and
// presentation: the Go port of the mj-transcript crate. Its modules
// (transcript, projection, summary, turn_context, authorization, handoff,
// github_item, github_history) are files of this one package.
//
// The crate's `github-history` feature has no Go counterpart: the history
// evidence functions it gates are always built.
package transcript
