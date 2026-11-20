// Port of core 1.99.1 src/fmt/mod.rs

package rs

import (
	"unicode/utf8"
)

// Alignment is Rust's fmt::Alignment.
type Alignment uint8

const (
	// Alignment_Left is `<`.
	Alignment_Left Alignment = iota
	// Alignment_Right is `>`.
	Alignment_Right
	// Alignment_Center is `^`.
	Alignment_Center
)

var _ = RegisterEnum[Alignment]("Alignment",
	Const(Alignment_Left, "Left"),
	Const(Alignment_Right, "Right"),
	Const(Alignment_Center, "Center"),
)

// FmtError is Rust's fmt::Error: what a formatting method returns when it
// cannot write. It carries no detail; a formatting method that fails for a
// reason of its own returns FmtError{}, as Rust code returns `Err(fmt::Error)`.
//
// The formatter treats every non-nil error from a FmtDisplay or FmtDebug
// method as this error.
type FmtError struct{}

var _ = RegisterUnitStruct[FmtError]("Error")

// Error is the Display text of fmt::Error.
func (FmtError) Error() string { return "an error occurred when formatting an argument" }

// FmtDisplay is `impl Display for fmt::Error`.
func (e FmtError) FmtDisplay(f *Formatter) error { return f.Pad(e.Error()) }

// fmtWrite is Rust's fmt::Write: where formatted text goes.
type fmtWrite interface {
	writeStr(s string) error
	writeChar(c rune) error
}

// fmtString is the `impl fmt::Write for String`: it never fails.
type fmtString struct {
	buf []byte
}

func (w *fmtString) writeStr(s string) error {
	w.buf = append(w.buf, s...)
	return nil
}

func (w *fmtString) writeChar(c rune) error {
	w.buf = utf8.AppendRune(w.buf, c)
	return nil
}

// fmtFormattingOptions is fmt::FormattingOptions.
type fmtFormattingOptions struct {
	// The fill character in the lower 21 bits, the flags above it.
	flags     uint32
	width     uint16
	precision uint16
}

// This needs to match with compiler/rustc_ast_lowering/src/format.rs.
const (
	fmtFlagSignPlus         uint32 = 1 << 21
	fmtFlagSignMinus        uint32 = 1 << 22
	fmtFlagAlternate        uint32 = 1 << 23
	fmtFlagSignAwareZeroPad uint32 = 1 << 24
	fmtFlagDebugLowerHex    uint32 = 1 << 25
	fmtFlagDebugUpperHex    uint32 = 1 << 26
	fmtFlagWidth            uint32 = 1 << 27
	fmtFlagPrecision        uint32 = 1 << 28
	fmtFlagAlignBits        uint32 = 0b11 << 29
	fmtFlagAlignLeft        uint32 = 0 << 29
	fmtFlagAlignRight       uint32 = 1 << 29
	fmtFlagAlignCenter      uint32 = 2 << 29
	fmtFlagAlignUnknown     uint32 = 3 << 29
)

// fmtFormattingOptionsNew is FormattingOptions::new: no flags, a space as the
// fill character, no alignment.
func fmtFormattingOptionsNew() fmtFormattingOptions {
	return fmtFormattingOptions{flags: ' ' | fmtFlagAlignUnknown, width: 0, precision: 0}
}

func (o *fmtFormattingOptions) setSignAwareZeroPad(signAwareZeroPad bool) {
	if signAwareZeroPad {
		o.flags |= fmtFlagSignAwareZeroPad
	} else {
		o.flags &^= fmtFlagSignAwareZeroPad
	}
}

func (o *fmtFormattingOptions) setAlternate(alternate bool) {
	if alternate {
		o.flags |= fmtFlagAlternate
	} else {
		o.flags &^= fmtFlagAlternate
	}
}

func (o *fmtFormattingOptions) setFill(fill rune) {
	o.flags = o.flags&^0x1FFFFF | uint32(fill)
}

func (o *fmtFormattingOptions) setAlign(align Alignment) {
	var bits uint32
	switch align {
	case Alignment_Left:
		bits = fmtFlagAlignLeft
	case Alignment_Right:
		bits = fmtFlagAlignRight
	case Alignment_Center:
		bits = fmtFlagAlignCenter
	}
	o.flags = o.flags&^fmtFlagAlignBits | bits
}

func (o *fmtFormattingOptions) setWidth(width uint16) {
	o.flags |= fmtFlagWidth
	o.width = width
}

func (o *fmtFormattingOptions) setPrecision(precision uint16) {
	o.flags |= fmtFlagPrecision
	o.precision = precision
}

func (o *fmtFormattingOptions) getAlternate() bool {
	return o.flags&fmtFlagAlternate != 0
}

func (o *fmtFormattingOptions) getFill() rune {
	return rune(o.flags & 0x1FFFFF)
}

func (o *fmtFormattingOptions) getAlign() (Alignment, bool) {
	switch o.flags & fmtFlagAlignBits {
	case fmtFlagAlignLeft:
		return Alignment_Left, true
	case fmtFlagAlignRight:
		return Alignment_Right, true
	case fmtFlagAlignCenter:
		return Alignment_Center, true
	default:
		return 0, false
	}
}

func (o *fmtFormattingOptions) getWidth() (uint16, bool) {
	if o.flags&fmtFlagWidth != 0 {
		return o.width, true
	}
	return 0, false
}

func (o *fmtFormattingOptions) getPrecision() (uint16, bool) {
	if o.flags&fmtFlagPrecision != 0 {
		return o.precision, true
	}
	return 0, false
}

// Formatter is Rust's fmt::Formatter: where a FmtDisplay or FmtDebug method
// writes, and where it reads the flags of the `{}` it is filling in. Methods
// keep Rust's names.
type Formatter struct {
	options fmtFormattingOptions

	buf fmtWrite
}

// fmtFormatterNew is Formatter::new.
func fmtFormatterNew(write fmtWrite, options fmtFormattingOptions) *Formatter {
	return &Formatter{options: options, buf: write}
}

// fmtPostPadding is fmt::PostPadding: the padding still to be written after
// the content.
type fmtPostPadding struct {
	fill    rune
	padding uint16
}

func (p fmtPostPadding) write(f *Formatter) error {
	for i := uint16(0); i < p.padding; i++ {
		if err := f.buf.writeChar(p.fill); err != nil {
			return err
		}
	}
	return nil
}

// wrapBuf is Formatter::wrap_buf: a formatter with the same options writing
// somewhere else.
func (f *Formatter) wrapBuf(buf fmtWrite) *Formatter {
	return &Formatter{
		// We want to change this
		buf: buf,

		// And preserve these
		options: f.options,
	}
}

// Helper methods used for padding and processing formatting arguments that
// all formatting traits can use.

// PadIntegral is Formatter::pad_integral: it writes an integer whose digits
// are already rendered, adding the sign, the prefix when `#` is given, and
// the padding. digits must be ASCII.
func (f *Formatter) PadIntegral(isNonnegative bool, prefix string, digits string) error {
	width := len(digits)

	var sign rune
	hasSign := false
	if !isNonnegative {
		sign, hasSign = '-', true
		width++
	} else if f.SignPlus() {
		sign, hasSign = '+', true
		width++
	}

	hasPrefix := false
	if f.Alternate() {
		width += utf8.RuneCountInString(prefix)
		hasPrefix = true
	}

	// Writes the sign if it exists, and then the prefix if it was requested
	writePrefix := func() error {
		if hasSign {
			if err := f.buf.writeChar(sign); err != nil {
				return err
			}
		}
		if hasPrefix {
			return f.buf.writeStr(prefix)
		}
		return nil
	}

	// The `width` field is more of a `min-width` parameter at this point.
	min := int(f.options.width)
	if width >= min {
		// We're over the minimum width, so then we can just write the bytes.
		if err := writePrefix(); err != nil {
			return err
		}
		return f.buf.writeStr(digits)
	} else if f.SignAwareZeroPad() {
		// The sign and prefix goes before the padding if the fill character
		// is zero
		oldOptions := f.options
		f.options.setFill('0')
		f.options.setAlign(Alignment_Right)
		if err := writePrefix(); err != nil {
			return err
		}
		postPadding, err := f.padding(uint16(min-width), Alignment_Right)
		if err != nil {
			return err
		}
		if err := f.buf.writeStr(digits); err != nil {
			return err
		}
		if err := postPadding.write(f); err != nil {
			return err
		}
		f.options = oldOptions
		return nil
	} else {
		// Otherwise, the sign and prefix goes after the padding
		postPadding, err := f.padding(uint16(min-width), Alignment_Right)
		if err != nil {
			return err
		}
		if err := writePrefix(); err != nil {
			return err
		}
		if err := f.buf.writeStr(digits); err != nil {
			return err
		}
		return postPadding.write(f)
	}
}

// Pad is Formatter::pad: write s honouring width, fill, alignment and, as a
// string precision, truncation.
func (f *Formatter) Pad(s string) error {
	// Make sure there's a fast path up front.
	if f.options.flags&(fmtFlagWidth|fmtFlagPrecision) == 0 {
		return f.buf.writeStr(s)
	}

	// The `precision` field can be interpreted as a maximum width for the
	// string being formatted.
	var charCount int
	if maxCharCount, ok := f.options.getPrecision(); ok {
		offset := 0
		for charCount < int(maxCharCount) && offset < len(s) {
			_, size := utf8.DecodeRuneInString(s[offset:])
			offset += size
			charCount++
		}
		s = s[:offset]
	} else {
		// Use the optimized char counting algorithm for the full string.
		charCount = utf8.RuneCountInString(s)
	}

	// The `width` field is more of a minimum width parameter at this point.
	if charCount < int(f.options.width) {
		// If we're under the minimum width, then fill up the minimum width
		// with the specified string + some alignment.
		postPadding, err := f.padding(f.options.width-uint16(charCount), Alignment_Left)
		if err != nil {
			return err
		}
		if err := f.buf.writeStr(s); err != nil {
			return err
		}
		return postPadding.write(f)
	}
	// If we're over the minimum width or there is no minimum width, we
	// can just emit the string.
	return f.buf.writeStr(s)
}

// padding is Formatter::padding: it writes the padding that goes before the
// content and returns what must be written after it.
func (f *Formatter) padding(padding uint16, defaultAlign Alignment) (fmtPostPadding, error) {
	align, ok := f.options.getAlign()
	if !ok {
		align = defaultAlign
	}
	fill := f.options.getFill()

	var paddingLeft uint16
	switch align {
	case Alignment_Left:
		paddingLeft = 0
	case Alignment_Right:
		paddingLeft = padding
	case Alignment_Center:
		paddingLeft = padding / 2
	}

	for i := uint16(0); i < paddingLeft; i++ {
		if err := f.buf.writeChar(fill); err != nil {
			return fmtPostPadding{}, err
		}
	}

	return fmtPostPadding{fill: fill, padding: padding - paddingLeft}, nil
}

// padFormattedParts is Formatter::pad_formatted_parts: it writes a number
// rendered in parts, with the padding.
func (f *Formatter) padFormattedParts(formatted *fmtNumFormatted) error {
	if f.options.width == 0 {
		// this is the common case and we take a shortcut
		return f.writeFormattedParts(formatted)
	}
	// for the sign-aware zero padding, we render the sign first and
	// behave as if we had no sign from the beginning.
	local := *formatted
	width := f.options.width
	oldOptions := f.options
	if f.SignAwareZeroPad() {
		// a sign always goes first
		sign := local.sign
		if err := f.buf.writeStr(sign); err != nil {
			return err
		}

		// remove the sign from the formatted parts
		local.sign = ""
		if width >= uint16(len(sign)) {
			width -= uint16(len(sign))
		} else {
			width = 0
		}
		f.options.setFill('0')
		f.options.setAlign(Alignment_Right)
	}

	// remaining parts go through the ordinary padding process.
	length := local.len()
	var ret error
	if int(width) <= length {
		// no padding
		ret = f.writeFormattedParts(&local)
	} else {
		// Padding widths are capped at `u16`, so reaching this branch means
		// the formatted output is also shorter than `u16::MAX`.
		postPadding, err := f.padding(width-uint16(length), Alignment_Right)
		if err != nil {
			return err
		}
		if err := f.writeFormattedParts(&local); err != nil {
			return err
		}
		ret = postPadding.write(f)
	}
	f.options = oldOptions
	return ret
}

// writeFormattedParts is Formatter::write_formatted_parts.
func (f *Formatter) writeFormattedParts(formatted *fmtNumFormatted) error {
	if formatted.sign != "" {
		if err := f.buf.writeStr(formatted.sign); err != nil {
			return err
		}
	}
	for i := range formatted.parts {
		part := &formatted.parts[i]
		switch part.kind {
		case fmtNumPartZero:
			const zeroes = "0000000000000000000000000000000000000000000000000000000000000000" // 64 zeroes
			nzeroes := part.zero
			for nzeroes > len(zeroes) {
				if err := f.buf.writeStr(zeroes); err != nil {
					return err
				}
				nzeroes -= len(zeroes)
			}
			if nzeroes > 0 {
				if err := f.buf.writeStr(zeroes[:nzeroes]); err != nil {
					return err
				}
			}
		case fmtNumPartNum:
			var s [5]byte
			length := part.len()
			v := part.num
			for i := length - 1; i >= 0; i-- {
				s[i] = '0' + byte(v%10)
				v /= 10
			}
			if err := f.buf.writeStr(string(s[:length])); err != nil {
				return err
			}
		case fmtNumPartCopy:
			if err := f.buf.writeStr(part.copy); err != nil {
				return err
			}
		}
	}
	return nil
}

// WriteStr is Formatter::write_str: it writes s as it is, without padding.
func (f *Formatter) WriteStr(s string) error {
	return f.buf.writeStr(s)
}

// WriteChar is Formatter::write_char.
func (f *Formatter) WriteChar(c rune) error {
	return f.buf.writeChar(c)
}

// WriteFmt is `write!(f, "...", args)` inside a formatting method. The
// formatter's own flags do not apply to what it writes, as in Rust.
func (f *Formatter) WriteFmt(format string, args ...any) error {
	return fmtWriteTemplate(f.buf, fmtParse(format), args)
}

// Fill is Formatter::fill.
func (f *Formatter) Fill() rune {
	return f.options.getFill()
}

// Align is Formatter::align.
func (f *Formatter) Align() Option[Alignment] {
	align, ok := f.options.getAlign()
	return OptionOf(align, ok)
}

// Width is Formatter::width.
func (f *Formatter) Width() Option[int] {
	if f.options.flags&fmtFlagWidth == 0 {
		return None[int]()
	}
	return Some(int(f.options.width))
}

// Precision is Formatter::precision.
func (f *Formatter) Precision() Option[int] {
	if f.options.flags&fmtFlagPrecision == 0 {
		return None[int]()
	}
	return Some(int(f.options.precision))
}

// SignPlus is Formatter::sign_plus: the `+` flag.
func (f *Formatter) SignPlus() bool {
	return f.options.flags&fmtFlagSignPlus != 0
}

// SignMinus is Formatter::sign_minus: the `-` flag.
func (f *Formatter) SignMinus() bool {
	return f.options.flags&fmtFlagSignMinus != 0
}

// Alternate is Formatter::alternate: the `#` flag.
func (f *Formatter) Alternate() bool {
	return f.options.flags&fmtFlagAlternate != 0
}

// SignAwareZeroPad is Formatter::sign_aware_zero_pad: the `0` flag.
func (f *Formatter) SignAwareZeroPad() bool {
	return f.options.flags&fmtFlagSignAwareZeroPad != 0
}

// FIXME: Decide what public API we want for these two flags.
// https://github.com/rust-lang/rust/issues/48584
func (f *Formatter) debugLowerHex() bool {
	return f.options.flags&fmtFlagDebugLowerHex != 0
}

func (f *Formatter) debugUpperHex() bool {
	return f.options.flags&fmtFlagDebugUpperHex != 0
}

// DebugStruct is Formatter::debug_struct.
func (f *Formatter) DebugStruct(name string) *DebugStruct {
	return fmtDebugStructNew(f, name)
}

// DebugTuple is Formatter::debug_tuple.
func (f *Formatter) DebugTuple(name string) *DebugTuple {
	return fmtDebugTupleNew(f, name)
}

// DebugList is Formatter::debug_list.
func (f *Formatter) DebugList() *DebugList {
	return fmtDebugListNew(f)
}

// DebugSet is Formatter::debug_set.
func (f *Formatter) DebugSet() *DebugSet {
	return fmtDebugSetNew(f)
}

// DebugMap is Formatter::debug_map.
func (f *Formatter) DebugMap() *DebugMap {
	return fmtDebugMapNew(f)
}

// fmtPointerFmtInner is fmt::pointer_fmt_inner: an address the way `{:p}`
// prints it.
func fmtPointerFmtInner(ptrAddr uintptr, f *Formatter) error {
	oldOptions := f.options

	// The alternate flag is already treated by LowerHex as being special-
	// it denotes whether to prefix with 0x. We use it to work out whether
	// or not to zero extend, and then unconditionally set it to get the
	// prefix.
	if f.options.getAlternate() {
		f.options.setSignAwareZeroPad(true)

		if _, ok := f.options.getWidth(); !ok {
			f.options.setWidth(uint16(fmtUsizeBits/4) + 2)
		}
	}
	f.options.setAlternate(true)

	ret := fmtRadix(uint64(ptrAddr), 16, false, f)

	f.options = oldOptions

	return ret
}

// fmtWidePointerFmtInner is fmt::wide_pointer_fmt_inner: a pointer that
// carries a length, as a slice or a string does.
func fmtWidePointerFmtInner(ptrAddr uintptr, metadata int, f *Formatter) error {
	return f.DebugStruct("Pointer").
		fieldWith("addr", func(f *Formatter) error { return fmtPointerFmtInner(ptrAddr, f) }).
		Field("metadata", metadata).
		Finish()
}
