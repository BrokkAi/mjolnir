# `go/rs/rsio` (`std::io`) and `go/rs/rsfs` (`std::fs` and its extensions)

Oracle module `go/port/oracle/src/std_io.rs`, feature `std_io`.

The Rust code calls `std::fs` about 5,400 times, and its error text reaches
users, logs and tests (`No such file or directory (os error 2)` where Go says
`open /x: no such file or directory`). Product code calls these packages, never
`os` directly. Reproduce Rust 1.99.1 (`std/src/io/`, `std/src/fs.rs`,
`std/src/sys/fs/`, `std/src/sys/pal/unix/os.rs`, `std/src/os/unix/fs.rs`,
`std/src/os/fd/`).

`rsio`:
- `rsio.Error`, Rust's `io::Error` as a Go `error`: kind plus a raw OS code, a
  custom inner error or a static message; `Kind()`, `RawOsError()`,
  `GetRef`/`IntoInner`, the constructors; `Error()` gives Rust's `Display`
  exactly (`"{strerror} (os error {code})"`, the kind descriptions, the inner
  error's text); `DebugString()` and `FmtDisplay`/`FmtDebug` give the `Debug`
  forms (`Os { code: 2, kind: NotFound, message: "..." }`, `Kind(NotFound)`,
  `Custom { kind: Other, error: ".." }`).
- strerror text as Rust prints it per platform: Linux has two C libraries in
  play (`mj` links glibc, `mj-worker` musl), so embed both tables and select
  with `rsio.LinuxLibc` (default glibc; the worker sets musl); Darwin's table
  (mark it unverified if you could not run macOS); Windows through
  `FormatMessageW` with trailing whitespace trimmed.
- `ErrorKind`: every variant of 1.99.1 as a data-free enum with the errno and
  Windows mappings of `decode_error_kind`.
- `rsio.From(err)` turning any Go error from `os`, `syscall`, `net` or `io`
  into the error Rust would have produced, and `rsio.KindOf(err)`.
- `Read`, `Write`, `Seek`, `BufRead` contracts as helpers over Go's
  interfaces with Rust's errors (`ReadToEnd`, `ReadToString` with `stream did
  not contain valid UTF-8`, `ReadExact` with `failed to fill whole buffer`,
  `WriteAll`, `ReadLine`, `Lines`, `ReadUntil`, `Split`, `Copy`, `Cursor`,
  `BufReader`/`BufWriter`/`LineWriter` with Rust's capacities and flush on
  `Drop()`, `Empty`, `Sink`, `Take`, `Chain`, the std streams with locking and
  `IsTerminal`), as far as Mjolnir uses them.

`rsfs`:
- Every `std::fs` function and method used, errors as `rsio.Error` without
  Go's path prefix: `Read`, `ReadToString`, `Write`, `Copy`, `Rename`,
  `RemoveFile`, `RemoveDir`, `RemoveDirAll`, `CreateDir`, `CreateDirAll`,
  `ReadDir` (directory order, unsorted), `Metadata`, `SymlinkMetadata`,
  `Canonicalize`, `ReadLink`, `HardLink`, `SetPermissions`, `Exists`/
  `TryExists`, `File` (`Open`, `Create`, `CreateNew`, `OpenOptions` with every
  flag and Unix `mode`/`custom_flags`, sync, `SetLen`, `TryClone`, times,
  locking with `TryLockError`), `Metadata` and `MetadataExt`, `Permissions`,
  `FileType` and its Unix extension, `Symlink`, ownership calls, `DirBuilder`,
  Windows symlink calls.
- The `Path` methods that touch the filesystem as `rsfs.PathExists(p)`,
  `rsfs.PathIsFile(p)`, and so on.
- `File` wraps `*os.File` with `Drop()` and the raw descriptor accessors.
- Creation modes and close-on-exec as Rust's defaults.

Grep the Rust sources for what is used and put the list with counts in
`doc.go`.

Tests: the std tests that apply, and differential tests running a table of
failing and succeeding operations in a temporary directory on both sides
(missing file, permission denied, is a directory, not a directory, directory
not empty, exists, invalid UTF-8 content, symlink loops), comparing kind, code,
`Display` and `Debug`; plus `ErrorKind` for errno 1 to 200 and the strerror
table of the host's C library.
