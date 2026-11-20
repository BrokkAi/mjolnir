# `go/rs/rspath` (`std::path`, `OsStr`) and `go/rs/rsenv` (`std::env`)

Oracle module `go/port/oracle/src/std_path.rs`, feature `std_path`.

A Rust `PathBuf`, `&Path`, `OsString` and `&OsStr` are a Go `string` here.
Only `rspath` interprets a path; product code never uses `path` or
`path/filepath`, which clean where Rust does not.

`rspath` (Rust 1.99.1 `std/src/path.rs`, `std/src/sys/path/`,
`std/src/ffi/os_str.rs`, tests in `std/src/path/tests.rs`), as functions over
`string`: `Components` with Rust's normalisation and typed components, `Iter`,
`Ancestors`, `Join`, `Push(*string, x)`, `Pop`, `Parent`, `FileName`,
`FileStem`, `FilePrefix`, `Extension`, `WithFileName`, `WithExtension`,
`SetFileName`, `SetExtension`, `AddExtension`, `StartsWith`, `EndsWith`
(component-wise), `StripPrefix` with `StripPrefixError`, `IsAbsolute`,
`IsRelative`, `HasRoot`, `ToStr` (None when not UTF-8), `ToStringLossy` and
`Display` (Rust's lossy substitution), `Eq` and `Cmp` by components, and the
`Debug` form. Both platform flavours exist and are testable on any host (the
Rust code handles remote Linux paths while running on Windows): the Unix and
Windows parsers (verbatim, UNC and drive prefixes, both separators) as
`rspath.Unix...`/`rspath.Windows...`, with the plain functions using the host
flavour. The filesystem-touching `Path` methods belong to `rsfs`; list in your
report which of them the Rust code calls. `OsStr`/`OsString` helpers the code
uses.

`rsenv` (`std/src/env.rs`): `Var` with `VarError` (`environment variable not
found`, `NotUnicode`), `VarOs`, `Vars`, `VarsOs`, `SetVar`, `RemoveVar`,
`CurrentDir`, `SetCurrentDir`, `CurrentExe`, `TempDir` (Rust's rule),
`HomeDir`, `Args`, `ArgsOs`, `JoinPaths`/`SplitPaths` (Windows quoting,
`JoinPathsError`), and the `consts` with Rust's values (`ARCH` is `x86_64` or
`aarch64`, `OS` is `macos`, ...).

Grep the Rust sources for every `Path`, `PathBuf`, `OsStr` and `env::` method
used and cover each.

Tests: port all of `std/src/path/tests.rs` (both flavours) and the env tests;
differential tests on the host flavour for every function over edge cases
(empty, `/`, `.`, `..`, trailing and repeated separators, hidden files,
several dots, non-UTF-8 bytes) and seeded random paths.
