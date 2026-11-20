package unicodesegmentation

// The generated strings the differential tests feed to both implementations.
//
// This file is the same, apart from its package clause, in
// go/crates/unicodewidth and go/crates/unicodesegmentation: the two packages
// are tested on the same strings, and a runtime package may not import
// another one's tests. Change both copies together.
//
// The strings are random, but built from the sequences that width and
// segmentation rules single out rather than from uniformly random characters,
// which would almost never form one: emoji with modifiers, variation selectors
// and zero width joiners, flags, tag and keycap sequences, combining marks,
// Hangul jamo, Indic conjuncts, the Arabic, Hebrew, Buginese, Khmer, Lisu,
// Tifinagh and Old Turkic ligatures, control characters and CRLF, zero-width
// and default-ignorable characters, wide and ambiguous-width characters, and
// the characters word boundaries turn on (quotes, mid-word punctuation,
// digits, Katakana, underscores).

import (
	"os"
	"strings"
)

// corpusRand is SplitMix64. The corpus must not change with the Go version,
// so it does not use math/rand.
type corpusRand struct{ state uint64 }

func (r *corpusRand) next() uint64 {
	r.state += 0x9E3779B97F4A7C15
	z := r.state
	z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
	z = (z ^ (z >> 27)) * 0x94D049BB133111EB
	return z ^ (z >> 31)
}

// intn returns a number in [0, n).
func (r *corpusRand) intn(n int) int { return int(r.next() % uint64(n)) }

// chance reports true percent times in a hundred.
func (r *corpusRand) chance(percent int) bool { return r.intn(100) < percent }

func (r *corpusRand) pick(pool []rune) rune { return pool[r.intn(len(pool))] }

// between returns a character in [lo, hi].
func (r *corpusRand) between(lo, hi rune) rune { return lo + rune(r.intn(int(hi-lo)+1)) }

// scalar returns any Unicode scalar value, leaning towards the planes that
// have characters assigned.
func (r *corpusRand) scalar() rune {
	for {
		var c rune
		switch n := r.intn(100); {
		case n < 55:
			c = rune(r.intn(0x10000))
		case n < 80:
			c = 0x10000 + rune(r.intn(0x10000))
		case n < 88:
			c = 0x20000 + rune(r.intn(0x20000))
		case n < 94:
			c = 0xE0000 + rune(r.intn(0x1000))
		default:
			c = rune(r.intn(0x110000))
		}
		if c < 0xD800 || c > 0xDFFF {
			return c
		}
	}
}

var (
	corpusASCII = []rune("abcdefghijklmnopqrstuvwxyzABCXYZ0123456789      .,;:'\"!?-_()[]<=>#*/\\@%&+~`^|{}$")

	corpusControls = []rune{0x00, 0x01, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x1B, 0x1F, 0x7F, 0x80, 0x85, 0x9F, 0x2028, 0x2029}

	// Ambiguous-width characters, the quotation marks that VS1 to VS3
	// change, and neighbours that are not ambiguous.
	corpusAmbiguous = []rune{
		0xA0, 0xA1, 0xA4, 0xA7, 0xA8, 0xA9, 0xAA, 0xAD, 0xAE, 0xB0, 0xB1, 0xB7, 0xC6, 0xD7, 0xE9, 0xF7,
		0x2C9, 0x387, 0x391, 0x3A9, 0x401, 0x410, 0x2010, 0x2014, 0x2018, 0x2019, 0x201C, 0x201D, 0x2020,
		0x2026, 0x2030, 0x203B, 0x2081, 0x2082, 0x20AC, 0x2103, 0x2116, 0x2122, 0x2190, 0x21D2, 0x2200,
		0x2260, 0x226E, 0x226F, 0x2460, 0x24EA, 0x2500, 0x2550, 0x25A0, 0x25CF, 0x2605, 0x2616, 0x2640,
		0x266A, 0x2780, 0x3248, 0xE000, 0xFFFD, 0xF0000, 0x100000,
	}

	corpusCombining = []rune{
		0x300, 0x301, 0x302, 0x308, 0x30A, 0x310, 0x323, 0x327, 0x332, 0x334, 0x338, 0x338, 0x33F, 0x345, 0x35C,
		0x361, 0x483, 0x488, 0x591, 0x5B0, 0x5BF, 0x610, 0x64B, 0x64E, 0x651, 0x65E, 0x65F, 0x670, 0x6D6,
		0x6DF, 0x711, 0x7A6, 0x7EB, 0x816, 0x859, 0x8CA, 0x8E3, 0x1AB0, 0x1ABE, 0x1DC0, 0x20D0, 0x20DD,
		0x20E3, 0x2CEF, 0x2DE0, 0x302A, 0x3099, 0x309A, 0xA66F, 0xA670, 0xA8E0, 0xFE20, 0x101FD, 0x1D165,
		0x1D167, 0x1E000, 0x1E94B, 0x1E8D0, 0xE0100, 0xE01EF,
	}

	corpusVariationSelectors = []rune{0xFE00, 0xFE01, 0xFE02, 0xFE03, 0xFE0E, 0xFE0E, 0xFE0F, 0xFE0F, 0xFE0F, 0xE0100, 0xE0101, 0xE01EF, 0x180B, 0x180D, 0x180F}

	// Zero-width, default-ignorable and format characters.
	corpusInvisible = []rune{
		0xAD, 0x34F, 0x61C, 0x115F, 0x1160, 0x17B4, 0x17B5, 0x180E, 0x200B, 0x200C, 0x200D, 0x200D, 0x200D,
		0x200E, 0x200F, 0x202A, 0x202E, 0x2060, 0x2061, 0x2064, 0x2066, 0x2069, 0x206A, 0x3164, 0xFEFF,
		0xFFA0, 0xFFF0, 0xFFF9, 0xFFFA, 0xFFFB, 0x1BCA0, 0x1D173, 0xE0000, 0xE0001, 0xE0020, 0xE007E,
		0xE007F, 0xE0080, 0xE0FFF, 0x13430, 0x13436, 0x1343C, 0x110BD, 0x110CD,
	}

	corpusEmoji = []rune{
		0x1F600, 0x1F60A, 0x1F923, 0x1F970, 0x1F971, 0x1F44D, 0x1F44B, 0x1F590, 0x270B, 0x270C, 0x261D,
		0x1F468, 0x1F469, 0x1F466, 0x1F467, 0x1F9D1, 0x1F476, 0x1F9D4, 0x1F471, 0x1F46A, 0x1F91D, 0x1F48B,
		0x2764, 0x1F525, 0x1F52C, 0x1F680, 0x1F308, 0x1F3F3, 0x1F3F4, 0x2620, 0x26A7, 0x1F4BB, 0x1F33E,
		0x1F373, 0x1F393, 0x1F3A4, 0x2695, 0x2696, 0x2708, 0x1F9AF, 0x1F9BC, 0x1F9B0, 0x1F9B3, 0x1F415,
		0x1F9BA, 0x1F408, 0x2B1B, 0x1F43B, 0x2744, 0x1F426, 0x1F54A, 0x1F6F3, 0x1F938, 0x1F3CA, 0x26F9,
		0x1F3CB, 0x1F574, 0x1F6B4, 0x1F9DE, 0x1F9DF, 0x1FAE0, 0x1FAF1, 0x1FAF2, 0x1FA77, 0x1FAE8, 0x1F6DC,
		0x1F1E6, 0x1F1FF, 0x1F3FB, 0x1F3FF, 0x1F004, 0x1F0CF, 0x1F18E, 0x1F191, 0x1F201, 0x1F202, 0x1F21A,
		0x1F22F, 0x1F232, 0x1F250, 0x1F300, 0x1F5FB, 0x1F64F, 0x1F6C0, 0x1F6CC, 0x1F90C, 0x1F9B5, 0x1FAC3,
		0x1FAF8, 0x1FC00, 0x1FFFD, 0x1F7E0, 0x1F7F0, 0x1F700, 0x1F100, 0x1F10D, 0x1F12F, 0x1F16C, 0x1F1AD,
	}

	// Characters whose presentation a variation selector changes, and
	// pictographs that are text by default.
	corpusTextEmoji = []rune{
		0x23, 0x2A, 0x30, 0x39, 0xA9, 0xAE, 0x203C, 0x2049, 0x2122, 0x2139, 0x2194, 0x21A9, 0x231A, 0x231B,
		0x2328, 0x23CF, 0x23E9, 0x23F3, 0x23F9, 0x24C2, 0x25AA, 0x25B6, 0x25FB, 0x25FE, 0x2600, 0x2604,
		0x260E, 0x2611, 0x2614, 0x2615, 0x2618, 0x2648, 0x2653, 0x267F, 0x2693, 0x26A1, 0x26BD, 0x26C4,
		0x26D4, 0x26EA, 0x26F2, 0x26FD, 0x2702, 0x2705, 0x2728, 0x274C, 0x2753, 0x2757, 0x2795, 0x27B0,
		0x2934, 0x2B05, 0x2B50, 0x2B55, 0x3030, 0x303D, 0x3297, 0x3299, 0x1F170, 0x1F17F, 0x1F321, 0x1F37D,
		0x1F39E, 0x1F3CC, 0x1F3D4, 0x1F3F5, 0x1F43F, 0x1F441, 0x1F4FD, 0x1F549, 0x1F56F, 0x1F587, 0x1F5A5,
		0x1F5E3, 0x1F6CB, 0x1F6E0, 0x1F6F0,
	}

	corpusHangulL   = []rune{0x1100, 0x1112, 0x115F, 0xA960, 0xA97C}
	corpusHangulV   = []rune{0x1160, 0x1161, 0x1175, 0x11A7, 0xD7B0, 0xD7C6}
	corpusHangulT   = []rune{0x11A8, 0x11C2, 0x11FF, 0xD7CB, 0xD7FB}
	corpusHangulLV  = []rune{0xAC00, 0xAC1C, 0xB300, 0xD788}
	corpusHangulLVT = []rune{0xAC01, 0xAC1B, 0xD7A3, 0xB9D0}

	// Indic and South-East Asian scripts: consonants, viramas (the linkers of
	// GB9c), nuktas, vowel signs that are spacing marks, and prepended marks.
	corpusIndicConsonants = []rune{
		0x915, 0x937, 0x924, 0x930, 0x939, 0x995, 0x9B7, 0x9B0, 0xA95, 0xAB0, 0xB15, 0xB30, 0xC15, 0xC37,
		0xD15, 0xD30, 0x1000, 0x101B, 0x1780, 0x1784, 0x179B, 0x17A0, 0x17A2, 0x1B13, 0x1B2D, 0xA98F,
		0x11103, 0x11392, 0x11F04, 0xD95, 0xDBB, 0xE01, 0xE81, 0xF40, 0xB95, 0xBB0, 0xC95, 0xCB0,
	}
	corpusIndicLinkers = []rune{
		0x94D, 0x9CD, 0xACD, 0xB4D, 0xC4D, 0xD4D, 0x1039, 0x17D2, 0x1A60, 0x1B44, 0x1BAB, 0xA9C0, 0xAAF6,
		0x10A3F, 0x11133, 0x113D0, 0x1193E, 0x11A47, 0x11A99, 0x11F42, 0xA4D, 0xBCD, 0xCCD, 0xDCA, 0xE3A,
		0xF84, 0x103A, 0x1714, 0x1B44, 0xA806, 0x11046, 0x110B9,
	}
	corpusIndicMarks = []rune{
		0x900, 0x902, 0x903, 0x93C, 0x93E, 0x93F, 0x940, 0x941, 0x948, 0x94B, 0x94E, 0x951, 0x9BC, 0x9BE,
		0x9BF, 0x9C7, 0x9CB, 0x9D7, 0xA3C, 0xABE, 0xB3E, 0xB57, 0xBBE, 0xBC6, 0xBCA, 0xBD7, 0xC3E, 0xC41,
		0xC46, 0xCBE, 0xCC0, 0xCC2, 0xCC7, 0xCCA, 0xCD5, 0xD3E, 0xD46, 0xD4E, 0xD57, 0xDCF, 0xDD9, 0xE31,
		0xE33, 0xE34, 0xE47, 0xEB1, 0xEB3, 0xF3E, 0xF71, 0xF7F, 0x102B, 0x102D, 0x1031, 0x1036, 0x17B6,
		0x17B7, 0x17BE, 0x17C1, 0x17C6, 0x17D3, 0x17DD, 0x1B35, 0x1B3B, 0x1B40, 0xA8FA, 0xA8FF, 0x11000,
		0x11038, 0x110C2, 0x11A89, 0x11A84, 0x11D46, 0x11F02, 0x16D63, 0x16D67, 0x16D68, 0x16D69, 0x16D6A,
		0x1E08F, 0x17A4, 0x17D4, 0x17D8, 0x179B,
	}

	// Arabic: letters of the Lam and Alef joining groups, transparent marks,
	// prepended concatenation marks; Hebrew; Syriac.
	corpusArabicLam  = []rune{0x644, 0x6B5, 0x6B6, 0x6B7, 0x6B8, 0x76A, 0x8A6, 0x8C7}
	corpusArabicAlef = []rune{0x622, 0x623, 0x625, 0x627, 0x671, 0x672, 0x673, 0x675, 0x773, 0x774, 0x870, 0x882}
	corpusArabic     = []rune{
		0x600, 0x601, 0x604, 0x605, 0x60C, 0x61B, 0x621, 0x628, 0x62D, 0x645, 0x647, 0x649, 0x64A, 0x660,
		0x669, 0x66B, 0x66C, 0x6DD, 0x6F0, 0x6F9, 0x70F, 0x710, 0x712, 0x890, 0x891, 0x8E2, 0xFDFD, 0xFE8E,
		0xFEFB, 0xFEFC, 0x5D0, 0x5D1, 0x5DC, 0x5E9, 0x5F3, 0x5F4, 0xFB1D, 0xFB4F, 0x7C0, 0x7CA, 0x10E60,
		0x1EE00, 0x1EC71,
	}

	// The scripts with ligatures of their own in unicode-width.
	corpusLigatureScripts = []rune{
		0xA4F8, 0xA4F9, 0xA4FB, 0xA4FC, 0xA4FD, 0xA4EA, 0xA4D0,
		0x1A15, 0x1A17, 0x1A10, 0x1A00, 0x1A18,
		0x2D31, 0x2D3E, 0x2D4F, 0x2D65, 0x2D66, 0x2D6F, 0x2D7F, 0x2D30,
		0x10C32, 0x10C03, 0x10C00,
	}

	corpusWide = []rune{
		0x1100, 0x2E80, 0x2FF0, 0x3000, 0x3001, 0x3042, 0x3093, 0x30A2, 0x30F3, 0x30FB, 0x30FC, 0x3105,
		0x3131, 0x3200, 0x3400, 0x4DBF, 0x4DC0, 0x4E00, 0x4E2D, 0x6587, 0x9FFF, 0xA000, 0xA4C6, 0xAC00,
		0xF900, 0xFE10, 0xFE30, 0xFE50, 0xFF01, 0xFF0C, 0xFF0E, 0xFF10, 0xFF21, 0xFF41, 0xFF5E, 0xFF61,
		0xFF66, 0xFF71, 0xFF9D, 0xFF9E, 0xFFDC, 0xFFE0, 0xFFE6, 0xFFE8, 0x16FE0, 0x16FE4, 0x17000, 0x18000,
		0x18CFF, 0x1AFF0, 0x1B000, 0x1B132, 0x1B170, 0x1D300, 0x1F200, 0x20000, 0x2A6DF, 0x2F800, 0x30000,
		0x3134A, 0x3FFFD, 0x18D00, 0x16D40, 0x16100, 0x1E5D0, 0x10D40, 0x1CC00, 0x1CCF0,
	}

	// The characters word boundaries turn on.
	corpusWordBreak = []rune{
		'\'', '"', '.', ',', ';', ':', '_', '-', '0', '1', '9', 'a', 'Z', ' ', ' ',
		0xB7, 0x2018, 0x2019, 0x2024, 0x2027, 0x202F, 0x203F, 0x2040, 0x2054, 0x2044, 0x37E, 0x387, 0x55F,
		0x589, 0x5F3, 0x5F4, 0x60C, 0x60D, 0x66B, 0x66C, 0x7F8, 0xFE10, 0xFE13, 0xFE14, 0xFE33, 0xFE4D,
		0xFE50, 0xFE52, 0xFE54, 0xFE55, 0xFF07, 0xFF0C, 0xFF0E, 0xFF1A, 0xFF1B, 0xFF3F, 0x1680, 0x2000,
		0x2006, 0x2008, 0x200A, 0x205F, 0x3000, 0x3031, 0x3035, 0x309B, 0x30A0, 0x30FF, 0x31F0, 0x32D0,
		0xFF70, 0x1B000, 0x1AFF0, 0x2D0, 0x2D7, 0x2DE, 0x5D0, 0x5EA, 0xFB1F, 0x1D7CE, 0x1F100, 0xE01, 0xE50,
		0x1D400, 0x2071, 0x24B6, 0xAA, 0xB5, 0xBA, 0xB2, 0xBC, 0x2160, 0x3007, 0x4E00,
	}

	// Sequences from the tests and documentation of the two crates.
	corpusSeeds = []string{
		"a̐éö̲\r\n",
		"\U0001F468‍\U0001F467‍\U0001F466",
		"\U0001F469‍\U0001F52C",
		"\U0001F9D1‍\U0001F91D‍\U0001F9D1",
		"\U0001F469\U0001F3FD‍❤️‍\U0001F48B‍\U0001F468\U0001F3FB",
		"\U0001F3F4\U000E0067\U000E0062\U000E0077\U000E006C\U000E0073\U000E007F",
		"\U0001F3F4\U000E0067\U000E0062\U000E0065\U000E006E\U000E0067\U000E007F",
		"\U0001F3F4‍☠️",
		"\U0001F3F3️‍\U0001F308",
		"\U0001F1EE\U0001F1F1\U0001F54A️\U0001F1F5\U0001F1F8",
		"\U0001F1F5\U0001F1F8‍\U0001F54A️‍\U0001F1EE\U0001F1F1",
		"\U0001F1E6\U0001F1E6‍\U0001F1E6\U0001F1E6\U0001F1E6",
		"*️⃣", "#️⃣", "1⃣", "*⃣️‍\U0001F46A",
		"♈︎", "♈️︎", "\U0001F21A︎", "Ⓜ️",
		"\U0001F938\U0001F3FE\U0001F3FE",
		"क्ष", "क्‍ष", "क़्ति", "हिन्दी",
		"لا", "لٟٞأ", "ڸ̸ا", "≠ا", "≮", "≠́",
		"א‍ל", "א️‍️ל️",
		"ᨕᨗ‍ᨐ", "ⵏ⵿ⴾ", "ⵏ‍ⴾ", "\U00010C32‍\U00010C03",
		"ꓹꓼ", "ꓪꓹꓼ",
		"ល្ង", "។ល។", "អឦ",
		"\U00016D67\U00016D67", "\U00016D63\U00016D67", "\U00016D68\U00016D67", "\U00016D69",
		"각", "각", "ᅟᅠ",
		"‘︀", "“︁", "’︂︁",
		"\r\n", "\n\r\n\r", "\r️\n", "\r‍\n", "1\t2\r\n3\u00854",
		"The quick (\"brown\") fox can't jump 32.3 feet, right?",
		"Brr, it's 29.3°F!", "e.g. var1 123,456 foo_bar example.com 127.0.0.1:9090",
		"א\"א", "א'", "a­:̀b", "3‍,‍4",
		"アート_カタカナ", "؀؀ ", " ؀ ",
		"ൎഉ", "\U00011A89",
		"ｈｅｌｌｏ", "₁₂₃₄", "“\U00018000”",
	}
)

// corpusAppendUnit appends one unit, a character or one of the sequences the
// rules know, to out.
func (r *corpusRand) corpusAppendUnit(out []rune) []rune {
	switch n := r.intn(1000); {
	case n < 150:
		return append(out, r.pick(corpusASCII))
	case n < 185:
		// A short ASCII word, for the word rules.
		for count := 1 + r.intn(5); count > 0; count-- {
			out = append(out, r.pick(corpusASCII[:62]))
		}
		return out
	case n < 215:
		if r.chance(40) {
			return append(out, '\r', '\n')
		}
		return append(out, r.pick(corpusControls))
	case n < 245:
		return append(out, r.pick(corpusAmbiguous))
	case n < 295:
		return append(out, r.pick(corpusCombining))
	case n < 345:
		return append(out, r.pick(corpusVariationSelectors))
	case n < 395:
		return append(out, r.pick(corpusInvisible))
	case n < 435:
		return append(out, r.pick(corpusEmoji))
	case n < 465:
		return append(out, r.pick(corpusTextEmoji))
	case n < 525:
		return r.corpusAppendEmojiSequence(out)
	case n < 555:
		// Flags: a run of regional indicators, of even or odd length.
		for count := 1 + r.intn(5); count > 0; count-- {
			out = append(out, r.between(0x1F1E6, 0x1F1FF))
			if r.chance(8) {
				out = append(out, r.pick(corpusInvisible))
			}
		}
		return out
	case n < 575:
		return r.corpusAppendTagSequence(out)
	case n < 615:
		return r.corpusAppendHangul(out)
	case n < 685:
		return r.corpusAppendIndic(out)
	case n < 735:
		return r.corpusAppendArabic(out)
	case n < 775:
		return r.corpusAppendLigature(out)
	case n < 815:
		return append(out, r.pick(corpusWide))
	case n < 875:
		return append(out, r.pick(corpusWordBreak))
	case n < 890:
		// A whole seed sequence.
		return append(out, []rune(corpusSeeds[r.intn(len(corpusSeeds))])...)
	case n < 905:
		// A repeat of what the string holds already.
		if len(out) > 0 {
			start := r.intn(len(out))
			end := start + 1 + r.intn(len(out)-start)
			return append(out, append([]rune(nil), out[start:end]...)...)
		}
		return out
	default:
		return append(out, r.scalar())
	}
}

// An emoji sequence: elements joined by zero width joiners, each a pictograph
// with an optional modifier or variation selector. Parts are sometimes left
// out or doubled so that malformed sequences occur as well.
func (r *corpusRand) corpusAppendEmojiSequence(out []rune) []rune {
	for elements := 1 + r.intn(4); elements > 0; elements-- {
		switch r.intn(10) {
		case 0, 1, 2, 3, 4:
			out = append(out, r.pick(corpusEmoji))
		case 5, 6:
			out = append(out, r.pick(corpusTextEmoji))
		case 7:
			out = append(out, r.between(0x1F1E6, 0x1F1FF), r.between(0x1F1E6, 0x1F1FF))
		case 8:
			out = append(out, r.pick([]rune("#*0123456789")))
			if r.chance(80) {
				out = append(out, 0xFE0F)
			}
			out = append(out, 0x20E3)
		default:
			out = r.corpusAppendTagSequence(out)
		}
		if r.chance(35) {
			out = append(out, r.between(0x1F3FB, 0x1F3FF))
		}
		if r.chance(35) {
			out = append(out, r.pick([]rune{0xFE0F, 0xFE0F, 0xFE0F, 0xFE0E, 0xFE00, 0x301}))
		}
		if elements > 1 {
			if r.chance(92) {
				out = append(out, 0x200D)
			}
			if r.chance(6) {
				out = append(out, 0x200D)
			}
		}
	}
	return out
}

// A tag sequence: a black flag, tag characters, the cancel tag. The number of
// tag letters and digits varies around what a subdivision flag has.
func (r *corpusRand) corpusAppendTagSequence(out []rune) []rune {
	if r.chance(90) {
		out = append(out, 0x1F3F4)
	}
	for letters := r.intn(8); letters > 0; letters-- {
		if r.chance(70) {
			out = append(out, r.between(0xE0061, 0xE007A))
		} else {
			out = append(out, r.between(0xE0030, 0xE0039))
		}
	}
	if r.chance(85) {
		out = append(out, 0xE007F)
	}
	return out
}

func (r *corpusRand) corpusAppendHangul(out []rune) []rune {
	for count := 1 + r.intn(4); count > 0; count-- {
		switch r.intn(6) {
		case 0:
			out = append(out, r.pick(corpusHangulL))
		case 1:
			out = append(out, r.pick(corpusHangulV))
		case 2:
			out = append(out, r.pick(corpusHangulT))
		case 3:
			out = append(out, r.pick(corpusHangulLV))
		case 4:
			out = append(out, r.pick(corpusHangulLVT))
		default:
			out = append(out, r.pick(corpusHangulL), r.pick(corpusHangulV), r.pick(corpusHangulT))
		}
	}
	return out
}

// An Indic syllable: consonants joined by viramas, with nuktas, joiners and
// vowel signs in and around them.
func (r *corpusRand) corpusAppendIndic(out []rune) []rune {
	if r.chance(10) {
		out = append(out, r.pick([]rune{0x600, 0x605, 0x6DD, 0x70F, 0x8E2, 0x110BD, 0xD4E, 0x111C2, 0x11A3A, 0x11D46}))
	}
	for consonants := 1 + r.intn(3); consonants > 0; consonants-- {
		if r.chance(85) {
			out = append(out, r.pick(corpusIndicConsonants))
		} else {
			out = append(out, r.pick(corpusASCII))
		}
		if r.chance(15) {
			out = append(out, r.pick(corpusIndicMarks))
		}
		if consonants > 1 || r.chance(20) {
			for linkers := 1 + r.intn(2); linkers > 0; linkers-- {
				if r.chance(90) {
					out = append(out, r.pick(corpusIndicLinkers))
				}
				if r.chance(20) {
					out = append(out, r.pick([]rune{0x200D, 0x200C, 0x93C, 0x300, 0xFE0F}))
				}
			}
		}
	}
	for marks := r.intn(3); marks > 0; marks-- {
		out = append(out, r.pick(corpusIndicMarks))
	}
	return out
}

func (r *corpusRand) corpusAppendArabic(out []rune) []rune {
	switch r.intn(4) {
	case 0, 1:
		// Lam, transparent characters, Alef.
		out = append(out, r.pick(corpusArabicLam))
		for marks := r.intn(3); marks > 0; marks-- {
			out = append(out, r.pick([]rune{0x64B, 0x65E, 0x65F, 0x670, 0x338, 0x200D, 0xFE0E, 0xFE0F, 0x17B5, 0x1E94B, 0x605, 0x8E2}))
		}
		return append(out, r.pick(corpusArabicAlef))
	case 2:
		// Alef, zero width joiners, Lamed.
		out = append(out, 0x5D0)
		for joiners := r.intn(3); joiners > 0; joiners-- {
			out = append(out, r.pick([]rune{0x200D, 0x200D, 0x200D, 0xFE0F, 0xFE0E, 0x5B0}))
		}
		return append(out, 0x5DC)
	default:
		return append(out, r.pick(corpusArabic))
	}
}

func (r *corpusRand) corpusAppendLigature(out []rune) []rune {
	transparent := []rune{0x200D, 0x200D, 0xFE0F, 0xFE0E, 0x17B5, 0x301, 0x338}
	maybe := func() {
		if r.chance(25) {
			out = append(out, r.pick(transparent))
		}
	}
	switch r.intn(8) {
	case 0:
		// Lisu tone letters.
		out = append(out, r.between(0xA4F8, 0xA4FD))
		maybe()
		return append(out, r.between(0xA4F8, 0xA4FD))
	case 1:
		// Buginese <a, -i> ya.
		out = append(out, 0x1A15)
		maybe()
		out = append(out, 0x1A17)
		maybe()
		if r.chance(85) {
			out = append(out, 0x200D)
		}
		maybe()
		return append(out, 0x1A10)
	case 2:
		// Tifinagh bi-consonants.
		out = append(out, r.pick([]rune{0x2D31, 0x2D3E, 0x2D4F, 0x2D65, 0x2D6F, 0x2D66}))
		maybe()
		out = append(out, r.pick([]rune{0x2D7F, 0x2D7F, 0x200D}))
		maybe()
		return append(out, r.pick([]rune{0x2D31, 0x2D3E, 0x2D4F, 0x2D65, 0x2D6F, 0x2D66, 0x2D7F}))
	case 3:
		// Old Turkic.
		out = append(out, 0x10C32)
		maybe()
		if r.chance(85) {
			out = append(out, 0x200D)
		}
		maybe()
		return append(out, 0x10C03)
	case 4:
		// Khmer coeng.
		if r.chance(60) {
			out = append(out, r.between(0x1780, 0x17B3))
		}
		out = append(out, 0x17D2)
		maybe()
		return append(out, r.between(0x1780, 0x17B3))
	case 5:
		// Kirat Rai vowel signs.
		for count := 1 + r.intn(3); count > 0; count-- {
			out = append(out, r.between(0x16D63, 0x16D6A))
			maybe()
		}
		return out
	case 6:
		// The solidus overlay after a comparison sign.
		out = append(out, r.pick([]rune{'<', '=', '>', '#', 0x2264, 0x644}))
		for marks := r.intn(3); marks > 0; marks-- {
			out = append(out, r.pick([]rune{0x301, 0x327, 0x334, 0xFE0F, 0xFE00, 0x200D, 0x20D2, 0x34F}))
		}
		out = append(out, 0x338)
		if r.chance(30) {
			out = append(out, r.pick(corpusArabicAlef))
		}
		return out
	default:
		return append(out, r.pick(corpusLigatureScripts))
	}
}

// corpusString builds one string of up to maxUnits units.
func (r *corpusRand) corpusString(maxUnits int) string {
	var out []rune
	for units := r.intn(maxUnits + 1); units > 0; units-- {
		out = r.corpusAppendUnit(out)
	}
	// Width is computed from the end of the string and reverse iteration
	// starts there, so the same characters in the opposite order are a
	// different case.
	if r.chance(5) {
		for i, j := 0, len(out)-1; i < j; i, j = i+1, j-1 {
			out[i], out[j] = out[j], out[i]
		}
	}
	return string(out)
}

// corpusFixed is the cases every run starts with: the empty string, single
// characters of every kind above and the seed sequences.
func corpusFixed() []string {
	fixed := []string{"", " ", "a", "\n", "\r", "\r\n", "\t", "\x00", "\x7f"}
	fixed = append(fixed, corpusSeeds...)
	for _, pool := range [][]rune{
		corpusControls, corpusAmbiguous, corpusCombining, corpusVariationSelectors, corpusInvisible,
		corpusTextEmoji, corpusIndicLinkers, corpusArabicLam, corpusArabicAlef, corpusLigatureScripts,
	} {
		fixed = append(fixed, string(pool))
	}
	return fixed
}

// corpusLong is the long inputs: thousands of characters of one kind, and of
// every kind mixed.
func corpusLong() []string {
	long := []string{
		strings.Repeat("a", 5000),
		strings.Repeat("hello, world! ", 400),
		strings.Repeat("\u0301", 3000),
		strings.Repeat("\U0001F1E6", 1001),
		strings.Repeat("\U0001F468\u200D", 700) + "\U0001F466",
		strings.Repeat("\u0915\u094D", 800) + "\u0937",
		strings.Repeat("\u4E2D\u6587", 1500),
		strings.Repeat("\r\n", 1200),
		"a" + strings.Repeat("\u200D\uFE0F\u0300", 900) + "b",
	}
	r := &corpusRand{state: 0x6C6F6E67}
	for range 6 {
		var out []rune
		for len(out) < 1500 {
			out = r.corpusAppendUnit(out)
		}
		long = append(long, string(out))
	}
	return long
}

// corpusStrings returns count generated strings for a seed.
func corpusStrings(seed uint64, count int) []string {
	r := &corpusRand{state: seed}
	generated := make([]string, 0, count)
	for range count {
		maxUnits := 10
		if r.chance(10) {
			maxUnits = 30
		}
		generated = append(generated, r.corpusString(maxUnits))
	}
	return generated
}

const (
	// corpusRecorded is the number of generated strings in the recorded
	// corpus, which every `go test` replays.
	corpusRecorded = 20000
	// corpusLive is the number of further strings, from other seeds, that a
	// run with MJ_ORACLE=live compares as well.
	corpusLive = 180000
	// corpusBatch is the number of strings in one oracle request.
	corpusBatch = 25
)

// corpusBatches yields the corpus in batches: the fixed cases and the
// recorded strings always, and the live strings when the test runs against
// the Rust oracle itself. Each long input is a batch of its own, so that a
// difference elsewhere is not reported together with thousands of characters.
func corpusBatches(yield func(batch []string)) {
	all := append(corpusFixed(), corpusStrings(0x756E69636F6465, corpusRecorded)...)
	if os.Getenv("MJ_ORACLE") == "live" {
		all = append(all, corpusStrings(0x6C697665, corpusLive)...)
	}
	for len(all) > 0 {
		size := min(corpusBatch, len(all))
		yield(all[:size])
		all = all[size:]
	}
	for _, long := range corpusLong() {
		yield([]string{long})
	}
}
