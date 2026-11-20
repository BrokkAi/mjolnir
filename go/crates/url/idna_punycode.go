// Port of idna 1.1.0 src/punycode.rs

package url

import (
	"sort"
)

// Punycode (RFC 3492, http://tools.ietf.org/html/rfc3492) implementation.
//
// Since Punycode fundamentally works on unicode code points, encode and
// decode take and return slices of rune.
//
// Only the internal caller of the idna crate is ported: the forms the UTS 46
// processing uses. The crate's public punycode functions (decode,
// decode_to_string, encode, encode_str) are not used by url.

// Bootstring parameters for Punycode
const (
	idnaPunycodeBase        uint32 = 36
	idnaPunycodeTMin        uint32 = 1
	idnaPunycodeTMax        uint32 = 26
	idnaPunycodeSkew        uint32 = 38
	idnaPunycodeDamp        uint32 = 700
	idnaPunycodeInitialBias uint32 = 72
	idnaPunycodeInitialN    uint32 = 0x80
)

func idnaPunycodeAdapt(delta uint32, numPoints uint32, firstTime bool) uint32 {
	if firstTime {
		delta /= idnaPunycodeDamp
	} else {
		delta /= 2
	}
	delta += delta / numPoints
	k := uint32(0)
	for delta > ((idnaPunycodeBase-idnaPunycodeTMin)*idnaPunycodeTMax)/2 {
		delta /= idnaPunycodeBase - idnaPunycodeTMin
		k += idnaPunycodeBase
	}
	return k + (((idnaPunycodeBase - idnaPunycodeTMin + 1) * delta) / (delta + idnaPunycodeSkew))
}

// idnaPunycodeDigitU8 is `<u8 as PunycodeCodeUnit>::digit`.
func idnaPunycodeDigitU8(b rune) (uint32, bool) {
	switch {
	case '0' <= b && b <= '9':
		return uint32(b-'0') + 26, true
	case 'A' <= b && b <= 'Z':
		return uint32(b - 'A'), true
	case 'a' <= b && b <= 'z':
		return uint32(b - 'a'), true
	}
	return 0, false
}

// idnaPunycodeDigitChar is `<char as PunycodeCodeUnit>::digit`: upper case is
// not a digit, because the label was lower-cased before.
func idnaPunycodeDigitChar(c rune) (uint32, bool) {
	switch {
	case '0' <= c && c <= '9':
		return uint32(c-'0') + 26, true
	case 'a' <= c && c <= 'z':
		return uint32(c - 'a'), true
	}
	return 0, false
}

type idnaPunycodeInsertion struct {
	index int
	c     rune
}

// idnaPunycodeDecode is `Decoder::default().decode::<T, InternalCaller>(input)`
// collected: the decoded label, or false for input that is not Punycode.
// units is true for the u8 form (the basic code points are lower-cased and
// upper-case digits are accepted) and false for the char form.
func idnaPunycodeDecode(input []rune, units bool) ([]rune, bool) {
	digitOf := idnaPunycodeDigitChar
	if units {
		digitOf = idnaPunycodeDigitU8
	}
	var insertions []idnaPunycodeInsertion
	// Handle "basic" (ASCII) code points.
	// They are encoded as-is before the last delimiter, if any.
	var base []rune
	position := -1
	for i := len(input) - 1; i >= 0; i-- {
		if input[i] == '-' {
			position = i
			break
		}
	}
	if position >= 0 {
		base = input[:position]
		if position > 0 {
			input = input[position+1:]
		}
	} else {
		base = input[:0]
	}

	baseLen := len(base)
	length := uint32(baseLen)
	codePoint := idnaPunycodeInitialN
	bias := idnaPunycodeInitialBias
	i := uint32(0)
	next := 0
	for {
		previousI := i
		weight := uint32(1)
		k := idnaPunycodeBase
		if next >= len(input) {
			break
		}
		unit := input[next]
		next++

		// Decode a generalized variable-length integer into delta,
		// which gets added to i.
		for {
			digit, ok := digitOf(unit)
			if !ok {
				return nil, false
			}
			product, ok := checkedMulU32(digit, weight)
			if !ok {
				return nil, false
			}
			i, ok = checkedAddU32(i, product)
			if !ok {
				return nil, false
			}
			var t uint32
			if k <= bias {
				t = idnaPunycodeTMin
			} else if k >= bias+idnaPunycodeTMax {
				t = idnaPunycodeTMax
			} else {
				t = k - bias
			}
			if digit < t {
				break
			}
			weight, ok = checkedMulU32(weight, idnaPunycodeBase-t)
			if !ok {
				return nil, false
			}
			k += idnaPunycodeBase
			if next >= len(input) {
				return nil, false // End of input before the end of this delta
			}
			unit = input[next]
			next++
		}

		bias = idnaPunycodeAdapt(i-previousI, length+1, previousI == 0)

		// i was supposed to wrap around from length+1 to 0,
		// incrementing code_point each time.
		var ok bool
		codePoint, ok = checkedAddU32(codePoint, i/(length+1))
		if !ok {
			return nil, false
		}
		i %= length + 1
		// char::from_u32
		if codePoint > 0x10FFFF || (0xD800 <= codePoint && codePoint <= 0xDFFF) {
			return nil, false
		}
		c := rune(codePoint)

		// Move earlier insertions farther out in the string
		for index := range insertions {
			if insertions[index].index >= int(i) {
				insertions[index].index++
			}
		}
		insertions = append(insertions, idnaPunycodeInsertion{index: int(i), c: c})
		length++
		i++
	}

	sort.SliceStable(insertions, func(a, b int) bool { return insertions[a].index < insertions[b].index })

	// The Decode iterator, collected.
	out := make([]rune, 0, baseLen+len(insertions))
	inserted := 0
	outPosition := 0
	baseIndex := 0
	for {
		if inserted < len(insertions) && insertions[inserted].index == outPosition {
			out = append(out, insertions[inserted].c)
			inserted++
			outPosition++
			continue
		}
		if baseIndex < len(base) {
			c := base[baseIndex]
			baseIndex++
			outPosition++
			if units && 'A' <= c && c <= 'Z' {
				c += 'a' - 'A'
			}
			out = append(out, c)
		} else if inserted >= len(insertions) {
			return out, true
		}
	}
}

func checkedMulU32(a uint32, b uint32) (uint32, bool) {
	product := uint64(a) * uint64(b)
	if product > 0xFFFFFFFF {
		return 0, false
	}
	return uint32(product), true
}

func checkedAddU32(a uint32, b uint32) (uint32, bool) {
	sum := uint64(a) + uint64(b)
	if sum > 0xFFFFFFFF {
		return 0, false
	}
	return uint32(sum), true
}

// idnaPunycodeEncodeInto is `encode_into::<_, _, InternalCaller>(input,
// output)`. The internal caller limits the length of the input, so the
// arithmetic cannot overflow; false is the Overflow error of the one check
// the Rust code keeps for it.
func idnaPunycodeEncodeInto(input []rune, output []byte) ([]byte, bool) {
	// Handle "basic" (ASCII) code points. They are encoded as-is.
	inputLength, basicLength := uint32(0), uint32(0)
	for _, c := range input {
		inputLength++
		if c < 0x80 {
			output = append(output, byte(c))
			basicLength++
		}
	}

	{
		// We should never get an overflow here with the internal caller being
		// length-limited, but let's check anyway once here trusting the math
		// from  RFC 3492 section 6.4 and then omit the overflow checks in the
		// loop below.
		lenPlusOne, ok := checkedAddU32(inputLength, 1)
		if !ok {
			return output, false
		}
		if _, ok := checkedMulU32(lenPlusOne, 0x10FFFF-idnaPunycodeInitialN); !ok {
			return output, false
		}
	}

	if basicLength > 0 {
		output = append(output, '-')
	}
	codePoint := idnaPunycodeInitialN
	delta := uint32(0)
	bias := idnaPunycodeInitialBias
	processed := basicLength
	for processed < inputLength {
		// All code points < code_point have been handled already.
		// Find the next larger one.
		minCodePoint := uint32(0xFFFFFFFF)
		for _, c := range input {
			if uint32(c) >= codePoint && uint32(c) < minCodePoint {
				minCodePoint = uint32(c)
			}
		}
		// Increase delta to advance the decoder’s <code_point,i> state to <min_code_point,0>
		delta += (minCodePoint - codePoint) * (processed + 1)
		codePoint = minCodePoint
		for _, r := range input {
			c := uint32(r)
			if c < codePoint {
				delta++
			}
			if c == codePoint {
				// Represent delta as a generalized variable-length integer:
				q := delta
				k := idnaPunycodeBase
				for {
					var t uint32
					if k <= bias {
						t = idnaPunycodeTMin
					} else if k >= bias+idnaPunycodeTMax {
						t = idnaPunycodeTMax
					} else {
						t = k - bias
					}
					if q < t {
						break
					}
					value := t + ((q - t) % (idnaPunycodeBase - t))
					output = append(output, idnaPunycodeValueToDigit(value))
					q = (q - t) / (idnaPunycodeBase - t)
					k += idnaPunycodeBase
				}
				output = append(output, idnaPunycodeValueToDigit(q))
				bias = idnaPunycodeAdapt(delta, processed+1, processed == basicLength)
				delta = 0
				processed++
			}
		}
		delta++
		codePoint++
	}
	return output, true
}

func idnaPunycodeValueToDigit(value uint32) byte {
	switch {
	case value <= 25:
		return byte(value) + 'a' // a..z
	case value <= 35:
		return byte(value) - 26 + '0' // 0..9
	default:
		panic("explicit panic")
	}
}
