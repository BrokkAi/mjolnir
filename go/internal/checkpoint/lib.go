// Port of mj-checkpoint/src/lib.rs.

// Package checkpoint is checkpoint capture, verified archives, native
// relocation and resource packaging.
//
// The Rust crate's modules (archive, checkpoint, native, resources) all live
// in this one package, one file per Rust module. The Rust archive module's
// `pub use mj_core::archive::*` is not repeated here: use the core types
// (core.ArchiveManifest, core.PayloadRole, ...) directly.
package checkpoint
