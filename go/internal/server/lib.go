// Port of mj-server/src/lib.rs.

// Package server is the daemon's HTTP API and web viewer (mj-server).
//
// Only the wire side of the documented `/api/v1` contract is here so far:
// the request and response types, headers and failure codes the `mj` CLI
// (go/internal/cli) sends and reads, declared from the Rust source at
// 3dde0c11b. The routes, handlers, viewer and the rest of each file arrive
// with the Go daemon (milestone 3), in the same files. Rust reaches these
// through mj-daemon's re-export (`mj_daemon::server`); Go imports this
// package directly.
//
// serde attributes are recorded in struct tags: `omitempty` for
// `skip_serializing_if`, `serde:"default"` for a member serde accepts
// missing. Types Rust marks `deny_unknown_fields` say so in their comment;
// the decoders that enforce it, and `serialize_with` encoders, are bodies
// still to port.
package server
