# Archives and compression

Packages: `go/crates/tar` (tar 0.4.46), `go/crates/flate2` (flate2 1.1.9 with
`rust_backend`, i.e. miniz_oxide), `go/crates/zip` (zip 2.4.2, features
`deflate`, `zstd`), `go/crates/zstd` (zstd 0.13.3, the C library) and
`go/crates/ruzstd` (ruzstd 0.9.1). Oracle modules: `tar_oracle.rs` (feature
`tar`), `flate2_oracle.rs` (`flate2`), `zip_oracle.rs` (`zip`),
`zstd_oracle.rs` (`zstd`).

Mjolnir packs and unpacks checkpoints, worker binaries, harness installs,
profiles and build caches with these crates (`mj-checkpoint`,
`mj-worker-staging`, `mj-build-cache`, `mj-core/src/archive.rs`,
`mj-core/src/harness_install.rs`, `mj-core/src/relay/protocol.rs`, ...).
Archives cross between the Rust and Go implementations and between machines,
and some are hashed.

Step 1: establish what must be byte-identical. Read
`.agents/docs/go-port/compressed-bytes.md` (every place the product hashes,
measures or compares archive or compressed bytes). If it is missing, do that
analysis for your crates and put it in each package's `doc.go`. Where bytes are
only decoded by the other side, any conforming encoder will do; where a digest,
a length or a test's expected value covers the encoded bytes, the encoder must
produce exactly what the Rust crate produces.

Step 2: build.

- `tar`: a port of `tar-0.4.46/src/header.rs` and `builder.rs`, not Go's
  `archive/tar` writer, which chooses formats differently. `Builder` (`append`,
  `append_data`, `append_path`, `append_path_with_name`, `append_file`,
  `append_dir`, `append_dir_all`, `append_link`, `mode(HeaderMode)`,
  `follow_symlinks`, `finish`, `into_inner`), `Header` (`new_gnu`, `new_ustar`,
  `new_old`, every setter and getter, `set_cksum`, `as_bytes`), `EntryType`,
  GNU long-name and long-link entries, PAX extensions where the crate emits
  them, numeric fields (octal and the binary extension), padding and the end
  blocks: header bytes equal to the crate's for the same inputs. Reading:
  `Archive`, `entries`, `Entry` (`path`, `link_name`, `header`, `size`,
  `unpack`, `unpack_in` with the crate's traversal protection and its
  permission, mtime, ownership and overwrite flags), `Archive::unpack`.
- `flate2`: `write::GzEncoder`, `read::GzDecoder`, `read::GzEncoder`,
  `Zlib*`, `Deflate*`, `bufread` forms, `Compression` levels, `GzBuilder`
  header fields, `MultiGzDecoder`, as used. Decode with `compress/flate`. If
  step 1 finds a case needing identical bytes, port miniz_oxide's deflate
  encoder (`deflate/core.rs`: hash chains, per-level parameters, block
  splitting, Huffman construction) and verify byte equality against the oracle
  at the levels used; otherwise use `compress/flate` and say so in `doc.go`.
  The gzip header and trailer match flate2's either way.
- `zip`: `ZipWriter` (`start_file`, `add_directory`, `add_symlink`, `finish`),
  `SimpleFileOptions`/`FileOptions`, `ZipArchive` (`new`, `len`, `by_index`,
  `by_name`, `file_names`, metadata, reading), `CompressionMethod`, the zip64
  thresholds, `DateTime`. Local headers, data descriptors, central directory
  and end records equal to the crate's bytes for the same inputs.
- `zstd`, `ruzstd`: `zstd::bulk::{compress, decompress}` and what else is
  called; `ruzstd` decoding and its `encoding` module as used. Use
  `github.com/klauspost/compress/zstd`. Frames decodable in both directions
  (verify through the oracle). If step 1 finds zstd-compressed bytes that are
  hashed or compared, report it prominently: that cannot be met without porting
  the C encoder and needs a product decision.

A writer that must be finished has `Finish()` and an idempotent `Drop()` with
the crate's drop behaviour (flate2 encoders finish on drop; a tar `Builder`
writes the trailer).

Tests: each crate's own tests for what is covered; differential tests for
generated file trees (names of every length class including over 100 and over
256 bytes, non-UTF-8 names, deep paths, symlinks, hard links, directories,
empty files, large sizes in headers, every mode and timestamp edge, ids above
the octal limit) comparing archive bytes; for payloads (empty, tiny, 1 MB of
zeros, random, text, already compressed, sizes around 64 KB and block
boundaries) compare what step 1 requires at each level used; cross-decoding in
both directions for everything.
