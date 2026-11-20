// Port of core 1.99.1 src/unicode/unicode_data.rs (skip_search and the
// lookups) and src/char/methods.rs (the predicates escape_debug_ext reads).

package rs

import "sort"

// fmtSkipSearch is unicode_data::skip_search. shortOffsetRuns holds
// ShortOffsetRunHeader values: a start index into offsets in the upper 11
// bits and a prefix sum (a code point) in the lower 21.
func fmtSkipSearch(needle uint32, shortOffsetRuns []uint32, offsets []uint8) bool {
	// binary_search_by_key(&(needle << 11), |header| header.0 << 11): the
	// shift drops the start index, so the keys are the prefix sums.
	key := needle << 11
	lastIdx := sort.Search(len(shortOffsetRuns), func(i int) bool {
		return shortOffsetRuns[i]<<11 >= key
	})
	if lastIdx < len(shortOffsetRuns) && shortOffsetRuns[lastIdx]<<11 == key {
		// Ok(idx) => idx + 1
		lastIdx++
	}

	offsetIdx := int(shortOffsetRuns[lastIdx] >> 21)
	var length int
	if lastIdx+1 < len(shortOffsetRuns) {
		length = int(shortOffsetRuns[lastIdx+1]>>21) - offsetIdx
	} else {
		length = len(offsets) - offsetIdx
	}

	var prev uint32
	if lastIdx > 0 {
		prev = shortOffsetRuns[lastIdx-1] & (1<<21 - 1)
	}

	total := needle - prev
	var prefixSum uint32
	for i := 0; i < length-1; i++ {
		prefixSum += uint32(offsets[offsetIdx])
		if prefixSum > total {
			break
		}
		offsetIdx++
	}
	return offsetIdx%2 == 1
}

// fmtUnicodeCf is unicode::Cf.
func fmtUnicodeCf(c rune) bool {
	return uint32(c) >= fmtUnicodeCfFirst &&
		fmtSkipSearch(uint32(c), fmtUnicodeCfShortOffsetRuns[:], fmtUnicodeCfOffsets[:])
}

// fmtUnicodeCnPlanes03 is unicode::Cn_planes_0_3.
func fmtUnicodeCnPlanes03(c rune) bool {
	return uint32(c) >= fmtUnicodeCnPlanes03First &&
		fmtSkipSearch(uint32(c), fmtUnicodeCnPlanes03ShortOffsetRuns[:], fmtUnicodeCnPlanes03Offsets[:])
}

// fmtUnicodeDefaultIgnorableCodePoint is unicode::Default_Ignorable_Code_Point.
func fmtUnicodeDefaultIgnorableCodePoint(c rune) bool {
	return uint32(c) >= fmtUnicodeDefaultIgnorableCodePointFirst &&
		fmtSkipSearch(uint32(c), fmtUnicodeDefaultIgnorableCodePointShortOffsetRuns[:], fmtUnicodeDefaultIgnorableCodePointOffsets[:])
}

// fmtUnicodeGraphemeExtend is unicode::Grapheme_Extend.
func fmtUnicodeGraphemeExtend(c rune) bool {
	return uint32(c) >= fmtUnicodeGraphemeExtendFirst &&
		fmtSkipSearch(uint32(c), fmtUnicodeGraphemeExtendShortOffsetRuns[:], fmtUnicodeGraphemeExtendOffsets[:])
}

// fmtUnicodeWhiteSpace is unicode::White_Space (unicode_data::white_space::lookup).
func fmtUnicodeWhiteSpace(c rune) bool {
	switch uint32(c) >> 8 {
	case 0:
		return fmtUnicodeWhitespaceMap[uint32(c)&0xff]&1 != 0
	case 22:
		return uint32(c) == 0x1680
	case 32:
		return fmtUnicodeWhitespaceMap[uint32(c)&0xff]&2 != 0
	case 48:
		return uint32(c) == 0x3000
	default:
		return false
	}
}

// fmtCharIsWhitespace is char::is_whitespace.
func fmtCharIsWhitespace(c rune) bool {
	switch {
	case c == ' ' || (c >= 0x09 && c <= 0x0d):
		return true
	case c >= 0 && c <= 0x84:
		return false
	default:
		return fmtUnicodeWhiteSpace(c)
	}
}

// fmtCharIsControl is char::is_control.
func fmtCharIsControl(c rune) bool {
	// According to
	// https://www.unicode.org/policies/stability_policy.html#Property_Value,
	// the set of codepoints in `Cc` will never change.
	// So we can just hard-code the patterns to match against instead of using a table.
	return (c >= 0 && c <= 0x1f) || (c >= 0x7f && c <= 0x9f)
}

// fmtCharIsPrivateUse is char::is_private_use.
func fmtCharIsPrivateUse(c rune) bool {
	// According to
	// https://www.unicode.org/policies/stability_policy.html#Property_Value,
	// the set of codepoints in `Co` will never change.
	// So we can just hard-code the patterns to match against instead of using a table.
	return (c >= 0xE000 && c <= 0xF8FF) || (c >= 0xF0000 && c <= 0xFFFFD) || (c >= 0x100000 && c <= 0x10FFFD)
}

// fmtCharIsFormatControl is char::is_format_control.
func fmtCharIsFormatControl(c rune) bool {
	return c > 0xAC && fmtUnicodeCf(c)
}

// fmtCharIsAssigned is char::is_assigned.
func fmtCharIsAssigned(c rune) bool {
	switch {
	case c >= 0 && c <= 0x377:
		return true
	case c >= 0x378 && c <= 0x3FFFD:
		return !fmtUnicodeCnPlanes03(c)
	// Assigned character ranges in planes 4 and above.
	// `src/tools/unicode-table-generator/src/main.rs` asserts that this is correct
	case c == 0xE0001,
		c >= 0xE0020 && c <= 0xE007F,
		c >= 0xE0100 && c <= 0xE01EF,
		c >= 0xF0000 && c <= 0xFFFFD,
		c >= 0x100000 && c <= 0x10FFFD:
		return true
	default:
		return false
	}
}

// fmtCharIsDefaultIgnorable is char::is_default_ignorable.
func fmtCharIsDefaultIgnorable(c rune) bool {
	return c > 0xAC && fmtUnicodeDefaultIgnorableCodePoint(c)
}

// fmtCharIsGraphemeExtender is char::is_grapheme_extender.
func fmtCharIsGraphemeExtender(c rune) bool {
	return c > 0x02FF && fmtUnicodeGraphemeExtend(c)
}
