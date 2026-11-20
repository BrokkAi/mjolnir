// Port of serde_json 1.0.150 src/read.rs and src/iter.rs

package serdejson

import (
	"bytes"
	"errors"
	"io"
	"unicode/utf8"
)

// position is `Position`.
type position struct {
	line   int
	column int
}

// reader is the `Read` trait and its three implementations in one: a
// `SliceRead` over bytes, a `StrRead` over a string (which is valid UTF-8,
// so its strings are not checked again), or an `IoRead` over a stream when
// stream is set.
type reader struct {
	// SliceRead and StrRead.
	slice []byte
	index int
	// rawBufferingStartIndex is where begin_raw_buffering was called.
	rawBufferingStartIndex int
	// isStr marks a StrRead.
	isStr bool

	// IoRead.
	stream *ioRead
}

// ioRead is the state of an `IoRead`: a `LineColIterator` over the bytes of
// the stream, the byte peek() looked at, and the buffer of raw_value.
type ioRead struct {
	source io.Reader
	buf    []byte
	start  int
	end    int
	// LineColIterator
	line        int
	col         int
	startOfLine int
	// ch is the byte peek() read and next() has not taken.
	ch    byte
	hasCh bool
	// rawBuffer holds what was read since begin_raw_buffering.
	rawBuffer    []byte
	rawBuffering bool
}

func newSliceRead(slice []byte) *reader { return &reader{slice: slice} }

func newStrRead(s string) *reader { return &reader{slice: []byte(s), isStr: true} }

func newIoRead(source io.Reader) *reader {
	return &reader{stream: &ioRead{source: source, buf: make([]byte, 4096), line: 1}}
}

// iterNext is LineColIterator::next over `io::Bytes`: the next byte of the
// stream, false at its end.
func (r *ioRead) iterNext() (byte, bool, error) {
	for r.start == r.end {
		n, err := r.source.Read(r.buf)
		if n > 0 {
			r.start, r.end = 0, n
			break
		}
		if errors.Is(err, io.EOF) {
			return 0, false, nil
		}
		if err != nil {
			return 0, false, err
		}
	}
	c := r.buf[r.start]
	r.start++
	if c == '\n' {
		r.startOfLine += r.col + 1
		r.line++
		r.col = 0
	} else {
		r.col++
	}
	return c, true, nil
}

func (r *reader) next() (byte, bool, error) {
	if s := r.stream; s != nil {
		if s.hasCh {
			s.hasCh = false
			if s.rawBuffering {
				s.rawBuffer = append(s.rawBuffer, s.ch)
			}
			return s.ch, true, nil
		}
		c, ok, err := s.iterNext()
		if err != nil {
			return 0, false, errorIo(err)
		}
		if ok && s.rawBuffering {
			s.rawBuffer = append(s.rawBuffer, c)
		}
		return c, ok, nil
	}
	if r.index < len(r.slice) {
		c := r.slice[r.index]
		r.index++
		return c, true, nil
	}
	return 0, false, nil
}

func (r *reader) peek() (byte, bool, error) {
	if s := r.stream; s != nil {
		if s.hasCh {
			return s.ch, true, nil
		}
		c, ok, err := s.iterNext()
		if err != nil {
			return 0, false, errorIo(err)
		}
		if ok {
			s.ch, s.hasCh = c, true
		}
		return c, ok, nil
	}
	if r.index < len(r.slice) {
		return r.slice[r.index], true, nil
	}
	return 0, false, nil
}

func (r *reader) discard() {
	if s := r.stream; s != nil {
		if s.hasCh {
			s.hasCh = false
			if s.rawBuffering {
				s.rawBuffer = append(s.rawBuffer, s.ch)
			}
		}
		return
	}
	r.index++
}

func (r *reader) positionOfIndex(i int) position {
	startOfLine := bytes.LastIndexByte(r.slice[:i], '\n') + 1
	return position{line: 1 + bytes.Count(r.slice[:startOfLine], []byte{'\n'}), column: i - startOfLine}
}

func (r *reader) position() position {
	if s := r.stream; s != nil {
		return position{line: s.line, column: s.col}
	}
	return r.positionOfIndex(r.index)
}

func (r *reader) peekPosition() position {
	if r.stream != nil {
		// The LineColIterator updates its position during peek() so it
		// has the right one here.
		return r.position()
	}
	// Cap it at slice.len() just in case the most recent call was next()
	// and it returned the last byte.
	return r.positionOfIndex(min(len(r.slice), r.index+1))
}

func (r *reader) byteOffset() int {
	if s := r.stream; s != nil {
		if s.hasCh {
			return s.startOfLine + s.col - 1
		}
		return s.startOfLine + s.col
	}
	return r.index
}

func isEscape(ch byte, includingControlCharacters bool) bool {
	return ch == '"' || ch == '\\' || (includingControlCharacters && ch < 0x20)
}

// errorAt is `error(read, reason)`: an error at the reader's position.
func (r *reader) errorAt(code errorCodeKind) *Error {
	p := r.position()
	return errorSyntax(code, p.line, p.column)
}

func (r *reader) nextOrEOF() (byte, error) {
	c, ok, err := r.next()
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, r.errorAt(errorCodeEofWhileParsingString)
	}
	return c, nil
}

func (r *reader) peekOrEOF() (byte, error) {
	c, ok, err := r.peek()
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, r.errorAt(errorCodeEofWhileParsingString)
	}
	return c, nil
}

// skipToEscape is `SliceRead::skip_to_escape`.
func (r *reader) skipToEscape(forbidControlCharacters bool) {
	for r.index < len(r.slice) && !isEscape(r.slice[r.index], forbidControlCharacters) {
		r.index++
	}
}

// parseStrBytes reads the rest of a string whose opening quote was
// consumed. validate is false for parse_str_raw, which keeps control
// characters and lone surrogates. borrowed says the result is a part of the
// input (Rust's Reference::Borrowed) rather than built in scratch.
func (r *reader) parseStrBytes(scratch *[]byte, validate bool) ([]byte, bool, error) {
	if s := r.stream; s != nil {
		for {
			ch, err := r.nextOrEOF()
			if err != nil {
				return nil, false, err
			}
			if !isEscape(ch, true) {
				*scratch = append(*scratch, ch)
				continue
			}
			switch ch {
			case '"':
				return *scratch, false, nil
			case '\\':
				if err := r.parseEscape(validate, scratch); err != nil {
					return nil, false, err
				}
			default:
				if validate {
					return nil, false, r.errorAt(errorCodeControlCharacterWhileParsingString)
				}
				*scratch = append(*scratch, ch)
			}
		}
	}
	// Index of the first byte not yet copied into the scratch space.
	start := r.index
	for {
		r.skipToEscape(validate)
		if r.index == len(r.slice) {
			return nil, false, r.errorAt(errorCodeEofWhileParsingString)
		}
		switch r.slice[r.index] {
		case '"':
			if len(*scratch) == 0 {
				// Fast path: return a slice of the raw JSON without any
				// copying.
				borrowed := r.slice[start:r.index]
				r.index++
				return borrowed, true, nil
			}
			*scratch = append(*scratch, r.slice[start:r.index]...)
			r.index++
			return *scratch, false, nil
		case '\\':
			*scratch = append(*scratch, r.slice[start:r.index]...)
			r.index++
			if err := r.parseEscape(validate, scratch); err != nil {
				return nil, false, err
			}
			start = r.index
		default:
			r.index++
			return nil, false, r.errorAt(errorCodeControlCharacterWhileParsingString)
		}
	}
}

// parseStr is `parse_str`: the string, and whether it was borrowed.
func (r *reader) parseStr(scratch *[]byte) (string, bool, error) {
	bytes, borrowed, err := r.parseStrBytes(scratch, true)
	if err != nil {
		return "", false, err
	}
	// The input of a StrRead came in as a str with a UTF-8 guarantee, and
	// the \u-escapes are checked along the way, so it is not checked here.
	if !r.isStr && !utf8.Valid(bytes) {
		return "", false, r.errorAt(errorCodeInvalidUnicodeCodePoint)
	}
	return string(bytes), borrowed, nil
}

// parseStrRaw is `parse_str_raw`.
func (r *reader) parseStrRaw(scratch *[]byte) ([]byte, bool, error) {
	return r.parseStrBytes(scratch, false)
}

// ignoreStr is `ignore_str`.
func (r *reader) ignoreStr() error {
	if r.stream != nil {
		for {
			ch, err := r.nextOrEOF()
			if err != nil {
				return err
			}
			if !isEscape(ch, true) {
				continue
			}
			switch ch {
			case '"':
				return nil
			case '\\':
				if err := r.ignoreEscape(); err != nil {
					return err
				}
			default:
				return r.errorAt(errorCodeControlCharacterWhileParsingString)
			}
		}
	}
	for {
		r.skipToEscape(true)
		if r.index == len(r.slice) {
			return r.errorAt(errorCodeEofWhileParsingString)
		}
		switch r.slice[r.index] {
		case '"':
			r.index++
			return nil
		case '\\':
			r.index++
			if err := r.ignoreEscape(); err != nil {
				return err
			}
		default:
			return r.errorAt(errorCodeControlCharacterWhileParsingString)
		}
	}
}

// decodeHexEscape is `decode_hex_escape`.
func (r *reader) decodeHexEscape() (uint16, error) {
	if r.stream != nil {
		var digits [4]byte
		for i := range digits {
			c, err := r.nextOrEOF()
			if err != nil {
				return 0, err
			}
			digits[i] = c
		}
		value, ok := decodeFourHexDigits(digits[0], digits[1], digits[2], digits[3])
		if !ok {
			return 0, r.errorAt(errorCodeInvalidEscape)
		}
		return value, nil
	}
	if len(r.slice)-r.index >= 4 {
		a, b, c, d := r.slice[r.index], r.slice[r.index+1], r.slice[r.index+2], r.slice[r.index+3]
		r.index += 4
		value, ok := decodeFourHexDigits(a, b, c, d)
		if !ok {
			return 0, r.errorAt(errorCodeInvalidEscape)
		}
		return value, nil
	}
	r.index = len(r.slice)
	return 0, r.errorAt(errorCodeEofWhileParsingString)
}

func (r *reader) beginRawBuffering() {
	if s := r.stream; s != nil {
		s.rawBuffer, s.rawBuffering = []byte{}, true
		return
	}
	r.rawBufferingStartIndex = r.index
}

// endRawBuffering returns the text read since begin_raw_buffering.
func (r *reader) endRawBuffering() (string, error) {
	if s := r.stream; s != nil {
		raw := s.rawBuffer
		s.rawBuffer, s.rawBuffering = nil, false
		if !utf8.Valid(raw) {
			return "", r.errorAt(errorCodeInvalidUnicodeCodePoint)
		}
		return string(raw), nil
	}
	raw := r.slice[r.rawBufferingStartIndex:r.index]
	if !r.isStr && !utf8.Valid(raw) {
		return "", r.errorAt(errorCodeInvalidUnicodeCodePoint)
	}
	return string(raw), nil
}

// shouldEarlyReturnIfFailed is `should_early_return_if_failed`.
func (r *reader) shouldEarlyReturnIfFailed() bool { return r.stream != nil }

// setFailed is `set_failed`.
func (r *reader) setFailed(failed *bool) {
	if r.stream != nil {
		*failed = true
		return
	}
	r.slice = r.slice[:r.index]
}

// parseEscape is `parse_escape`: the escape after a backslash.
func (r *reader) parseEscape(validate bool, scratch *[]byte) error {
	ch, err := r.nextOrEOF()
	if err != nil {
		return err
	}
	switch ch {
	case '"':
		*scratch = append(*scratch, '"')
	case '\\':
		*scratch = append(*scratch, '\\')
	case '/':
		*scratch = append(*scratch, '/')
	case 'b':
		*scratch = append(*scratch, '\b')
	case 'f':
		*scratch = append(*scratch, '\f')
	case 'n':
		*scratch = append(*scratch, '\n')
	case 'r':
		*scratch = append(*scratch, '\r')
	case 't':
		*scratch = append(*scratch, '\t')
	case 'u':
		return r.parseUnicodeEscape(validate, scratch)
	default:
		return r.errorAt(errorCodeInvalidEscape)
	}
	return nil
}

// parseUnicodeEscape is `parse_unicode_escape`.
func (r *reader) parseUnicodeEscape(validate bool, scratch *[]byte) error {
	n, err := r.decodeHexEscape()
	if err != nil {
		return err
	}

	// Non-BMP characters are encoded as a sequence of two hex escapes,
	// representing UTF-16 surrogates. If deserializing a utf-8 string the
	// surrogates are required to be paired, whereas deserializing a byte
	// string accepts lone surrogates.
	if validate && n >= 0xDC00 && n <= 0xDFFF {
		// XXX: This is actually a trailing surrogate.
		return r.errorAt(errorCodeLoneLeadingSurrogateInHexEscape)
	}

	for {
		if n < 0xD800 || n > 0xDBFF {
			// Every u16 outside of the surrogate ranges is guaranteed to be
			// a legal char.
			pushWtf8Codepoint(uint32(n), scratch)
			return nil
		}

		// n is a leading surrogate, we now expect a trailing surrogate.
		n1 := n

		c, err := r.peekOrEOF()
		if err != nil {
			return err
		}
		if c == '\\' {
			r.discard()
		} else {
			if validate {
				r.discard()
				return r.errorAt(errorCodeUnexpectedEndOfHexEscape)
			}
			pushWtf8Codepoint(uint32(n1), scratch)
			return nil
		}

		c, err = r.peekOrEOF()
		if err != nil {
			return err
		}
		if c == 'u' {
			r.discard()
		} else {
			if validate {
				r.discard()
				return r.errorAt(errorCodeUnexpectedEndOfHexEscape)
			}
			pushWtf8Codepoint(uint32(n1), scratch)
			// The \ prior to this byte started an escape sequence, so we
			// need to parse that now. This recursive call does not blow the
			// stack on malicious input because the escape is not \u, so it
			// will be handled by one of the easy nonrecursive cases.
			return r.parseEscape(validate, scratch)
		}

		n2, err := r.decodeHexEscape()
		if err != nil {
			return err
		}

		if n2 < 0xDC00 || n2 > 0xDFFF {
			if validate {
				return r.errorAt(errorCodeLoneLeadingSurrogateInHexEscape)
			}
			pushWtf8Codepoint(uint32(n1), scratch)
			// If n2 is a leading surrogate, we need to restart.
			n = n2
			continue
		}

		// This value is in range U+10000..=U+10FFFF, which is always a valid
		// codepoint.
		code := (uint32(n1-0xD800)<<10 | uint32(n2-0xDC00)) + 0x1_0000
		pushWtf8Codepoint(code, scratch)
		return nil
	}
}

// pushWtf8Codepoint is `push_wtf8_codepoint`: n encoded as UTF-8, a lone
// surrogate included.
func pushWtf8Codepoint(n uint32, scratch *[]byte) {
	switch {
	case n < 0x80:
		*scratch = append(*scratch, byte(n))
	case n < 0x800:
		*scratch = append(*scratch, byte(n>>6&0b0001_1111|0b1100_0000), byte(n&0b0011_1111|0b1000_0000))
	case n < 0x1_0000:
		*scratch = append(*scratch, byte(n>>12&0b0000_1111|0b1110_0000), byte(n>>6&0b0011_1111|0b1000_0000), byte(n&0b0011_1111|0b1000_0000))
	default:
		*scratch = append(*scratch, byte(n>>18&0b0000_0111|0b1111_0000), byte(n>>12&0b0011_1111|0b1000_0000), byte(n>>6&0b0011_1111|0b1000_0000), byte(n&0b0011_1111|0b1000_0000))
	}
}

// ignoreEscape is `ignore_escape`.
func (r *reader) ignoreEscape() error {
	ch, err := r.nextOrEOF()
	if err != nil {
		return err
	}
	switch ch {
	case '"', '\\', '/', 'b', 'f', 'n', 'r', 't':
	case 'u':
		// At this point we don't care if the codepoint is valid. We just
		// want to consume it. We don't actually know what is valid or not
		// at this point, because that depends on if this string will
		// ultimately be parsed into a string or a byte buffer in the "real"
		// parse.
		if _, err := r.decodeHexEscape(); err != nil {
			return err
		}
	default:
		return r.errorAt(errorCodeInvalidEscape)
	}
	return nil
}

func decodeHexVal(c byte) (uint16, bool) {
	switch {
	case c >= '0' && c <= '9':
		return uint16(c - '0'), true
	case c >= 'A' && c <= 'F':
		return uint16(c - 'A' + 10), true
	case c >= 'a' && c <= 'f':
		return uint16(c - 'a' + 10), true
	}
	return 0, false
}

func decodeFourHexDigits(a, b, c, d byte) (uint16, bool) {
	var value uint16
	for _, digit := range [4]byte{a, b, c, d} {
		v, ok := decodeHexVal(digit)
		if !ok {
			return 0, false
		}
		value = value<<4 | v
	}
	return value, true
}
