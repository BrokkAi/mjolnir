# Small crates: indexmap, imbl, tempfile, dirs, cap-std, hashing

Build each as its own package, finishing one (code, ported tests, oracle
comparison, `doc.go`) before starting the next. For each, first grep the Rust
product sources for exactly what is used and cover that completely.

1. `go/crates/indexmap` for indexmap 2.14.0 (oracle `indexmap_oracle.rs`,
   feature `indexmap`): `IndexMap[K, V]` (and `IndexSet` if used) with
   insertion order and the exact order effects of `insert` (an existing key
   keeps its position), `shift_remove`, `swap_remove`, `remove`, `insert_full`,
   `get_index`, `get_full`, `entry`, `retain`, `sort_keys`/`sort_by`, `pop`,
   `first`/`last`, iteration, `extend`. Zero value usable; implements
   `rs.ReflectMap`.
2. `go/crates/imbl` for imbl 7.0.2 (same oracle feature): `OrdMap[K, V]` as
   used in `mj-core/src/snapshot_map.rs`, with persistent semantics: a clone is
   cheap and independent. A real persistent balanced tree is ideal; say what
   you chose. Ordering via `rs.Cmp`; implements `rs.ReflectMap`.
3. `go/crates/tempfile` for tempfile 3.27.0 (oracle `tempfile_oracle.rs`):
   `tempdir()`, `tempdir_in`, `TempDir` (`path`, `close`, `keep`, removed on
   `Drop()`), `Builder` (`prefix`, `suffix`, `rand_bytes`, permissions, and its
   `tempdir`/`tempfile` forms), `NamedTempFile` (`new`, `new_in`, `path`,
   `as_file`, `persist`, `persist_noclobber`, `keep`, `into_temp_path`,
   `reopen`, removed on drop) with `PersistError`, `TempPath`, unnamed
   `tempfile()`. Name pattern, creation modes (0700 directories, 0600 files),
   retry on collision, and where `TMPDIR` is read, as the crate does. About
   2,600 uses, nearly all `tempfile::tempdir()` in tests.
4. `go/crates/dirs` for dirs 6.0.0 (oracle `dirs_oracle.rs`): every function
   used, with the crate's exact rules on Linux (XDG variables, a relative value
   is ignored, the fallbacks), macOS and Windows (Known Folders through
   `golang.org/x/sys/windows`), returning `rs.Option[string]`. Mjolnir's config
   and data locations come from these and both implementations must agree.
5. `go/crates/capstd` for cap-std 4.0.3 and cap-fs-ext 4.0.3: the `Dir` API
   `mj-core/src/confined_path.rs` uses (read it and its tests), over Go's
   `os.Root` and, where that cannot express an operation, `openat`/`openat2`
   through `golang.org/x/sys/unix`. No escape through `..` or symlinks; the
   errors are the ones cap-std returns (see `cap-primitives`), built through
   `go/rs/rsio`.
6. `go/crates/getrandom`, `go/crates/sha2`, `go/crates/hmac`,
   `go/crates/blake3`, `go/crates/ring` (oracle `hash.rs`, feature `hash`, has
   sha2, hmac, blake3): thin wrappers with the shapes the code uses.
   `getrandom::fill`, `Sha256` (`new`, `update`, `finalize`, `digest`),
   `Hmac<Sha256>` (`new_from_slice`, `update`, `finalize().into_bytes()`,
   `verify_slice`), `blake3::hash` and `to_hex`; for `ring`, exactly what the
   one file using `ring::signature` and `ring::rand` needs, with the same
   algorithm, padding and encoding, verified to cross-validate with Rust.
