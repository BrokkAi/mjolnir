# Terminal UI, part 2: widgets, terminal and backend

Package `go/crates/ratatui` (the same package as part 1), plus
`go/crates/tuiterm` (tui-term 0.3.4) and `go/crates/ratfocus` (rat-focus
2.1.1). Oracle module `ratatui_oracle.rs`, feature `ratatui` (it has all
three). Start after part 1 (`ratatui-core.md`), `go/crates/crossterm` and
`go/crates/vt100` exist.

Inventory which widgets and which of their options Mjolnir uses (grep
`ratatui::widgets`, `Paragraph`, `Block`, `List`, `Table`, `Tabs`,
`Scrollbar`, `Gauge`, `Clear`, `Wrap`, `Borders`, `Padding`, `BorderType`,
`Cell`, `Row`, `ListItem`, `ListState`, `TableState`, `ScrollbarState`,
`Sparkline`, `Chart`, `Canvas`, `Calendar`, ...) into `doc.go`, and port each
used widget completely from `ratatui-widgets-0.3.0/src/`:

- `Block` (borders, border types and sets, titles top and bottom with
  alignment and style, padding, `inner`, merge strategies), `Paragraph`
  (`wrap` with `trim`, `scroll`, alignment, `line_count`, `line_width`, and
  the reflow module: `WordWrapper`, `LineTruncator`, exactly as written:
  where a word breaks, how trailing and leading whitespace is handled, how
  wide graphemes and zero-width characters are placed; the workspace enables
  `unstable-rendered-line-info`), `List` and `ListState` (highlight symbol,
  spacing, direction, scroll offset arithmetic, `scroll_padding`), `Table` and
  `TableState` (column width solving through `Layout`, spacing, header and
  footer, selection highlight in each mode), `Tabs`, `Scrollbar` and
  `ScrollbarState` (thumb size and position arithmetic), `Gauge`, `LineGauge`,
  `Clear`, `Sparkline`, `BarChart`, `Chart`, `Canvas`, `Calendar` only if
  used.
- `Terminal` (`new`, `with_options`, `draw`, `try_draw`, `autoresize`,
  `resize`, `clear`, `flush`, `hide_cursor`, `show_cursor`,
  `set_cursor_position`, `get_frame`, `current_buffer_mut`, `swap_buffers`,
  `insert_before`, `Viewport` in its three kinds), `Frame` (`area`,
  `render_widget`, `render_stateful_widget`, `set_cursor_position`,
  `buffer_mut`, `count`), `CompletedFrame`.
- Backends: `CrosstermBackend` over `go/crates/crossterm`, writing exactly the
  byte stream `ratatui-crossterm` writes for a buffer diff (cursor moves only
  when needed, style changes as modifier diffs then colors, the reset at the
  end, `underline-color` handling), and `TestBackend` (`new`, `buffer`,
  `assert_buffer`, `assert_buffer_lines`, `resize`, its `Display`), which the
  product's tests render into.
- `tuiterm`: `PseudoTerminal` widget over a `vt100` screen (cell conversion,
  cursor rendering options, style mapping).
- `ratfocus`: `FocusFlag`, `Focus`, `FocusBuilder`, `HasFocus`, `Navigation`
  and the traversal order and transitions as used.

Tests: the crates' own tests for every widget you port (each widget file has
a large test module with expected buffers; they are the specification), and
differential tests against the oracle: for each used widget, generated
configurations and content (including wide and combining characters, empty
content, areas of width or height 0 and 1, scroll offsets past the end)
rendered on both sides, comparing every cell; for the crossterm backend,
sequences of frames, comparing the bytes written.
