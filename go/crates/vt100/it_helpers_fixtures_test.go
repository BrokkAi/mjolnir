// Port of vt100 0.16.2 tests/helpers/fixtures.rs

package vt100_test

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/collections"
)

// FixtureCell is a cell as a fixture writes it. Every field but contents is
// left out of the JSON when it has its default value.
type fixturesFixtureCell struct {
	contents           string      `rs:"contents"`
	isWide             bool        `rs:"is_wide"`
	isWideContinuation bool        `rs:"is_wide_continuation"`
	fgcolor            vt100.Color `rs:"fgcolor"`
	bgcolor            vt100.Color `rs:"bgcolor"`
	bold               bool        `rs:"bold"`
	dim                bool        `rs:"dim"`
	italic             bool        `rs:"italic"`
	underline          bool        `rs:"underline"`
	inverse            bool        `rs:"inverse"`
}

var _ = rs.RegisterStruct[fixturesFixtureCell]("FixtureCell")

// fixturesFixtureCellDefault is `FixtureCell::default()`.
func fixturesFixtureCellDefault() fixturesFixtureCell {
	return fixturesFixtureCell{
		contents:           "",
		isWide:             false,
		isWideContinuation: false,
		fgcolor:            vt100.ColorDefault(),
		bgcolor:            vt100.ColorDefault(),
		bold:               false,
		dim:                false,
		italic:             false,
		underline:          false,
		inverse:            false,
	}
}

func fixturesFixtureCellFromCell(cell *vt100.Cell) fixturesFixtureCell {
	return fixturesFixtureCell{
		contents:           cell.Contents(),
		isWide:             cell.IsWide(),
		isWideContinuation: cell.IsWideContinuation(),
		fgcolor:            cell.Fgcolor(),
		bgcolor:            cell.Bgcolor(),
		bold:               cell.Bold(),
		dim:                cell.Dim(),
		italic:             cell.Italic(),
		underline:          cell.Underline(),
		inverse:            cell.Inverse(),
	}
}

// fixtureCellJSON is the serde form of FixtureCell: optional fields are
// pointers, absent when they hold the default.
type fixtureCellJSON struct {
	Contents           string  `json:"contents"`
	IsWide             *bool   `json:"is_wide,omitempty"`
	IsWideContinuation *bool   `json:"is_wide_continuation,omitempty"`
	Fgcolor            *string `json:"fgcolor,omitempty"`
	Bgcolor            *string `json:"bgcolor,omitempty"`
	Bold               *bool   `json:"bold,omitempty"`
	Dim                *bool   `json:"dim,omitempty"`
	Italic             *bool   `json:"italic,omitempty"`
	Underline          *bool   `json:"underline,omitempty"`
	Inverse            *bool   `json:"inverse,omitempty"`
}

func (self *fixturesFixtureCell) UnmarshalJSON(text []byte) error {
	var raw struct {
		Contents           *string          `json:"contents"`
		IsWide             bool             `json:"is_wide"`
		IsWideContinuation bool             `json:"is_wide_continuation"`
		Fgcolor            *json.RawMessage `json:"fgcolor"`
		Bgcolor            *json.RawMessage `json:"bgcolor"`
		Bold               bool             `json:"bold"`
		Dim                bool             `json:"dim"`
		Italic             bool             `json:"italic"`
		Underline          bool             `json:"underline"`
		Inverse            bool             `json:"inverse"`
	}
	if err := json.Unmarshal(text, &raw); err != nil {
		return err
	}
	if raw.Contents == nil {
		return errors.New("missing field `contents`")
	}
	*self = fixturesFixtureCellDefault()
	self.contents = *raw.Contents
	self.isWide = raw.IsWide
	self.isWideContinuation = raw.IsWideContinuation
	if raw.Fgcolor != nil {
		color, err := fixturesDeserializeColor(*raw.Fgcolor)
		if err != nil {
			return err
		}
		self.fgcolor = color
	}
	if raw.Bgcolor != nil {
		color, err := fixturesDeserializeColor(*raw.Bgcolor)
		if err != nil {
			return err
		}
		self.bgcolor = color
	}
	self.bold = raw.Bold
	self.dim = raw.Dim
	self.italic = raw.Italic
	self.underline = raw.Underline
	self.inverse = raw.Inverse
	return nil
}

func (self fixturesFixtureCell) MarshalJSON() ([]byte, error) {
	out := fixtureCellJSON{Contents: self.contents}
	flag := func(value bool) *bool {
		if fixturesIsDefault(value) {
			return nil
		}
		return &value
	}
	color := func(value vt100.Color) *string {
		if fixturesIsDefault(value) {
			return nil
		}
		text := fixturesSerializeColor(value)
		return &text
	}
	out.IsWide = flag(self.isWide)
	out.IsWideContinuation = flag(self.isWideContinuation)
	out.Fgcolor = color(self.fgcolor)
	out.Bgcolor = color(self.bgcolor)
	out.Bold = flag(self.bold)
	out.Dim = flag(self.dim)
	out.Italic = flag(self.italic)
	out.Underline = flag(self.underline)
	out.Inverse = flag(self.inverse)
	return json.Marshal(out)
}

type fixturesFixtureScreen struct {
	contents              string                                            `rs:"contents"`
	cells                 collections.BTreeMap[string, fixturesFixtureCell] `rs:"cells"`
	cursorPosition        rs.Tuple2[uint16, uint16]                         `rs:"cursor_position"`
	applicationKeypad     bool                                              `rs:"application_keypad"`
	applicationCursor     bool                                              `rs:"application_cursor"`
	hideCursor            bool                                              `rs:"hide_cursor"`
	bracketedPaste        bool                                              `rs:"bracketed_paste"`
	mouseProtocolMode     vt100.MouseProtocolMode                           `rs:"mouse_protocol_mode"`
	mouseProtocolEncoding vt100.MouseProtocolEncoding                       `rs:"mouse_protocol_encoding"`
}

var _ = rs.RegisterStruct[fixturesFixtureScreen]("FixtureScreen")

func (self *fixturesFixtureScreen) UnmarshalJSON(text []byte) error {
	var raw struct {
		Contents              *string                         `json:"contents"`
		Cells                 *map[string]fixturesFixtureCell `json:"cells"`
		CursorPosition        *[2]uint16                      `json:"cursor_position"`
		ApplicationKeypad     bool                            `json:"application_keypad"`
		ApplicationCursor     bool                            `json:"application_cursor"`
		HideCursor            bool                            `json:"hide_cursor"`
		BracketedPaste        bool                            `json:"bracketed_paste"`
		MouseProtocolMode     *string                         `json:"mouse_protocol_mode"`
		MouseProtocolEncoding *string                         `json:"mouse_protocol_encoding"`
	}
	if err := json.Unmarshal(text, &raw); err != nil {
		return err
	}
	switch {
	case raw.Contents == nil:
		return errors.New("missing field `contents`")
	case raw.Cells == nil:
		return errors.New("missing field `cells`")
	case raw.CursorPosition == nil:
		return errors.New("missing field `cursor_position`")
	}
	*self = fixturesFixtureScreen{
		contents:              *raw.Contents,
		cells:                 collections.BTreeMap[string, fixturesFixtureCell]{},
		cursorPosition:        rs.T2(raw.CursorPosition[0], raw.CursorPosition[1]),
		applicationKeypad:     raw.ApplicationKeypad,
		applicationCursor:     raw.ApplicationCursor,
		hideCursor:            raw.HideCursor,
		bracketedPaste:        raw.BracketedPaste,
		mouseProtocolMode:     vt100.MouseProtocolModeDefault(),
		mouseProtocolEncoding: vt100.MouseProtocolEncodingDefault(),
	}
	for key, cell := range *raw.Cells {
		self.cells.Insert(key, cell)
	}
	if raw.MouseProtocolMode != nil {
		self.mouseProtocolMode = fixturesDeserializeMouseProtocolMode(*raw.MouseProtocolMode)
	}
	if raw.MouseProtocolEncoding != nil {
		self.mouseProtocolEncoding = fixturesDeserializeMouseProtocolEncoding(*raw.MouseProtocolEncoding)
	}
	return nil
}

func (self *fixturesFixtureScreen) MarshalJSON() ([]byte, error) {
	cells := map[string]fixturesFixtureCell{}
	for key, cell := range self.cells.Iter() {
		cells[key] = *cell
	}
	out := map[string]any{
		"contents":        self.contents,
		"cells":           cells,
		"cursor_position": [2]uint16{self.cursorPosition.F0, self.cursorPosition.F1},
	}
	for name, value := range map[string]bool{
		"application_keypad": self.applicationKeypad,
		"application_cursor": self.applicationCursor,
		"hide_cursor":        self.hideCursor,
		"bracketed_paste":    self.bracketedPaste,
	} {
		if !fixturesIsDefault(value) {
			out[name] = value
		}
	}
	if !fixturesIsDefault(self.mouseProtocolMode) {
		out["mouse_protocol_mode"] = fixturesSerializeMouseProtocolMode(self.mouseProtocolMode)
	}
	if !fixturesIsDefault(self.mouseProtocolEncoding) {
		out["mouse_protocol_encoding"] = fixturesSerializeMouseProtocolEncoding(self.mouseProtocolEncoding)
	}
	return json.Marshal(out)
}

func fixturesFixtureScreenLoad(r io.Reader) fixturesFixtureScreen {
	var screen fixturesFixtureScreen
	if err := json.NewDecoder(r).Decode(&screen); err != nil {
		panic(rs.Panicf("called `Result::unwrap()` on an `Err` value: {}", err.Error()))
	}
	return screen
}

func fixturesFixtureScreenFromScreen(screen *vt100.Screen) fixturesFixtureScreen {
	emptyParser := vt100.ParserDefault()
	emptyScreen := emptyParser.Screen().Clone()
	emptyCell := emptyScreen.Cell(0, 0).Unwrap()
	cells := collections.BTreeMap[string, fixturesFixtureCell]{}
	rows, cols := screen.Size()
	for row := uint32(0); row < uint32(rows); row++ {
		for col := uint32(0); col < uint32(cols); col++ {
			cell := screen.Cell(uint16(row), uint16(col)).Unwrap()
			if !cell.Equal(emptyCell) {
				cells.Insert(
					strconv.Itoa(int(row))+","+strconv.Itoa(int(col)),
					fixturesFixtureCellFromCell(cell),
				)
			}
		}
	}
	cursorRow, cursorCol := screen.CursorPosition()
	return fixturesFixtureScreen{
		contents:              screen.Contents(),
		cells:                 cells,
		cursorPosition:        rs.T2(cursorRow, cursorCol),
		applicationKeypad:     screen.ApplicationKeypad(),
		applicationCursor:     screen.ApplicationCursor(),
		hideCursor:            screen.HideCursor(),
		bracketedPaste:        screen.BracketedPaste(),
		mouseProtocolMode:     screen.MouseProtocolMode(),
		mouseProtocolEncoding: screen.MouseProtocolEncoding(),
	}
}

// fixturesIsDefault is `is_default`: `t == &T::default()`.
func fixturesIsDefault[T any](t T) bool {
	return rs.Eq(t, rs.Default[T]())
}

func fixturesDeserializeColor(text json.RawMessage) (vt100.Color, error) {
	var val *string
	if err := json.Unmarshal(text, &val); err != nil {
		return nil, err
	}
	switch {
	case val == nil:
		return vt100.ColorDefault(), nil
	case len(*val) > 0 && (*val)[0] == '#':
		x := []uint8(*val)
		if len(x) != 7 {
			return nil, errors.New("invalid rgb color")
		}
		r, err := helpersHex(x[1], x[2])
		if err != nil {
			return nil, err
		}
		g, err := helpersHex(x[3], x[4])
		if err != nil {
			return nil, err
		}
		b, err := helpersHex(x[5], x[6])
		if err != nil {
			return nil, err
		}
		return &vt100.Color_Rgb{F0: r, F1: g, F2: b}, nil
	default:
		index, err := strconv.ParseUint(*val, 10, 8)
		if err != nil {
			return nil, err
		}
		return &vt100.Color_Idx{F0: uint8(index)}, nil
	}
}

func fixturesSerializeColor(color vt100.Color) string {
	switch color := color.(type) {
	case *vt100.Color_Default:
		panic(rs.Unreachable())
	case *vt100.Color_Idx:
		return strconv.Itoa(int(color.F0))
	case *vt100.Color_Rgb:
		return "#" + hex2(color.F0) + hex2(color.F1) + hex2(color.F2)
	default:
		panic(rs.Unreachable(color))
	}
}

func fixturesDeserializeMouseProtocolMode(name string) vt100.MouseProtocolMode {
	switch name {
	case "none":
		return vt100.MouseProtocolMode_None
	case "press":
		return vt100.MouseProtocolMode_Press
	case "press_release":
		return vt100.MouseProtocolMode_PressRelease
	case "button_motion":
		return vt100.MouseProtocolMode_ButtonMotion
	case "any_motion":
		return vt100.MouseProtocolMode_AnyMotion
	default:
		panic(rs.Unimplemented())
	}
}

func fixturesSerializeMouseProtocolMode(mode vt100.MouseProtocolMode) string {
	switch mode {
	case vt100.MouseProtocolMode_None:
		return "none"
	case vt100.MouseProtocolMode_Press:
		return "press"
	case vt100.MouseProtocolMode_PressRelease:
		return "press_release"
	case vt100.MouseProtocolMode_ButtonMotion:
		return "button_motion"
	case vt100.MouseProtocolMode_AnyMotion:
		return "any_motion"
	default:
		panic(rs.Unreachable(mode))
	}
}

func fixturesDeserializeMouseProtocolEncoding(name string) vt100.MouseProtocolEncoding {
	switch name {
	case "default":
		return vt100.MouseProtocolEncoding_Default
	case "utf8":
		return vt100.MouseProtocolEncoding_Utf8
	case "sgr":
		return vt100.MouseProtocolEncoding_Sgr
	default:
		panic(rs.Unimplemented())
	}
}

func fixturesSerializeMouseProtocolEncoding(encoding vt100.MouseProtocolEncoding) string {
	switch encoding {
	case vt100.MouseProtocolEncoding_Default:
		return "default"
	case vt100.MouseProtocolEncoding_Utf8:
		return "utf8"
	case vt100.MouseProtocolEncoding_Sgr:
		return "sgr"
	default:
		panic(rs.Unreachable(encoding))
	}
}

// fixturesOpen is `std::fs::File::open(path).ok()` for a fixture file.
func fixturesOpen(t testing.TB, path string) rs.Option[[]byte] {
	t.Helper()
	files, err := vt100.TestdataFixtures()
	if err != nil {
		t.Fatalf("read the fixtures: %v", err)
	}
	data, ok := files[path]
	if !ok {
		return rs.None[[]byte]()
	}
	return rs.Some(data)
}

func fixturesLoadInput(t testing.TB, name string, i int) rs.Option[[]uint8] {
	t.Helper()
	return fixturesOpen(t, "tests/data/fixtures/"+name+"/"+strconv.Itoa(i)+".typescript")
}

func fixturesLoadScreen(t testing.TB, name string, i int) rs.Option[fixturesFixtureScreen] {
	t.Helper()
	file, ok := fixturesOpen(t, "tests/data/fixtures/"+name+"/"+strconv.Itoa(i)+".json").Get()
	if !ok {
		return rs.None[fixturesFixtureScreen]()
	}
	return rs.Some(fixturesFixtureScreenLoad(bytes.NewReader(file)))
}

func fixturesAssertProduces(t testing.TB, input []uint8, expected *fixturesFixtureScreen) {
	t.Helper()
	parser := vt100.ParserDefault()
	parser.Process(input)

	assertEq(t, parser.Screen().Contents(), expected.contents)
	assertEq(t, pair(parser.Screen().CursorPosition()), expected.cursorPosition)
	assertEq(t,
		parser.Screen().ApplicationKeypad(),
		expected.applicationKeypad,
	)
	assertEq(t,
		parser.Screen().ApplicationCursor(),
		expected.applicationCursor,
	)
	assertEq(t, parser.Screen().HideCursor(), expected.hideCursor)
	assertEq(t, parser.Screen().BracketedPaste(), expected.bracketedPaste)
	assertEq(t,
		parser.Screen().MouseProtocolMode(),
		expected.mouseProtocolMode,
	)
	assertEq(t,
		parser.Screen().MouseProtocolEncoding(),
		expected.mouseProtocolEncoding,
	)

	rows, cols := parser.Screen().Size()
	for row := uint32(0); row < uint32(rows); row++ {
		for col := uint32(0); col < uint32(cols); col++ {
			expectedCell := fixturesFixtureCellDefault()
			if cell, ok := expected.cells.Get(strconv.Itoa(int(row)) + "," + strconv.Itoa(int(col))); ok {
				expectedCell = *cell
			}
			gotCell := parser.Screen().Cell(uint16(row), uint16(col)).Unwrap()
			assertEq(t, gotCell.Contents(), expectedCell.contents)
			assertEq(t, gotCell.IsWide(), expectedCell.isWide)
			assertEq(t,
				gotCell.IsWideContinuation(),
				expectedCell.isWideContinuation,
			)
			assertEq(t, gotCell.Fgcolor(), expectedCell.fgcolor)
			assertEq(t, gotCell.Bgcolor(), expectedCell.bgcolor)
			assertEq(t, gotCell.Bold(), expectedCell.bold)
			assertEq(t, gotCell.Dim(), expectedCell.dim)
			assertEq(t, gotCell.Italic(), expectedCell.italic)
			assertEq(t, gotCell.Underline(), expectedCell.underline)
			assertEq(t, gotCell.Inverse(), expectedCell.inverse)
		}
	}
}

func helpersFixture(t testing.TB, name string) {
	t.Helper()
	i := 1
	prevInput := []uint8{}
	for {
		input, ok := fixturesLoadInput(t, name, i).Get()
		if !ok {
			break
		}
		helpersAssertReproducesStateFrom(t, input, prevInput)
		prevInput = append(prevInput, input...)

		expected := fixturesLoadScreen(t, name, i).Unwrap()
		fixturesAssertProduces(t, prevInput, &expected)

		i++
	}
	if !(i > 1) {
		t.Fatalf("couldn't find fixtures to test")
	}
}
