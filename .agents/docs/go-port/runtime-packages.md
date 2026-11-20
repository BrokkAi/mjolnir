# The Go runtime packages that stand in for Rust's libraries

Mjolnir's Rust code leans on the standard library and on 88 third-party
crates. Much of what a user sees comes from them: how an error reads, how a
number or a path is written, how a channel closes, how a help page wraps, where
a terminal cell lands. The Go port keeps that behaviour by giving every one of
them a Go package with the same names and the same results. Product code is
then translated call for call (`.agents/docs/go-port/translation-rules.md`),
and nobody porting a product file has to decide what the Go equivalent of a
library call is.

Two kinds of package exist:

- A **port** reproduces the crate's behaviour from its source, file by file,
  with the crate's own tests carried over. Use it wherever output or ordering
  is visible (text, bytes, layout, scheduling).
- A **wrapper** gives a Go library or the Go standard library the crate's
  names and error behaviour. Use it where only the effect matters (hashing,
  TLS, compression that the other side decodes).

Either way the package is checked against the real crate: a test feeds the same
inputs to the Rust crate through the oracle (`go/port/oracle`, described at the
end) and compares the results.

## Rules for every runtime package

- Path `go/crates/<crate name without - or _>` (`go/rs/...` for the standard
  library). Package name the same. One flat package per crate.
- Names follow the prefix rule over the path Rust code writes:
  `tokio::sync::mpsc::Sender` is `tokio.SyncMpscSender`, `chrono::DateTime` is
  `chrono.DateTime`, `serde_json::Value` is `serdejson.Value`. A method keeps
  its name in PascalCase. An associated function is type name plus function
  name (`chrono.UtcNow()`, `semver.VersionParse(s)`).
- Cover what Mjolnir uses, completely. Find the uses with
  `grep -rn '<crate>::' --include='*.rs' mj-* voice-worker` and by reading how
  the returned types are then used. For a type you provide, provide all of its
  commonly used methods, not only the ones called today. Do not port features
  nothing uses.
- Reproduce observable behaviour exactly: results, error text, panic text,
  ordering, and what happens on close, cancel and drop. Follow the crate's
  source, not its documentation or your memory of it. Crate sources are under
  `~/.cargo/registry/src/*/<crate>-<version>/`; the standard library is under
  `~/.cache/mjolnir/build/go-port/rust-src/src/rust/library/`
  (`go/port/fetch-reference.sh` fetches both).
- Follow the translation rules for types: `rs.Option[T]`, `(T, error)`,
  `collections.BTreeMap`, `Drop()` on anything that must be dropped,
  `ctx context.Context` first on anything that awaits.
- No C. Everything builds with `CGO_ENABLED=0` for linux, darwin and windows
  on amd64 and arm64. Third-party Go modules must be pure Go and under a
  licence that can be combined with GPL-3.0-only (MIT, BSD, Apache-2.0, MPL-2.0
  for tests). Prefer the standard library and `golang.org/x`.
- Each file that follows a Rust source file starts with
  `// Port of <crate> <version> <path in the crate>`.
- Tests: port the crate's own tests for the parts you cover, and add
  differential tests against the oracle with a recorded corpus under
  `testdata/oracle/`. These packages are libraries with their own contract, so
  the product rule against unit tests does not apply to them.
- `doc.go` states what the package stands in for, the crate version, what is
  covered, and what is deliberately not.

## Standard library

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `core::option`, `result`, `cmp`, `mem`, `ops`, `num`, `char`, `clone`, `default`, `convert`, `panic`, `cell`, `any`, `iter`, `slice` | `go/rs` | port | Option and Result methods and their panic text; checked, saturating and wrapping arithmetic; `as` casts; structural `Clone`, `Eq`, `Cmp` by reflection; `sort_unstable` (Rust's algorithm, for the order of equal elements); `Arc`/`Weak` counting; panic hook, `catch_unwind`, exit status 101 |
| `core::fmt`, `alloc::fmt`, `format!` | `go/rs` (`rs.Format`, `rs.Formatter`) | port | the whole format-spec grammar; `Display` and `Debug` of every primitive, float, string (escape table), `Option`, slice, map, tuple, `Duration`, `Path`; `{:#?}` pretty form; derived `Debug` from registered names |
| `core::str`, `alloc::string`, `char` methods | `go/rs/strs` | port | Unicode tables of the pinned toolchain (case mapping, `White_Space`, `Alphabetic`, `Numeric`), `lines`, `split*`, `trim*`, lossy UTF-8 substitution, `escape_debug`, `parse::<T>()` and its error text |
| `std::collections` | `go/rs/collections` | port | key order of `BTreeMap`/`BTreeSet`; `entry`, `range`, `retain`, `split_off`, `pop_first`; `VecDeque`; pointers into entries stay valid until the next structural change |
| `std::path`, `std::ffi::OsStr` | `go/rs/rspath` | port | `components`, `join`, `parent`, `file_name`, `extension`, `strip_prefix`, `starts_with`, `display`, on Unix and Windows forms, with no cleaning |
| `std::fs`, `std::io`, `std::os::unix::fs`, `std::os::fd` | `go/rs/rsfs`, `go/rs/rsio` | port over `os` | `io::Error` kinds, `Display` ("... (os error N)" with the C library's text for the platform) and `Debug`; `read_dir` order (unsorted); `create_dir_all`, `remove_dir_all`, `copy`, `rename`, `OpenOptions`, permissions, `BufReader`/`BufWriter`/`Cursor`, `read_to_string` UTF-8 checking |
| `std::env` | `go/rs/rsenv` | wrapper | `var` vs `var_os` (invalid Unicode), `current_exe`, `temp_dir`, `home_dir`, `args` |
| `std::process` | `go/rs/rsprocess` | port over `os/exec` | `Command` environment and argument handling, `Stdio`, `ExitStatus` `Display`, `pre_exec`, process groups, `kill`, `wait`, `Output` |
| `std::sync`, `std::sync::atomic`, `std::sync::mpsc` | `go/rs/rssync` | port | poisoning, guards, `Condvar` with timeouts, `OnceLock`, `LazyLock`, `Barrier`, orderings accepted and ignored |
| `std::thread` | `go/rs/rsthread` | port | `spawn`/`join` with panics delivered to the joiner, `scope`, `sleep`, `park`, names, `available_parallelism` |
| `std::time` | `go/rs/rstime` | wrapper | `Instant` (monotonic, checked and saturating arithmetic), `SystemTime`/`UNIX_EPOCH` arithmetic and its error, `Duration` methods and `Debug` form |
| `std::net`, `std::os::unix::net` | `go/rs/rsnet` | wrapper | address parsing and `Display`, listeners and streams with Rust's error kinds |
| test harness (`#[test]`, `assert_eq!`, `#[should_panic]`) | `go/rs/rstest` | new | Rust's assertion messages, repository paths, building the Go binaries for tests |

## Async runtime

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `tokio` 1.52.3 (`runtime`, `task`, `sync`, `time`, `select!`, `join!`, `io`, `net`, `process`, `fs`, `signal`, `test`) | `go/crates/tokio` | port | current-thread scheduling order and multi-thread flavour; task abort and drop-at-await cancellation; `JoinSet`; every channel's capacity, closing, lagging and fairness rules; `Notify` permits; `Semaphore` fairness; `select!` polling order, `biased`, preconditions, `else`; paused clock, `advance`, auto-advance; `timeout`, `interval` and its missed-tick modes; `duplex`; child processes with `kill_on_drop`; Unix signals |
| `tokio-util` 0.7.18 (`sync::CancellationToken`, `DropGuard`, `task`, `io`, `codec`, `compat`) | `go/crates/tokioutil` | port | child tokens, `cancelled()` after the fact, `run_until_cancelled` |
| `tokio-stream` 0.1.19 | `go/crates/tokiostream` | port | `StreamExt` methods used, channel wrappers |
| `futures` 0.3.32 (`future`, `stream`, `FutureExt`, `StreamExt`, `poll!`, `executor::block_on`) | `go/crates/futures` | port | `join_all` order, `select_all`, `FuturesUnordered`, `Shared`, `now_or_never`, `catch_unwind` |
| `rayon` 1.12.0 | `go/crates/rayon` | wrapper | `join`, `par_iter().map().collect()` keeping order, pool size |

## Data formats

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `serde` 1.0.229 (derive behaviour, data model, `de::Error` text) | `go/crates/serde` | port | every container, variant and field attribute Mjolnir uses; the four enum representations; `flatten`; `deny_unknown_fields`; missing, duplicate and unknown field errors word for word; integer range checks |
| `serde_json` 1.0.150 (`preserve_order`, `float_roundtrip`, `raw_value`) | `go/crates/serdejson` | port | compact and pretty output byte for byte; float and string escaping; `Value`, `Map` (insertion order), `Number`; `json!`; parse errors with line and column; `RawValue`. `Value` is the `jsonvalue` representation of `github.com/BrokkAi/acp-go` so ACP payloads need no conversion |
| `serde_ignored` 0.1.14 | in `go/crates/serde` | port | the path given for each ignored key |
| `toml` 0.8.2, `toml_edit` 0.20.2 | `go/crates/toml`, `go/crates/tomledit` | port | parse errors with their source excerpt; `to_string` and `to_string_pretty` output; `Value`, `Table`; editing a document while keeping comments and layout |
| `yaml-rust2` 0.13.0 | `go/crates/yamlrust2` | port | the loader's node tree for the documents Mjolnir reads |
| `jsonc-parser` 0.34.0 | `go/crates/jsoncparser` | port | `parse_to_serde_value` with comments and trailing commas, its errors |
| `base64` 0.22.1, hex | `go/crates/base64` | wrapper | engines (standard, URL-safe, padded or not), decode errors |
| `url` 2.5.8 | `go/crates/url` | port | WHATWG parsing and serialization, `Host`, `form_urlencoded`, `join` |
| `semver` 1.0.28 | `go/crates/semver` | port | parsing, ordering, error text |
| `chrono` 0.4.44, `chrono-tz` 0.10.4 | `go/crates/chrono`, `go/crates/chronotz` | port over `time` | RFC 3339 output for each `SecondsFormat`, `format` specifiers, parsing and its errors, `Local`, arithmetic overflow rules, serde forms |
| `bytesize` 2.7.0 | `go/crates/bytesize` | port | `Display` output |
| `indexmap` 2.14.0, `imbl` 7.0.2 | `go/crates/indexmap`, `go/crates/imbl` | port | order after `insert`, `shift_remove`, `swap_remove`; `OrdMap` API |

## Errors, logging, command line

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `anyhow` 1.0.102 | `go/crates/anyhow` | port | `{}`, `{:#}`, `{:?}` forms; `context` on results and options; `downcast_ref` through context layers; `chain`, `root_cause` |
| `tracing` 0.1.44, `tracing-subscriber` 0.3.23 | `go/crates/tracing`, `go/crates/tracingsubscriber` | port | the default line format, field order and quoting, levels, `EnvFilter` directive grammar, spans, non-blocking writer |
| `clap` 4.6.1 (derive) | `go/crates/clap` | port | parsing (subcommands, groups, conflicts, defaults, environment, value parsers, `--` and trailing arguments), every error message with its usage and suggestion, `--help` and `-h` rendering and wrapping, `--version`, colour choice |

## Storage, compression, hashing

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `rusqlite` 0.41.1 (bundled SQLite, `backup`, `functions`, `fallible_uint`) | `go/crates/rusqlite` | wrapper over `modernc.org/sqlite` | the same database file readable and writable by both implementations; transaction behaviours; error codes and text; `FromSql`/`ToSql` conversions and their errors; backup; user functions; pragmas |
| `tar` 0.4.46 | `go/crates/tar` | port | header bytes written by `Builder`/`Header` (so archive digests agree), entry types, long names, reading |
| `flate2` 1.1.9 (miniz_oxide) | `go/crates/flate2` | port for writing | compressed bytes equal to miniz_oxide's at each level where a digest covers them; reading through `compress/flate` |
| `zip` 2.4.2 | `go/crates/zip` | port for writing | local headers, central directory, zip64 thresholds, stored and deflated entries |
| `zstd` 0.13.3, `ruzstd` 0.9.1 | `go/crates/zstd`, `go/crates/ruzstd` | wrapper over `klauspost/compress` | frames each side can read; state in `doc.go` which callers hash compressed bytes |
| `sha2`, `hmac`, `blake3`, `ring` (signatures, random), `getrandom` | `go/crates/sha2`, `hmac`, `blake3`, `ring`, `getrandom` | wrapper | digests, key formats, signature bytes |
| `tempfile` 3.27.0 | `go/crates/tempfile` | port | name pattern, permissions, removal on drop, `persist`, `keep` |
| `cap-std`, `cap-fs-ext` 4.0.3 | `go/crates/capstd` | wrapper over `os.Root` | no escape through `..` or symlinks; the same errors |
| `dirs` 6.0.0 | `go/crates/dirs` | port | each directory on Linux, macOS and Windows |
| `sysinfo` 0.32.1 | `go/crates/sysinfo` | port | process fields and CPU figures as read from `/proc`, `libproc` and Windows |

## Network

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `reqwest` 0.13.4, `rustls` 0.23.40 | `go/crates/reqwest`, `go/crates/rustls` | wrapper over `net/http`, `crypto/tls` | request line, headers and their order where a server could notice; redirect policy; proxy environment handling; timeouts; error classification (`is_timeout`, `is_connect`, `status`) and text |
| `axum` 0.8.9, `axum-server` 0.7.3, `tower` 0.5.3, `tower-http` 0.6.11, `http-body-util` | `go/crates/axum`, `go/crates/tower` | port over `net/http` | route matching and precedence, 404/405 answers, extractor rejections (status and body text), middleware order, compression negotiation, graceful shutdown |
| `agent-client-protocol` 5.0.0 | `github.com/BrokkAi/acp-go` | existing module | used directly. `.agents/docs/go-port/acp-mapping.md` maps each Rust path to its acp-go name and records every difference between the 5.0.0 crate and what acp-go tracks |
| `brokk-anvil-client` 0.28.6 | `go/crates/anvilclient` | port | the clients Mjolnir calls: requests, streaming, authentication files, retries |
| `brokk-sessionwiki` 0.34.27 | `go/crates/sessionwiki` | port | the index's on-disk format and query results, since both implementations open the same index |

## Terminal and text

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `ratatui` 0.30.0 (`ratatui-core`, `ratatui-widgets`, `ratatui-crossterm`), `kasuari` | `go/crates/ratatui` | port | every cell of every buffer: layout solving, text wrapping and truncation, styles, each widget Mjolnir uses, buffer diffing and the bytes written to the terminal |
| `crossterm` 0.29.0 | `go/crates/crossterm` | port | input parsing (keys, kitty protocol, mouse, paste, focus, resize), commands and their escape sequences, raw mode on Unix and Windows |
| `vt100` 0.16.2, `vte` 0.15.0, `tui-term` 0.3.4 | `go/crates/vt100`, `go/crates/tuiterm` | port | the screen after any byte stream |
| `rat-focus` 2.1.1 | `go/crates/ratfocus` | port | focus order and transitions |
| `unicode-width` 0.2.2 (`cjk`), `unicode-segmentation` 1.13.3 | `go/crates/unicodewidth`, `go/crates/unicodesegmentation` | port | the crates' own tables, not another Unicode version |
| `pulldown-cmark` 0.10.3 | `go/crates/pulldowncmark` | port | the event stream for any input |
| `linkify` 0.11.0 | `go/crates/linkify` | port | link spans |
| `similar` 2.7.0 | `go/crates/similar` | port | the same edit script and unified-diff text |
| `regex` 1.12.3, `aho-corasick` 1.1.4, `memchr` | `go/crates/regex`, `go/crates/ahocorasick` | port over `regexp` | Unicode-aware `\d`, `\w`, `\s`, `\b` as Rust has them; match kinds |
| `qrcode` 0.14.1 | `go/crates/qrcode` | port | the same module matrix |
| `image` 0.25.10, `png` 0.18.1 | `go/crates/image` | wrapper over `image/png` | decoded pixels; encoded bytes where they are stored or compared |
| `arboard` 3.6.1, `webbrowser` 1.2.4 | `go/crates/arboard`, `go/crates/webbrowser` | port | the same clipboard mechanisms and browser launch commands per platform |
| `reedline` 0.51.0 | `go/crates/reedline` | port of what is used | to be scoped from its uses |
| `portable-pty` 0.9.0, `signal-hook` 0.3.18, `libc`, `mach2`, `windows-sys` | `go/crates/portablepty`, `go/crates/signalhook`; `golang.org/x/sys` | wrapper | pty sizes and modes, signal dispositions |

## Models and parsing

| Rust | Go package | Kind | What must match |
| --- | --- | --- | --- |
| `tokenizers` 0.22.2 | `go/crates/tokenizers` | port | token ids for the tokenizer files Mjolnir loads |
| `ort` 2.0.0-rc.15, `half`, `libloading` | `go/crates/ort` | wrapper, loading ONNX Runtime at run time without C | the same outputs from the same model |
| `tree-sitter` 0.25.10, `tree-sitter-bash` 0.25.1 | `go/crates/treesitter` | the same C sources compiled to WebAssembly and run in pure Go | identical syntax trees |
| `cpal` 0.16.0 | `go/crates/cpal` | wrapper | capture format for the voice worker |
| `mimalloc`, `tikv-jemallocator` | none | not applicable | Go has its own allocator |
| `tiny_http`, `rcgen`, `proptest`, `tempfile` (tests) | `go/crates/tinyhttp`, `rcgen`, `proptest` | wrapper | test-only |

## The oracle

`go/port/oracle` is a standalone Rust program (not a workspace member) that
exposes the real crates so a Go test can ask "what does Rust do with this
input?". Each runtime package has one module in it, behind a Cargo feature of
the same name, so one package's module failing to build never blocks another.

    go/port/oracle/src/<name>.rs       reads one JSON request per line on stdin,
                                       writes one JSON reply per line on stdout
    cargo run --release --features <name> -- <name>

In Go, `oracle.Check(t, "<name>", cases)` runs recorded cases from
`testdata/oracle/<name>.jsonl` and, when `MJ_ORACLE=live` is set, also starts
the Rust program, generates fresh inputs and compares. `MJ_ORACLE=record`
rewrites the corpus from the Rust program. Plain `go test` therefore needs no
Rust toolchain, and CI runs the live comparison.
