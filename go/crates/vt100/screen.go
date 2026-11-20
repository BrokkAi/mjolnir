// Port of vt100 0.16.2 src/screen.rs

package vt100

import (
	"iter"

	"github.com/BrokkAi/mjolnir/go/crates/vte"
	"github.com/BrokkAi/mjolnir/go/rs"
)

const modeApplicationKeypad uint8 = 0b0000_0001
const modeApplicationCursor uint8 = 0b0000_0010
const modeHideCursor uint8 = 0b0000_0100
const modeAlternateScreen uint8 = 0b0000_1000
const modeBracketedPaste uint8 = 0b0001_0000

// The xterm mouse handling mode currently in use.
type MouseProtocolMode uint8

const (
	// Mouse handling is disabled.
	MouseProtocolMode_None MouseProtocolMode = iota

	// Mouse button events should be reported on button press. Also known as
	// X10 mouse mode.
	MouseProtocolMode_Press

	// Mouse button events should be reported on button press and release.
	// Also known as VT200 mouse mode.
	MouseProtocolMode_PressRelease

	// Highlight,
	// Mouse button events should be reported on button press and release, as
	// well as when the mouse moves between cells while a button is held
	// down.
	MouseProtocolMode_ButtonMotion

	// Mouse button events should be reported on button press and release,
	// and mouse motion events should be reported when the mouse moves
	// between cells regardless of whether a button is held down or not.
	MouseProtocolMode_AnyMotion
	// DecLocator,
)

var _ = rs.RegisterEnum[MouseProtocolMode]("MouseProtocolMode",
	rs.Const(MouseProtocolMode_None, "None"),
	rs.Const(MouseProtocolMode_Press, "Press"),
	rs.Const(MouseProtocolMode_PressRelease, "PressRelease"),
	rs.Const(MouseProtocolMode_ButtonMotion, "ButtonMotion"),
	rs.Const(MouseProtocolMode_AnyMotion, "AnyMotion"),
)

// MouseProtocolModeDefault is `MouseProtocolMode::default()`.
func MouseProtocolModeDefault() MouseProtocolMode {
	return MouseProtocolMode_None
}

// The encoding to use for the enabled [`MouseProtocolMode`].
type MouseProtocolEncoding uint8

const (
	// Default single-printable-byte encoding.
	MouseProtocolEncoding_Default MouseProtocolEncoding = iota

	// UTF-8-based encoding.
	MouseProtocolEncoding_Utf8

	// SGR-like encoding.
	MouseProtocolEncoding_Sgr
	// Urxvt,
)

var _ = rs.RegisterEnum[MouseProtocolEncoding]("MouseProtocolEncoding",
	rs.Const(MouseProtocolEncoding_Default, "Default"),
	rs.Const(MouseProtocolEncoding_Utf8, "Utf8"),
	rs.Const(MouseProtocolEncoding_Sgr, "Sgr"),
)

// MouseProtocolEncodingDefault is `MouseProtocolEncoding::default()`.
func MouseProtocolEncodingDefault() MouseProtocolEncoding {
	return MouseProtocolEncoding_Default
}

// Represents the overall terminal state.
type Screen struct {
	grid          gridGrid `rs:"grid"`
	alternateGrid gridGrid `rs:"alternate_grid"`

	attrs      attrsAttrs `rs:"attrs"`
	savedAttrs attrsAttrs `rs:"saved_attrs"`

	modes                 uint8                 `rs:"modes"`
	mouseProtocolMode     MouseProtocolMode     `rs:"mouse_protocol_mode"`
	mouseProtocolEncoding MouseProtocolEncoding `rs:"mouse_protocol_encoding"`
}

var _ = rs.RegisterStruct[Screen]("Screen")

// Clone is `screen.clone()`: a copy that shares nothing with the screen it
// was made from.
func (self *Screen) Clone() Screen {
	return Screen{
		grid:                  self.grid.clone(),
		alternateGrid:         self.alternateGrid.clone(),
		attrs:                 self.attrs,
		savedAttrs:            self.savedAttrs,
		modes:                 self.modes,
		mouseProtocolMode:     self.mouseProtocolMode,
		mouseProtocolEncoding: self.mouseProtocolEncoding,
	}
}

func screenNew(
	size gridSize,
	scrollbackLen int,
) Screen {
	grid := gridGridNew(size, scrollbackLen)
	grid.allocateRows()
	return Screen{
		grid:          grid,
		alternateGrid: gridGridNew(size, 0),

		attrs:      attrsAttrs{},
		savedAttrs: attrsAttrs{},

		modes:                 0,
		mouseProtocolMode:     MouseProtocolModeDefault(),
		mouseProtocolEncoding: MouseProtocolEncodingDefault(),
	}
}

// Resizes the terminal.
func (self *Screen) SetSize(rows uint16, cols uint16) {
	self.grid.setSize(gridSize{rows: rows, cols: cols})
	self.alternateGrid.
		setSize(gridSize{rows: rows, cols: cols})
}

// Returns the current size of the terminal.
//
// The return value will be (rows, cols).
func (self *Screen) Size() (uint16, uint16) {
	size := self.gridMethod().sizeMethod()
	return size.rows, size.cols
}

// Scrolls to the given position in the scrollback.
//
// This position indicates the offset from the top of the screen, and
// should be `0` to put the normal screen in view.
//
// This affects the return values of methods called on the screen: for
// instance, `screen.cell(0, 0)` will return the top left corner of the
// screen after taking the scrollback offset into account.
//
// The value given will be clamped to the actual size of the scrollback.
func (self *Screen) SetScrollback(rows int) {
	self.gridMut().setScrollback(rows)
}

// Returns the current position in the scrollback.
//
// This position indicates the offset from the top of the screen, and is
// `0` when the normal screen is in view.
func (self *Screen) Scrollback() int {
	return self.gridMethod().scrollbackMethod()
}

// Returns the text contents of the terminal.
//
// This will not include any formatting information, and will be in plain
// text format.
func (self *Screen) Contents() string {
	contents := []uint8{}
	self.writeContents(&contents)
	return string(contents)
}

func (self *Screen) writeContents(contents *[]uint8) {
	self.gridMethod().writeContents(contents)
}

// Returns the text contents of the terminal by row, restricted to the
// given subset of columns.
//
// This will not include any formatting information, and will be in plain
// text format.
//
// Newlines will not be included.
//
// The rows are produced as they are asked for, as the Rust iterator's are.
func (self *Screen) Rows(
	start uint16,
	width uint16,
) iter.Seq[string] {
	grid := self.gridMethod()
	return func(yield func(string) bool) {
		grid.visibleRows(func(_ int, row *rowRow) bool {
			contents := []uint8{}
			row.writeContents(&contents, start, width, false)
			return yield(string(contents))
		})
	}
}

// Returns the text contents of the terminal logically between two cells.
// This will include the remainder of the starting row after `start_col`,
// followed by the entire contents of the rows between `start_row` and
// `end_row`, followed by the beginning of the `end_row` up until
// `end_col`. This is useful for things like determining the contents of
// a clipboard selection.
func (self *Screen) ContentsBetween(
	startRow uint16,
	startCol uint16,
	endRow uint16,
	endCol uint16,
) string {
	switch {
	case startRow < endRow:
		_, cols := self.Size()
		contents := []uint8{}
		self.
			gridMethod().
			visibleRows(func(i int, row *rowRow) bool {
				if i < int(startRow) {
					return true
				}
				if i-int(startRow) >= int(endRow)-int(startRow)+1 {
					return false
				}
				if i == int(startRow) {
					row.writeContents(
						&contents,
						startCol,
						cols-startCol,
						false,
					)
					if !row.wrappedMethod() {
						contents = append(contents, '\n')
					}
				} else if i == int(endRow) {
					row.writeContents(&contents, 0, endCol, false)
				} else {
					row.writeContents(&contents, 0, cols, false)
					if !row.wrappedMethod() {
						contents = append(contents, '\n')
					}
				}
				return true
			})
		return string(contents)
	case startRow == endRow:
		if startCol < endCol {
			index := 0
			for row := range self.Rows(startCol, endCol-startCol) {
				if index == int(startRow) {
					return row
				}
				index++
			}
			return ""
		} else {
			return ""
		}
	default:
		return ""
	}
}

// Return escape codes sufficient to reproduce the entire contents of the
// current terminal state. This is a convenience wrapper around
// [`contents_formatted`](Self::contents_formatted) and
// [`input_mode_formatted`](Self::input_mode_formatted).
func (self *Screen) StateFormatted() []uint8 {
	contents := []uint8{}
	self.writeContentsFormatted(&contents)
	self.writeInputModeFormatted(&contents)
	return contents
}

// Return escape codes sufficient to turn the terminal state of the
// screen `prev` into the current terminal state. This is a convenience
// wrapper around [`contents_diff`](Self::contents_diff) and
// [`input_mode_diff`](Self::input_mode_diff).
func (self *Screen) StateDiff(prev *Screen) []uint8 {
	contents := []uint8{}
	self.writeContentsDiff(&contents, prev)
	self.writeInputModeDiff(&contents, prev)
	return contents
}

// Returns the formatted visible contents of the terminal.
//
// Formatting information will be included inline as terminal escape
// codes. The result will be suitable for feeding directly to a raw
// terminal parser, and will result in the same visual output.
func (self *Screen) ContentsFormatted() []uint8 {
	contents := []uint8{}
	self.writeContentsFormatted(&contents)
	return contents
}

func (self *Screen) writeContentsFormatted(contents *[]uint8) {
	termHideCursorNew(self.HideCursor()).writeBuf(contents)
	prevAttrs := self.gridMethod().writeContentsFormatted(contents)
	self.attrs.writeEscapeCodeDiff(contents, &prevAttrs)
}

// Returns the formatted visible contents of the terminal by row,
// restricted to the given subset of columns.
//
// Formatting information will be included inline as terminal escape
// codes. The result will be suitable for feeding directly to a raw
// terminal parser, and will result in the same visual output.
//
// You are responsible for positioning the cursor before printing each
// row, and the final cursor position after displaying each row is
// unspecified.
//
// The rows are produced as they are asked for, as the Rust iterator's are.
func (self *Screen) RowsFormatted(
	start uint16,
	width uint16,
) iter.Seq[[]uint8] {
	grid := self.gridMethod()
	return func(yield func([]uint8) bool) {
		wrapping := false
		grid.visibleRows(func(i_ int, row *rowRow) bool {
			// number of rows in a grid is stored in a u16 (see Size), so
			// visible_rows can never return enough rows to overflow here
			i := uint16(i_)
			contents := []uint8{}
			row.writeContentsFormatted(
				&contents,
				start,
				width,
				i,
				wrapping,
				rs.None[gridPos](),
				rs.None[attrsAttrs](),
			)
			if start == 0 && width == self.grid.sizeMethod().cols {
				wrapping = row.wrappedMethod()
			}
			return yield(contents)
		})
	}
}

// Returns a terminal byte stream sufficient to turn the visible contents
// of the screen described by `prev` into the visible contents of the
// screen described by `self`.
//
// The result of rendering `prev.contents_formatted()` followed by
// `self.contents_diff(prev)` should be equivalent to the result of
// rendering `self.contents_formatted()`. This is primarily useful when
// you already have a terminal parser whose state is described by `prev`,
// since the diff will likely require less memory and cause less
// flickering than redrawing the entire screen contents.
func (self *Screen) ContentsDiff(prev *Screen) []uint8 {
	contents := []uint8{}
	self.writeContentsDiff(&contents, prev)
	return contents
}

func (self *Screen) writeContentsDiff(contents *[]uint8, prev *Screen) {
	if self.HideCursor() != prev.HideCursor() {
		termHideCursorNew(self.HideCursor()).
			writeBuf(contents)
	}
	prevAttrs := self.gridMethod().writeContentsDiff(
		contents,
		prev.gridMethod(),
		prev.attrs,
	)
	self.attrs.writeEscapeCodeDiff(contents, &prevAttrs)
}

// Returns a sequence of terminal byte streams sufficient to turn the
// visible contents of the subset of each row from `prev` (as described
// by `start` and `width`) into the visible contents of the corresponding
// row subset in `self`.
//
// You are responsible for positioning the cursor before printing each
// row, and the final cursor position after displaying each row is
// unspecified.
//
// The rows are produced as they are asked for, as the Rust iterator's are.
func (self *Screen) RowsDiff(
	prev *Screen,
	start uint16,
	width uint16,
) iter.Seq[[]uint8] {
	grid, prevGrid := self.gridMethod(), prev.gridMethod()
	return func(yield func([]uint8) bool) {
		rows, prevRows := grid.visibleRowsSlice(), prevGrid.visibleRowsSlice()
		for i_ := 0; i_ < len(rows) && i_ < len(prevRows); i_++ {
			row, prevRow := rows[i_], prevRows[i_]
			// number of rows in a grid is stored in a u16 (see Size), so
			// visible_rows can never return enough rows to overflow here
			i := uint16(i_)
			contents := []uint8{}
			row.writeContentsDiff(
				&contents,
				prevRow,
				start,
				width,
				i,
				false,
				false,
				gridPos{row: i, col: start},
				attrsAttrs{},
			)
			if !yield(contents) {
				return
			}
		}
	}
}

// Returns terminal escape sequences sufficient to set the current
// terminal's input modes.
//
// Supported modes are:
// * application keypad
// * application cursor
// * bracketed paste
// * xterm mouse support
func (self *Screen) InputModeFormatted() []uint8 {
	contents := []uint8{}
	self.writeInputModeFormatted(&contents)
	return contents
}

func (self *Screen) writeInputModeFormatted(contents *[]uint8) {
	termApplicationKeypadNew(
		self.mode(modeApplicationKeypad),
	).
		writeBuf(contents)
	termApplicationCursorNew(
		self.mode(modeApplicationCursor),
	).
		writeBuf(contents)
	termBracketedPasteNew(self.mode(modeBracketedPaste)).
		writeBuf(contents)
	termMouseProtocolModeNew(
		self.mouseProtocolMode,
		MouseProtocolMode_None,
	).
		writeBuf(contents)
	termMouseProtocolEncodingNew(
		self.mouseProtocolEncoding,
		MouseProtocolEncoding_Default,
	).
		writeBuf(contents)
}

// Returns terminal escape sequences sufficient to change the previous
// terminal's input modes to the input modes enabled in the current
// terminal.
func (self *Screen) InputModeDiff(prev *Screen) []uint8 {
	contents := []uint8{}
	self.writeInputModeDiff(&contents, prev)
	return contents
}

func (self *Screen) writeInputModeDiff(contents *[]uint8, prev *Screen) {
	if self.mode(modeApplicationKeypad) !=
		prev.mode(modeApplicationKeypad) {
		termApplicationKeypadNew(
			self.mode(modeApplicationKeypad),
		).
			writeBuf(contents)
	}
	if self.mode(modeApplicationCursor) !=
		prev.mode(modeApplicationCursor) {
		termApplicationCursorNew(
			self.mode(modeApplicationCursor),
		).
			writeBuf(contents)
	}
	if self.mode(modeBracketedPaste) != prev.mode(modeBracketedPaste) {
		termBracketedPasteNew(self.mode(modeBracketedPaste)).
			writeBuf(contents)
	}
	termMouseProtocolModeNew(
		self.mouseProtocolMode,
		prev.mouseProtocolMode,
	).
		writeBuf(contents)
	termMouseProtocolEncodingNew(
		self.mouseProtocolEncoding,
		prev.mouseProtocolEncoding,
	).
		writeBuf(contents)
}

// Returns terminal escape sequences sufficient to set the current
// terminal's drawing attributes.
//
// Supported drawing attributes are:
// * fgcolor
// * bgcolor
// * bold
// * dim
// * italic
// * underline
// * inverse
//
// This is not typically necessary, since
// [`contents_formatted`](Self::contents_formatted) will leave
// the current active drawing attributes in the correct state, but this
// can be useful in the case of drawing additional things on top of a
// terminal output, since you will need to restore the terminal state
// without the terminal contents necessarily being the same.
func (self *Screen) AttributesFormatted() []uint8 {
	contents := []uint8{}
	self.writeAttributesFormatted(&contents)
	return contents
}

func (self *Screen) writeAttributesFormatted(contents *[]uint8) {
	termClearAttrs{}.writeBuf(contents)
	self.attrs.writeEscapeCodeDiff(
		contents,
		&attrsAttrs{},
	)
}

// Returns the current cursor position of the terminal.
//
// The return value will be (row, col).
func (self *Screen) CursorPosition() (uint16, uint16) {
	pos := self.gridMethod().posMethod()
	return pos.row, pos.col
}

// Returns terminal escape sequences sufficient to set the current
// cursor state of the terminal.
//
// This is not typically necessary, since
// [`contents_formatted`](Self::contents_formatted) will leave
// the cursor in the correct state, but this can be useful in the case of
// drawing additional things on top of a terminal output, since you will
// need to restore the terminal state without the terminal contents
// necessarily being the same.
//
// Note that the bytes returned by this function may alter the active
// drawing attributes, because it may require redrawing existing cells in
// order to position the cursor correctly (for instance, in the case
// where the cursor is past the end of a row). Therefore, you should
// ensure to reset the active drawing attributes if necessary after
// processing this data, for instance by using
// [`attributes_formatted`](Self::attributes_formatted).
func (self *Screen) CursorStateFormatted() []uint8 {
	contents := []uint8{}
	self.writeCursorStateFormatted(&contents)
	return contents
}

func (self *Screen) writeCursorStateFormatted(contents *[]uint8) {
	termHideCursorNew(self.HideCursor()).writeBuf(contents)
	self.gridMethod().
		writeCursorPositionFormatted(contents, rs.None[gridPos](), rs.None[attrsAttrs]())

	// we don't just call write_attributes_formatted here, because that
	// would still be confusing - consider the case where the user sets
	// their own unrelated drawing attributes (on a different parser
	// instance) and then calls cursor_state_formatted. just documenting
	// it and letting the user handle it on their own is more
	// straightforward.
}

// Returns the [`Cell`](crate::Cell) object at the given location in the
// terminal, if it exists.
//
// The cell is the screen's own: it is valid until the screen is next
// changed, as the Rust borrow is.
func (self *Screen) Cell(row uint16, col uint16) rs.Option[*Cell] {
	return self.gridMethod().visibleCell(gridPos{row: row, col: col})
}

// Returns whether the text in row `row` should wrap to the next line.
func (self *Screen) RowWrapped(row uint16) bool {
	if row, ok := self.gridMethod().visibleRow(row).Get(); ok {
		return row.wrappedMethod()
	}
	return false
}

// Returns whether the alternate screen is currently in use.
func (self *Screen) AlternateScreen() bool {
	return self.mode(modeAlternateScreen)
}

// Returns whether the terminal should be in application keypad mode.
func (self *Screen) ApplicationKeypad() bool {
	return self.mode(modeApplicationKeypad)
}

// Returns whether the terminal should be in application cursor mode.
func (self *Screen) ApplicationCursor() bool {
	return self.mode(modeApplicationCursor)
}

// Returns whether the terminal should be in hide cursor mode.
func (self *Screen) HideCursor() bool {
	return self.mode(modeHideCursor)
}

// Returns whether the terminal should be in bracketed paste mode.
func (self *Screen) BracketedPaste() bool {
	return self.mode(modeBracketedPaste)
}

// Returns the currently active [`MouseProtocolMode`].
func (self *Screen) MouseProtocolMode() MouseProtocolMode {
	return self.mouseProtocolMode
}

// Returns the currently active [`MouseProtocolEncoding`].
func (self *Screen) MouseProtocolEncoding() MouseProtocolEncoding {
	return self.mouseProtocolEncoding
}

// Returns the currently active foreground color.
func (self *Screen) Fgcolor() Color {
	return self.attrs.fgcolor.color()
}

// Returns the currently active background color.
func (self *Screen) Bgcolor() Color {
	return self.attrs.bgcolor.color()
}

// Returns whether newly drawn text should be rendered with the bold text
// attribute.
func (self *Screen) Bold() bool {
	return self.attrs.bold()
}

// Returns whether newly drawn text should be rendered with the dim text
// attribute.
func (self *Screen) Dim() bool {
	return self.attrs.dim()
}

// Returns whether newly drawn text should be rendered with the italic
// text attribute.
func (self *Screen) Italic() bool {
	return self.attrs.italic()
}

// Returns whether newly drawn text should be rendered with the
// underlined text attribute.
func (self *Screen) Underline() bool {
	return self.attrs.underline()
}

// Returns whether newly drawn text should be rendered with the inverse
// text attribute.
func (self *Screen) Inverse() bool {
	return self.attrs.inverse()
}

func (self *Screen) gridMethod() *gridGrid {
	if self.mode(modeAlternateScreen) {
		return &self.alternateGrid
	} else {
		return &self.grid
	}
}

func (self *Screen) gridMut() *gridGrid {
	if self.mode(modeAlternateScreen) {
		return &self.alternateGrid
	} else {
		return &self.grid
	}
}

func (self *Screen) enterAlternateGrid() {
	self.gridMut().setScrollback(0)
	self.setMode(modeAlternateScreen)
	self.alternateGrid.allocateRows()
}

func (self *Screen) exitAlternateGrid() {
	self.clearMode(modeAlternateScreen)
}

func (self *Screen) saveCursor() {
	self.gridMut().saveCursor()
	self.savedAttrs = self.attrs
}

func (self *Screen) restoreCursor() {
	self.gridMut().restoreCursor()
	self.attrs = self.savedAttrs
}

func (self *Screen) setMode(mode uint8) {
	self.modes |= mode
}

func (self *Screen) clearMode(mode uint8) {
	self.modes &= ^mode
}

func (self *Screen) mode(mode uint8) bool {
	return self.modes&mode != 0
}

func (self *Screen) setMouseMode(mode MouseProtocolMode) {
	self.mouseProtocolMode = mode
}

func (self *Screen) clearMouseMode(mode MouseProtocolMode) {
	if self.mouseProtocolMode == mode {
		self.mouseProtocolMode = MouseProtocolModeDefault()
	}
}

func (self *Screen) setMouseEncoding(encoding MouseProtocolEncoding) {
	self.mouseProtocolEncoding = encoding
}

func (self *Screen) clearMouseEncoding(encoding MouseProtocolEncoding) {
	if self.mouseProtocolEncoding == encoding {
		self.mouseProtocolEncoding = MouseProtocolEncodingDefault()
	}
}

func (self *Screen) text(c rune) {
	pos := self.gridMethod().posMethod()
	size := self.gridMethod().sizeMethod()
	attrs := self.attrs

	width_ := unicodeWidthCharWidth(c)
	if width_.IsNone() && uint32(c) < 256 {
		// don't even try to draw control characters
		return
	}
	// width() can only return 0, 1, or 2
	width := uint16(width_.
		UnwrapOr(1))

	// it doesn't make any sense to wrap if the last column in a row
	// didn't already have contents. don't try to handle the case where a
	// character wraps because there was only one column left in the
	// previous row - literally everything handles this case differently,
	// and this is tmux behavior (and also the simplest). i'm open to
	// reconsidering this behavior, but only with a really good reason
	// (xterm handles this by introducing the concept of triple width
	// cells, which i really don't want to do).
	wrap := false
	if pos.col > size.cols-width {
		lastCell := self.
			gridMethod().
			drawingCell(gridPos{
				row: pos.row,
				col: size.cols - 1,
			}).
			// pos.row is valid, since it comes directly from
			// self.grid().pos() which we assume to always have a valid
			// row value. size.cols - 1 is also always a valid column.
			Unwrap()
		if lastCell.HasContents() || lastCell.IsWideContinuation() {
			wrap = true
		}
	}
	self.gridMut().colWrap(width, wrap)
	pos = self.gridMethod().posMethod()

	if width == 0 {
		if pos.col > 0 {
			prevCell := self.
				gridMut().
				drawingCellMut(gridPos{
					row: pos.row,
					col: pos.col - 1,
				}).
				// pos.row is valid, since it comes directly from
				// self.grid().pos() which we assume to always have a
				// valid row value. pos.col - 1 is valid because we just
				// checked for pos.col > 0.
				Unwrap()
			if prevCell.IsWideContinuation() {
				prevCell = self.
					gridMut().
					drawingCellMut(gridPos{
						row: pos.row,
						col: pos.col - 2,
					}).
					// pos.row is valid, since it comes directly from
					// self.grid().pos() which we assume to always have a
					// valid row value. we know pos.col - 2 is valid
					// because the cell at pos.col - 1 is a wide
					// continuation character, which means there must be
					// the first half of the wide character before it.
					Unwrap()
			}
			prevCell.append(c)
		} else if pos.row > 0 {
			prevRow := self.
				gridMethod().
				drawingRow(pos.row - 1).
				// pos.row is valid, since it comes directly from
				// self.grid().pos() which we assume to always have a
				// valid row value. pos.row - 1 is valid because we just
				// checked for pos.row > 0.
				Unwrap()
			if prevRow.wrappedMethod() {
				prevCell := self.
					gridMut().
					drawingCellMut(gridPos{
						row: pos.row - 1,
						col: size.cols - 1,
					}).
					// pos.row is valid, since it comes directly from
					// self.grid().pos() which we assume to always have a
					// valid row value. pos.row - 1 is valid because we
					// just checked for pos.row > 0. col of size.cols - 1
					// is always valid.
					Unwrap()
				if prevCell.IsWideContinuation() {
					prevCell = self.
						gridMut().
						drawingCellMut(gridPos{
							row: pos.row - 1,
							col: size.cols - 2,
						}).
						// pos.row is valid, since it comes directly from
						// self.grid().pos() which we assume to always
						// have a valid row value. pos.row - 1 is valid
						// because we just checked for pos.row > 0. col of
						// size.cols - 2 is valid because the cell at
						// size.cols - 1 is a wide continuation character,
						// so it must have the first half of the wide
						// character before it.
						Unwrap()
				}
				prevCell.append(c)
			}
		}
	} else {
		if self.
			gridMethod().
			drawingCell(pos).
			// pos.row is valid because we assume self.grid().pos() to
			// always have a valid row value. pos.col is valid because we
			// called col_wrap() immediately before this, which ensures
			// that self.grid().pos().col has a valid value.
			Unwrap().
			IsWideContinuation() {
			prevCell := self.
				gridMut().
				drawingCellMut(gridPos{
					row: pos.row,
					col: pos.col - 1,
				}).
				// pos.row is valid because we assume self.grid().pos() to
				// always have a valid row value. pos.col is valid because
				// we called col_wrap() immediately before this, which
				// ensures that self.grid().pos().col has a valid value.
				// pos.col - 1 is valid because the cell at pos.col is a
				// wide continuation character, so it must have the first
				// half of the wide character before it.
				Unwrap()
			prevCell.clear(attrs)
		}

		if self.
			gridMethod().
			drawingCell(pos).
			// pos.row is valid because we assume self.grid().pos() to
			// always have a valid row value. pos.col is valid because we
			// called col_wrap() immediately before this, which ensures
			// that self.grid().pos().col has a valid value.
			Unwrap().
			IsWide() {
			nextCell := self.
				gridMut().
				drawingCellMut(gridPos{
					row: pos.row,
					col: pos.col + 1,
				}).
				// pos.row is valid because we assume self.grid().pos() to
				// always have a valid row value. pos.col is valid because
				// we called col_wrap() immediately before this, which
				// ensures that self.grid().pos().col has a valid value.
				// pos.col + 1 is valid because the cell at pos.col is a
				// wide character, so it must have the second half of the
				// wide character after it.
				Unwrap()
			nextCell.set(' ', attrs)
		}

		cell := self.
			gridMut().
			drawingCellMut(pos).
			// pos.row is valid because we assume self.grid().pos() to
			// always have a valid row value. pos.col is valid because we
			// called col_wrap() immediately before this, which ensures
			// that self.grid().pos().col has a valid value.
			Unwrap()
		cell.set(c, attrs)
		self.gridMut().colInc(1)
		if width > 1 {
			pos := self.gridMethod().posMethod()
			if self.
				gridMethod().
				drawingCell(pos).
				// pos.row is valid because we assume self.grid().pos() to
				// always have a valid row value. pos.col is valid because
				// we called col_wrap() earlier, which ensures that
				// self.grid().pos().col has a valid value. this is true
				// even though we just called col_inc, because this branch
				// only happens if width > 1, and col_wrap takes width
				// into account.
				Unwrap().
				IsWide() {
				nextNextPos := gridPos{
					row: pos.row,
					col: pos.col + 1,
				}
				nextNextCell := self.
					gridMut().
					drawingCellMut(nextNextPos).
					// pos.row is valid because we assume
					// self.grid().pos() to always have a valid row value.
					// pos.col is valid because we called col_wrap()
					// earlier, which ensures that self.grid().pos().col
					// has a valid value. this is true even though we just
					// called col_inc, because this branch only happens if
					// width > 1, and col_wrap takes width into account.
					// pos.col + 1 is valid because the cell at pos.col is
					// wide, and so it must have the second half of the
					// wide character after it.
					Unwrap()
				nextNextCell.clear(attrs)
				if nextNextPos.col == size.cols-1 {
					self.gridMut().
						drawingRowMut(pos.row).
						// we assume self.grid().pos().row is always valid
						Unwrap().
						wrap(false)
				}
			}
			nextCell := self.
				gridMut().
				drawingCellMut(pos).
				// pos.row is valid because we assume self.grid().pos() to
				// always have a valid row value. pos.col is valid because
				// we called col_wrap() earlier, which ensures that
				// self.grid().pos().col has a valid value. this is true
				// even though we just called col_inc, because this branch
				// only happens if width > 1, and col_wrap takes width
				// into account.
				Unwrap()
			nextCell.clear(attrsAttrs{})
			nextCell.setWideContinuation(true)
			self.gridMut().colInc(1)
		}
	}
}

// control codes

func (self *Screen) bs() {
	self.gridMut().colDec(1)
}

func (self *Screen) tab() {
	self.gridMut().colTab()
}

func (self *Screen) lf() {
	self.gridMut().rowIncScroll(1)
}

func (self *Screen) vt() {
	self.lf()
}

func (self *Screen) ff() {
	self.lf()
}

func (self *Screen) cr() {
	self.gridMut().colSet(0)
}

// escape codes

// ESC 7
func (self *Screen) decsc() {
	self.saveCursor()
}

// ESC 8
func (self *Screen) decrc() {
	self.restoreCursor()
}

// ESC =
func (self *Screen) deckpam() {
	self.setMode(modeApplicationKeypad)
}

// ESC >
func (self *Screen) deckpnm() {
	self.clearMode(modeApplicationKeypad)
}

// ESC M
func (self *Screen) ri() {
	self.gridMut().rowDecScroll(1)
}

// ESC c
func (self *Screen) ris() {
	*self = screenNew(self.grid.sizeMethod(), self.grid.scrollbackLenMethod())
}

// csi codes

// CSI @
func (self *Screen) ich(count uint16) {
	self.gridMut().insertCells(count)
}

// CSI A
func (self *Screen) cuu(offset uint16) {
	self.gridMut().rowDecClamp(offset)
}

// CSI B
func (self *Screen) cud(offset uint16) {
	self.gridMut().rowIncClamp(offset)
}

// CSI C
func (self *Screen) cuf(offset uint16) {
	self.gridMut().colIncClamp(offset)
}

// CSI D
func (self *Screen) cub(offset uint16) {
	self.gridMut().colDec(offset)
}

// CSI E
func (self *Screen) cnl(offset uint16) {
	self.gridMut().colSet(0)
	self.gridMut().rowIncClamp(offset)
}

// CSI F
func (self *Screen) cpl(offset uint16) {
	self.gridMut().colSet(0)
	self.gridMut().rowDecClamp(offset)
}

// CSI G
func (self *Screen) cha(col uint16) {
	self.gridMut().colSet(col - 1)
}

// CSI H
func (self *Screen) cup(row uint16, col uint16) {
	self.gridMut().setPos(gridPos{
		row: row - 1,
		col: col - 1,
	})
}

// CSI J
func (self *Screen) ed(
	mode uint16,
	unhandled func(*Screen),
) {
	attrs := self.attrs
	switch mode {
	case 0:
		self.gridMut().eraseAllForward(attrs)
	case 1:
		self.gridMut().eraseAllBackward(attrs)
	case 2:
		self.gridMut().eraseAll(attrs)
	default:
		unhandled(self)
	}
}

// CSI ? J
func (self *Screen) decsed(
	mode uint16,
	unhandled func(*Screen),
) {
	self.ed(mode, unhandled)
}

// CSI K
func (self *Screen) el(
	mode uint16,
	unhandled func(*Screen),
) {
	attrs := self.attrs
	switch mode {
	case 0:
		self.gridMut().eraseRowForward(attrs)
	case 1:
		self.gridMut().eraseRowBackward(attrs)
	case 2:
		self.gridMut().eraseRow(attrs)
	default:
		unhandled(self)
	}
}

// CSI ? K
func (self *Screen) decsel(
	mode uint16,
	unhandled func(*Screen),
) {
	self.el(mode, unhandled)
}

// CSI L
func (self *Screen) il(count uint16) {
	self.gridMut().insertLines(count)
}

// CSI M
func (self *Screen) dl(count uint16) {
	self.gridMut().deleteLines(count)
}

// CSI P
func (self *Screen) dch(count uint16) {
	self.gridMut().deleteCells(count)
}

// CSI S
func (self *Screen) su(count uint16) {
	self.gridMut().scrollUp(count)
}

// CSI T
func (self *Screen) sd(count uint16) {
	self.gridMut().scrollDown(count)
}

// CSI X
func (self *Screen) ech(count uint16) {
	attrs := self.attrs
	self.gridMut().eraseCells(count, attrs)
}

// CSI d
func (self *Screen) vpa(row uint16) {
	self.gridMut().rowSet(row - 1)
}

// isParam reports whether a parameter is the single value n, which is the
// slice pattern `[n]`.
func isParam(param []uint16, n uint16) bool {
	return len(param) == 1 && param[0] == n
}

// CSI ? h
func (self *Screen) decset(
	params *vte.Params,
	unhandled func(*Screen),
) {
	for iter := params.Iter(); ; {
		param, ok := iter.Next().Get()
		if !ok {
			break
		}
		switch {
		case isParam(param, 1):
			self.setMode(modeApplicationCursor)
		case isParam(param, 6):
			self.gridMut().setOriginMode(true)
		case isParam(param, 9):
			self.setMouseMode(MouseProtocolMode_Press)
		case isParam(param, 25):
			self.clearMode(modeHideCursor)
		case isParam(param, 47):
			self.enterAlternateGrid()
		case isParam(param, 1000):
			self.setMouseMode(MouseProtocolMode_PressRelease)
		case isParam(param, 1002):
			self.setMouseMode(MouseProtocolMode_ButtonMotion)
		case isParam(param, 1003):
			self.setMouseMode(MouseProtocolMode_AnyMotion)
		case isParam(param, 1005):
			self.setMouseEncoding(MouseProtocolEncoding_Utf8)
		case isParam(param, 1006):
			self.setMouseEncoding(MouseProtocolEncoding_Sgr)
		case isParam(param, 1049):
			self.decsc()
			self.alternateGrid.clear()
			self.enterAlternateGrid()
		case isParam(param, 2004):
			self.setMode(modeBracketedPaste)
		default:
			unhandled(self)
		}
	}
}

// CSI ? l
func (self *Screen) decrst(
	params *vte.Params,
	unhandled func(*Screen),
) {
	for iter := params.Iter(); ; {
		param, ok := iter.Next().Get()
		if !ok {
			break
		}
		switch {
		case isParam(param, 1):
			self.clearMode(modeApplicationCursor)
		case isParam(param, 6):
			self.gridMut().setOriginMode(false)
		case isParam(param, 9):
			self.clearMouseMode(MouseProtocolMode_Press)
		case isParam(param, 25):
			self.setMode(modeHideCursor)
		case isParam(param, 47):
			self.exitAlternateGrid()
		case isParam(param, 1000):
			self.clearMouseMode(MouseProtocolMode_PressRelease)
		case isParam(param, 1002):
			self.clearMouseMode(MouseProtocolMode_ButtonMotion)
		case isParam(param, 1003):
			self.clearMouseMode(MouseProtocolMode_AnyMotion)
		case isParam(param, 1005):
			self.clearMouseEncoding(MouseProtocolEncoding_Utf8)
		case isParam(param, 1006):
			self.clearMouseEncoding(MouseProtocolEncoding_Sgr)
		case isParam(param, 1049):
			self.exitAlternateGrid()
			self.decrc()
		case isParam(param, 2004):
			self.clearMode(modeBracketedPaste)
		default:
			unhandled(self)
		}
	}
}

// CSI m
func (self *Screen) sgr(
	params *vte.Params,
	unhandled func(*Screen),
) {
	// XXX really i want to just be able to pass in a default Params
	// instance with a 0 in it, but vte doesn't allow creating new Params
	// instances
	if params.IsEmpty() {
		self.attrs = attrsAttrs{}
		return
	}

	iter := params.Iter()

	// nextParamU8 is the macro `next_param_u8!`: the next parameter when it
	// is a single value that fits a u8. Where it reports false the Rust
	// macro returns from sgr.
	nextParamU8 := func() (uint8, bool) {
		param, ok := iter.Next().Get()
		if !ok {
			return 0, false
		}
		if len(param) != 1 {
			return 0, false
		}
		return u16ToU8(param[0]).Get()
	}

	for {
		param, ok := iter.Next().Get()
		if !ok {
			return
		}
		switch {
		case isParam(param, 0):
			self.attrs = attrsAttrs{}
		case isParam(param, 1):
			self.attrs.setBold()
		case isParam(param, 2):
			self.attrs.setDim()
		case isParam(param, 3):
			self.attrs.setItalic(true)
		case isParam(param, 4):
			self.attrs.setUnderline(true)
		case isParam(param, 7):
			self.attrs.setInverse(true)
		case isParam(param, 22):
			self.attrs.setNormalIntensity()
		case isParam(param, 23):
			self.attrs.setItalic(false)
		case isParam(param, 24):
			self.attrs.setUnderline(false)
		case isParam(param, 27):
			self.attrs.setInverse(false)
		case len(param) == 1 && param[0] >= 30 && param[0] <= 37:
			n, ok := u16ToU8(param[0]).Get()
			if !ok {
				return
			}
			self.attrs.fgcolor = colorOfIdx(n - 30)
		case len(param) == 5 && param[0] == 38 && param[1] == 2:
			r, ok := u16ToU8(param[2]).Get()
			if !ok {
				return
			}
			g, ok := u16ToU8(param[3]).Get()
			if !ok {
				return
			}
			b, ok := u16ToU8(param[4]).Get()
			if !ok {
				return
			}
			self.attrs.fgcolor =
				colorOfRgb(r, g, b)
		case len(param) == 3 && param[0] == 38 && param[1] == 5:
			i, ok := u16ToU8(param[2]).Get()
			if !ok {
				return
			}
			self.attrs.fgcolor = colorOfIdx(i)
		case isParam(param, 38):
			next, ok := iter.Next().Get()
			if !ok {
				return
			}
			switch {
			case isParam(next, 2):
				r, ok := nextParamU8()
				if !ok {
					return
				}
				g, ok := nextParamU8()
				if !ok {
					return
				}
				b, ok := nextParamU8()
				if !ok {
					return
				}
				self.attrs.fgcolor = colorOfRgb(r, g, b)
			case isParam(next, 5):
				i, ok := nextParamU8()
				if !ok {
					return
				}
				self.attrs.fgcolor =
					colorOfIdx(i)
			default:
				unhandled(self)
				return
			}
		case isParam(param, 39):
			self.attrs.fgcolor = color{}
		case len(param) == 1 && param[0] >= 40 && param[0] <= 47:
			n, ok := u16ToU8(param[0]).Get()
			if !ok {
				return
			}
			self.attrs.bgcolor = colorOfIdx(n - 40)
		case len(param) == 5 && param[0] == 48 && param[1] == 2:
			r, ok := u16ToU8(param[2]).Get()
			if !ok {
				return
			}
			g, ok := u16ToU8(param[3]).Get()
			if !ok {
				return
			}
			b, ok := u16ToU8(param[4]).Get()
			if !ok {
				return
			}
			self.attrs.bgcolor =
				colorOfRgb(r, g, b)
		case len(param) == 3 && param[0] == 48 && param[1] == 5:
			i, ok := u16ToU8(param[2]).Get()
			if !ok {
				return
			}
			self.attrs.bgcolor = colorOfIdx(i)
		case isParam(param, 48):
			next, ok := iter.Next().Get()
			if !ok {
				return
			}
			switch {
			case isParam(next, 2):
				r, ok := nextParamU8()
				if !ok {
					return
				}
				g, ok := nextParamU8()
				if !ok {
					return
				}
				b, ok := nextParamU8()
				if !ok {
					return
				}
				self.attrs.bgcolor = colorOfRgb(r, g, b)
			case isParam(next, 5):
				i, ok := nextParamU8()
				if !ok {
					return
				}
				self.attrs.bgcolor =
					colorOfIdx(i)
			default:
				unhandled(self)
				return
			}
		case isParam(param, 49):
			self.attrs.bgcolor = color{}
		case len(param) == 1 && param[0] >= 90 && param[0] <= 97:
			n, ok := u16ToU8(param[0]).Get()
			if !ok {
				return
			}
			self.attrs.fgcolor = colorOfIdx(n - 82)
		case len(param) == 1 && param[0] >= 100 && param[0] <= 107:
			n, ok := u16ToU8(param[0]).Get()
			if !ok {
				return
			}
			self.attrs.bgcolor = colorOfIdx(n - 92)
		default:
			unhandled(self)
		}
	}
}

// CSI r
func (self *Screen) decstbm(top uint16, bottom uint16) {
	self.gridMut().setScrollRegion(top-1, bottom-1)
}

func u16ToU8(i uint16) rs.Option[uint8] {
	if i > uint16(0xFF) {
		return rs.None[uint8]()
	} else {
		// safe because we just ensured that the value fits in a u8
		return rs.Some(uint8(i))
	}
}
