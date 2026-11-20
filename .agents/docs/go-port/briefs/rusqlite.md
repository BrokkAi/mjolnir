# SQLite: `go/crates/rusqlite` for rusqlite 0.41.1

Features `backup`, `bundled`, `fallible_uint`, `functions`. Oracle module
`rusqlite_oracle.rs`, feature `rusqlite`.

Mjolnir's daemon keeps everything in SQLite through rusqlite: about 60 files,
mostly `mj-store/src/database/`, also `mj-checkpoint`, `mj-accounts`,
`mj-import`, `mj-sessionwiki`, `mj-worker`, `mj-session-runtime`,
`mj-controller`, `mj-daemon`. The Go and Rust daemons open the same database
files (one hands off to the other), so each must leave the file in a state the
other accepts, and every query must behave the same.

Use `modernc.org/sqlite` (pure Go, in `go.mod`), through its lower-level API
where `database/sql` cannot express rusqlite's behaviour: one `Connection` is
one connection, with its own transaction state, statement cache, busy handler
and pragmas.

Inventory what the Rust code uses, with counts, into `doc.go`. Then build from
`rusqlite-0.41.1/src/`:

- `Connection`: `open`, `open_with_flags` (each `OpenFlags` combination used,
  URI names, `open_in_memory`), `close`, `execute`, `execute_batch`, `prepare`,
  `prepare_cached` and its capacity, `query_row` and the forms used,
  `transaction`, `transaction_with_behavior`, `unchecked_transaction`,
  `savepoint`, `pragma_query_value`, `pragma_update`, `pragma_query`,
  `busy_timeout`, `busy_handler`, `last_insert_rowid`, `changes`,
  `total_changes`, `is_autocommit`, `path`, interrupt handles, hooks if used.
- `Transaction` with `TransactionBehavior::{Deferred, Immediate, Exclusive}`
  (the exact `BEGIN`), `commit`, `rollback`, `set_drop_behavior`; an
  uncommitted transaction rolls back when dropped (`Drop()`). `Savepoint`.
- `Statement`: `execute`, `query`, `query_map`, `query_and_then`, `query_row`,
  `exists`, `insert`, raw binding, column and parameter metadata, named and
  positional parameters, `params!`, `named_params!`, `params_from_iter`.
  `Rows`/`Row`: `next`, `get`, `get_ref`, `get_unwrap`.
- Conversions exactly as rusqlite's `ToSql`/`FromSql` and `fallible_uint`
  define them: every integer width with range errors, `u64`/`usize`, `f64`,
  `bool`, strings, blobs, `Option<T>`, and the optional-feature types the build
  enables (check `Cargo.lock`). The error for a column of the wrong SQLite type
  or a NULL into a non-Option. Go interfaces `ToSql`/`FromSql` so ported types
  with hand-written impls plug in; built-in mappings for `string`, `[]byte`,
  the integer types, `float64`, `bool`, `rs.Option[T]`, and hooks for `time`,
  `chrono` and `serdejson.Value`.
- `Error`: every variant the code matches or prints, `ErrorCode`,
  `sqlite_error_code()`, extended codes, exact `Display` text, and
  `OptionalExtension::optional()`.
- `backup::Backup` (`new`, `step`, `progress`, `run_to_completion`) and
  `functions` (`create_scalar_function`, `FunctionFlags`, `Context`).
- The library itself. `bundled` compiles a specific SQLite with specific
  options (`libsqlite3-sys-*/build.rs`). Record in `doc.go` the version and
  compile options on both sides (`sqlite_version()`, `PRAGMA compile_options`
  through the oracle and through modernc) and every difference. Check that
  every SQL feature Mjolnir's schema and queries use works identically (read
  `mj-store/src/database/*.sql` and the SQL strings: FTS, JSON functions,
  `RETURNING`, `STRICT`, generated columns, window functions, `WITHOUT ROWID`,
  partial indexes, WAL, `user_version`, foreign keys, triggers), and set
  explicitly on open whatever defaults differ by compile option (foreign keys,
  `synchronous`, `journal_mode`, `trusted_schema`, ...).

Tests: rusqlite's own inline tests for what is covered. Differential tests:
identical generated scripts on both sides (schema from Mjolnir's real
migration files, inserts of every value type and edge value, queries,
constraint violations, type mismatches, nested transactions and savepoints,
busy and locked contention between two connections with timeouts, backup
while writing), comparing every result, every error (variant, codes, text),
`changes`, `last_insert_rowid`, the final dump and `PRAGMA integrity_check`.
File interop in both directions: a database created by Rust is opened, read,
changed and closed by Go and then verified by Rust, and the reverse, including
WAL mode with an unclean close on either side.
