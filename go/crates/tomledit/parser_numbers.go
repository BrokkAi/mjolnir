// Port of toml_edit 0.20.2 src/parser/numbers.rs

package tomledit

import (
	"errors"
	"math"
	"strconv"
	"strings"
)

// ;; Boolean

// boolean = true / false

func parseTrue(in *input) (bool, parseErr) {
	// (peek(TRUE[0]), cut_err(TRUE)).value(true)
	if b, ok := in.peek(); !ok || b != 't' {
		return false, backtrack()
	}
	if !in.tag("true") {
		return false, cutEmpty()
	}
	return true, parseErr{}
}

func parseFalse(in *input) (bool, parseErr) {
	// (peek(FALSE[0]), cut_err(FALSE)).value(false)
	if b, ok := in.peek(); !ok || b != 'f' {
		return false, backtrack()
	}
	if !in.tag("false") {
		return false, cutEmpty()
	}
	return false, parseErr{}
}

// ;; Integer

// integer = dec-int / hex-int / oct-int / bin-int
func integer(in *input) (int64, parseErr) {
	// dispatch! {peek(opt::<_, &[u8], _, _>(take(2usize)));
	if in.eofOffset() >= 2 {
		switch in.data[in.pos : in.pos+2] {
		case "0x":
			// cut_err(hex_int.try_map(|s| i64::from_str_radix(&s.replace('_', ""), 16)))
			return radixInteger(in, hexInt, 16)
		case "0o":
			// cut_err(oct_int.try_map(|s| i64::from_str_radix(&s.replace('_', ""), 8)))
			return radixInteger(in, octInt, 8)
		case "0b":
			// cut_err(bin_int.try_map(|s| i64::from_str_radix(&s.replace('_', ""), 2)))
			return radixInteger(in, binInt, 2)
		}
	}
	// dec_int.and_then(cut_err(rest.try_map(|s: &str| s.replace('_', "").parse())))
	start := in.pos
	s, e := decInt(in)
	if e.failed() {
		return 0, e
	}
	parsed, err := parseI64(strings.ReplaceAll(s, "_", ""), 10)
	if err != nil {
		in.pos = start
		return 0, external(err).cut()
	}
	return parsed, parseErr{}
}

func radixInteger(in *input, digits func(*input) (string, parseErr), radix int) (int64, parseErr) {
	start := in.pos
	s, e := digits(in)
	if e.failed() {
		return 0, e.cut()
	}
	parsed, err := parseI64(strings.ReplaceAll(s, "_", ""), radix)
	if err != nil {
		in.pos = start
		return 0, external(err).cut()
	}
	return parsed, parseErr{}
}

// parseIntError is core::num::ParseIntError for the two ways a string the
// grammar accepted can still fail to be an i64.
type parseIntError struct {
	negative bool
}

func (e *parseIntError) Error() string {
	if e.negative {
		return "number too small to fit in target type"
	}
	return "number too large to fit in target type"
}

// parseI64 is `i64::from_str_radix(s, radix)` (and `s.parse::<i64>()`) for a
// string of digits of that radix with an optional sign.
func parseI64(s string, radix int) (int64, error) {
	parsed, err := strconv.ParseInt(s, radix, 64)
	if err != nil {
		if errors.Is(err, strconv.ErrRange) {
			return 0, &parseIntError{negative: strings.HasPrefix(s, "-")}
		}
		return 0, err
	}
	return parsed, nil
}

// dec-int = [ minus / plus ] unsigned-dec-int
// unsigned-dec-int = DIGIT / digit1-9 1*( DIGIT / underscore DIGIT )
func decInt(in *input) (string, parseErr) {
	start := in.pos
	e := func() parseErr {
		// opt(one_of((b'+', b'-')))
		in.oneOf(isSign)
		// alt((
		//     (one_of(DIGIT1_9), repeat(0.., alt((digit, (one_of(b'_'), cut_err(digit)...))))),
		//     digit,
		// ))
		if in.oneOf(isDigit19) {
			return digitsOrUnderscores(in, isDigit)
		}
		if in.oneOf(isDigit) {
			return parseErr{}
		}
		return backtrack()
	}()
	if e.failed() {
		return "", e.add(label("integer"))
	}
	return in.data[start:in.pos], parseErr{}
}

// digitsOrUnderscores is the tail every integer form shares:
//
//	repeat(
//	    0..,
//	    alt((
//	        digit.value(()),
//	        (
//	            one_of(b'_'),
//	            cut_err(digit).context(StrContext::Expected(
//	                StrContextValue::Description("digit"),
//	            )),
//	        )
//	            .value(()),
//	    )),
//	)
func digitsOrUnderscores(in *input, digit func(byte) bool) parseErr {
	for {
		if in.oneOf(digit) {
			continue
		}
		if in.eat('_') {
			if in.oneOf(digit) {
				continue
			}
			return cutEmpty().add(expectedDescription("digit"))
		}
		return parseErr{}
	}
}

func isSign(b byte) bool { return b == '+' || b == '-' }

func isDigit19(b byte) bool { return b >= '1' && b <= '9' }

// hex-prefix = %x30.78               ; 0x
// hex-int = hex-prefix HEXDIG *( HEXDIG / underscore HEXDIG )
func hexInt(in *input) (string, parseErr) {
	return prefixedInt(in, "0x", isHexdig, "hexadecimal integer")
}

// oct-prefix = %x30.6F               ; 0o
// oct-int = oct-prefix digit0-7 *( digit0-7 / underscore digit0-7 )
func octInt(in *input) (string, parseErr) {
	return prefixedInt(in, "0o", isDigit07, "octal integer")
}

func isDigit07(b byte) bool { return b >= '0' && b <= '7' }

// bin-prefix = %x30.62               ; 0b
// bin-int = bin-prefix digit0-1 *( digit0-1 / underscore digit0-1 )
func binInt(in *input) (string, parseErr) {
	return prefixedInt(in, "0b", isDigit01, "binary integer")
}

func isDigit01(b byte) bool { return b >= '0' && b <= '1' }

// prefixedInt is the shape hex_int, oct_int and bin_int share:
//
//	preceded(
//	    PREFIX,
//	    cut_err((digit, repeat(0.., alt((digit, (one_of(b'_'), cut_err(digit)...)))))).recognize(),
//	)
//	.context(StrContext::Label(name))
func prefixedInt(in *input, prefix string, digit func(byte) bool, name string) (string, parseErr) {
	if !in.tag(prefix) {
		return "", backtrack().add(label(name))
	}
	start := in.pos
	if !in.oneOf(digit) {
		return "", cutEmpty().add(label(name))
	}
	if e := digitsOrUnderscores(in, digit); e.failed() {
		return "", e.cut().add(label(name))
	}
	return in.data[start:in.pos], parseErr{}
}

// ;; Float

// float = float-int-part ( exp / frac [ exp ] )
// float =/ special-float
// float-int-part = dec-int
func float(in *input) (float64, parseErr) {
	parsed, e := func() (float64, parseErr) {
		// alt((
		//     float_.and_then(cut_err(
		//         rest.try_map(|s: &str| s.replace('_', "").parse())
		//             .verify(|f: &f64| *f != f64::INFINITY),
		//     )),
		//     special_float,
		// ))
		start := in.pos
		s, e := floatUnderscored(in)
		switch e.mode {
		case errNone:
			f, err := parseF64(strings.ReplaceAll(s, "_", ""))
			if err != nil {
				in.pos = start
				return 0, external(err).cut()
			}
			if math.IsInf(f, 1) {
				in.pos = start
				return 0, cutEmpty()
			}
			return f, parseErr{}
		case errCut:
			return 0, e
		}
		in.pos = start
		return specialFloat(in)
	}()
	if e.failed() {
		return 0, e.add(label("floating-point number"))
	}
	return parsed, parseErr{}
}

// parseF64 is `s.parse::<f64>()` for the strings the float grammar accepts:
// both languages round a decimal to the nearest float, and a number too
// large is an infinity.
func parseF64(s string) (float64, error) {
	f, err := strconv.ParseFloat(s, 64)
	if err != nil && !errors.Is(err, strconv.ErrRange) {
		return 0, err
	}
	return f, nil
}

// floatUnderscored is the Rust `float_`.
func floatUnderscored(in *input) (string, parseErr) {
	// (
	//     dec_int,
	//     alt((exp.void(), (frac.void(), opt(exp.void())).void())),
	// )
	//     .recognize()
	start := in.pos
	if _, e := decInt(in); e.failed() {
		return "", e
	}
	afterInt := in.pos
	e := exp(in)
	if e.mode == errBacktrack {
		in.pos = afterInt
		e = frac(in)
		if !e.failed() {
			beforeExp := in.pos
			e = exp(in)
			if e.mode == errBacktrack {
				in.pos = beforeExp
				e = parseErr{}
			}
		}
	}
	if e.failed() {
		return "", e
	}
	return in.data[start:in.pos], parseErr{}
}

// frac = decimal-point zero-prefixable-int
// decimal-point = %x2E               ; .
func frac(in *input) parseErr {
	// (
	//     b'.',
	//     cut_err(zero_prefixable_int)
	//         .context(StrContext::Expected(StrContextValue::Description("digit"))),
	// )
	if !in.eat('.') {
		return backtrack()
	}
	if e := zeroPrefixableInt(in); e.failed() {
		return e.cut().add(expectedDescription("digit"))
	}
	return parseErr{}
}

// zero-prefixable-int = DIGIT *( DIGIT / underscore DIGIT )
func zeroPrefixableInt(in *input) parseErr {
	if !in.oneOf(isDigit) {
		return backtrack()
	}
	return digitsOrUnderscores(in, isDigit)
}

// exp = "e" float-exp-part
// float-exp-part = [ minus / plus ] zero-prefixable-int
func exp(in *input) parseErr {
	// (
	//     one_of((b'e', b'E')),
	//     opt(one_of([b'+', b'-'])),
	//     cut_err(zero_prefixable_int),
	// )
	if !in.oneOf(func(b byte) bool { return b == 'e' || b == 'E' }) {
		return backtrack()
	}
	in.oneOf(isSign)
	if e := zeroPrefixableInt(in); e.failed() {
		return e.cut()
	}
	return parseErr{}
}

// special-float = [ minus / plus ] ( inf / nan )
func specialFloat(in *input) (float64, parseErr) {
	// (opt(one_of((b'+', b'-'))), alt((inf, nan)))
	negative := false
	if b, ok := in.peek(); ok && isSign(b) {
		negative = b == '-'
		in.pos++
	}
	f, e := parseInf(in)
	if e.failed() {
		f, e = parseNan(in)
	}
	if e.failed() {
		return 0, e
	}
	if negative {
		// -f
		f = math.Float64frombits(math.Float64bits(f) ^ (1 << 63))
	}
	return f, parseErr{}
}

// inf = %x69.6e.66  ; inf
func parseInf(in *input) (float64, parseErr) {
	if !in.tag("inf") {
		return 0, backtrack()
	}
	return math.Inf(1), parseErr{}
}

// nan = %x6e.61.6e  ; nan
func parseNan(in *input) (float64, parseErr) {
	if !in.tag("nan") {
		return 0, backtrack()
	}
	// f64::NAN
	return math.Float64frombits(0x7ff8000000000000), parseErr{}
}

// DIGIT = %x30-39 ; 0-9
func isDigit(b byte) bool { return b >= '0' && b <= '9' }

// HEXDIG = DIGIT / "A" / "B" / "C" / "D" / "E" / "F"
func isHexdig(b byte) bool {
	return isDigit(b) || (b >= 'A' && b <= 'F') || (b >= 'a' && b <= 'f')
}
