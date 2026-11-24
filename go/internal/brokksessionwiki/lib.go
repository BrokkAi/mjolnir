// Port of brokk-sessionwiki 0.34.28 src/lib.rs, the external crate Mjolnir
// links as `sessionwiki` (see .agents/docs/sessionwiki-fork.md).

// Package brokksessionwiki is the part of SessionWiki that Mjolnir uses: the
// session model, the adapter interface and the adapters for the harnesses
// Mjolnir runs, and the incremental SQLite index over parsed sessions that
// Mjolnir shares with the standalone `sessionwiki` command.
//
// SessionWiki is not a Mjolnir crate; Mjolnir links the published
// `brokk-sessionwiki` crate (Cargo.lock pins 0.34.28 for the Rust build this
// port follows). Go has no equivalent library, and the Go build must read and
// write the same index file (schema version 9) as the `sessionwiki` command
// and the Rust build, so this package ports the crate's API as far as
// Mjolnir's packages (sessionwiki, mjimport, the daemon tests) use it. It is
// named after the crate, with the dash removed, because package sessionwiki
// is the port of mj-sessionwiki.
//
// Declarations only so far: every body that is not short and obvious panics
// with "unported: brokk-sessionwiki ...". Read the Rust source from the
// crate's published 0.34.28 sources.
//
// The SQLite connection is store.Connection, the Go port of the rusqlite
// Connection both crates pass around: Mjolnir runs its own SQL on the
// connection this package opens. Functions that only run statements take a
// store.Queryer; those that begin a transaction take *store.Connection.
// Paths (Rust's PathBuf) are strings.
package brokksessionwiki
