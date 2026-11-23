// Port of mj-store/src/lib.rs.

// Package store is the Mjolnir daemon's store: the SQLite schema and its
// migrations, the session, environment and event records, the single
// database writer, and the gates that order recovery and worker lifecycle
// against it. It builds on the targets package for the target types a record
// holds and knows nothing of how sessions are run.
//
// This is a separate package so the code above it (session runtime,
// lifecycle operations, the daemon) can be split into packages that each
// depend on the store without depending on each other.
//
// The Rust crate re-exports mj_core::upgrade and mj_core::storage::*, and
// names mj_targets::targets under its old path. Go has no re-exports: callers
// use those items from the core and targets packages directly. All of the
// crate's modules (database/*, mailbox_outbox, managed_archive,
// recovery_gate, web_push, worker_lifecycle) are files of this one package,
// named as in mj-core (database/mailbox.rs is database_mailbox.go).
package store
