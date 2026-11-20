# Terminal UI, part 1: `go/crates/ratatui` core

`ratatui-core` 0.1.0 with its layout solver `kasuari` 0.4.12. Oracle module
`ratatui_oracle.rs`, feature `ratatui`.

Mjolnir's dashboard is drawn with ratatui 0.30.0 (`ratatui-core` 0.1.0,
`ratatui-widgets` 0.3.0, `ratatui-crossterm` 0.1.0). About 90 product files use
it and 83 golden files hold whole rendered screens that the Go port must
reproduce cell for cell. Layout arithmetic, text measurement, wrapping, style
merging and buffer writes must give identical cells, so this is a port from
source.

This is part 1, the core. Widgets, the terminal, the backend and `Frame` are
part 2 (`ratatui-widgets.md`), built on this; design for them.

One flat package `go/crates/ratatui`, names by the Rust path rule
(`ratatui.LayoutRect`, `ratatui.LayoutConstraint_Length{F0: 3}`,
`ratatui.BufferBuffer`, `ratatui.StyleStyle`, `ratatui.TextLine`). These names
appear thousands of times in product code: decide whether to offer shorter
aliases and document the scheme in `doc.go`.

1. `kasuari` (the Cassowary solver): complete and exact, including the order
   of floating-point operations, strengths, the tableau's row and symbol
   ordering and tie-breaking (find out which map and hasher it uses, because
   iteration order decides which of several optimal solutions comes out, and
   reproduce that order deterministically), edit variables, `fetch_changes`.
2. `layout/`: `Rect` (every method, saturating arithmetic), `Position`,
   `Size`, `Offset`, `Margin`, the alignment types, `Direction`, `Constraint`
   (all six kinds), `Flex` (all modes), `Spacing`, `Layout` (`split`, `areas`,
   `spacers`, `split_with_spacers`, margins, the solver setup with its
   strengths and rounding; the LRU cache is invisible and optional).
3. `style/`: `Style` (`patch`, modifiers, colors, the `Stylize` shorthands
   product code uses heavily: `.bold()`, `.red()`, `.on_blue()`, `.dim()`),
   `Color` (every variant, parsing, `Display`, conversions), `Modifier` with
   its `Debug` form, palettes and serde forms only if used.
4. `text/`: `Span`, `Line`, `Text`, `Masked`, `StyledGrapheme` with all
   constructors and conversions, `width()`, `styled_graphemes` (with ratatui's
   control-character filtering and zero-width handling), alignment,
   `patch_style`, `reset_style`, pushes, iterators, `Display`, and their
   rendering into a buffer (truncation and alignment exactly, including wide
   graphemes at the edge).
5. `buffer/`: `Cell`, `Buffer` (`empty`, `filled`, `with_lines`, indexing and
   its panic text, `cell`, `cell_mut`, `set_string`, `set_stringn`,
   `set_line`, `set_span`, `set_style`, `resize`, `reset`, `merge`, `diff`
   with the multi-width invalidation, and the `Debug` format). Check how the
   `mj-tui` and `mj-chat` goldens turn a `Buffer` into text and make sure
   everything they call exists.
6. The widget traits as Go interfaces, and `symbols`.

Width and segmentation come from `go/crates/unicodewidth` and
`go/crates/unicodesegmentation`. Do not write your own tables.

Tests: the crates' own tests, thoroughly. `ratatui-core` has thousands beside
the code (the `layout.rs` module alone covers every constraint and flex
combination with expected splits); they are the specification. Differential
tests: generated layouts (1 to 8 constraints of every kind, every flex mode,
spacing including overlap, margins, areas from 0 to 300 cells), comparing
rects and spacers exactly; generated styled text (ASCII, wide CJK, emoji
sequences, combining marks, zero-width and control characters) rendered at
various widths and alignments, comparing every cell; random buffer operation
sequences, comparing buffers and `diff` output.
