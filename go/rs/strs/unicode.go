// Port of core 1.99.1 src/unicode/unicode_data.rs and src/unicode/mod.rs
//
// This file holds the lookup functions. The tables they read are in
// unicode_data.go, which gen_unicode_data.go writes from the Rust source, so
// that every answer is the one Unicode version the pinned toolchain ships
// (unicodeVersion), not the one Go's unicode package was built from.

package strs

import (
	"math/bits"
	"sort"
)

// bitsetSearch is bitset_search.
func bitsetSearch(
	needle uint32,
	chunkIdxMap []uint8,
	bitsetChunkIdx [][16]uint8,
	bitsetCanonical []uint64,
	bitsetCanonicalized [][2]uint8,
) bool {
	const chunkSize = 16
	bucketIdx := int(needle / 64)
	chunkMapIdx := bucketIdx / chunkSize
	chunkPiece := bucketIdx % chunkSize
	if chunkMapIdx >= len(chunkIdxMap) {
		return false
	}
	chunkIdx := chunkIdxMap[chunkMapIdx]
	idx := int(bitsetChunkIdx[chunkIdx][chunkPiece])
	var word uint64
	if idx < len(bitsetCanonical) {
		word = bitsetCanonical[idx]
	} else {
		realIdx, mapping := bitsetCanonicalized[idx-len(bitsetCanonical)][0], bitsetCanonicalized[idx-len(bitsetCanonical)][1]
		word = bitsetCanonical[realIdx]
		shouldInvert := mapping&(1<<6) != 0
		if shouldInvert {
			word = ^word
		}
		// Lower 6 bits
		quantity := mapping & ((1 << 6) - 1)
		if mapping&(1<<7) != 0 {
			// shift
			word >>= uint64(quantity)
		} else {
			word = bits.RotateLeft64(word, int(quantity))
		}
	}
	return word&(1<<(needle%64)) != 0
}

// A short offset run header packs a start index into the offsets (the upper
// 11 bits) and a prefix sum of code points (the lower 21 bits).
const shortOffsetRunPrefixSumMask = 1<<21 - 1

// skipSearch is skip_search. The last element of shortOffsetRuns is greater
// than char::MAX, which the caller's range check relies on.
func skipSearch(needle rune, shortOffsetRuns []uint32, offsets []uint8) bool {
	n := uint32(needle)

	// binary_search_by_key(&(needle << 11), |header| header.0 << 11), with
	// Ok(idx) => idx + 1 and Err(idx) => idx.
	key := n << 11
	lastIdx := sort.Search(len(shortOffsetRuns), func(i int) bool { return shortOffsetRuns[i]<<11 >= key })
	if lastIdx < len(shortOffsetRuns) && shortOffsetRuns[lastIdx]<<11 == key {
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
		prev = shortOffsetRuns[lastIdx-1] & shortOffsetRunPrefixSumMask
	}

	total := n - prev
	var prefixSum uint32
	for i := 0; i < length-1; i++ {
		offset := offsets[offsetIdx]
		prefixSum += uint32(offset)
		if prefixSum > total {
			break
		}
		offsetIdx++
	}
	return offsetIdx%2 == 1
}

// isChar reports whether c is a Rust `char`: a Unicode scalar value. Every
// lookup answers false for anything else, so a Go rune that is not a char
// never indexes a table out of range.
func isChar(c rune) bool {
	return (uint32(c)^0xD800)-0x800 < 0x110000-0x800
}

// unicodeAlphabetic is unicode::Alphabetic.
func unicodeAlphabetic(c rune) bool {
	return c >= alphabeticMin && isChar(c) && skipSearch(c, alphabeticShortOffsetRuns[:], alphabeticOffsets[:])
}

// unicodeCaseIgnorable is unicode::Case_Ignorable.
func unicodeCaseIgnorable(c rune) bool {
	return c >= caseIgnorableMin && isChar(c) && skipSearch(c, caseIgnorableShortOffsetRuns[:], caseIgnorableOffsets[:])
}

// unicodeCf is unicode::Cf.
func unicodeCf(c rune) bool {
	return c >= cfMin && isChar(c) && skipSearch(c, cfShortOffsetRuns[:], cfOffsets[:])
}

// unicodeCnPlanes03 is unicode::Cn_planes_0_3.
func unicodeCnPlanes03(c rune) bool {
	return c >= cnPlanes03Min && isChar(c) && skipSearch(c, cnPlanes03ShortOffsetRuns[:], cnPlanes03Offsets[:])
}

// unicodeDefaultIgnorableCodePoint is unicode::Default_Ignorable_Code_Point.
func unicodeDefaultIgnorableCodePoint(c rune) bool {
	return c >= defaultIgnorableCodePointMin && isChar(c) &&
		skipSearch(c, defaultIgnorableCodePointShortOffsetRuns[:], defaultIgnorableCodePointOffsets[:])
}

// unicodeGraphemeExtend is unicode::Grapheme_Extend.
func unicodeGraphemeExtend(c rune) bool {
	return c >= graphemeExtendMin && isChar(c) && skipSearch(c, graphemeExtendShortOffsetRuns[:], graphemeExtendOffsets[:])
}

// unicodeLowercase is unicode::Lowercase.
func unicodeLowercase(c rune) bool {
	return c >= lowercaseMin && isChar(c) && bitsetSearch(uint32(c), lowercaseBitsetChunksMap[:],
		lowercaseBitsetIndexChunks[:], lowercaseBitsetCanonical[:], lowercaseBitsetMapping[:])
}

// unicodeLt is unicode::Lt.
func unicodeLt(c rune) bool {
	return c >= ltMin && isChar(c) && skipSearch(c, ltShortOffsetRuns[:], ltOffsets[:])
}

// unicodeN is unicode::N.
func unicodeN(c rune) bool {
	return c >= nMin && isChar(c) && skipSearch(c, nShortOffsetRuns[:], nOffsets[:])
}

// unicodeUppercase is unicode::Uppercase.
func unicodeUppercase(c rune) bool {
	return c >= uppercaseMin && isChar(c) && bitsetSearch(uint32(c), uppercaseBitsetChunksMap[:],
		uppercaseBitsetIndexChunks[:], uppercaseBitsetCanonical[:], uppercaseBitsetMapping[:])
}

// unicodeWhiteSpace is unicode::White_Space.
func unicodeWhiteSpace(c rune) bool {
	return isChar(c) && whiteSpaceLookup(c)
}

// caseSingle is one entry of conversions::L2Lut::singles: the code points
// start, start+1 (or start+2 when parity is set), .., start+len map to
// themselves plus delta, wrapping in 16 bits.
type caseSingle struct {
	start  uint16
	len    uint8
	parity bool
	delta  int16
}

func (r caseSingle) end() uint16 { return r.start + uint16(r.len) }

// caseMulti is one entry of conversions::L2Lut::multis: a mapping to up to
// three code points of the same plane, padded with zeros.
type caseMulti struct {
	from uint16
	to   [3]uint16
}

// caseL2Lut is conversions::L2Lut: the mappings of one plane.
type caseL2Lut struct {
	singles []caseSingle
	multis  []caseMulti
}

// caseReconstruct is conversions::reconstruct.
func caseReconstruct(plane, low uint16) rune {
	return rune(uint32(plane)<<16 | uint32(low))
}

// caseLookup is conversions::lookup.
func caseLookup(input rune, l1Lut *[2]caseL2Lut) ([3]rune, bool) {
	inputHigh, inputLow := uint16(uint32(input)>>16), uint16(uint32(input))
	if int(inputHigh) >= len(l1Lut) || !isChar(input) {
		return [3]rune{}, false
	}
	l2Lut := &l1Lut[inputHigh]

	singles := l2Lut.singles
	idx := sort.Search(len(singles), func(i int) bool { return inputLow <= singles[i].end() })
	if idx < len(singles) && inputLow >= singles[idx].start {
		r := singles[idx]
		var mask uint16
		if r.parity {
			mask = 1
		}
		if inputLow&mask == r.start&mask {
			outputLow := inputLow + uint16(r.delta) // wrapping_add_signed
			return [3]rune{caseReconstruct(inputHigh, outputLow), 0, 0}, true
		}
	}

	multis := l2Lut.multis
	idx = sort.Search(len(multis), func(i int) bool { return multis[i].from >= inputLow })
	if idx < len(multis) && multis[idx].from == inputLow {
		to := multis[idx].to
		return [3]rune{
			caseReconstruct(inputHigh, to[0]),
			caseReconstruct(inputHigh, to[1]),
			caseReconstruct(inputHigh, to[2]),
		}, true
	}

	return [3]rune{}, false
}

// conversionsToLower is conversions::to_lower. The result is padded with
// zeros, as Rust's `[char; 3]` is.
func conversionsToLower(c rune) [3]rune {
	if c < toLowerMin {
		return [3]rune{CharToAsciiLowercase(c), 0, 0}
	}
	if mapped, ok := caseLookup(c, &lowercaseLut); ok {
		return mapped
	}
	return [3]rune{c, 0, 0}
}

// conversionsToUpper is conversions::to_upper.
func conversionsToUpper(c rune) [3]rune {
	if c < toUpperMin {
		return [3]rune{CharToAsciiUppercase(c), 0, 0}
	}
	if mapped, ok := caseLookup(c, &uppercaseLut); ok {
		return mapped
	}
	return [3]rune{c, 0, 0}
}

// conversions::to_title and conversions::to_casefold back `char::to_titlecase`
// and `char::to_casefold_unnormalized`, which are unstable in 1.99.1, so
// Mjolnir cannot call them and they are not ported.
