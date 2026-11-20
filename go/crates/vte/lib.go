// Port of vte 0.15.0 src/lib.rs

// Parser for implementing virtual terminal emulators
//
// [`Parser`] is implemented according to [Paul Williams' ANSI parser state
// machine]. The state machine doesn't assign meaning to the parsed data and is
// thus not itself sufficient for writing a terminal emulator. Instead, it is
// expected that an implementation of [`Perform`] is provided which does
// something useful with the parsed data. The [`Parser`] handles the book
// keeping, and the [`Perform`] gets to simply handle actions.
//
// # Differences from original state machine description
//
// * UTF-8 Support for Input
// * OSC Strings can be terminated by 0x07
// * Only supports 7-bit codes
//
// [Paul Williams' ANSI parser state machine]: https://vt100.net/emu/dec_ansi_parser

package vte

import (
	"bytes"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

const maxIntermediates = 2
const maxOscParams = 16
const maxOscRaw = 1024

// Parser for raw _VTE_ protocol which delegates actions to a [`Perform`]
//
// The Rust type is generic over the size of the raw Operating System Command
// buffer, which is only used when the crate's `std` feature is not enabled.
// This is the `std` build: the buffer grows without limit.
//
// The zero value is a new parser, as `Parser::default()` is.
type Parser struct {
	state           state
	intermediates   [maxIntermediates]uint8
	intermediateIdx int
	params          Params
	param           uint16
	oscRaw          []uint8
	oscParams       [maxOscParams]rs.Tuple2[int, int]
	oscNumParams    int
	ignoring        bool
	partialUtf8     [4]uint8
	partialUtf8Len  int
}

// Create a new Parser
func ParserNew() Parser {
	return ParserDefault()
}

// ParserDefault is `Parser::default()`.
func ParserDefault() Parser {
	return Parser{}
}

func (self *Parser) paramsMethod() *Params {
	return &self.params
}

func (self *Parser) intermediatesMethod() []uint8 {
	return self.intermediates[:self.intermediateIdx]
}

// Advance the parser state.
//
// Requires a [`Perform`] implementation to handle the triggered actions.
//
// The Rust method is generic over the performer's type; here the performer
// is passed as the interface.
func (self *Parser) Advance(performer Perform, bytes_ []uint8) {
	i := 0

	// Handle partial codepoints from previous calls to `advance`.
	if self.partialUtf8Len != 0 {
		i += self.advancePartialUtf8(performer, bytes_)
	}

	for i != len(bytes_) {
		switch self.state {
		case state_Ground:
			i += self.advanceGround(performer, bytes_[i:])
		default:
			// Inlining it results in worse codegen.
			byte_ := bytes_[i]
			self.changeState(performer, byte_)
			i += 1
		}
	}
}

// Partially advance the parser state.
//
// This is equivalent to [`Self::advance`], but stops when
// [`Perform::terminated`] is true after reading a byte.
//
// Returns the number of bytes read before termination.
//
// See [`Perform::advance`] for more details.
func (self *Parser) AdvanceUntilTerminated(performer Perform, bytes_ []uint8) int {
	i := 0

	// Handle partial codepoints from previous calls to `advance`.
	if self.partialUtf8Len != 0 {
		i += self.advancePartialUtf8(performer, bytes_)
	}

	for i != len(bytes_) && !performer.Terminated() {
		switch self.state {
		case state_Ground:
			i += self.advanceGround(performer, bytes_[i:])
		default:
			// Inlining it results in worse codegen.
			byte_ := bytes_[i]
			self.changeState(performer, byte_)
			i += 1
		}
	}

	return i
}

func (self *Parser) changeState(performer Perform, byte_ uint8) {
	switch self.state {
	case state_CsiEntry:
		self.advanceCsiEntry(performer, byte_)
	case state_CsiIgnore:
		self.advanceCsiIgnore(performer, byte_)
	case state_CsiIntermediate:
		self.advanceCsiIntermediate(performer, byte_)
	case state_CsiParam:
		self.advanceCsiParam(performer, byte_)
	case state_DcsEntry:
		self.advanceDcsEntry(performer, byte_)
	case state_DcsIgnore:
		self.anywhere(performer, byte_)
	case state_DcsIntermediate:
		self.advanceDcsIntermediate(performer, byte_)
	case state_DcsParam:
		self.advanceDcsParam(performer, byte_)
	case state_DcsPassthrough:
		self.advanceDcsPassthrough(performer, byte_)
	case state_Escape:
		self.advanceEsc(performer, byte_)
	case state_EscapeIntermediate:
		self.advanceEscIntermediate(performer, byte_)
	case state_OscString:
		self.advanceOscString(performer, byte_)
	case state_SosPmApcString:
		self.anywhere(performer, byte_)
	case state_Ground:
		panic(rs.Unreachable())
	default:
		panic(rs.Unreachable())
	}
}

// isC0 reports whether byte_ is in `0x00..=0x17 | 0x19 | 0x1C..=0x1F`, the
// control characters every state but the string states acts on.
func isC0(byte_ uint8) bool {
	return byte_ <= 0x17 || byte_ == 0x19 || (byte_ >= 0x1C && byte_ <= 0x1F)
}

func (self *Parser) advanceCsiEntry(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
		performer.Execute(byte_)
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
		self.state = state_CsiIntermediate
	case byte_ >= 0x30 && byte_ <= 0x39:
		self.actionParamnext(byte_)
		self.state = state_CsiParam
	case byte_ == 0x3A:
		self.actionSubparam()
		self.state = state_CsiParam
	case byte_ == 0x3B:
		self.actionParam()
		self.state = state_CsiParam
	case byte_ >= 0x3C && byte_ <= 0x3F:
		self.actionCollect(byte_)
		self.state = state_CsiParam
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.actionCsiDispatch(performer, byte_)
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceCsiIgnore(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
		performer.Execute(byte_)
	case byte_ >= 0x20 && byte_ <= 0x3F:
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.state = state_Ground
	case byte_ == 0x7F:
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceCsiIntermediate(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
		performer.Execute(byte_)
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
	case byte_ >= 0x30 && byte_ <= 0x3F:
		self.state = state_CsiIgnore
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.actionCsiDispatch(performer, byte_)
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceCsiParam(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
		performer.Execute(byte_)
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
		self.state = state_CsiIntermediate
	case byte_ >= 0x30 && byte_ <= 0x39:
		self.actionParamnext(byte_)
	case byte_ == 0x3A:
		self.actionSubparam()
	case byte_ == 0x3B:
		self.actionParam()
	case byte_ >= 0x3C && byte_ <= 0x3F:
		self.state = state_CsiIgnore
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.actionCsiDispatch(performer, byte_)
	case byte_ == 0x7F:
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceDcsEntry(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
		self.state = state_DcsIntermediate
	case byte_ >= 0x30 && byte_ <= 0x39:
		self.actionParamnext(byte_)
		self.state = state_DcsParam
	case byte_ == 0x3A:
		self.actionSubparam()
		self.state = state_DcsParam
	case byte_ == 0x3B:
		self.actionParam()
		self.state = state_DcsParam
	case byte_ >= 0x3C && byte_ <= 0x3F:
		self.actionCollect(byte_)
		self.state = state_DcsParam
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.actionHook(performer, byte_)
	case byte_ == 0x7F:
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceDcsIntermediate(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
	case byte_ >= 0x30 && byte_ <= 0x3F:
		self.state = state_DcsIgnore
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.actionHook(performer, byte_)
	case byte_ == 0x7F:
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceDcsParam(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
		self.state = state_DcsIntermediate
	case byte_ >= 0x30 && byte_ <= 0x39:
		self.actionParamnext(byte_)
	case byte_ == 0x3A:
		self.actionSubparam()
	case byte_ == 0x3B:
		self.actionParam()
	case byte_ >= 0x3C && byte_ <= 0x3F:
		self.state = state_DcsIgnore
	case byte_ >= 0x40 && byte_ <= 0x7E:
		self.actionHook(performer, byte_)
	case byte_ == 0x7F:
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceDcsPassthrough(performer Perform, byte_ uint8) {
	switch {
	case byte_ <= 0x17 || byte_ == 0x19 || (byte_ >= 0x1C && byte_ <= 0x7E):
		performer.Put(byte_)
	case byte_ == 0x18 || byte_ == 0x1A:
		performer.Unhook()
		performer.Execute(byte_)
		self.state = state_Ground
	case byte_ == 0x1B:
		performer.Unhook()
		self.resetParams()
		self.state = state_Escape
	case byte_ == 0x7F:
	case byte_ == 0x9C:
		performer.Unhook()
		self.state = state_Ground
	default:
	}
}

func (self *Parser) advanceEsc(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
		performer.Execute(byte_)
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
		self.state = state_EscapeIntermediate
	case byte_ >= 0x30 && byte_ <= 0x4F:
		performer.EscDispatch(self.intermediatesMethod(), self.ignoring, byte_)
		self.state = state_Ground
	case byte_ == 0x50:
		self.resetParams()
		self.state = state_DcsEntry
	case byte_ >= 0x51 && byte_ <= 0x57:
		performer.EscDispatch(self.intermediatesMethod(), self.ignoring, byte_)
		self.state = state_Ground
	case byte_ == 0x58:
		self.state = state_SosPmApcString
	case byte_ >= 0x59 && byte_ <= 0x5A:
		performer.EscDispatch(self.intermediatesMethod(), self.ignoring, byte_)
		self.state = state_Ground
	case byte_ == 0x5B:
		self.resetParams()
		self.state = state_CsiEntry
	case byte_ == 0x5C:
		performer.EscDispatch(self.intermediatesMethod(), self.ignoring, byte_)
		self.state = state_Ground
	case byte_ == 0x5D:
		self.oscRaw = self.oscRaw[:0]
		self.oscNumParams = 0
		self.state = state_OscString
	case byte_ >= 0x5E && byte_ <= 0x5F:
		self.state = state_SosPmApcString
	case byte_ >= 0x60 && byte_ <= 0x7E:
		performer.EscDispatch(self.intermediatesMethod(), self.ignoring, byte_)
		self.state = state_Ground
	// Anywhere.
	case byte_ == 0x18 || byte_ == 0x1A:
		performer.Execute(byte_)
		self.state = state_Ground
	case byte_ == 0x1B:
	default:
	}
}

func (self *Parser) advanceEscIntermediate(performer Perform, byte_ uint8) {
	switch {
	case isC0(byte_):
		performer.Execute(byte_)
	case byte_ >= 0x20 && byte_ <= 0x2F:
		self.actionCollect(byte_)
	case byte_ >= 0x30 && byte_ <= 0x7E:
		performer.EscDispatch(self.intermediatesMethod(), self.ignoring, byte_)
		self.state = state_Ground
	case byte_ == 0x7F:
	default:
		self.anywhere(performer, byte_)
	}
}

func (self *Parser) advanceOscString(performer Perform, byte_ uint8) {
	switch {
	case byte_ <= 0x06 || (byte_ >= 0x08 && byte_ <= 0x17) || byte_ == 0x19 || (byte_ >= 0x1C && byte_ <= 0x1F):
	case byte_ == 0x07:
		self.oscEnd(performer, byte_)
		self.state = state_Ground
	case byte_ == 0x18 || byte_ == 0x1A:
		self.oscEnd(performer, byte_)
		performer.Execute(byte_)
		self.state = state_Ground
	case byte_ == 0x1B:
		self.oscEnd(performer, byte_)
		self.resetParams()
		self.state = state_Escape
	case byte_ == 0x3B:
		self.actionOscPutParam()
	default:
		self.actionOscPut(byte_)
	}
}

func (self *Parser) anywhere(performer Perform, byte_ uint8) {
	switch {
	case byte_ == 0x18 || byte_ == 0x1A:
		performer.Execute(byte_)
		self.state = state_Ground
	case byte_ == 0x1B:
		self.resetParams()
		self.state = state_Escape
	default:
	}
}

func (self *Parser) actionCsiDispatch(performer Perform, byte_ uint8) {
	if self.params.isFull() {
		self.ignoring = true
	} else {
		self.params.push(self.param)
	}
	performer.CsiDispatch(self.paramsMethod(), self.intermediatesMethod(), self.ignoring, rune(byte_))

	self.state = state_Ground
}

func (self *Parser) actionHook(performer Perform, byte_ uint8) {
	if self.params.isFull() {
		self.ignoring = true
	} else {
		self.params.push(self.param)
	}
	performer.Hook(self.paramsMethod(), self.intermediatesMethod(), self.ignoring, rune(byte_))
	self.state = state_DcsPassthrough
}

func (self *Parser) actionCollect(byte_ uint8) {
	if self.intermediateIdx == maxIntermediates {
		self.ignoring = true
	} else {
		self.intermediates[self.intermediateIdx] = byte_
		self.intermediateIdx += 1
	}
}

// Advance to the next subparameter.
func (self *Parser) actionSubparam() {
	if self.params.isFull() {
		self.ignoring = true
	} else {
		self.params.extend(self.param)
		self.param = 0
	}
}

// Advance to the next parameter.
func (self *Parser) actionParam() {
	if self.params.isFull() {
		self.ignoring = true
	} else {
		self.params.push(self.param)
		self.param = 0
	}
}

// Advance inside the parameter without terminating it.
func (self *Parser) actionParamnext(byte_ uint8) {
	if self.params.isFull() {
		self.ignoring = true
	} else {
		// Continue collecting bytes into param.
		self.param = saturatingMulU16(self.param, 10)
		self.param = saturatingAddU16(self.param, uint16(byte_-'0'))
	}
}

// Add OSC param separator.
func (self *Parser) actionOscPutParam() {
	idx := len(self.oscRaw)

	paramIdx := self.oscNumParams
	switch paramIdx {
	// First param is special - 0 to current byte index.
	case 0:
		self.oscParams[paramIdx] = rs.T2(0, idx)

	// Only process up to MAX_OSC_PARAMS.
	case maxOscParams:
		return

	// All other params depend on previous indexing.
	default:
		prev := self.oscParams[paramIdx-1]
		begin := prev.F1
		self.oscParams[paramIdx] = rs.T2(begin, idx)
	}

	self.oscNumParams += 1
}

func (self *Parser) actionOscPut(byte_ uint8) {
	self.oscRaw = append(self.oscRaw, byte_)
}

func (self *Parser) oscEnd(performer Perform, byte_ uint8) {
	self.actionOscPutParam()
	self.oscDispatch(performer, byte_)
	self.oscRaw = self.oscRaw[:0]
	self.oscNumParams = 0
}

// Reset escape sequence parameters and intermediates.
func (self *Parser) resetParams() {
	self.intermediateIdx = 0
	self.ignoring = false
	self.param = 0

	self.params.clear()
}

// Separate method for osc_dispatch that borrows self as read-only
//
// The aliasing is needed here for multiple slices into self.osc_raw
func (self *Parser) oscDispatch(performer Perform, byte_ uint8) {
	var slices [maxOscParams][]uint8

	for i := 0; i < self.oscNumParams; i++ {
		indices := self.oscParams[i]
		// The capacity is cut so that a performer appending to a parameter
		// cannot write into the one after it.
		slices[i] = self.oscRaw[indices.F0:indices.F1:indices.F1]
	}

	numParams := self.oscNumParams
	params := slices[:numParams]
	performer.OscDispatch(params, byte_ == 0x07)
}

// Advance the parser state from ground.
//
// The ground state is handled separately since it can only be left using
// the escape character (`\x1b`). This allows more efficient parsing by
// using SIMD search with [`memchr`].
func (self *Parser) advanceGround(performer Perform, bytes_ []uint8) int {
	// Find the next escape character.
	numBytes := len(bytes_)
	plainChars := bytes.IndexByte(bytes_, 0x1B)
	if plainChars < 0 {
		plainChars = numBytes
	}

	// If the next character is ESC, just process it and short-circuit.
	if plainChars == 0 {
		self.state = state_Escape
		self.resetParams()
		return 1
	}

	validBytes, errorLen, ok := fromUtf8(bytes_[:plainChars])
	if ok {
		parserGroundDispatch(performer, bytes_[:plainChars])
		processed := plainChars

		// If there's another character, it must be escape so process it directly.
		if processed < numBytes {
			self.state = state_Escape
			self.resetParams()
			processed += 1
		}

		return processed
	}

	// Handle invalid and partial utf8.

	// Dispatch all the valid bytes.
	parserGroundDispatch(performer, bytes_[:validBytes])

	if len_, ok := errorLen.Get(); ok {
		// Execute C1 escapes or emit replacement character.
		if len_ == 1 && bytes_[validBytes] <= 0x9F {
			performer.Execute(bytes_[validBytes])
		} else {
			performer.Print('�')
		}

		// Restart processing after the invalid bytes.
		//
		// While we could theoretically try to just re-parse
		// `bytes[valid_bytes + len..plain_chars]`, it's easier
		// to just skip it and invalid utf8 is pretty rare anyway.
		return validBytes + len_
	}

	if plainChars < numBytes {
		// Process bytes cut off by escape.
		performer.Print('�')
		self.state = state_Escape
		self.resetParams()
		return plainChars + 1
	}

	// Process bytes cut off by the buffer end.
	extraBytes := numBytes - validBytes
	partialLen := self.partialUtf8Len + extraBytes
	copy(self.partialUtf8[self.partialUtf8Len:partialLen], bytes_[validBytes:validBytes+extraBytes])
	self.partialUtf8Len = partialLen
	return numBytes
}

// Advance the parser while processing a partial utf8 codepoint.
func (self *Parser) advancePartialUtf8(performer Perform, bytes_ []uint8) int {
	// Try to copy up to 3 more characters, to ensure the codepoint is complete.
	oldBytes := self.partialUtf8Len
	toCopy := min(len(bytes_), len(self.partialUtf8)-oldBytes)
	copy(self.partialUtf8[oldBytes:oldBytes+toCopy], bytes_[:toCopy])
	self.partialUtf8Len += toCopy

	// Parse the unicode character.
	validBytes, errorLen, ok := fromUtf8(self.partialUtf8[:self.partialUtf8Len])
	if ok {
		// If the entire buffer is valid, use the first character and continue parsing.
		c, size := utf8.DecodeRune(self.partialUtf8[:self.partialUtf8Len])
		performer.Print(c)

		self.partialUtf8Len = 0
		return size - oldBytes
	}

	// If we have any valid bytes, that means we partially copied another
	// utf8 character into `partial_utf8`. Since we only care about the
	// first character, we just ignore the rest.
	if validBytes > 0 {
		c, _ := utf8.DecodeRune(self.partialUtf8[:validBytes])

		performer.Print(c)

		self.partialUtf8Len = 0
		return validBytes - oldBytes
	}

	if invalidLen, ok := errorLen.Get(); ok {
		// If the partial character was also invalid, emit the replacement
		// character.
		performer.Print('�')

		self.partialUtf8Len = 0
		return invalidLen - oldBytes
	}

	// If the character still isn't complete, wait for more data.
	return toCopy
}

// Handle ground dispatch of print/execute for all characters in a string.
// text is valid UTF-8.
func parserGroundDispatch(performer Perform, text []uint8) {
	for len(text) > 0 {
		c, size := utf8.DecodeRune(text)
		text = text[size:]
		if c <= 0x1f || (c >= 0x80 && c <= 0x9f) {
			performer.Execute(uint8(c))
		} else {
			performer.Print(c)
		}
	}
}

// saturatingMulU16 is `u16::saturating_mul`.
func saturatingMulU16(a, b uint16) uint16 {
	product := uint32(a) * uint32(b)
	if product > 0xFFFF {
		return 0xFFFF
	}
	return uint16(product)
}

// saturatingAddU16 is `u16::saturating_add`.
func saturatingAddU16(a, b uint16) uint16 {
	sum := uint32(a) + uint32(b)
	if sum > 0xFFFF {
		return 0xFFFF
	}
	return uint16(sum)
}

type state uint8

const (
	state_CsiEntry           state = 1
	state_CsiIgnore          state = 2
	state_CsiIntermediate    state = 3
	state_CsiParam           state = 4
	state_DcsEntry           state = 5
	state_DcsIgnore          state = 6
	state_DcsIntermediate    state = 7
	state_DcsParam           state = 8
	state_DcsPassthrough     state = 9
	state_Escape             state = 10
	state_EscapeIntermediate state = 11
	state_OscString          state = 12
	state_SosPmApcString     state = 13
	// Ground is the default, so it is the zero value.
	state_Ground state = 0
)

var _ = rs.RegisterEnum[state]("State",
	rs.Const(state_CsiEntry, "CsiEntry"),
	rs.Const(state_CsiIgnore, "CsiIgnore"),
	rs.Const(state_CsiIntermediate, "CsiIntermediate"),
	rs.Const(state_CsiParam, "CsiParam"),
	rs.Const(state_DcsEntry, "DcsEntry"),
	rs.Const(state_DcsIgnore, "DcsIgnore"),
	rs.Const(state_DcsIntermediate, "DcsIntermediate"),
	rs.Const(state_DcsParam, "DcsParam"),
	rs.Const(state_DcsPassthrough, "DcsPassthrough"),
	rs.Const(state_Escape, "Escape"),
	rs.Const(state_EscapeIntermediate, "EscapeIntermediate"),
	rs.Const(state_OscString, "OscString"),
	rs.Const(state_SosPmApcString, "SosPmApcString"),
	rs.Const(state_Ground, "Ground"),
)

// Performs actions requested by the Parser
//
// Actions in this case mean, for example, handling a CSI escape sequence
// describing cursor movement, or simply printing characters to the screen.
//
// The methods on this type correspond to actions described in
// <http://vt100.net/emu/dec_ansi_parser>. I've done my best to describe them in
// a useful way in my own words for completeness, but the site should be
// referenced if something isn't clear. If the site disappears at some point in
// the future, consider checking archive.org.
//
// Every method has a default in Rust that does nothing (and `terminated`
// answers false). An implementation that relies on one calls the matching
// PerformDefault function from a one-line method.
//
// The slices and the Params a method receives belong to the parser and are
// only valid until the method returns, as the borrows are in Rust: copy what
// is kept.
type Perform interface {
	// Draw a character to the screen and update states.
	Print(c rune)

	// Execute a C0 or C1 control function.
	Execute(byte_ uint8)

	// Invoked when a final character arrives in first part of device control
	// string.
	//
	// The control function should be determined from the private marker, final
	// character, and execute with a parameter list. A handler should be
	// selected for remaining characters in the string; the handler
	// function should subsequently be called by `put` for every character in
	// the control string.
	//
	// The `ignore` flag indicates that more than two intermediates arrived and
	// subsequent characters were ignored.
	Hook(params *Params, intermediates []uint8, ignore bool, action rune)

	// Pass bytes as part of a device control string to the handle chosen in
	// `hook`. C0 controls will also be passed to the handler.
	Put(byte_ uint8)

	// Called when a device control string is terminated.
	//
	// The previously selected handler should be notified that the DCS has
	// terminated.
	Unhook()

	// Dispatch an operating system command.
	OscDispatch(params [][]uint8, bellTerminated bool)

	// A final character has arrived for a CSI sequence
	//
	// The `ignore` flag indicates that either more than two intermediates
	// arrived or the number of parameters exceeded the maximum supported
	// length, and subsequent characters were ignored.
	CsiDispatch(params *Params, intermediates []uint8, ignore bool, action rune)

	// The final character of an escape sequence has arrived.
	//
	// The `ignore` flag indicates that more than two intermediates arrived and
	// subsequent characters were ignored.
	EscDispatch(intermediates []uint8, ignore bool, byte_ uint8)

	// Whether the parser should terminate prematurely.
	//
	// This can be used in conjunction with
	// [`Parser::advance_until_terminated`] to terminate the parser after
	// receiving certain escape sequences like synchronized updates.
	//
	// This is checked after every parsed byte, so no expensive computation
	// should take place in this function.
	Terminated() bool
}

// PerformDefaultPrint is the default body of Perform::print.
func PerformDefaultPrint(self Perform, c rune) {}

// PerformDefaultExecute is the default body of Perform::execute.
func PerformDefaultExecute(self Perform, byte_ uint8) {}

// PerformDefaultHook is the default body of Perform::hook.
func PerformDefaultHook(self Perform, params *Params, intermediates []uint8, ignore bool, action rune) {
}

// PerformDefaultPut is the default body of Perform::put.
func PerformDefaultPut(self Perform, byte_ uint8) {}

// PerformDefaultUnhook is the default body of Perform::unhook.
func PerformDefaultUnhook(self Perform) {}

// PerformDefaultOscDispatch is the default body of Perform::osc_dispatch.
func PerformDefaultOscDispatch(self Perform, params [][]uint8, bellTerminated bool) {}

// PerformDefaultCsiDispatch is the default body of Perform::csi_dispatch.
func PerformDefaultCsiDispatch(self Perform, params *Params, intermediates []uint8, ignore bool, action rune) {
}

// PerformDefaultEscDispatch is the default body of Perform::esc_dispatch.
func PerformDefaultEscDispatch(self Perform, intermediates []uint8, ignore bool, byte_ uint8) {}

// PerformDefaultTerminated is the default body of Perform::terminated.
func PerformDefaultTerminated(self Perform) bool {
	return false
}
