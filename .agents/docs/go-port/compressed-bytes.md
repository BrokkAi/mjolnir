# Compressed and archived bytes: where the Go port must match Rust

The Go port will not use the Rust compressors (`miniz_oxide` behind `flate2`,
the C `zstd` library, `ruzstd`, `fdeflate` inside `png`, `brotli`) unless they
are ported, so the bytes it writes for the same input will differ. This
document lists every place where product code writes compressed or archived
bytes, follows those bytes to whoever hashes, measures or compares them, and
says for each whether a difference matters.

Read at commit `9f24e3da9`. "Product code" is every `*.rs` file under `mj-*/`
and `voice-worker/`; `voice-worker/` has no hit.

## 0. Summary

- **No production code path needs compressed bytes that are identical to
  Rust's in order to work or to interoperate.** Every digest of an archive is
  computed over a stored file by whoever holds that file, and is compared with
  the digest that was recorded when the same bytes were written. Nothing
  re-creates an archive and compares it with one written earlier or elsewhere.
- **One test pins exact archive bytes, and they are uncompressed tar bytes.**
  The golden `mj-checkpoint/tests/golden/clone-archive-manifest.txt` records
  the SHA-256 and size of a tar that the test builds with `tar::Builder` and
  `tar::Header::new_gnu()`. The Go `tar` package must reproduce the `tar`
  crate's header bytes exactly.
- **One production rule depends on the exact length of an empty tar** (1024
  bytes; `mj-controller/src/controller/publication.rs:572-583`).
- **Two production decisions depend on compressed length**: the skills archive
  limits (gzip, default level) and the image optimiser (PNG and JPEG encoders
  against a 700 KiB limit). A different compressor changes the outcome only
  for inputs near the limit, and no test pins the outcome, but under rule 0 of
  the translation rules ("the same behaviour on every path") these are the
  places where a bit-exact encoder is the only way to be identical.
- Everything else needs only a decoder the other side accepts, or determinism
  within one implementation.

Verdicts used below:

- **(A)** the bytes only need to be decodable by the other side;
- **(B)** deterministic output within one implementation is enough;
- **(C)** must be byte-identical to Rust's output.

## 1. Method

Sites were found with one grep and each hit was read, then the bytes were
followed through their callers:

    grep -rnE 'flate2|zstd|ruzstd|\btar::|\bzip::|GzEncoder|GzDecoder|DeflateEncoder|DeflateDecoder|ZlibEncoder|ZlibDecoder|Compression::|GzBuilder|ZipWriter|ZipArchive|SimpleFileOptions|FileOptions|CompressionMethod|tar::Builder|tar::Archive|tar::Header|HeaderMode|EntryType|async_compression|CompressionLayer|DecompressionLayer|brotli|lz4|\bxz2?\b|bzip2|snap::|PngEncoder|JpegEncoder|write_to\(|image::codecs|png::Encoder|encode_all|decode_all' \
        --include='*.rs' mj-* voice-worker

202 lines in 32 files: 115 in production code, 87 in tests. No
`DeflateEncoder`, `ZlibEncoder`, `GzBuilder`, `encode_all`, lz4, xz, bzip2 or
snappy use exists. Shell-level archiving was found with a second grep for
`tar`, `gzip`, `zstd`, `sha256sum` and `shasum` in command strings and scripts.
Digest comparisons were found with a grep for `sha256`/`digest` next to `==`
or `!=` (85 production lines), and fixed digests with a grep for 64-digit
hexadecimal literals (24 lines) and for the same pattern in checked-in golden
and fixture files (7 files).

### What is compiled in

From `Cargo.lock` and `cargo metadata` (features unified across the workspace):

| Crate | Version | Features in this build | Backend |
| --- | --- | --- | --- |
| `flate2` | 1.1.9 | `rust_backend` | `miniz_oxide` 0.8.9 (with `simd`) |
| `zip` | 2.4.2 | `deflate`, `zstd` (no `time`, no `aes-crypto`, no `bzip2`, `lzma`, `xz`, `deflate64`) | deflate through `flate2`; `zopfli` 0.8.3 is linked but only used for levels above 9, which nothing asks for; zstd through the `zstd` crate |
| `zstd` | 0.13.3 (`zstd-safe` 7.2.4, `zstd-sys` 2.0.16+zstd.1.5.7) | none (no `zstdmt`) | the C library, single-threaded |
| `ruzstd` | 0.9.1 | `std`, `hash` | pure Rust |
| `tar` | 0.4.46 | `xattr` | |
| `image` | 0.25.10 | `png`, `jpeg`, `webp`, `bmp`, `tiff` | `png` 0.18.1 with `fdeflate` 0.3.7 and `flate2` |
| `tower-http` | 0.6.11 | `compression-gzip`, `compression-br` | `async-compression` 0.4.50: gzip through `flate2`, Brotli through `brotli` 9.0.0 |
| `reqwest` | 0.13.4 | no `gzip`, `brotli`, `zstd` or `deflate` | responses are not decompressed by the client |

Because `zip` is built without its `time` feature, `SimpleFileOptions::default()`
stamps every entry 1980-01-01 00:00:00 (`zip-2.4.2/src/types.rs:143-147`), so
Rust's zip output does not depend on the clock.

## 2. Writers in production code

### 2.1 Checkpoint archives (`*.hel.zip`): verdict (A), and (B) for one test

`mj-checkpoint/src/archive.rs`.

Writer and settings:

- `zip::ZipWriter<&mut File>` (`write_zip`, `:1251-1340`).
- `manifest.json`: `serde_json::to_vec_pretty`, method Deflated at level 1
  (`DEFLATE_LEVEL`, `:37`, used `:1264-1272`).
- Payloads by role (`payload_compression`, `:1182-1198`): Git bundles are
  Stored; every other role (canonical session JSON, native artifacts, patches,
  the untracked, conflicted and operation-state tars) is Zstd at level 1
  (`ZSTD_LEVEL`, `:38`). No payload is Deflated.
- A compressible payload larger than 16 MiB is split into entries named
  `<path>.helpart.NNNNN` (`PAYLOAD_PART_BYTES`, `:33-34`; `plan_payload_parts`,
  `:1232-1249`).
- Each compressed entry is first written to a private one-entry zip in a spool
  directory and copied into the archive with `raw_copy_file`
  (`compress_stream_entry`, `:1390-1400`; `append_stream_entries`, `:1342-1377`),
  in windows of `rayon::current_num_threads()` entries. Entry order in the
  archive is the manifest's order whatever the thread count.
- Entry options (`write_stream_entry`, `:1435-1466`): `unix_permissions(mode)`
  and `large_file(size > zip::ZIP64_BYTES_THR)`.

What is hashed or measured:

| What | Where | Over which bytes | Compared by |
| --- | --- | --- | --- |
| `archive_sha256` | written at `:328-332`; recomputed at `:392`, `:402`, `:411`, `:518`, `:566`; `checkpoint_sha256` in `mj-checkpoint/src/checkpoint.rs:488` | the whole zip file | Stored as `CheckpointMetadata.sha256` (`mj-core/src/state.rs:1441-1447`) and checked whenever the file is used again: `mj-controller/src/controller/publication.rs:290`, `network_git.rs:514`, `resume.rs:1237`, `checkpoint/staging.rs:572`, `checkpoint/latched.rs:760`, `move_session.rs:1026` and `:2723`, `mj-daemon/src/daemon/serve.rs:502`, `mj-session-runtime/src/session_manager/standalone.rs:384`. After a download from a target, `mj-session-runtime/src/checkpoint_transfer.rs:166-168` hashes the downloaded file and compares it with the digest the target reported for the file it wrote |
| payload `sha256` and `size` in the manifest | `:1064-1094` | the uncompressed payload | the reader, per entry (`read_verified_entry`, `:1797-1863`; `:475`, `:656`, `:704`, `:1897`) |
| part `sha256` and `size` | `:1245` | each uncompressed 16 MiB slice | the reader, per part |
| manifest digest | `:1271` | the manifest JSON | the writer only, while copying |

In every case the comparison is between a digest recorded for a file and a
fresh hash of the same file. A Go worker's archive is hashed by the Go worker
and re-hashed by a Rust daemon after transfer, and the reverse. The archive is
never rebuilt for comparison.

What the other side requires of the writer (the structural checks in
`read_verified_zip_cancellable`, `:1618-1795`, and `read_verified_entry`):

- only file entries; safe relative names; no duplicate names; exactly one
  `manifest.json`, at most 8 MiB;
- every other entry is listed in the manifest as a payload or a part, and the
  counts match;
- the entry's uncompressed size equals the manifest's;
- the entry's Unix mode (`entry.unix_mode().unwrap_or(0o600) & 0o7777`) equals
  the manifest's `mode`, so the Go writer must set the creator system and
  external attributes the way the `zip` crate does;
- the content hashes to the manifest's digest;
- tar payloads that are not split are parsed while they stream past
  (`tar_payload_validator`, `:1202-1223`) and must contain only entries the
  validators accept (`archive/git.rs:3365-3410`);
- the reader opens entries with the `zip` crate, which in this build
  understands Stored, Deflated and Zstandard (method 93) entries and zip64.

Tests: `writing_the_same_input_twice_produces_identical_archives`
(`archive/tests.rs:1029-1038`) compares two archives of the same input byte
for byte. That is determinism within one implementation: no clock in the
entries, and the same compressed bytes whatever the scheduling of the
parallel part compression. Several tests overwrite bytes at an entry's
`data_start()` and expect verification to fail (`archive/tests.rs:997-1026`,
`1078-1100`, `1176-1192`); they need a decoder that rejects a damaged
Zstandard frame, or the digest check, not particular bytes.

### 2.2 Tar payloads inside checkpoints: verdict (C) for the tar writer

`mj-checkpoint/src/archive/git.rs:3193-3345` (`build_untracked_tar`,
`build_conflicted_worktree_tar`, `append_worktree_entry`) and
`mj-checkpoint/src/archive/git/operation.rs:473-489` (`build_state_tar`).

Writer and settings: `tar::Builder::new(&mut Vec<u8>)`, uncompressed, GNU
headers (`tar::Header::new_gnu()`), `append_data(&mut header, path, reader)`,
`builder.finish()`.

- Regular files: `header.set_metadata(metadata)` (mode, uid, gid, mtime and
  size from the file), then `set_cksum`.
- Symlinks: entry type Symlink, size 0, mode `0o777`, `set_link_name(target)`.
- Operation state: `set_metadata_in_mode(.., HeaderMode::Deterministic)`
  (mtime 1153704088, uid and gid 0, mode `0o644` or `0o755`), entries in
  sorted order.

These tars are checkpoint payloads. Their SHA-256 and size go into the
manifest and are verified by the reader against the same bytes, which by
itself would be (A). Two things make the bytes matter:

1. **A golden pins them.** `clone_manifest_without_a_git_operation_matches_golden`
   (`archive/tests.rs:1045-1071`) compares the whole manifest with
   `mj-checkpoint/tests/golden/clone-archive-manifest.txt`, whose lines 114-117
   and 154-157 read `"path": "repositories/hel/untracked.tar"`,
   `"sha256": "2a5e8bed8d19f92aa99257685c6dc1a98de0ed67ace815a164dfae8a7fba5af4"`,
   `"size": 2048`. The tar is built in the test by `tar_with_file("scripts/tool.sh",
   b"#!/bin/sh\n", 0o755)` (`archive/tests.rs:130-143`, used at `:167`):
   `Header::new_gnu()`, `set_path`, `set_size`, `set_mode`, `set_cksum`,
   `builder.append(&header, contents)`, `builder.finish()`. The Go test calls
   the Go `tar` package the same way and must get the same 2048 bytes: the GNU
   magic `ustar ` and version ` \0`, the mtime field written by `set_mtime(0)`,
   the other numeric fields left as zero bytes, the octal encodings, the
   checksum format, one padded data block and a 1024-byte end marker.
2. **A production rule reads the length.** `publication.rs:572-583` decides
   whether a checkpoint holds unpublished work with
   `payload.path == untracked_tar_path && payload.size > 1024`, commented "An
   empty tar contains two zero blocks." `tar::Builder::finish` writes exactly
   1024 zero bytes (`tar-0.4.46/src/builder.rs:561-567`) and nothing else for
   an archive without entries. A daemon of either implementation applies this
   to manifests written by a worker of either implementation, so an empty tar
   must be exactly 1024 bytes from both (a writer that pads to a 10240-byte
   record, as GNU `tar` does, would make every clean checkout look dirty).

For the general case (headers filled from file metadata) no digest is compared
across implementations, but since the writer has to be exact for the cases
above, port it whole.

### 2.3 Directory fetch (`mj fetch` of a directory): verdict (B)

`mj-checkpoint/src/archive/fetch.rs:517-575` (`zip_directory`).

Writer and settings: `zip::ZipWriter`; directories with `add_directory`,
symlinks with `add_symlink`, both with default options; files Deflated at the
`zip` crate's default level, which is `flate2`'s default, 6
(`zip-2.4.2/src/write.rs:1660-1675`), `large_file(true)` always, Unix mode
`mode & 0o777`; entries in the order `collect_entries` produced them.

What is measured: the zip's length. It is the `length` field of the JSON
header line that precedes the payload on the wire (`fetch.rs:222-231`), is
checked by the receiver against the bytes that arrived (`:236-262`), and is
reported to the caller as `SavedFetch.bytes` (`:212-217`).

Who compares: the receiving process, against the same stream. The number a
user sees for a fetched directory is the size of whatever zip the worker
wrote, so it will differ between a Rust and a Go worker for the same
directory. Nothing breaks. If that number must be the same, the deflate
stream at level 6 and the zip layout must be byte-identical; no test asks for
it (`fetch.rs:690-800` reads the entries back).

### 2.4 Attached resource stream: verdict (A)

`mj-checkpoint/src/resources.rs:50-110`.

Writer and settings: `GzEncoder::new(output, Compression::fast())` (level 1)
around `tar::Builder`; GNU headers; directory children sorted by name; empty
directories get a Directory entry with mode `0o755`; files use
`set_metadata`. Symlinks are refused.

Consumer: `mj-worker install-resource` on the target reads the stream from its
standard input (`mj-controller/src/controller/provisioning.rs:1464-1480`,
`mj-worker-bin/src/main.rs:678`, `install_resource_stream` at
`resources.rs:138-200`). Nothing is hashed or measured. The daemon writes and
the worker reads, so each implementation must read the other's stream.

### 2.5 Profile archives sent to target hosts: verdict (A), read by system `tar`

`mj-worker-staging/src/worker_binary/install.rs:1621-1680`
(`build_profile_archive`) and
`mj-worker-staging/src/worker_binary/profile_transfer.rs:80-148`
(`private_bundle`, `batch`, `archive`).

Writer and settings: `GzEncoder::new(&mut file, Compression::fast())` around
`tar::Builder` with `follow_symlinks(true)`; `append_dir`,
`append_path_with_name` (header from file metadata), `append_link` with a
Symlink header of mode `0o700`, and `append_data` for `.mj-keys`,
`manifest.json` and the nested `private.tar.gz` with mode `0o600`.

What is hashed: file contents, never the archive. The content keys are
`mj_core::hex::file_sha256` of each shared file, with `.x` appended for
executables (`install.rs:1575-1582`), and the receiver recomputes them over
the extracted files (`install.rs:1491-1502`, `profile_receiver.pl:164`).

Consumer: the target host's own `tar`: `tar -xzf - --no-same-owner
--no-same-permissions -C ..` (`install.rs:1469`, `profile_receiver.pl:158` and
`:197`). That may be GNU tar, bsdtar or BusyBox. The requirement is therefore
"what those tools read", and the safe way to meet it is to keep the `tar`
crate's GNU format (long names as `L` entries, not PAX records).

Tests: `install.rs:2483-2512` recompresses an archive with
`Compression::none()`, asserts it is longer than 64 KiB, and expects the
remote `tar` to fail on the archive cut short by 1024 bytes.

### 2.6 Move: per-file tars: verdict (A)

`mj-worker/src/move_workspace.rs:337-355`.

Writer and settings: one uncompressed tar per untracked file:
`tar::Builder::new(File::create(..))`, `follow_symlinks(false)`,
`append_path_with_name(input, path)`, `finish()`.

What is hashed: each staged file, tars included, is listed in the transfer
manifest with its byte count and SHA-256 (`add_file`, `:196-204`) and checked
at the destination against the transferred file (`:463`). The same bytes are
hashed twice; the destination then unpacks with `tar::Archive` (`:578`).

### 2.7 MicroVM copy streams: verdict (A)

`mj-worker/src/microvm/tarcopy.rs:51-90` (`pack`).

Writer and settings: `tar::Builder` with `follow_symlinks(false)` and
`append_dir_all` (directory order as the file system returns it) or a single
item. Consumer: `unpack` (`:93-250`) in the guest agent or on the host, and
container tools that read a tar stream. Nothing is hashed.

### 2.8 Skills archives: verdict (B); (C) for the limit decision if it must be identical

`mj-core/src/skills/mod.rs`.

Writer and settings: the `HELSKIL2` format is the 8-byte magic followed by
`GzEncoder::new(.., Compression::default())` (level 6) over the plain
encoding (`encode`, `:200-216`).

What is hashed: `fingerprint` (`:196-198`) is SHA-256 over the *plain*
encoding "so a fingerprint does not change with the wire format". It does not
depend on the compressor.

What is measured: the compressed length, by compressing into a byte counter
(`gzip_len`, `:107-116`):

- `content_problem` (`:89-104`): a file larger than 1 MiB is left out of the
  tree if it compresses to more than `MAX_SKILLS_FILE_BYTES` (1 MiB). The
  message carries the number: "is {len} bytes and compresses to {compressed}
  bytes, above the 1048576 byte limit".
- `ensure_fits` (`:232-256`): the whole tree fails if magic plus compressed
  body exceeds `MAX_SKILLS_ARCHIVE_BYTES` (4 MiB): "{tree} compresses to
  {compressed} bytes, above the 4194304 byte limit".
- The receiver refuses an archive longer than 4 MiB before inflating
  (`decode`, `:277-283`).

Who compares: the sender against constants. With a different deflate
implementation, a file or tree within a few bytes of a limit is accepted by
one implementation and refused by the other, and the number in the message
differs. The tests use margins (`:1016-1240`: random bytes that cannot shrink,
generated HTML that shrinks well) and one inequality (`:936-939`, gzip smaller
than plain); none pins a size. Only a gzip encoder that reproduces
`miniz_oxide` at level 6 makes the decision and the message identical.

### 2.9 Project memory over the relay: verdict (A)

`mj-core/src/relay/protocol.rs:55-67` (`ProjectMemoryWireTree::encode`).

Writer and settings: `GzEncoder::new(Vec::new(), Compression::default())` over
`serde_json::to_writer(files)`, then standard base64, as the string field
`gzip` of a JSON value.

What is measured: the length of the encoded value. A snapshot is serialized
once and served in pages (`mj-worker/src/worker_runtime/unix/requests.rs:599-625`
reports `total_bytes`; uploads do the same from the daemon,
`mj-session-runtime/src/worker_client/relay.rs:612-640`). The page count and
byte count appear in a log line (`relay.rs:592-600`). The receiver reassembles
and decodes; nothing compares the bytes with anything else.

Tests: `mj-worker/src/worker_runtime/relay_tests.rs:7823-7875` asserts that 10
MiB of incompressible text encodes to more than `MAX_FRAME_BYTES` (8 MiB).

### 2.10 Sealed relay journal segments: verdict (A)

`mj-worker/src/relay/journal.rs:319-340` (`seal_active_relay_segment`).

Writer and settings: `GzEncoder::new(file, Compression::default())` over one
JSON event per line, to `segment-<first>-<last>.jsonl.gz`.

Consumer: the worker itself, later, including a worker of the other
implementation that takes over the same journal directory
(`visit_relay_journal_file`, `:447-465`). Sealing is triggered by the size of
the *uncompressed* active file (`:877`); the size of a sealed segment is not
read anywhere. A Move transfers the files and checks them by digest as in 2.6.

### 2.11 Packed transcript text: verdict (B), in memory only

`mj-core/src/packed_text.rs:5-9` and `:62-75`.

Writer and settings: thread-local `zstd::bulk::Compressor::new(3)`; a string
is kept compressed when the frame is shorter than the text.

The frame never leaves the process: `Serialize` writes the text, `PartialEq`
compares frames or texts. The only way to see the bytes is `{:?}` of a
`PackedText`, which derives `Debug`; no golden contains one. Any Zstandard
implementation serves; the choice only changes memory use.

### 2.12 Image optimisation: verdict (B); (C) if the chosen format, quality and size must be identical

`mj-client/src/image.rs:105-170`.

Writer and settings:

- PNG, for images with transparency: `PngEncoder::new_with_quality(writer,
  CompressionType::Best, FilterType::Adaptive)` (`:152-156`), which is
  `png::Compression::High` with `png::Filter::Adaptive`
  (`image-0.25.10/src/codecs/png.rs:672-693`).
- JPEG, otherwise: `JpegEncoder::new_with_quality(writer, quality)` for
  quality 90, then 85, then 80 (`:18`, `:130`, `:166`).
- Both write into a `LimitedWriter` that fails past `MAX_OUTPUT_BYTES`
  (700 KiB, `mj-core/src/attachment.rs:16`).

What is measured: the encoded length decides the result. If the PNG or every
JPEG quality is over the limit, the image is resized to four fifths with
Lanczos3 (`:140-147`, `:206`) and encoding starts again. So the encoder
decides the format's quality step, the pixel dimensions and, for JPEG, the
pixels themselves.

What is hashed: the encoded bytes become an attachment named by their SHA-256
(`mj-core/src/attachment.rs:42-80`; store files are named by digest,
`:290-345`), and the reference (digest, size, width, height, MIME type) is
written into prompts and relay events. Each side verifies the bytes it
receives against the reference it receives, so the two implementations
interoperate. The same picture pasted into a Rust client and into a Go client
gives different attachments.

An image that already fits (a JPEG or PNG of at most 700 KiB with no
orientation transform) is passed through untouched (`:64-76`).

Test: `noisy_image_is_resized_to_fit_output_budget` (`:344-351`) asserts JPEG
output within the limit and smaller dimensions, not which dimensions.

### 2.13 HTTP responses and static asset ETags: verdict (B)

`mj-server/src/server/routes.rs:227`:
`CompressionLayer::new().quality(CompressionLevel::Fastest)`, with gzip and
Brotli compiled in.

What is hashed: `finalize_static_etag` (`mj-server/src/server/assets.rs:197-256`)
gives a static asset served with a `Content-Encoding` an ETag that is the
SHA-256 of the *compressed* body, cached per (raw ETag, encoding) for the life
of the process.

Who compares: the same server, against the `If-None-Match` a browser sends
back. A browser that cached an asset from a Rust daemon revalidates against a
Go daemon with an ETag the Go daemon does not produce and downloads the asset
once more. Nothing else.

Tests: `viewer_responses_negotiate_fast_compression`
(`mj-server/src/server/tests.rs:4043-4095`) asks for `/viewer.js` with gzip
and with Brotli, requires the body to be smaller than the source, computes
SHA-256 of the body it received and expects `304` and the same ETag when it
sends that back. A second test (`:4100-4130`) requires a Brotli-encoded API
response to be smaller than the plain one. Both need a gzip and a Brotli
*encoder*; neither pins bytes.

## 3. Decoders in production code

These read bytes written by something else and need a conforming decoder only.

| Site | What is read |
| --- | --- |
| `mj-build-cache/src/mbx/install.rs:913` | tar.gz release of `mbx`, after its pinned SHA-256 (`:17-18`) was checked over the download |
| `mj-controller/src/controller/update.rs:957`, `:969` | the release archive of Mjolnir itself (`.zip` or `.tar.gz`), after the published `.sha256` was checked over the download (`:880-890`). Release archives and their checksum files are made by CI with `tar czf`, `zip -q -r -X` and `shasum` (`.github/workflows/release.yml`), not by product code |
| `mj-core/src/harness_install.rs:1154`, `:1825` | tar.gz harness packages, after `file_sha256` of the download (`:1206`) |
| `mj-core/src/harness_install.rs:1975` | a `.zst` release file, with `ruzstd::decoding::StreamingDecoder` |
| `mj-import/src/import/codex.rs:456` | Zstandard-compressed Codex session files, with `ruzstd` |
| `mj-github-relevance/src/github_student/runtime.rs:276`, `:287` | zip and tar.gz runtime downloads |
| `mj-checkpoint/src/resources.rs:142`, `mj-core/src/skills/mod.rs:340`, `mj-core/src/relay/protocol.rs:77`, `mj-worker/src/relay/journal.rs:458` | the gzip streams of section 2 |
| `mj-checkpoint/src/archive.rs:1503-1525` and its callers, `mj-checkpoint/src/archive/git.rs:3367`, `:3407`, `archive/git/operation.rs:705`, `:734`, `:820`, `mj-worker/src/move_workspace.rs:578`, `mj-worker/src/microvm/tarcopy.rs:118` | the zips and tars of section 2 |
| `mj-client/src/image.rs:45-62` | JPEG, PNG and WebP images |

The library `brokk-sessionwiki` also reads Zstandard through `ruzstd` and
writes none (see `library-usage.md`).

## 4. Archives made and read by external tools

Listed so that nobody looks for a compressor to port. In each case a shell
command on a target host produces or consumes the bytes, and both
implementations send the same command text.

- `mj-controller/src/controller/move_session/transfer.rs:339`:
  `tar cf - -C "$1" . | <engine> exec -i "$2" tar xpf - --no-same-owner -C "$3"`.
- `mj-targets/src/targets/container_upgrade.rs:373-540`: `tar` inside
  containers, after a `tar --help` capability probe.
- `mj-worker/src/microvm/image.rs:326-345`: `crane export | tar -x`.
- `mj-cli/src/ec2.rs:1704-1710`: `sha256sum -c` and `tar -xzf` on downloaded
  tools with pinned digests (`:1685-1691`).
- `mj-worker-staging/src/worker_binary/store.rs:450`, `:556`, `:576`,
  `upgrade.rs:777`, `mj-build-cache/src/mbx/configuration.rs:135`:
  `sha256sum`/`shasum -a 256` of uploaded files, compared with digests of the
  uncompressed file computed locally.

## 5. Tests that pin bytes, digests or sizes of compressed or archived output

| Test | Pins | Needs |
| --- | --- | --- |
| `mj-checkpoint/src/archive/tests.rs:1045` `clone_manifest_without_a_git_operation_matches_golden`, golden `mj-checkpoint/tests/golden/clone-archive-manifest.txt:114-117`, `:154-157` | SHA-256 `2a5e8bed..5af4` and size 2048 of a tar built by `tar::Builder` and `Header::new_gnu` | **byte-identical tar writer** |
| `mj-checkpoint/src/archive/tests.rs:1029` `writing_the_same_input_twice_produces_identical_archives` | two archives of one input are equal | deterministic zip, deflate level 1 and Zstandard level 1, including the split, parallel path |
| `mj-checkpoint/src/archive/tests.rs:997`, `1078`, `1176` | damaged entry data fails verification; the error names the part | a decoder that reports corruption |
| `mj-server/src/server/tests.rs:4043` `viewer_responses_negotiate_fast_compression`; `:4100-4130` | ETag equals SHA-256 of the compressed body it was served; compressed is smaller | gzip and Brotli encoders |
| `mj-core/src/skills/mod.rs:936-939`, `1058`, `1086`, `1217`, `1248` | gzip smaller than plain; a random 1.2 MB file is skipped; a large HTML file is collected; an incompressible tree fails; 65 MiB of zeros at `Compression::fast()` stays under 4 MiB | a reasonable deflate at levels 1 and 6 |
| `mj-worker/src/worker_runtime/relay_tests.rs:7865`, `7875` | 10 MiB of incompressible text is over 8 MiB after gzip and base64 | any gzip |
| `mj-worker-staging/src/worker_binary/install.rs:2483-2512` | a stored (`Compression::none()`) gzip is over 64 KiB; the system `tar` rejects it when truncated | gzip level 0 |
| `mj-client/src/image.rs:344` `noisy_image_is_resized_to_fit_output_budget` | JPEG, within 700 KiB, smaller than the input | a JPEG encoder and Lanczos3 resize |

No test asserts a fixed digest of gzip, deflate, Zstandard or zip output, and
the repository has no checked-in archive fixture (`git ls-files` has no
`.zip`, `.tar`, `.gz` or `.zst` file under `mj-*/` or `tests/`; the PNG files
under `mj-server/src/icons/` are served as they are). The other fixed digests
in the code are digests of third-party downloads (`mj-build-cache/src/mbx/install.rs:17-18`,
`mj-cli/src/ec2.rs:1685-1691`, `mj-cli/src/image_commands.rs:242`), of relay
events, or placeholders, and the attachment digest in
`mj-daemon/tests/golden/daemon-upload-exchange.txt` is over literal bytes the
test builds (`mj-daemon/src/daemon/tests.rs:15017-15018`), not encoder output.

Encoders that only tests call, and which the Go test code therefore needs in
some form: `ruzstd::encoding::compress_to_vec(.., CompressionLevel::Fastest)`
(`mj-core/src/harness_install.rs:2299`, `mj-sessionwiki/src/sessionwiki/top_level.rs:365`),
`png::Encoder` (`mj-chat/src/clipboard.rs:849`, `mj-chat/src/chat/attachments.rs:635`,
`mj-chat/src/chat/remote.rs:1564`, `mj-chat/src/chat/tests.rs:99`),
`zip::ZipWriter` and `tar::Builder` to build hostile archives
(`mj-checkpoint/src/archive/tests.rs`, `mj-checkpoint/src/checkpoint/tests.rs`,
`mj-controller/src/controller/update/tests.rs`, `mj-worker/src/microvm/tests.rs`,
`mj-core/src/harness_install.rs:2347-2960`).

## 6. Conclusion: what must be ported exactly

**Must be byte-identical to Rust:**

- The `tar` 0.4.46 writer: `Header::new_gnu` and its setters (`set_path`,
  `set_size`, `set_mode`, `set_mtime`, `set_uid`, `set_gid`, `set_entry_type`,
  `set_link_name`, `set_cksum`, `set_metadata`, `set_metadata_in_mode` with
  `HeaderMode::Deterministic`), `Builder::append`, `append_data`,
  `append_link`, `append_path_with_name`, `append_dir`, `append_dir_all`,
  `follow_symlinks`, GNU long-name entries, block padding and the 1024-byte
  end marker. Reasons: the golden digest (2.2), the 1024-byte rule (2.2) and
  extraction by system `tar` on target hosts (2.5).

**Must be byte-identical only if the outcome at a size limit and reported
byte counts have to be the same in both implementations** (no test requires
it; rule 0 of the translation rules does):

- gzip as `flate2` 1.1.9 writes it over `miniz_oxide` 0.8.9 at level 6
  (`Compression::default()`): the skills limits and their messages (2.8).
  Level 6 is also used for project memory and journal segments, level 1
  (`Compression::fast()`) for resource and profile streams, and the HTTP layer
  uses its fastest gzip; none of those is measured against a limit.
- raw deflate at level 6 inside `zip` for `mj fetch` of a directory (the
  reported size, 2.3).
- the `image` 0.25.10 PNG encoder at `Compression::High` with adaptive
  filtering, its JPEG encoder at qualities 90, 85 and 80, and
  `resize_exact(.., Lanczos3)` (2.12).

**Must be deterministic and readable by Rust, not identical:**

- the `zip` 2.4.2 writer: Stored, Deflated (level 1 for the checkpoint
  manifest, 6 for fetch) and Zstandard (level 1) entries, zip64 by
  `large_file`, Unix permissions, `add_directory`, `add_symlink`,
  `raw_copy_file`, entries stamped 1980-01-01;
- Zstandard frames at level 1 (in zips) and level 3 (in memory);
- gzip and Brotli for HTTP responses.

**Decoder only:** `ruzstd` (production code only decodes), gzip, zip (Stored,
Deflated, Zstandard), tar, PNG, JPEG and WebP images.

### Against the plan in `runtime-packages.md`

- `tar` as a port with exact header bytes: confirmed, and it is the one
  package where exactness is required.
- `flate2` as "port for writing, compressed bytes equal to miniz_oxide's at
  each level where a digest covers them": no digest covers Mjolnir-written
  deflate bytes across implementations. The only reasons to port
  `miniz_oxide`'s compressor are the size-dependent behaviours above (level 6
  for skills and fetch). If those may differ by a few bytes at a limit, a
  wrapper over `compress/flate` is enough, provided it is deterministic.
- `zip` as "port for writing": confirmed for structure (the Rust reader checks
  modes, names and sizes), with one addition the row does not mention:
  Zstandard entries (method 93), which every checkpoint uses.
- `zstd` and `ruzstd` as wrappers: confirmed. No caller hashes Zstandard
  bytes except as part of a whole archive file that is re-hashed, never
  rebuilt.
- `image` as "wrapper over `image/png`": the product also needs a JPEG
  encoder, JPEG and WebP decoders, EXIF orientation, a Lanczos3 resize and an
  output-size limit, and the encoders' output lengths decide behaviour (2.12).
- Not in the table: a Brotli encoder for the `tower-http` compression layer
  (2.13).
