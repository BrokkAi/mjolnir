package vt100

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The Rust side of these tests is the `vt100` and `width` operations of
// go/port/oracle/src/vt100_oracle.rs, which run vt100 0.16.2. Both sides
// write everything a caller can observe of a screen to a dump, one line for
// each fact, and compare digests of the lines. The functions here mirror the
// ones there (dump_screen, cell_text, Recorder, vt100_run, vt100_matrix,
// width_probe) and must be kept in step with them line for line.

// --- shared helpers ---------------------------------------------------------

// lineHash is FNV-1a, 64 bits.
type lineHash struct{ hash uint64 }

func newLineHash() lineHash { return lineHash{hash: 0xcbf29ce484222325} }

func (self *lineHash) write(text string) {
	for i := 0; i < len(text); i++ {
		self.hash ^= uint64(text[i])
		self.hash *= 0x100000001b3
	}
}

func (self *lineHash) digest8() string {
	return hexN(uint64(uint32(self.hash)), 8)
}

const hexDigits = "0123456789abcdef"

func hexN(value uint64, digits int) string {
	out := make([]byte, digits)
	for i := digits - 1; i >= 0; i-- {
		out[i] = hexDigits[value&15]
		value >>= 4
	}
	return string(out)
}

func hex2(byte_ uint8) string { return hexN(uint64(byte_), 2) }

func bit(value bool) string {
	if value {
		return "1"
	}
	return "0"
}

// esc writes bytes as printable ASCII: everything outside 0x20..=0x7e, the
// backslash and the separators `|` and `;` become `\xNN`.
func esc(bytes_ []uint8) string {
	out := make([]byte, 0, len(bytes_))
	for _, byte_ := range bytes_ {
		if byte_ >= 0x20 && byte_ <= 0x7e && byte_ != '\\' && byte_ != '|' && byte_ != ';' {
			out = append(out, byte_)
		} else {
			out = append(out, '\\', 'x', hexDigits[byte_>>4], hexDigits[byte_&15])
		}
	}
	return string(out)
}

// panicMessage is the message of a recovered panic: Rust's text for the
// panics the port raises on purpose, and Go's own for any other.
func panicMessage(recovered any) string {
	return rs.PanicInfoOf(recovered).Message
}

// guard is the value of f, or `PANIC <message>` when it panics.
func guard(f func() string) (text string) {
	defer func() {
		if recovered := recover(); recovered != nil {
			text = "PANIC " + panicMessage(recovered)
		}
	}()
	return f()
}

func u(value uint16) string { return strconv.Itoa(int(value)) }

// --- callbacks --------------------------------------------------------------

// recorder writes down every callback, with the cursor position the screen
// had when it was called.
type recorder struct {
	lines       []string
	applyResize bool
}

func (self *recorder) push(screen *Screen, line string) {
	row, col := screen.CursorPosition()
	self.lines = append(self.lines, line+" @"+u(row)+","+u(col))
}

func optionByte(value rs.Option[uint8]) string {
	if byte_, ok := value.Get(); ok {
		return hex2(byte_)
	}
	return "-"
}

func (self *recorder) AudibleBell(screen *Screen) { self.push(screen, "bell") }

func (self *recorder) VisualBell(screen *Screen) { self.push(screen, "vbell") }

func (self *recorder) Resize(screen *Screen, request rs.Tuple2[uint16, uint16]) {
	rows, cols := request.F0, request.F1
	self.push(screen, "resize "+u(rows)+" "+u(cols))
	// A real terminal resizes; sizes are bounded so that a request for
	// 65535 columns does not allocate gigabytes.
	if self.applyResize && rows <= 200 && cols <= 200 {
		screen.SetSize(rows, cols)
	}
}

func (self *recorder) SetWindowIconName(screen *Screen, iconName []uint8) {
	self.push(screen, "icon "+esc(iconName))
}

func (self *recorder) SetWindowTitle(screen *Screen, title []uint8) {
	self.push(screen, "title "+esc(title))
}

func (self *recorder) CopyToClipboard(screen *Screen, ty []uint8, data []uint8) {
	self.push(screen, "copy "+esc(ty)+"|"+esc(data))
}

func (self *recorder) PasteFromClipboard(screen *Screen, ty []uint8) {
	self.push(screen, "paste "+esc(ty))
}

func (self *recorder) UnhandledChar(screen *Screen, c rune) {
	self.push(screen, "char "+strconv.FormatInt(int64(c), 16))
}

func (self *recorder) UnhandledControl(screen *Screen, b uint8) {
	self.push(screen, "control "+hex2(b))
}

func (self *recorder) UnhandledEscape(screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], b uint8) {
	self.push(screen, "escape "+optionByte(i1)+" "+optionByte(i2)+" "+hex2(b))
}

func (self *recorder) UnhandledCsi(screen *Screen, i1 rs.Option[uint8], i2 rs.Option[uint8], params [][]uint16, c rune) {
	joined := make([]string, len(params))
	for i, param := range params {
		values := make([]string, len(param))
		for j, value := range param {
			values[j] = u(value)
		}
		joined[i] = strings.Join(values, ":")
	}
	self.push(screen, "csi "+optionByte(i1)+" "+optionByte(i2)+" ["+strings.Join(joined, ";")+"] "+strconv.FormatInt(int64(c), 16))
}

func (self *recorder) UnhandledOsc(screen *Screen, params [][]uint8) {
	joined := make([]string, len(params))
	for i, param := range params {
		joined[i] = esc(param)
	}
	self.push(screen, "osc "+strconv.Itoa(len(params))+" "+strings.Join(joined, "|"))
}

// --- the dump ---------------------------------------------------------------

var sectionNames = [7]string{"cur", "cells", "text", "fmt", "diff", "sb", "cb"}

const (
	sectionCur = iota
	sectionCells
	sectionText
	sectionFmt
	sectionDiff
	sectionSb
	sectionCb
)

type dump struct {
	// Every line, in the order written.
	all      lineHash
	sections [7]lineHash
	full     *strings.Builder
}

func newDump(full bool) *dump {
	d := &dump{all: newLineHash()}
	for i := range d.sections {
		d.sections[i] = newLineHash()
	}
	if full {
		d.full = &strings.Builder{}
	}
	return d
}

func (self *dump) line(section int, text string) {
	line := sectionNames[section] + " " + text + "\n"
	self.all.write(line)
	self.sections[section].write(line)
	if self.full != nil {
		self.full.WriteString(line)
	}
}

func (self *dump) digests() string {
	var out strings.Builder
	for i := range self.sections {
		out.WriteString(self.sections[i].digest8())
	}
	return out.String()
}

func colorText(color Color) string {
	switch color := color.(type) {
	case *Color_Default:
		return "d"
	case *Color_Idx:
		return "i" + strconv.Itoa(int(color.F0))
	case *Color_Rgb:
		return "#" + hex2(color.F0) + hex2(color.F1) + hex2(color.F2)
	default:
		panic(rs.Unreachable(color))
	}
}

func isColorDefault(color Color) bool {
	_, ok := color.(*Color_Default)
	return ok
}

func mouseModeText(mode MouseProtocolMode) string {
	switch mode {
	case MouseProtocolMode_None:
		return "none"
	case MouseProtocolMode_Press:
		return "press"
	case MouseProtocolMode_PressRelease:
		return "press_release"
	case MouseProtocolMode_ButtonMotion:
		return "button_motion"
	case MouseProtocolMode_AnyMotion:
		return "any_motion"
	default:
		panic(rs.Unreachable(mode))
	}
}

func mouseEncodingText(encoding MouseProtocolEncoding) string {
	switch encoding {
	case MouseProtocolEncoding_Default:
		return "default"
	case MouseProtocolEncoding_Utf8:
		return "utf8"
	case MouseProtocolEncoding_Sgr:
		return "sgr"
	default:
		panic(rs.Unreachable(encoding))
	}
}

func cellText(option rs.Option[*Cell]) string {
	cell, ok := option.Get()
	if !ok {
		return "-"
	}
	plain := !cell.HasContents() &&
		!cell.IsWide() &&
		!cell.IsWideContinuation() &&
		isColorDefault(cell.Fgcolor()) &&
		isColorDefault(cell.Bgcolor()) &&
		!cell.Bold() &&
		!cell.Dim() &&
		!cell.Italic() &&
		!cell.Underline() &&
		!cell.Inverse()
	if plain {
		return "."
	}
	return esc([]byte(cell.Contents())) + "/" +
		bit(cell.HasContents()) + bit(cell.IsWide()) + bit(cell.IsWideContinuation()) + "/" +
		colorText(cell.Fgcolor()) + "/" +
		colorText(cell.Bgcolor()) + "/" +
		bit(cell.Bold()) + bit(cell.Dim()) + bit(cell.Italic()) + bit(cell.Underline()) + bit(cell.Inverse())
}

func rowText(screen *Screen, row uint16, cols uint16) string {
	var out strings.Builder
	for col := uint32(0); col < uint32(cols); col++ {
		if col != 0 {
			out.WriteByte(';')
		}
		out.WriteString(cellText(screen.Cell(row, uint16(col))))
	}
	return out.String()
}

func joinedRows(rows func(yield func([]uint8) bool)) string {
	escaped := []string{}
	for row := range rows {
		escaped = append(escaped, esc(row))
	}
	return strings.Join(escaped, "|")
}

func joinedTextRows(rows func(yield func(string) bool)) string {
	escaped := []string{}
	for row := range rows {
		escaped = append(escaped, esc([]byte(row)))
	}
	return strings.Join(escaped, "|")
}

// dumpScreen writes everything a caller can observe of screen to the dump.
// prev is the screen before the last step and base the screen the parser
// started with.
func dumpScreen(d *dump, screen *Screen, prev *Screen, base *Screen) {
	rows, cols := screen.Size()
	cursorRow, cursorCol := screen.CursorPosition()

	d.line(sectionCur, "size "+u(rows)+" "+u(cols))
	d.line(sectionCur, "cursor "+u(cursorRow)+" "+u(cursorCol))
	d.line(sectionCur,
		"modes alt="+bit(screen.AlternateScreen())+
			" keypad="+bit(screen.ApplicationKeypad())+
			" appcursor="+bit(screen.ApplicationCursor())+
			" hide="+bit(screen.HideCursor())+
			" paste="+bit(screen.BracketedPaste())+
			" mouse="+mouseModeText(screen.MouseProtocolMode())+
			" enc="+mouseEncodingText(screen.MouseProtocolEncoding()))
	d.line(sectionCur,
		"attrs fg="+colorText(screen.Fgcolor())+
			" bg="+colorText(screen.Bgcolor())+
			" bold="+bit(screen.Bold())+
			" dim="+bit(screen.Dim())+
			" italic="+bit(screen.Italic())+
			" underline="+bit(screen.Underline())+
			" inverse="+bit(screen.Inverse()))
	d.line(sectionCur, "scrollback "+strconv.Itoa(screen.Scrollback()))

	for row := uint32(0); row < uint32(rows); row++ {
		row := uint16(row)
		text := guard(func() string { return bit(screen.RowWrapped(row)) + " " + rowText(screen, row, cols) })
		d.line(sectionCells, "row "+u(row)+" "+text)
	}
	d.line(sectionCells,
		"outside "+cellText(screen.Cell(rows, 0))+" "+cellText(screen.Cell(0, cols))+" "+bit(screen.RowWrapped(rows)))

	partStart := cols / 3
	partWidth := cols/2 + 1
	lastRow := saturatingSubU16(rows, 1)
	d.line(sectionText, "contents "+guard(func() string { return esc([]byte(screen.Contents())) }))
	d.line(sectionText, "rows "+guard(func() string { return joinedTextRows(screen.Rows(0, cols)) }))
	d.line(sectionText, "rows_part "+guard(func() string { return joinedTextRows(screen.Rows(partStart, partWidth)) }))
	d.line(sectionText, "rows_past "+guard(func() string { return joinedTextRows(screen.Rows(1, cols)) }))
	between := [][4]uint16{
		{0, 0, lastRow, cols},
		{cursorRow, min(cursorCol, cols), lastRow, cols / 2},
		{rows / 2, partStart, rows / 2, cols},
		{rows / 2, partStart, rows / 2, partStart},
		{1, 0, 0, 0},
		{0, saturatingSubU16(cols, 1), min(lastRow, 1), 1},
		{0, partStart, rows, partWidth},
	}
	for _, b := range between {
		d.line(sectionText,
			"between "+u(b[0])+" "+u(b[1])+" "+u(b[2])+" "+u(b[3])+" "+
				guard(func() string { return esc([]byte(screen.ContentsBetween(b[0], b[1], b[2], b[3]))) }))
	}

	d.line(sectionFmt, "contents_formatted "+guard(func() string { return esc(screen.ContentsFormatted()) }))
	d.line(sectionFmt, "state_formatted "+guard(func() string { return esc(screen.StateFormatted()) }))
	d.line(sectionFmt, "input_mode_formatted "+guard(func() string { return esc(screen.InputModeFormatted()) }))
	d.line(sectionFmt, "attributes_formatted "+guard(func() string { return esc(screen.AttributesFormatted()) }))
	d.line(sectionFmt, "cursor_state_formatted "+guard(func() string { return esc(screen.CursorStateFormatted()) }))
	d.line(sectionFmt, "rows_formatted "+guard(func() string { return joinedRows(screen.RowsFormatted(0, cols)) }))
	d.line(sectionFmt, "rows_formatted_part "+guard(func() string { return joinedRows(screen.RowsFormatted(partStart, partWidth)) }))

	d.line(sectionDiff, "contents_diff "+guard(func() string { return esc(screen.ContentsDiff(prev)) }))
	d.line(sectionDiff, "state_diff "+guard(func() string { return esc(screen.StateDiff(prev)) }))
	d.line(sectionDiff, "input_mode_diff "+guard(func() string { return esc(screen.InputModeDiff(prev)) }))
	d.line(sectionDiff, "rows_diff "+guard(func() string { return joinedRows(screen.RowsDiff(prev, 0, cols)) }))
	d.line(sectionDiff, "rows_diff_part "+guard(func() string { return joinedRows(screen.RowsDiff(prev, partStart, partWidth)) }))
	d.line(sectionDiff, "contents_diff_base "+guard(func() string { return esc(screen.ContentsDiff(base)) }))
	d.line(sectionDiff, "contents_diff_back "+guard(func() string { return esc(prev.ContentsDiff(screen)) }))
	d.line(sectionDiff, "contents_diff_self "+guard(func() string { return esc(screen.ContentsDiff(screen)) }))

	// The scrollback is read through a copy, one offset at a time: at offset
	// k the top visible row is the k-th newest row that scrolled off.
	probe := screen.Clone()
	probe.SetScrollback(rs.UsizeMax)
	depth := probe.Scrollback()
	d.line(sectionSb, "depth "+strconv.Itoa(depth))
	if depth > 0 {
		d.line(sectionSb, "oldest_contents "+guard(func() string { return esc([]byte(probe.Contents())) }))
		d.line(sectionSb, "oldest_formatted "+guard(func() string { return esc(probe.ContentsFormatted()) }))
		d.line(sectionSb, "oldest_diff "+guard(func() string { return esc(probe.ContentsDiff(screen)) }))
	}
	for offset := 1; offset <= depth; offset++ {
		probe.SetScrollback(offset)
		text := guard(func() string { return bit(probe.RowWrapped(0)) + " " + rowText(&probe, 0, cols) })
		d.line(sectionSb, "at "+strconv.Itoa(offset)+" "+text)
	}
}

// --- running a case ---------------------------------------------------------

// step is one step of a case: bytes to process, a new size, or a scrollback
// offset.
type step struct {
	process       []byte
	setSize       *[2]uint16
	setScrollback *int
}

func (self step) MarshalJSON() ([]byte, error) {
	switch {
	case self.setSize != nil:
		return json.Marshal(*self.setSize)
	case self.setScrollback != nil:
		return json.Marshal(*self.setScrollback)
	case self.process == nil:
		return json.Marshal([]byte{})
	default:
		return json.Marshal(self.process)
	}
}

func (self *step) UnmarshalJSON(text []byte) error {
	*self = step{}
	switch {
	case len(text) > 0 && text[0] == '[':
		self.setSize = &[2]uint16{}
		return json.Unmarshal(text, self.setSize)
	case len(text) > 0 && text[0] == '"':
		self.process = []byte{}
		return json.Unmarshal(text, &self.process)
	default:
		self.setScrollback = new(int)
		return json.Unmarshal(text, self.setScrollback)
	}
}

func processStep(bytes_ []byte) step { return step{process: bytes_} }

func sizeStep(rows, cols uint16) step { return step{setSize: &[2]uint16{rows, cols}} }

func scrollbackStep(offset int) step { return step{setScrollback: &offset} }

type failure struct {
	at      int
	message string
}

// runSteps runs the steps on a new parser, writing the state to the dump
// before the first step and after every step. It returns the step that
// panicked (counted from 1, with 0 for the creation of the parser) and the
// panic message.
func runSteps(d *dump, rows, cols uint16, scrollback int, applyResize bool, steps []step) *failure {
	callbacks := &recorder{applyResize: applyResize}
	var parser Parser[*recorder]
	if message, panicked := catch(func() {
		parser = ParserNewWithCallbacks(rows, cols, scrollback, callbacks)
	}); panicked {
		return &failure{at: 0, message: message}
	}
	base := parser.Screen().Clone()
	d.line(sectionCur, "start")
	dumpScreen(d, parser.Screen(), &base, &base)
	for index, s := range steps {
		prev := parser.Screen().Clone()
		message, panicked := catch(func() {
			switch {
			case s.setSize != nil:
				parser.ScreenMut().SetSize(s.setSize[0], s.setSize[1])
			case s.setScrollback != nil:
				parser.ScreenMut().SetScrollback(*s.setScrollback)
			default:
				parser.Process(s.process)
			}
		})
		// What the callbacks saw is reported even when the step then
		// panicked.
		lines := parser.CallbacksMut().lines
		parser.CallbacksMut().lines = nil
		d.line(sectionCur, "step "+strconv.Itoa(index))
		for _, line := range lines {
			d.line(sectionCb, line)
		}
		if panicked {
			return &failure{at: index + 1, message: message}
		}
		dumpScreen(d, parser.Screen(), &prev, &base)
	}
	return nil
}

func catch(f func()) (message string, panicked bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			message, panicked = panicMessage(recovered), true
		}
	}()
	f()
	return "", false
}

type vt100Case struct {
	Rows       uint16 `json:"r"`
	Cols       uint16 `json:"c"`
	Scrollback int    `json:"sb,omitempty"`
	Resize     bool   `json:"z,omitempty"`
	Steps      []step `json:"s"`
}

type csiMatrix struct {
	Prefixes []string `json:"prefixes"`
	Params   []string `json:"params"`
	Inter    []string `json:"inter"`
	Lo       uint8    `json:"lo"`
	Hi       uint8    `json:"hi"`
}

type vt100Matrix struct {
	Rows       uint16     `json:"r"`
	Cols       uint16     `json:"c"`
	Scrollback int        `json:"sb,omitempty"`
	Resize     bool       `json:"z,omitempty"`
	Setup      []byte     `json:"setup"`
	Tail       []byte     `json:"tail"`
	Csi        *csiMatrix `json:"csi,omitempty"`
	Seqs       [][]byte   `json:"seqs,omitempty"`
}

type vt100Request struct {
	Op     string       `json:"op"`
	Cases  []vt100Case  `json:"cases,omitempty"`
	Matrix *vt100Matrix `json:"matrix,omitempty"`
	Full   bool         `json:"full,omitempty"`
	From   int          `json:"from,omitempty"`
	To     int          `json:"to,omitempty"`
}

// matrixRun runs one sequence of a matrix and adds what it did to hash.
func (self *vt100Matrix) run(hash *lineHash, sequence []byte) {
	d := newDump(false)
	steps := []step{processStep(self.Setup), processStep(sequence), processStep(self.Tail)}
	failed := runSteps(d, self.Rows, self.Cols, self.Scrollback, self.Resize, steps)
	hash.write(hexN(d.all.hash, 16) + "\n")
	if failed != nil {
		hash.write("panic " + strconv.Itoa(failed.at) + " " + failed.message + "\n")
	}
}

// csiSequences calls yield with every sequence of one (prefix, params,
// intermediate) group of the matrix.
func (self *csiMatrix) group(prefix, params, intermediate string, yield func(sequence []byte)) {
	for last := int(self.Lo); last <= int(self.Hi); last++ {
		sequence := []byte("\x1b[")
		sequence = append(sequence, prefix...)
		sequence = append(sequence, params...)
		sequence = append(sequence, intermediate...)
		sequence = append(sequence, byte(last))
		yield(sequence)
	}
}

func runVt100Matrix(spec *vt100Matrix) map[string]any {
	reply := map[string]any{}
	if spec.Csi != nil {
		var each strings.Builder
		for _, prefix := range spec.Csi.Prefixes {
			for _, params := range spec.Csi.Params {
				for _, intermediate := range spec.Csi.Inter {
					hash := newLineHash()
					spec.Csi.group(prefix, params, intermediate, func(sequence []byte) {
						spec.run(&hash, sequence)
					})
					each.WriteString(hash.digest8())
				}
			}
		}
		reply["csi"] = each.String()
	}
	if spec.Seqs != nil {
		var each strings.Builder
		for _, sequence := range spec.Seqs {
			hash := newLineHash()
			spec.run(&hash, sequence)
			each.WriteString(hash.digest8())
		}
		reply["seqs"] = each.String()
	}
	return reply
}

// widthProbe is where the cursor is after c is printed at row 0, column 6 of
// a 2x8 screen, as row*100+col, plus 1000 when the character was added to
// the cell before the cursor. Widths 0, 1, 2 and 3 give 1006, 7, 8 and 102.
func widthProbe(c rune) uint32 {
	parser := ParserNew(2, 8, 0)
	parser.Process([]byte("\x1b[1;7H"))
	parser.Process([]byte(string(c)))
	row, col := parser.Screen().CursorPosition()
	code := uint32(row)*100 + uint32(col)
	if cell, ok := parser.Screen().Cell(0, 5).Get(); ok && cell.HasContents() {
		code += 1000
	}
	return code
}

func runWidth(from, to int) string {
	var runs strings.Builder
	first, last, current, open := 0, 0, uint32(0), false
	flush := func() {
		if open {
			fmt.Fprintf(&runs, "%x-%x:%d,", first, last, current)
		}
	}
	for codePoint := from; codePoint <= to; codePoint++ {
		if codePoint >= 0xD800 && codePoint <= 0xDFFF || codePoint > 0x10FFFF {
			continue
		}
		code := widthProbe(rune(codePoint))
		if open && current == code && last+1 == codePoint {
			last = codePoint
			continue
		}
		flush()
		first, last, current, open = codePoint, codePoint, code, true
	}
	flush()
	return runs.String()
}

func answerVt100(request json.RawMessage) any {
	var decoded vt100Request
	if err := json.Unmarshal(request, &decoded); err != nil {
		panic(err)
	}
	if decoded.Op == "width" {
		return map[string]any{"runs": runWidth(decoded.From, decoded.To)}
	}
	if decoded.Matrix != nil {
		return runVt100Matrix(decoded.Matrix)
	}
	each := []any{}
	dumps := []string{}
	for _, c := range decoded.Cases {
		d := newDump(decoded.Full)
		failed := runSteps(d, c.Rows, c.Cols, c.Scrollback, c.Resize, c.Steps)
		if failed == nil {
			each = append(each, d.digests())
		} else {
			each = append(each, map[string]any{"s": d.digests(), "panic": failed.message, "at": failed.at})
		}
		if decoded.Full {
			dumps = append(dumps, d.full.String())
		}
	}
	reply := map[string]any{"each": each}
	if decoded.Full {
		reply["dumps"] = dumps
	}
	return reply
}

func TestVt100AgainstRust(t *testing.T) {
	oracle.Check(t, "vt100", "vt100", generateVt100, answerVt100)
}

// Every code point is printed through both implementations, which compares
// the width tables as vt100 uses them.
func TestWidthAgainstRust(t *testing.T) {
	oracle.Check(t, "vt100", "width", generateWidth, answerVt100)
}
