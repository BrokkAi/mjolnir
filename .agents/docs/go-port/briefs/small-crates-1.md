# Five small crates: similar, linkify, semver, bytesize, base64

Build each as its own package, finishing (code, ported tests, oracle
comparison, `doc.go`) one before starting the next.

1. `go/crates/similar` for similar 2.7.0 (oracle `similar_oracle.rs`, feature
   `similar`). Mjolnir uses `TextDiff::from_lines`, `.unified_diff().header(a,
   b).to_string()`, `iter_all_changes`/`ChangeTag` and more (grep
   `mj-core/src/diff.rs`, `golden.rs`, `project_memory.rs`). Port the Myers
   algorithm as the crate implements it, the line tokenizer, op grouping with
   context radius and the unified printer, so the edit script and unified text
   equal Rust's for any input. Differential test on random and adversarial
   line sequences.
2. `go/crates/linkify` for linkify 0.11.0 (oracle `linkify_oracle.rs`): the
   finder as `mj-chat` uses it, with the same span boundaries (trailing
   punctuation, brackets, quotes, IDN). Differential test over generated text
   with links in every delimiter context.
3. `go/crates/semver` for semver 1.0.28 (oracle `semver_oracle.rs`): `Version`
   parse, `Display`, ordering with pre-release and build metadata,
   `BuildMetadata`, `Prerelease`, `VersionReq` only if used; exact error text.
4. `go/crates/bytesize` for bytesize 2.7.0 (oracle `bytesize_oracle.rs`): what
   Mjolnir uses (`ByteSize` and its `Display`), exact output for all
   magnitudes and both unit styles used.
5. `go/crates/base64` for base64 0.22.1 (oracle `base64_oracle.rs`): the
   engines used (`STANDARD`, `STANDARD_NO_PAD`, `URL_SAFE`, `URL_SAFE_NO_PAD`),
   encode and decode as called, `DecodeError` variants with exact text
   (`Invalid symbol 61, offset 4.`, `Invalid padding`, `Invalid last symbol
   ...`, `Invalid input length`), and the crate's strictness on padding and
   trailing bits (Go's `encoding/base64` is more lenient: verify each
   rejection against the oracle).

Names follow the Rust path rule of the brief. Report per package.
