# `go/crates/unicodewidth` (unicode-width 0.2.2) and `go/crates/unicodesegmentation` (unicode-segmentation 1.13.3)

Oracle module `go/port/oracle/src/unicode.rs`, feature `unicode`.

These decide where every character lands on the terminal: the ratatui port and
Mjolnir's own wrapping call them, and golden screens are compared cell for
cell. Reproduce these crate versions' tables and algorithms, not another
Unicode version or another library's idea of width.

`unicodewidth`: `UnicodeWidthChar::width` (None for control characters),
`width_cjk`, `UnicodeWidthStr::width`, `width_cjk`. Version 0.2 computes string
width with context (emoji and text presentation selectors, ZWJ sequences, the
Lisu, Buginese and Arabic ligature rules, CRLF, default-ignorables): port all
of `src/tables.rs`, generating Go tables from the crate's data. Check the
features in Mjolnir's build and the uses in `mj-*/` and in ratatui 0.30
(`ratatui-core-0.1.0`, `ratatui-widgets-0.3.0`).

`unicodesegmentation`: `graphemes(true|false)`, `grapheme_indices`,
`GraphemeCursor` if Mjolnir or ratatui uses it, `unicode_words`,
`split_word_bounds`, `split_word_bound_indices`, sentence functions only if
used: the crate's algorithms and tables, including emoji ZWJ, regional
indicators and GB9c as implemented in 1.13.3.

API: plain functions over `string` and `rune` named by the Rust path rule,
with documented short aliases if the long names are unwieldy.

Tests: the crates' own tests, including the Unicode conformance tables the
crate ships (`src/testdata.rs`). Differential tests: exhaustive per-code-point
comparison of `width` and `width_cjk` for every scalar value; string width,
grapheme and word boundaries over at least 20,000 generated strings biased to
the hard cases (emoji with skin tones and ZWJ, flags, variation selectors,
combining marks, Hangul jamo, Indic scripts, Arabic, controls, CRLF, tabs,
zero-width characters, wide CJK, mixed scripts).
