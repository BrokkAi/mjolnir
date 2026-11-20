# Three crates: url, qrcode, regex (with aho-corasick)

Build each as its own package, finishing one before starting the next.

1. `go/crates/url` for url 2.5.8 (oracle `url_oracle.rs`, feature `url`).
   Mjolnir uses `url::Url` in about 20 files: parsing, `join`, component
   getters and setters, `query_pairs`/`query_pairs_mut`, file path
   conversions, `make_relative`, `url::Host`, `form_urlencoded`, percent
   encode sets, serde as a string. `url` implements the WHATWG URL Standard,
   which differs from Go's `net/url` in many ways, so port the parser
   (`parser.rs`, `host.rs`, `path_segments.rs`, `quirks.rs`, `origin.rs`,
   `slicing.rs`) and the relevant parts of `form_urlencoded` and
   `percent-encoding`. For IDNA (find what 2.5.8 resolves to in `Cargo.lock`)
   use `golang.org/x/net/idna` configured to match as closely as possible and
   state the known differences; a conformance test that fails only because of
   a documented IDNA difference is recorded as such in `doc.go`, with the
   failing cases listed, rather than left red. Exact `ParseError` variants and
   text. Tests: the crate's tests with the WHATWG `urltestdata.json` and
   `setters_tests.json` it ships, and differential tests over generated URLs
   and relative references for parse, join, every getter and setter, and
   `query_pairs`.
2. `go/crates/qrcode` for qrcode 0.14.1, `default-features = false` (oracle
   `qrcode_oracle.rs`): used once in `mj-tui` to draw a QR code; the module
   matrix must be identical. Port `types.rs`, `bits.rs` with the segment
   optimiser, `ec.rs`, `canvas.rs` (patterns, placement, all masks and the
   penalty scoring), version and level selection, and the accessors used.
   Differential tests over thousands of inputs at every level.
3. `go/crates/regex` for regex 1.12.3 with `go/crates/ahocorasick` for
   aho-corasick 1.1.4 (oracle `regex_oracle.rs`): list every pattern and
   method Mjolnir uses. `regex.RegexNew(pattern)` translates a Rust pattern
   into an equivalent Go `regexp` pattern (expanding Unicode-aware `\d`, `\w`,
   `\s`, `\b` and case folding from Rust's tables where Go differs), or errors
   naming the construct it cannot translate exactly; methods as used
   (`is_match`, `find`, `find_iter`, `captures`, `captures_iter`, `replace`,
   `replace_all` with Rust's `$name` syntax, `split`, named groups) returning
   options and Rust-shaped matches. Verify every pattern Mjolnir uses plus a
   broad syntax corpus against the oracle. `ahocorasick`: the options and
   match kind Mjolnir uses with identical results.

Report per package.
