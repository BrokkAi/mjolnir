// Port of serde_json 1.0.150 src/de.rs

package serdejson

import (
	"io"
	"math"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Deserialize JSON data to a Rust data structure.

// Deserializer is `serde_json::Deserializer`: a structure that deserializes
// JSON into Rust values. *Deserializer is a serde.Deserializer.
type Deserializer struct {
	read            *reader
	scratch         []byte
	remainingDepth  uint8
	singlePrecision bool
}

func newDeserializer(read *reader) *Deserializer {
	return &Deserializer{read: read, remainingDepth: 128}
}

// DeserializerFromReader is `Deserializer::from_reader`: a JSON
// deserializer from an `io::Read`.
func DeserializerFromReader(reader io.Reader) *Deserializer {
	return newDeserializer(newIoRead(reader))
}

// DeserializerFromSlice is `Deserializer::from_slice`: a JSON deserializer
// from a `&[u8]`.
func DeserializerFromSlice(bytes []byte) *Deserializer {
	return newDeserializer(newSliceRead(bytes))
}

// DeserializerFromStr is `Deserializer::from_str`: a JSON deserializer from
// a `&str`.
func DeserializerFromStr(s string) *Deserializer {
	return newDeserializer(newStrRead(s))
}

// overflow is `overflow!(a * 10 + b, c)`.
func overflowU64(a, b uint64) bool {
	const c = math.MaxUint64
	return a >= c/10 && (a > c/10 || b > c%10)
}

func overflowI32(a, b int32) bool {
	const c = math.MaxInt32
	return a >= c/10 && (a > c/10 || b > c%10)
}

// parserNumber is `ParserNumber`.
type parserNumber struct {
	kind uint8 // parserF64, parserU64 or parserI64
	f    float64
	u    uint64
	i    int64
}

const (
	parserF64 uint8 = iota
	parserU64
	parserI64
)

func (n parserNumber) visit(visitor serde.DeVisitor) (any, error) {
	switch n.kind {
	case parserF64:
		return serde.DeVisitorVisitF64(visitor, n.f)
	case parserU64:
		return serde.DeVisitorVisitU64(visitor, n.u)
	}
	return serde.DeVisitorVisitI64(visitor, n.i)
}

func (n parserNumber) invalidType(exp serde.DeExpected) error {
	switch n.kind {
	case parserF64:
		return serde.DeErrorInvalidType(&serde.DeUnexpectedFloat{F0: n.f}, exp)
	case parserU64:
		return serde.DeErrorInvalidType(&serde.DeUnexpectedUnsigned{F0: n.u}, exp)
	}
	return serde.DeErrorInvalidType(&serde.DeUnexpectedSigned{F0: n.i}, exp)
}

// End is `Deserializer::end`: the `Deserializer::end` method should be
// called after a value has been fully deserialized. This allows the
// `Deserializer` to validate that the input stream is at the end or that it
// only has trailing whitespace.
func (d *Deserializer) End() error {
	_, ok, err := d.parseWhitespace()
	if err != nil {
		return err
	}
	if ok {
		return d.peekError(errorCodeTrailingCharacters)
	}
	return nil
}

func (d *Deserializer) peek() (byte, bool, error) { return d.read.peek() }

func (d *Deserializer) peekOrNull() (byte, error) {
	c, ok, err := d.peek()
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, nil
	}
	return c, nil
}

func (d *Deserializer) eatChar() { d.read.discard() }

func (d *Deserializer) nextChar() (byte, bool, error) { return d.read.next() }

func (d *Deserializer) nextCharOrNull() (byte, error) {
	c, ok, err := d.nextChar()
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, nil
	}
	return c, nil
}

// error is an error at the current position.
func (d *Deserializer) error(code errorCodeKind) *Error {
	p := d.read.position()
	return errorSyntax(code, p.line, p.column)
}

// peekError is an error at the position of the byte peek() looked at.
func (d *Deserializer) peekError(code errorCodeKind) *Error {
	p := d.read.peekPosition()
	return errorSyntax(code, p.line, p.column)
}

// parseWhitespace returns the first non-whitespace byte without consuming
// it, and false at the end of the input.
func (d *Deserializer) parseWhitespace() (byte, bool, error) {
	for {
		c, ok, err := d.peek()
		if err != nil || !ok {
			return 0, false, err
		}
		switch c {
		case ' ', '\n', '\t', '\r':
			d.eatChar()
		default:
			return c, true, nil
		}
	}
}

// peekInvalidType is `peek_invalid_type`: the invalid_type error for the
// value at the input's position.
func (d *Deserializer) peekInvalidType(exp serde.DeExpected) error {
	c, _ := d.peekOrNull()
	var err error
	switch {
	case c == 'n':
		d.eatChar()
		if err := d.parseIdent("ull"); err != nil {
			return err
		}
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedUnit{}, exp)
	case c == 't':
		d.eatChar()
		if err := d.parseIdent("rue"); err != nil {
			return err
		}
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedBool{F0: true}, exp)
	case c == 'f':
		d.eatChar()
		if err := d.parseIdent("alse"); err != nil {
			return err
		}
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedBool{F0: false}, exp)
	case c == '-':
		d.eatChar()
		n, parseErr := d.parseAnyNumber(false)
		if parseErr != nil {
			return parseErr
		}
		err = n.invalidType(exp)
	case c >= '0' && c <= '9':
		n, parseErr := d.parseAnyNumber(true)
		if parseErr != nil {
			return parseErr
		}
		err = n.invalidType(exp)
	case c == '"':
		d.eatChar()
		d.scratch = d.scratch[:0]
		s, _, parseErr := d.read.parseStr(&d.scratch)
		if parseErr != nil {
			return parseErr
		}
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedStr{F0: s}, exp)
	case c == '[':
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedSeq{}, exp)
	case c == '{':
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedMap{}, exp)
	default:
		err = d.peekError(errorCodeExpectedSomeValue)
	}
	return d.fixPosition(err)
}

// deserializeNumber is `deserialize_number`.
func (d *Deserializer) deserializeNumber(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}
	var value any
	switch {
	case peek == '-':
		d.eatChar()
		n, parseErr := d.parseInteger(false)
		if parseErr != nil {
			return nil, parseErr
		}
		value, err = n.visit(visitor)
	case peek >= '0' && peek <= '9':
		n, parseErr := d.parseInteger(true)
		if parseErr != nil {
			return nil, parseErr
		}
		value, err = n.visit(visitor)
	default:
		err = d.peekInvalidType(visitor)
	}
	if err != nil {
		return nil, d.fixPosition(err)
	}
	return value, nil
}

// doDeserializeF32 is `do_deserialize_f32`.
func (d *Deserializer) doDeserializeF32(visitor serde.DeVisitor) (any, error) {
	d.singlePrecision = true
	value, err := d.deserializeNumber(visitor)
	d.singlePrecision = false
	return value, err
}

// fixPosition is `fix_position`: an error without a position takes the
// current one.
func (d *Deserializer) fixPosition(err error) *Error {
	e := toError(err)
	return e.fixPosition(func(code errorCode) *Error {
		p := d.read.position()
		return &Error{code: code, line: p.line, column: p.column}
	})
}

func (d *Deserializer) parseIdent(ident string) error {
	for i := 0; i < len(ident); i++ {
		c, ok, err := d.nextChar()
		if err != nil {
			return err
		}
		if !ok {
			return d.error(errorCodeEofWhileParsingValue)
		}
		if c != ident[i] {
			return d.error(errorCodeExpectedSomeIdent)
		}
	}
	return nil
}

func (d *Deserializer) parseInteger(positive bool) (parserNumber, error) {
	next, ok, err := d.nextChar()
	if err != nil {
		return parserNumber{}, err
	}
	if !ok {
		return parserNumber{}, d.error(errorCodeEofWhileParsingValue)
	}
	switch {
	case next == '0':
		// There can be only one leading '0'.
		c, err := d.peekOrNull()
		if err != nil {
			return parserNumber{}, err
		}
		if c >= '0' && c <= '9' {
			return parserNumber{}, d.peekError(errorCodeInvalidNumber)
		}
		return d.parseNumber(positive, 0)
	case next >= '1' && next <= '9':
		significand := uint64(next - '0')
		for {
			c, err := d.peekOrNull()
			if err != nil {
				return parserNumber{}, err
			}
			if c < '0' || c > '9' {
				return d.parseNumber(positive, significand)
			}
			digit := uint64(c - '0')
			// We need to be careful with overflow. If we can, try to keep
			// the number as a `u64` until we grow too large. At that point,
			// switch to parsing the value as a `f64`.
			if overflowU64(significand, digit) {
				f, err := d.parseLongInteger(positive, significand)
				if err != nil {
					return parserNumber{}, err
				}
				return parserNumber{kind: parserF64, f: f}, nil
			}
			d.eatChar()
			significand = significand*10 + digit
		}
	}
	return parserNumber{}, d.error(errorCodeInvalidNumber)
}

func (d *Deserializer) parseNumber(positive bool, significand uint64) (parserNumber, error) {
	c, err := d.peekOrNull()
	if err != nil {
		return parserNumber{}, err
	}
	switch c {
	case '.':
		f, err := d.parseDecimal(positive, significand, 0)
		return parserNumber{kind: parserF64, f: f}, err
	case 'e', 'E':
		f, err := d.parseExponent(positive, significand, 0)
		return parserNumber{kind: parserF64, f: f}, err
	}
	if positive {
		return parserNumber{kind: parserU64, u: significand}, nil
	}
	neg := -int64(significand)
	// Convert into a float if we underflow, or on `-0`.
	if neg >= 0 {
		return parserNumber{kind: parserF64, f: -float64(significand)}, nil
	}
	return parserNumber{kind: parserI64, i: neg}, nil
}

func (d *Deserializer) parseDecimal(positive bool, significand uint64, exponentBeforeDecimalPoint int32) (float64, error) {
	d.eatChar()

	var exponentAfterDecimalPoint int32
	for {
		c, err := d.peekOrNull()
		if err != nil {
			return 0, err
		}
		if c < '0' || c > '9' {
			break
		}
		digit := uint64(c - '0')
		if overflowU64(significand, digit) {
			exponent := exponentBeforeDecimalPoint + exponentAfterDecimalPoint
			return d.parseDecimalOverflow(positive, significand, exponent)
		}
		d.eatChar()
		significand = significand*10 + digit
		exponentAfterDecimalPoint--
	}

	// Error if there is not at least one digit after the decimal point.
	if exponentAfterDecimalPoint == 0 {
		_, ok, err := d.peek()
		if err != nil {
			return 0, err
		}
		if ok {
			return 0, d.peekError(errorCodeInvalidNumber)
		}
		return 0, d.peekError(errorCodeEofWhileParsingValue)
	}

	exponent := exponentBeforeDecimalPoint + exponentAfterDecimalPoint
	c, err := d.peekOrNull()
	if err != nil {
		return 0, err
	}
	if c == 'e' || c == 'E' {
		return d.parseExponent(positive, significand, exponent)
	}
	return d.f64FromParts(positive, significand, exponent)
}

func (d *Deserializer) parseExponent(positive bool, significand uint64, startingExp int32) (float64, error) {
	d.eatChar()

	positiveExp := true
	c, err := d.peekOrNull()
	if err != nil {
		return 0, err
	}
	switch c {
	case '+':
		d.eatChar()
	case '-':
		d.eatChar()
		positiveExp = false
	}

	next, ok, err := d.nextChar()
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, d.error(errorCodeEofWhileParsingValue)
	}

	// Make sure a digit follows the exponent place.
	if next < '0' || next > '9' {
		return 0, d.error(errorCodeInvalidNumber)
	}
	exp := int32(next - '0')

	for {
		c, err := d.peekOrNull()
		if err != nil {
			return 0, err
		}
		if c < '0' || c > '9' {
			break
		}
		d.eatChar()
		digit := int32(c - '0')
		if overflowI32(exp, digit) {
			return d.parseExponentOverflow(positive, significand == 0, positiveExp)
		}
		exp = exp*10 + digit
	}

	var finalExp int32
	if positiveExp {
		finalExp = saturatingAddI32(startingExp, exp)
	} else {
		finalExp = saturatingAddI32(startingExp, -exp)
	}
	return d.f64FromParts(positive, significand, finalExp)
}

func saturatingAddI32(a, b int32) int32 {
	sum := int64(a) + int64(b)
	switch {
	case sum > math.MaxInt32:
		return math.MaxInt32
	case sum < math.MinInt32:
		return math.MinInt32
	}
	return int32(sum)
}

// parseFloatText reads the decimal text as `lexical` does with
// float_roundtrip: correctly rounded, to an f32 when the target is one.
func (d *Deserializer) parseFloatText(text string) float64 {
	bits := 64
	if d.singlePrecision {
		bits = 32
	}
	// The text is digits with an optional point and exponent, which
	// ParseFloat always accepts; out of range it returns an infinity.
	f, _ := strconv.ParseFloat(text, bits)
	return f
}

// f64FromParts is `f64_from_parts` with float_roundtrip.
func (d *Deserializer) f64FromParts(positive bool, significand uint64, exponent int32) (float64, error) {
	f := d.parseFloatText(strconv.FormatUint(significand, 10) + "e" + strconv.FormatInt(int64(exponent), 10))
	if math.IsInf(f, 0) {
		return 0, d.error(errorCodeNumberOutOfRange)
	}
	if positive {
		return f, nil
	}
	return -f, nil
}

// parseLongInteger is `parse_long_integer` with float_roundtrip.
func (d *Deserializer) parseLongInteger(positive bool, partialSignificand uint64) (float64, error) {
	// To deserialize floats we'll first push the integer and fraction
	// parts, both as byte strings, into the scratch buffer and then feed
	// both slices to lexical's parser. For example if the input is
	// `12.34e5` we'll push b"1234" into scratch and then pass b"12" and
	// b"34" to lexical. `integer_end` will be used to track where to split
	// the scratch buffer.
	//
	// Note that lexical expects the integer part to contain *no* leading
	// zeroes and the fraction part to contain *no* trailing zeroes. The
	// first requirement is already handled by the integer parsing logic.
	// The second requirement will be enforced just before passing the
	// slices to lexical in f64_long_from_parts.
	d.scratch = strconv.AppendUint(d.scratch[:0], partialSignificand, 10)

	for {
		c, err := d.peekOrNull()
		if err != nil {
			return 0, err
		}
		switch {
		case c >= '0' && c <= '9':
			d.scratch = append(d.scratch, c)
			d.eatChar()
		case c == '.':
			d.eatChar()
			return d.parseLongDecimal(positive, len(d.scratch))
		case c == 'e' || c == 'E':
			return d.parseLongExponent(positive, len(d.scratch))
		default:
			return d.f64LongFromParts(positive, len(d.scratch), 0)
		}
	}
}

// parseLongDecimal is `parse_long_decimal`.
func (d *Deserializer) parseLongDecimal(positive bool, integerEnd int) (float64, error) {
	atLeastOneDigit := integerEnd < len(d.scratch)
	for {
		c, err := d.peekOrNull()
		if err != nil {
			return 0, err
		}
		if c < '0' || c > '9' {
			break
		}
		d.scratch = append(d.scratch, c)
		d.eatChar()
		atLeastOneDigit = true
	}

	if !atLeastOneDigit {
		_, ok, err := d.peek()
		if err != nil {
			return 0, err
		}
		if ok {
			return 0, d.peekError(errorCodeInvalidNumber)
		}
		return 0, d.peekError(errorCodeEofWhileParsingValue)
	}

	c, err := d.peekOrNull()
	if err != nil {
		return 0, err
	}
	if c == 'e' || c == 'E' {
		return d.parseLongExponent(positive, integerEnd)
	}
	return d.f64LongFromParts(positive, integerEnd, 0)
}

// parseLongExponent is `parse_long_exponent`.
func (d *Deserializer) parseLongExponent(positive bool, integerEnd int) (float64, error) {
	d.eatChar()

	positiveExp := true
	c, err := d.peekOrNull()
	if err != nil {
		return 0, err
	}
	switch c {
	case '+':
		d.eatChar()
	case '-':
		d.eatChar()
		positiveExp = false
	}

	next, ok, err := d.nextChar()
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, d.error(errorCodeEofWhileParsingValue)
	}

	// Make sure a digit follows the exponent place.
	if next < '0' || next > '9' {
		return 0, d.error(errorCodeInvalidNumber)
	}
	exp := int32(next - '0')

	for {
		c, err := d.peekOrNull()
		if err != nil {
			return 0, err
		}
		if c < '0' || c > '9' {
			break
		}
		d.eatChar()
		digit := int32(c - '0')
		if overflowI32(exp, digit) {
			zeroSignificand := true
			for _, b := range d.scratch {
				if b != '0' {
					zeroSignificand = false
				}
			}
			return d.parseExponentOverflow(positive, zeroSignificand, positiveExp)
		}
		exp = exp*10 + digit
	}

	finalExp := exp
	if !positiveExp {
		finalExp = -exp
	}
	return d.f64LongFromParts(positive, integerEnd, finalExp)
}

// parseDecimalOverflow is `parse_decimal_overflow` with float_roundtrip.
func (d *Deserializer) parseDecimalOverflow(positive bool, significand uint64, exponent int32) (float64, error) {
	digits := strconv.FormatUint(significand, 10)
	fractionDigits := int(-exponent)
	d.scratch = d.scratch[:0]
	if zeros := fractionDigits - (len(digits) + 1); zeros >= 0 {
		for range zeros + 1 {
			d.scratch = append(d.scratch, '0')
		}
	}
	d.scratch = append(d.scratch, digits...)
	integerEnd := len(d.scratch) - fractionDigits
	return d.parseLongDecimal(positive, integerEnd)
}

// parseExponentOverflow is `parse_exponent_overflow`.
func (d *Deserializer) parseExponentOverflow(positive bool, zeroSignificand bool, positiveExp bool) (float64, error) {
	// Error instead of +/- infinity.
	if !zeroSignificand && positiveExp {
		return 0, d.error(errorCodeNumberOutOfRange)
	}
	for {
		c, err := d.peekOrNull()
		if err != nil {
			return 0, err
		}
		if c < '0' || c > '9' {
			break
		}
		d.eatChar()
	}
	if positive {
		return 0, nil
	}
	return math.Copysign(0, -1), nil
}

// f64LongFromParts is `f64_long_from_parts` with float_roundtrip.
func (d *Deserializer) f64LongFromParts(positive bool, integerEnd int, exponent int32) (float64, error) {
	integer := string(d.scratch[:integerEnd])
	fraction := string(d.scratch[integerEnd:])
	if integer == "" {
		integer = "0"
	}
	f := d.parseFloatText(integer + "." + fraction + "0e" + strconv.FormatInt(int64(exponent), 10))
	if math.IsInf(f, 0) {
		return 0, d.error(errorCodeNumberOutOfRange)
	}
	if positive {
		return f, nil
	}
	return -f, nil
}

// parseAnySignedNumber is `parse_any_signed_number`.
func (d *Deserializer) parseAnySignedNumber() (parserNumber, error) {
	peek, ok, err := d.peek()
	if err != nil {
		return parserNumber{}, err
	}
	if !ok {
		return parserNumber{}, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value parserNumber
	switch {
	case peek == '-':
		d.eatChar()
		value, err = d.parseAnyNumber(false)
	case peek >= '0' && peek <= '9':
		value, err = d.parseAnyNumber(true)
	default:
		err = d.peekError(errorCodeInvalidNumber)
	}

	_, more, peekErr := d.peek()
	if peekErr != nil {
		return parserNumber{}, peekErr
	}
	if more {
		err = d.peekError(errorCodeInvalidNumber)
	}

	if err != nil {
		// The de::Error impl creates errors with unknown line and column.
		// Fill in the position here by looking at the current index in the
		// input. There is no way to tell whether this should call `error`
		// or `peek_error` so pick the one that seems correct more often.
		// Worst case, the position is off by one character.
		return parserNumber{}, d.fixPosition(err)
	}
	return value, nil
}

func (d *Deserializer) parseAnyNumber(positive bool) (parserNumber, error) {
	return d.parseInteger(positive)
}

func (d *Deserializer) parseObjectColon() error {
	c, ok, err := d.parseWhitespace()
	if err != nil {
		return err
	}
	switch {
	case !ok:
		return d.peekError(errorCodeEofWhileParsingObject)
	case c == ':':
		d.eatChar()
		return nil
	}
	return d.peekError(errorCodeExpectedColon)
}

func (d *Deserializer) endSeq() error {
	c, ok, err := d.parseWhitespace()
	if err != nil {
		return err
	}
	switch {
	case !ok:
		return d.peekError(errorCodeEofWhileParsingList)
	case c == ']':
		d.eatChar()
		return nil
	case c == ',':
		d.eatChar()
		c, ok, err := d.parseWhitespace()
		if err == nil && ok && c == ']' {
			return d.peekError(errorCodeTrailingComma)
		}
		return d.peekError(errorCodeTrailingCharacters)
	}
	return d.peekError(errorCodeTrailingCharacters)
}

func (d *Deserializer) endMap() error {
	c, ok, err := d.parseWhitespace()
	if err != nil {
		return err
	}
	switch {
	case !ok:
		return d.peekError(errorCodeEofWhileParsingObject)
	case c == '}':
		d.eatChar()
		return nil
	case c == ',':
		return d.peekError(errorCodeTrailingComma)
	}
	return d.peekError(errorCodeTrailingCharacters)
}

// ignoreValue is `ignore_value`: it reads past one value without building
// it.
func (d *Deserializer) ignoreValue() error {
	d.scratch = d.scratch[:0]
	var enclosing byte

	for {
		peek, ok, err := d.parseWhitespace()
		if err != nil {
			return err
		}
		if !ok {
			return d.peekError(errorCodeEofWhileParsingValue)
		}

		var frame byte
		switch {
		case peek == 'n':
			d.eatChar()
			if err := d.parseIdent("ull"); err != nil {
				return err
			}
		case peek == 't':
			d.eatChar()
			if err := d.parseIdent("rue"); err != nil {
				return err
			}
		case peek == 'f':
			d.eatChar()
			if err := d.parseIdent("alse"); err != nil {
				return err
			}
		case peek == '-':
			d.eatChar()
			if err := d.ignoreInteger(); err != nil {
				return err
			}
		case peek >= '0' && peek <= '9':
			if err := d.ignoreInteger(); err != nil {
				return err
			}
		case peek == '"':
			d.eatChar()
			if err := d.read.ignoreStr(); err != nil {
				return err
			}
		case peek == '[' || peek == '{':
			if enclosing != 0 {
				d.scratch = append(d.scratch, enclosing)
				enclosing = 0
			}
			d.eatChar()
			frame = peek
		default:
			return d.peekError(errorCodeExpectedSomeValue)
		}

		var acceptComma bool
		switch {
		case frame != 0:
			acceptComma = false
		case enclosing != 0:
			acceptComma, frame = true, enclosing
			enclosing = 0
		case len(d.scratch) > 0:
			acceptComma, frame = true, d.scratch[len(d.scratch)-1]
			d.scratch = d.scratch[:len(d.scratch)-1]
		default:
			return nil
		}

		for {
			c, ok, err := d.parseWhitespace()
			if err != nil {
				return err
			}
			if !ok {
				if frame == '[' {
					return d.peekError(errorCodeEofWhileParsingList)
				}
				return d.peekError(errorCodeEofWhileParsingObject)
			}
			if c == ',' && acceptComma {
				d.eatChar()
				break
			}
			if !(c == ']' && frame == '[') && !(c == '}' && frame == '{') {
				if acceptComma {
					if frame == '[' {
						return d.peekError(errorCodeExpectedListCommaOrEnd)
					}
					return d.peekError(errorCodeExpectedObjectCommaOrEnd)
				}
				break
			}

			d.eatChar()
			if len(d.scratch) == 0 {
				return nil
			}
			frame = d.scratch[len(d.scratch)-1]
			d.scratch = d.scratch[:len(d.scratch)-1]
			acceptComma = true
		}

		if frame == '{' {
			c, ok, err := d.parseWhitespace()
			if err != nil {
				return err
			}
			switch {
			case !ok:
				return d.peekError(errorCodeEofWhileParsingObject)
			case c == '"':
				d.eatChar()
			default:
				return d.peekError(errorCodeKeyMustBeAString)
			}
			if err := d.read.ignoreStr(); err != nil {
				return err
			}
			c, ok, err = d.parseWhitespace()
			if err != nil {
				return err
			}
			switch {
			case !ok:
				return d.peekError(errorCodeEofWhileParsingObject)
			case c == ':':
				d.eatChar()
			default:
				return d.peekError(errorCodeExpectedColon)
			}
		}

		enclosing = frame
	}
}

func (d *Deserializer) ignoreInteger() error {
	c, err := d.nextCharOrNull()
	if err != nil {
		return err
	}
	switch {
	case c == '0':
		// There can be only one leading '0'.
		next, err := d.peekOrNull()
		if err != nil {
			return err
		}
		if next >= '0' && next <= '9' {
			return d.peekError(errorCodeInvalidNumber)
		}
	case c >= '1' && c <= '9':
		for {
			next, err := d.peekOrNull()
			if err != nil {
				return err
			}
			if next < '0' || next > '9' {
				break
			}
			d.eatChar()
		}
	default:
		return d.error(errorCodeInvalidNumber)
	}

	c, err = d.peekOrNull()
	if err != nil {
		return err
	}
	switch c {
	case '.':
		return d.ignoreDecimal()
	case 'e', 'E':
		return d.ignoreExponent()
	}
	return nil
}

func (d *Deserializer) ignoreDecimal() error {
	d.eatChar()

	atLeastOneDigit := false
	for {
		c, err := d.peekOrNull()
		if err != nil {
			return err
		}
		if c < '0' || c > '9' {
			break
		}
		d.eatChar()
		atLeastOneDigit = true
	}

	if !atLeastOneDigit {
		return d.peekError(errorCodeInvalidNumber)
	}

	c, err := d.peekOrNull()
	if err != nil {
		return err
	}
	if c == 'e' || c == 'E' {
		return d.ignoreExponent()
	}
	return nil
}

func (d *Deserializer) ignoreExponent() error {
	d.eatChar()

	c, err := d.peekOrNull()
	if err != nil {
		return err
	}
	if c == '+' || c == '-' {
		d.eatChar()
	}

	// Make sure a digit follows the exponent place.
	c, err = d.nextCharOrNull()
	if err != nil {
		return err
	}
	if c < '0' || c > '9' {
		return d.error(errorCodeInvalidNumber)
	}

	for {
		c, err := d.peekOrNull()
		if err != nil {
			return err
		}
		if c < '0' || c > '9' {
			return nil
		}
		d.eatChar()
	}
}

// deserializeRawValue is `deserialize_raw_value`.
func (d *Deserializer) deserializeRawValue(visitor serde.DeVisitor) (any, error) {
	if _, _, err := d.parseWhitespace(); err != nil {
		return nil, err
	}
	d.read.beginRawBuffering()
	if err := d.ignoreValue(); err != nil {
		return nil, err
	}
	raw, err := d.read.endRawBuffering()
	if err != nil {
		return nil, err
	}
	return serde.DeVisitorVisitMap(visitor, &rawDeserializer{rawValue: raw, present: true})
}

// NumberFromStr is `impl FromStr for Number`.
func NumberFromStr(s string) (Number, error) {
	n, err := DeserializerFromStr(s).parseAnySignedNumber()
	if err != nil {
		return Number{}, err
	}
	return numberFromParser(n), nil
}

// enter is the start of `check_recursion!`.
func (d *Deserializer) enter() error {
	d.remainingDepth--
	if d.remainingDepth == 0 {
		return d.peekError(errorCodeRecursionLimitExceeded)
	}
	d.eatChar()
	return nil
}

// leave is the end of `check_recursion!`.
func (d *Deserializer) leave() { d.remainingDepth++ }

// visitSeqHere reads the array whose `[` is next: `check_recursion!` around
// visit_seq, then end_seq even when the visitor failed.
func (d *Deserializer) visitSeqHere(visitor serde.DeVisitor) (any, error) {
	if err := d.enter(); err != nil {
		return nil, err
	}
	ret, retErr := serde.DeVisitorVisitSeq(visitor, &seqAccess{de: d, first: true})
	d.leave()
	endErr := d.endSeq()
	switch {
	case retErr != nil:
		return nil, retErr
	case endErr != nil:
		return nil, endErr
	}
	return ret, nil
}

// visitMapHere reads the object whose `{` is next, as visitSeqHere does.
func (d *Deserializer) visitMapHere(visitor serde.DeVisitor) (any, error) {
	if err := d.enter(); err != nil {
		return nil, err
	}
	ret, retErr := serde.DeVisitorVisitMap(visitor, &mapAccess{de: d, first: true})
	d.leave()
	endErr := d.endMap()
	switch {
	case retErr != nil:
		return nil, retErr
	case endErr != nil:
		return nil, endErr
	}
	return ret, nil
}

// finish is the end of most deserialize methods: an error takes the
// current position when it has none.
func (d *Deserializer) finish(value any, err error) (any, error) {
	if err != nil {
		return nil, d.fixPosition(err)
	}
	return value, nil
}

// leaveAsError returns err as this format's error, without a position
// when it had none.
func leaveAsError(value any, err error) (any, error) {
	if err != nil {
		return nil, toError(err)
	}
	return value, nil
}

func (d *Deserializer) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch {
	case peek == 'n':
		d.eatChar()
		if err := d.parseIdent("ull"); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitUnit(visitor)
	case peek == 't':
		d.eatChar()
		if err := d.parseIdent("rue"); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitBool(visitor, true)
	case peek == 'f':
		d.eatChar()
		if err := d.parseIdent("alse"); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitBool(visitor, false)
	case peek == '-':
		d.eatChar()
		n, parseErr := d.parseAnyNumber(false)
		if parseErr != nil {
			return nil, parseErr
		}
		value, err = n.visit(visitor)
	case peek >= '0' && peek <= '9':
		n, parseErr := d.parseAnyNumber(true)
		if parseErr != nil {
			return nil, parseErr
		}
		value, err = n.visit(visitor)
	case peek == '"':
		d.eatChar()
		d.scratch = d.scratch[:0]
		s, borrowed, parseErr := d.read.parseStr(&d.scratch)
		if parseErr != nil {
			return nil, parseErr
		}
		if borrowed {
			value, err = serde.DeVisitorVisitBorrowedStr(visitor, s)
		} else {
			value, err = serde.DeVisitorVisitStr(visitor, s)
		}
	case peek == '[':
		value, err = d.visitSeqHere(visitor)
	case peek == '{':
		value, err = d.visitMapHere(visitor)
	default:
		err = d.peekError(errorCodeExpectedSomeValue)
	}
	// The de::Error impl creates errors with unknown line and column. Fill
	// in the position here by looking at the current index in the input.
	// There is no way to tell whether this should call `error` or
	// `peek_error` so pick the one that seems correct more often. Worst
	// case, the position is off by one character.
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeBool(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case 't':
		d.eatChar()
		if err := d.parseIdent("rue"); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitBool(visitor, true)
	case 'f':
		d.eatChar()
		if err := d.parseIdent("alse"); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitBool(visitor, false)
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeI8(v serde.DeVisitor) (any, error)  { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeI16(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeI32(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeI64(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeU8(v serde.DeVisitor) (any, error)  { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeU16(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeU32(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeU64(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }
func (d *Deserializer) DeserializeF32(v serde.DeVisitor) (any, error) { return d.doDeserializeF32(v) }
func (d *Deserializer) DeserializeF64(v serde.DeVisitor) (any, error) { return d.deserializeNumber(v) }

func (d *Deserializer) DeserializeChar(visitor serde.DeVisitor) (any, error) {
	return d.DeserializeStr(visitor)
}

func (d *Deserializer) DeserializeStr(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case '"':
		d.eatChar()
		d.scratch = d.scratch[:0]
		s, borrowed, parseErr := d.read.parseStr(&d.scratch)
		if parseErr != nil {
			return nil, parseErr
		}
		if borrowed {
			value, err = serde.DeVisitorVisitBorrowedStr(visitor, s)
		} else {
			value, err = serde.DeVisitorVisitStr(visitor, s)
		}
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeString(visitor serde.DeVisitor) (any, error) {
	return d.DeserializeStr(visitor)
}

func (d *Deserializer) DeserializeBytes(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case '"':
		d.eatChar()
		d.scratch = d.scratch[:0]
		b, borrowed, parseErr := d.read.parseStrRaw(&d.scratch)
		if parseErr != nil {
			return nil, parseErr
		}
		if borrowed {
			value, err = serde.DeVisitorVisitBorrowedBytes(visitor, b)
		} else {
			value, err = serde.DeVisitorVisitBytes(visitor, b)
		}
	case '[':
		value, err = d.DeserializeSeq(visitor)
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeByteBuf(visitor serde.DeVisitor) (any, error) {
	return d.DeserializeBytes(visitor)
}

func (d *Deserializer) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	c, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if ok && c == 'n' {
		d.eatChar()
		if err := d.parseIdent("ull"); err != nil {
			return nil, err
		}
		return leaveAsError(serde.DeVisitorVisitNone(visitor))
	}
	return leaveAsError(serde.DeVisitorVisitSome(visitor, d))
}

func (d *Deserializer) DeserializeUnit(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case 'n':
		d.eatChar()
		if err := d.parseIdent("ull"); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitUnit(visitor)
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeUnitStruct(_ string, visitor serde.DeVisitor) (any, error) {
	return d.DeserializeUnit(visitor)
}

func (d *Deserializer) DeserializeNewtypeStruct(name string, visitor serde.DeVisitor) (any, error) {
	if name == rawValueToken {
		return leaveAsError(d.deserializeRawValue(visitor))
	}
	return leaveAsError(serde.DeVisitorVisitNewtypeStruct(visitor, d))
}

func (d *Deserializer) DeserializeSeq(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case '[':
		value, err = d.visitSeqHere(visitor)
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeTuple(_ int, visitor serde.DeVisitor) (any, error) {
	return d.DeserializeSeq(visitor)
}

func (d *Deserializer) DeserializeTupleStruct(_ string, _ int, visitor serde.DeVisitor) (any, error) {
	return d.DeserializeSeq(visitor)
}

func (d *Deserializer) DeserializeMap(visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case '{':
		value, err = d.visitMapHere(visitor)
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeStruct(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	peek, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case '[':
		value, err = d.visitSeqHere(visitor)
	case '{':
		value, err = d.visitMapHere(visitor)
	default:
		err = d.peekInvalidType(visitor)
	}
	return d.finish(value, err)
}

func (d *Deserializer) DeserializeEnum(_ string, _ []string, visitor serde.DeVisitor) (any, error) {
	c, ok, err := d.parseWhitespace()
	if err != nil {
		return nil, err
	}
	switch {
	case !ok:
		return nil, d.peekError(errorCodeEofWhileParsingValue)
	case c == '{':
		if err := d.enter(); err != nil {
			return nil, err
		}
		value, err := serde.DeVisitorVisitEnum(visitor, &variantAccess{de: d})
		d.leave()
		if err != nil {
			return nil, toError(err)
		}
		c, ok, err := d.parseWhitespace()
		if err != nil {
			return nil, err
		}
		switch {
		case !ok:
			return nil, d.error(errorCodeEofWhileParsingObject)
		case c == '}':
			d.eatChar()
			return value, nil
		}
		return nil, d.error(errorCodeExpectedSomeValue)
	case c == '"':
		return leaveAsError(serde.DeVisitorVisitEnum(visitor, &unitVariantAccess{de: d}))
	}
	return nil, d.peekError(errorCodeExpectedSomeValue)
}

func (d *Deserializer) DeserializeIdentifier(visitor serde.DeVisitor) (any, error) {
	return d.DeserializeStr(visitor)
}

func (d *Deserializer) DeserializeIgnoredAny(visitor serde.DeVisitor) (any, error) {
	if err := d.ignoreValue(); err != nil {
		return nil, err
	}
	return leaveAsError(serde.DeVisitorVisitUnit(visitor))
}

func (d *Deserializer) IsHumanReadable() bool { return true }

// seqAccess is the `SeqAccess` of an array.
type seqAccess struct {
	de    *Deserializer
	first bool
}

func (s *seqAccess) hasNextElement() (bool, error) {
	peek, ok, err := s.de.parseWhitespace()
	if err != nil {
		return false, err
	}
	if !ok {
		return false, s.de.peekError(errorCodeEofWhileParsingList)
	}
	switch {
	case peek == ']':
		return false, nil
	case s.first:
		s.first = false
		return true, nil
	case peek == ',':
		s.de.eatChar()
		c, ok, err := s.de.parseWhitespace()
		if err != nil {
			return false, err
		}
		switch {
		case !ok:
			return false, s.de.peekError(errorCodeEofWhileParsingValue)
		case c == ']':
			return false, s.de.peekError(errorCodeTrailingComma)
		}
		return true, nil
	}
	return false, s.de.peekError(errorCodeExpectedListCommaOrEnd)
}

func (s *seqAccess) NextElementSeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	more, err := s.hasNextElement()
	if err != nil || !more {
		return nil, false, err
	}
	value, err := seed.DeserializeSeed(s.de)
	if err != nil {
		return nil, false, toError(err)
	}
	return value, true, nil
}

func (s *seqAccess) SizeHint() rs.Option[int] { return rs.None[int]() }

// mapAccess is the `MapAccess` of an object.
type mapAccess struct {
	de    *Deserializer
	first bool
}

func (m *mapAccess) hasNextKey() (bool, error) {
	peek, ok, err := m.de.parseWhitespace()
	if err != nil {
		return false, err
	}
	if !ok {
		return false, m.de.peekError(errorCodeEofWhileParsingObject)
	}
	switch {
	case peek == '}':
		return false, nil
	case m.first:
		m.first = false
		if peek == '"' {
			return true, nil
		}
		return false, m.de.peekError(errorCodeKeyMustBeAString)
	case peek == ',':
		m.de.eatChar()
		c, ok, err := m.de.parseWhitespace()
		if err != nil {
			return false, err
		}
		switch {
		case !ok:
			return false, m.de.peekError(errorCodeEofWhileParsingValue)
		case c == '"':
			return true, nil
		case c == '}':
			return false, m.de.peekError(errorCodeTrailingComma)
		}
		return false, m.de.peekError(errorCodeKeyMustBeAString)
	}
	return false, m.de.peekError(errorCodeExpectedObjectCommaOrEnd)
}

func (m *mapAccess) NextKeySeed(seed serde.DeDeserializeSeed) (any, bool, error) {
	more, err := m.hasNextKey()
	if err != nil || !more {
		return nil, false, err
	}
	key, err := seed.DeserializeSeed(&mapKey{de: m.de})
	if err != nil {
		return nil, false, toError(err)
	}
	return key, true, nil
}

func (m *mapAccess) NextValueSeed(seed serde.DeDeserializeSeed) (any, error) {
	if err := m.de.parseObjectColon(); err != nil {
		return nil, err
	}
	return leaveAsError(seed.DeserializeSeed(m.de))
}

func (m *mapAccess) NextEntrySeed(keySeed serde.DeDeserializeSeed, valueSeed serde.DeDeserializeSeed) (any, any, bool, error) {
	return serde.DeMapAccessDefaultNextEntrySeed(m, keySeed, valueSeed)
}

func (m *mapAccess) SizeHint() rs.Option[int] { return rs.None[int]() }

// variantAccess is the `VariantAccess` of an enum written as an object.
type variantAccess struct{ de *Deserializer }

func (v *variantAccess) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	c, ok, err := v.de.parseWhitespace()
	if err != nil {
		return nil, nil, err
	}
	switch {
	case !ok:
		return nil, nil, v.de.peekError(errorCodeEofWhileParsingObject)
	case c == '}':
		return nil, nil, v.de.peekError(errorCodeExpectedSomeValue)
	case c != '"':
		return nil, nil, v.de.peekError(errorCodeKeyMustBeAString)
	}
	value, err := seed.DeserializeSeed(&mapKey{de: v.de})
	if err != nil {
		return nil, nil, v.de.fixPosition(err)
	}
	if err := v.de.parseObjectColon(); err != nil {
		return nil, nil, err
	}
	return value, v, nil
}

func (v *variantAccess) UnitVariant() error {
	_, err := serde.Deserialize[rs.Unit](v.de)
	if err != nil {
		return toError(err)
	}
	return nil
}

func (v *variantAccess) NewtypeVariantSeed(seed serde.DeDeserializeSeed) (any, error) {
	return leaveAsError(seed.DeserializeSeed(v.de))
}

func (v *variantAccess) TupleVariant(_ int, visitor serde.DeVisitor) (any, error) {
	return v.de.DeserializeSeq(visitor)
}

func (v *variantAccess) StructVariant(fields []string, visitor serde.DeVisitor) (any, error) {
	return v.de.DeserializeStruct("", fields, visitor)
}

// unitVariantAccess is the `VariantAccess` of an enum written as a string.
type unitVariantAccess struct{ de *Deserializer }

func (v *unitVariantAccess) VariantSeed(seed serde.DeDeserializeSeed) (any, serde.DeVariantAccess, error) {
	variant, err := seed.DeserializeSeed(v.de)
	if err != nil {
		return nil, nil, toError(err)
	}
	return variant, v, nil
}

func (v *unitVariantAccess) UnitVariant() error { return nil }

func (v *unitVariantAccess) NewtypeVariantSeed(serde.DeDeserializeSeed) (any, error) {
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("newtype variant")))
}

func (v *unitVariantAccess) TupleVariant(int, serde.DeVisitor) (any, error) {
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("tuple variant")))
}

func (v *unitVariantAccess) StructVariant([]string, serde.DeVisitor) (any, error) {
	return nil, toError(serde.DeErrorInvalidType(&serde.DeUnexpectedUnitVariant{}, serde.DeExpectedStr("struct variant")))
}

// mapKey is `MapKey`: only strings are allowed as JSON object keys, so a
// key is read from the string after the opening quote.
type mapKey struct{ de *Deserializer }

// numericKey is `deserialize_numeric_key!`: a number in quotes.
func (k *mapKey) numericKey(delegate func(serde.DeVisitor) (any, error), visitor serde.DeVisitor) (any, error) {
	k.de.eatChar()

	c, ok, err := k.de.peek()
	if err != nil {
		return nil, err
	}
	if !ok || !(c >= '0' && c <= '9' || c == '-') {
		return nil, k.de.error(errorCodeExpectedNumericKey)
	}

	value, err := delegate(visitor)
	if err != nil {
		return nil, err
	}

	c, ok, err = k.de.peek()
	if err != nil {
		return nil, err
	}
	if !ok || c != '"' {
		return nil, k.de.peekError(errorCodeExpectedDoubleQuote)
	}
	k.de.eatChar()
	return value, nil
}

func (k *mapKey) DeserializeAny(visitor serde.DeVisitor) (any, error) {
	k.de.eatChar()
	k.de.scratch = k.de.scratch[:0]
	s, borrowed, err := k.de.read.parseStr(&k.de.scratch)
	if err != nil {
		return nil, err
	}
	if borrowed {
		return leaveAsError(serde.DeVisitorVisitBorrowedStr(visitor, s))
	}
	return leaveAsError(serde.DeVisitorVisitStr(visitor, s))
}

func (k *mapKey) DeserializeI8(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeI16(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeI32(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeI64(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeU8(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeU16(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeU32(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeU64(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}
func (k *mapKey) DeserializeF32(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.DeserializeF32, v)
}
func (k *mapKey) DeserializeF64(v serde.DeVisitor) (any, error) {
	return k.numericKey(k.de.deserializeNumber, v)
}

func (k *mapKey) DeserializeBool(visitor serde.DeVisitor) (any, error) {
	k.de.eatChar()

	peek, ok, err := k.de.nextChar()
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, k.de.peekError(errorCodeEofWhileParsingValue)
	}

	var value any
	switch peek {
	case 't':
		if err := k.de.parseIdent("rue\""); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitBool(visitor, true)
	case 'f':
		if err := k.de.parseIdent("alse\""); err != nil {
			return nil, err
		}
		value, err = serde.DeVisitorVisitBool(visitor, false)
	default:
		k.de.scratch = k.de.scratch[:0]
		s, _, parseErr := k.de.read.parseStr(&k.de.scratch)
		if parseErr != nil {
			return nil, parseErr
		}
		err = serde.DeErrorInvalidType(&serde.DeUnexpectedStr{F0: s}, visitor)
	}
	return k.de.finish(value, err)
}

func (k *mapKey) DeserializeOption(visitor serde.DeVisitor) (any, error) {
	// Map keys cannot be null.
	return leaveAsError(serde.DeVisitorVisitSome(visitor, k))
}

func (k *mapKey) DeserializeNewtypeStruct(name string, visitor serde.DeVisitor) (any, error) {
	if name == rawValueToken {
		return leaveAsError(k.de.deserializeRawValue(visitor))
	}
	return leaveAsError(serde.DeVisitorVisitNewtypeStruct(visitor, k))
}

func (k *mapKey) DeserializeEnum(name string, variants []string, visitor serde.DeVisitor) (any, error) {
	return k.de.DeserializeEnum(name, variants, visitor)
}

func (k *mapKey) DeserializeBytes(visitor serde.DeVisitor) (any, error) {
	return k.de.DeserializeBytes(visitor)
}

func (k *mapKey) DeserializeByteBuf(visitor serde.DeVisitor) (any, error) {
	return k.de.DeserializeBytes(visitor)
}

func (k *mapKey) DeserializeChar(v serde.DeVisitor) (any, error)   { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeStr(v serde.DeVisitor) (any, error)    { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeString(v serde.DeVisitor) (any, error) { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeUnit(v serde.DeVisitor) (any, error)   { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeUnitStruct(_ string, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k *mapKey) DeserializeSeq(v serde.DeVisitor) (any, error) { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeTuple(_ int, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k *mapKey) DeserializeTupleStruct(_ string, _ int, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k *mapKey) DeserializeMap(v serde.DeVisitor) (any, error) { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeStruct(_ string, _ []string, v serde.DeVisitor) (any, error) {
	return k.DeserializeAny(v)
}
func (k *mapKey) DeserializeIdentifier(v serde.DeVisitor) (any, error) { return k.DeserializeAny(v) }
func (k *mapKey) DeserializeIgnoredAny(v serde.DeVisitor) (any, error) { return k.DeserializeAny(v) }
func (k *mapKey) IsHumanReadable() bool                                { return true }

// StreamDeserializer is `serde_json::StreamDeserializer`: an iterator that
// deserializes a stream into multiple JSON values. A stream deserializer
// can be created from any JSON deserializer using the
// `Deserializer::into_iter` method.
//
// The data can consist of any JSON value. Values need to be a
// self-delineating value e.g. arrays, objects, or strings, or be followed
// by whitespace or a self-delineating value.
type StreamDeserializer[T any] struct {
	de     *Deserializer
	offset int
	failed bool
}

// DeserializerIntoIter is `Deserializer::into_iter::<T>`: turn a JSON
// deserializer into an iterator over values of type T.
func DeserializerIntoIter[T any](d *Deserializer) *StreamDeserializer[T] {
	return &StreamDeserializer[T]{de: d, offset: d.read.byteOffset()}
}

// ByteOffset is `StreamDeserializer::byte_offset`: the number of bytes so
// far deserialized into a successful `T`.
func (s *StreamDeserializer[T]) ByteOffset() int { return s.offset }

func (s *StreamDeserializer[T]) peekEndOfValue() error {
	c, ok, err := s.de.peek()
	if err != nil {
		return err
	}
	if !ok {
		return nil
	}
	switch c {
	case ' ', '\n', '\t', '\r', '"', '[', ']', '{', '}', ',', ':':
		return nil
	}
	p := s.de.read.peekPosition()
	return errorSyntax(errorCodeTrailingCharacters, p.line, p.column)
}

// Next is `Iterator::next`: the next value, None at the end of the stream.
func (s *StreamDeserializer[T]) Next() rs.Option[rs.Result[T]] {
	if s.de.read.shouldEarlyReturnIfFailed() && s.failed {
		return rs.None[rs.Result[T]]()
	}

	// skip whitespaces, if any
	// this helps with trailing whitespaces, since whitespaces between
	// values are handled for us.
	b, ok, err := s.de.parseWhitespace()
	if err != nil {
		s.de.read.setFailed(&s.failed)
		return rs.Some(rs.Err[T](err))
	}
	if !ok {
		s.offset = s.de.read.byteOffset()
		return rs.None[rs.Result[T]]()
	}
	// If the value does not have a clear way to show the end of the value
	// (like numbers, null, true etc.) we have to look for whitespace or the
	// beginning of a self-delineated value.
	selfDelineatedValue := b == '[' || b == '"' || b == '{'
	s.offset = s.de.read.byteOffset()
	value, err := serde.Deserialize[T](s.de)
	if err != nil {
		s.de.read.setFailed(&s.failed)
		return rs.Some(rs.Err[T](toError(err)))
	}
	s.offset = s.de.read.byteOffset()
	if selfDelineatedValue {
		return rs.Some(rs.Ok(value))
	}
	if err := s.peekEndOfValue(); err != nil {
		return rs.Some(rs.Err[T](err))
	}
	return rs.Some(rs.Ok(value))
}

// fromTrait is `from_trait`.
func fromTrait[T any](d *Deserializer) (T, error) {
	value, err := serde.Deserialize[T](d)
	if err != nil {
		var zero T
		return zero, toError(err)
	}

	// Make sure the whole stream has been consumed.
	if err := d.End(); err != nil {
		var zero T
		return zero, err
	}
	return value, nil
}

// FromReader is `serde_json::from_reader`: deserialize an instance of type
// T from an I/O stream of JSON.
func FromReader[T any](reader io.Reader) (T, error) {
	return fromTrait[T](DeserializerFromReader(reader))
}

// FromSlice is `serde_json::from_slice`: deserialize an instance of type T
// from bytes of JSON text.
func FromSlice[T any](v []byte) (T, error) {
	return fromTrait[T](DeserializerFromSlice(v))
}

// FromStr is `serde_json::from_str`: deserialize an instance of type T from
// a string of JSON text.
func FromStr[T any](s string) (T, error) {
	return fromTrait[T](DeserializerFromStr(s))
}

var (
	_ serde.Deserializer = (*Deserializer)(nil)
	_ serde.Deserializer = (*mapKey)(nil)
	_                    = strings.Builder{}
)
