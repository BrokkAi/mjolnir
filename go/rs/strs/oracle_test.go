package strs

import (
	"encoding/hex"
	"encoding/json"
	"math/rand/v2"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// These tests hold the package against the Rust standard library itself,
// through the oracle module go/port/oracle/src/std_str.rs. Each reply below
// is built the way that module builds it.

// oracleRequest is every member a std_str request can have.
type oracleRequest struct {
	Op    string   `json:"op"`
	F     string   `json:"f"`
	S     string   `json:"s"`
	A     uint64   `json:"a"`
	B     uint64   `json:"b"`
	N     uint64   `json:"n"`
	I     uint64   `json:"i"`
	Mid   uint64   `json:"mid"`
	Start uint32   `json:"start"`
	End   uint32   `json:"end"`
	Radix uint32   `json:"radix"`
	C     rune     `json:"c"`
	Cs    []rune   `json:"cs"`
	Ns    []uint32 `json:"ns"`
	U     []uint16 `json:"u"`
	To    string   `json:"to"`
	With  string   `json:"with"`
	Keep  string   `json:"keep"`
	Bytes string   `json:"b_hex"`
	Pat   *struct {
		S  *string `json:"s"`
		C  *rune   `json:"c"`
		Cs []rune  `json:"cs"`
		F  *string `json:"f"`
	} `json:"pat"`
}

func decodeRequest(t *testing.T, raw json.RawMessage) (oracleRequest, map[string]json.RawMessage) {
	t.Helper()
	var request oracleRequest
	// "a" and "b" are numbers in most requests and hex strings in "bytes";
	// decode those by hand.
	var members map[string]json.RawMessage
	if err := json.Unmarshal(raw, &members); err != nil {
		t.Fatalf("request %s: %v", raw, err)
	}
	var op string
	_ = json.Unmarshal(members["op"], &op)
	if op == "bytes" || op == "from_utf8" || op == "pair" {
		request.Op = op
		return request, members
	}
	if err := json.Unmarshal(raw, &request); err != nil {
		t.Fatalf("request %s: %v", raw, err)
	}
	return request, members
}

func text(t *testing.T, members map[string]json.RawMessage, key string) string {
	t.Helper()
	var value string
	if raw, ok := members[key]; ok {
		if err := json.Unmarshal(raw, &value); err != nil {
			t.Fatalf("member %s: %v", key, err)
		}
	}
	return value
}

func unhex(t *testing.T, text string) []byte {
	t.Helper()
	decoded, err := hex.DecodeString(text)
	if err != nil {
		t.Fatal(err)
	}
	return decoded
}

// list is a Rust Vec in JSON: never null.
func list[T any](items []T) []T {
	if items == nil {
		return []T{}
	}
	return items
}

func option[T any](o rs.Option[T]) any {
	if value, ok := o.Get(); ok {
		return value
	}
	return nil
}

func halves(o rs.Option[rs.Tuple2[string, string]]) any {
	if pair, ok := o.Get(); ok {
		return []string{pair.F0, pair.F1}
	}
	return nil
}

func indexPairs(items []rs.Tuple2[int, string]) []any {
	pairs := []any{}
	for _, item := range items {
		pairs = append(pairs, []any{item.F0, item.F1})
	}
	return pairs
}

// errorReply is the oracle's `{"err": <Display>, "debug": <Debug>}`.
func errorReply(err error) map[string]any {
	debug, ok := err.(interface{ debugString() string })
	if !ok {
		return map[string]any{"err": err.Error(), "debug": "<no debugString>"}
	}
	return map[string]any{"err": err.Error(), "debug": debug.debugString()}
}

func result[T any](value T, err error) any {
	if err != nil {
		return errorReply(err)
	}
	return map[string]any{"ok": value}
}

func predicate(t *testing.T, name string) func(rune) bool {
	t.Helper()
	switch name {
	case "is_whitespace":
		return IsWhitespace
	case "is_alphabetic":
		return IsAlphabetic
	case "is_numeric":
		return IsNumeric
	case "is_alphanumeric":
		return IsAlphanumeric
	case "is_uppercase":
		return IsUppercase
	case "is_lowercase":
		return IsLowercase
	case "is_control":
		return IsControl
	case "is_ascii_digit":
		return IsAsciiDigit[rune]
	case "is_ascii_punctuation":
		return IsAsciiPunctuation[rune]
	case "is_ascii":
		return CharIsAscii[rune]
	case "not_ascii":
		return func(c rune) bool { return !CharIsAscii(c) }
	case "always":
		return func(rune) bool { return true }
	case "never":
		return func(rune) bool { return false }
	}
	t.Fatalf("unknown predicate %s", name)
	return nil
}

func flags(set ...bool) uint32 {
	var mask uint32
	for bit, on := range set {
		if on {
			mask |= 1 << bit
		}
	}
	return mask
}

func classMask(c rune) uint32 {
	return flags(
		IsAlphabetic(c),
		IsNumeric(c),
		IsAlphanumeric(c),
		IsWhitespace(c),
		IsLowercase(c),
		IsUppercase(c),
		IsControl(c),
		CharIsAscii(c),
		IsAsciiAlphabetic(c),
		IsAsciiDigit(c),
		IsAsciiAlphanumeric(c),
		IsAsciiHexdigit(c),
		IsAsciiPunctuation(c),
		IsAsciiGraphic(c),
		IsAsciiWhitespace(c),
		IsAsciiControl(c),
		IsAsciiUppercase(c),
		IsAsciiLowercase(c),
		CharsCount(CharEscapeDebug(c)) != 1,
		CharsCount(EscapeDebug("a" + string(c))[1:]) != 1,
		CharsCount(CharEscapeDefault(c)) != 1,
	)
}

func answerChars(t *testing.T, raw json.RawMessage) any {
	request, _ := decodeRequest(t, raw)
	switch request.Op {
	case "char_class":
		runs := []uint32{}
		for code := request.Start; code < request.End; code++ {
			var mask uint32
			if CharFromU32(code).IsSome() {
				mask = classMask(rune(code))
			}
			if n := len(runs); n >= 2 && runs[n-2] == mask {
				runs[n-1]++
			} else {
				runs = append(runs, mask, 1)
			}
		}
		return runs
	case "char_case":
		changed := []any{}
		for code := request.Start; code < request.End; code++ {
			if CharFromU32(code).IsNone() {
				continue
			}
			c := rune(code)
			lower, upper := CharToLowercase(c), CharToUppercase(c)
			if len(lower) != 1 || lower[0] != c || len(upper) != 1 || upper[0] != c {
				changed = append(changed, []any{c, lower, upper, CharToAsciiLowercase(c), CharToAsciiUppercase(c)})
			}
		}
		return changed
	case "char_misc":
		replies := []any{}
		for _, c := range request.Cs {
			replies = append(replies, map[string]any{
				"len_utf8":       CharLenUtf8(c),
				"len_utf16":      CharLenUtf16(c),
				"utf8":           hex.EncodeToString([]byte(CharEncodeUtf8(c))),
				"utf16":          CharEncodeUtf16(c),
				"escape_debug":   CharEscapeDebug(c),
				"escape_default": CharEscapeDefault(c),
				"escape_unicode": CharEscapeUnicode(c),
				"debug":          charDebug(c),
				"lower":          CharToLowercase(c),
				"upper":          CharToUppercase(c),
				"ascii_lower":    CharToAsciiLowercase(c),
				"ascii_upper":    CharToAsciiUppercase(c),
			})
		}
		return replies
	case "digit":
		return oracle.Catch(func() any {
			replies := []any{}
			switch request.F {
			case "to_digit":
				for _, c := range request.Cs {
					replies = append(replies, option(CharToDigit(c, request.Radix)))
				}
			case "is_digit":
				for _, c := range request.Cs {
					replies = append(replies, CharIsDigit(c, request.Radix))
				}
			case "from_digit":
				for _, n := range request.Ns {
					replies = append(replies, option(CharFromDigit(n, request.Radix)))
				}
			case "from_u32":
				for _, n := range request.Ns {
					replies = append(replies, option(CharFromU32(n)))
				}
			default:
				t.Fatalf("unknown digit request %s", raw)
			}
			return replies
		})
	}
	t.Fatalf("unknown request %s", raw)
	return nil
}

func TestDiffAgainstRust_chars(t *testing.T) {
	oracle.Check(t, "std_str", "chars", func(yield func(any)) {
		// Every code point, for the classes, the escapes and the case
		// mappings.
		for start := 0; start < 0x110000; start += 0x8000 {
			yield(map[string]any{"op": "char_class", "start": start, "end": start + 0x8000})
		}
		for start := 0; start < 0x110000; start += 0x10000 {
			yield(map[string]any{"op": "char_case", "start": start, "end": start + 0x10000})
		}
		yield(map[string]any{"op": "char_misc", "cs": interestingChars})
		random := rand.New(rand.NewPCG(11, 1))
		for i := 0; i < 40; i++ {
			cs := make([]rune, 60)
			for j := range cs {
				cs[j] = randomChar(random)
			}
			yield(map[string]any{"op": "char_misc", "cs": cs})
		}
		// Digits in every radix, including the radixes Rust panics on.
		var printable []rune
		for c := rune(0); c < 0x180; c++ {
			printable = append(printable, c)
		}
		printable = append(printable, 0x660, 0x661, 0xFF10, 0xFF11, 0xFF21, 0xFF41, 0x10FFFF)
		for radix := 0; radix <= 38; radix++ {
			yield(map[string]any{"op": "digit", "f": "to_digit", "cs": printable, "radix": radix})
			yield(map[string]any{"op": "digit", "f": "is_digit", "cs": printable, "radix": radix})
			yield(map[string]any{"op": "digit", "f": "from_digit", "radix": radix,
				"ns": []uint32{0, 1, 9, 10, 11, 15, 16, 35, 36, 37, 255, 256, 1 << 31, 1<<32 - 1}})
		}
		yield(map[string]any{"op": "digit", "f": "from_u32", "ns": []uint32{
			0, 0x7F, 0x80, 0xD7FF, 0xD800, 0xDBFF, 0xDC00, 0xDFFF, 0xE000, 0xFFFF, 0x10000, 0x10FFFF, 0x110000,
			0x7FFFFFFF, 0x80000000, 0xFFFFFFFF,
		}})
	}, func(raw json.RawMessage) any { return answerChars(t, raw) })
}

// interestingChars are the chars the escaping and case code branch on.
var interestingChars = []rune{
	0, 1, 7, 8, '\t', '\n', 0x0B, 0x0C, '\r', 0x1B, 0x1F, ' ', '!', '"', '\'', '\\', '0', '9', 'A', 'Z', 'a', 'z',
	'~', 0x7F, 0x80, 0x85, 0x9F, 0xA0, 0xA9, 0xAA, 0xAC, 0xAD, 0xB5, 0xBF, 0xC0, 0xDF, 0xFF, 0x130, 0x131, 0x149,
	0x178, 0x1C4, 0x1C5, 0x1C6, 0x1F0, 0x2FF, 0x300, 0x301, 0x345, 0x377, 0x378, 0x390, 0x3A3, 0x3C2, 0x3C3, 0x587,
	0x600, 0x61C, 0x7FF, 0x800, 0x1680, 0x180E, 0x1E9E, 0x1F88, 0x1FB3, 0x2000, 0x200B, 0x200D, 0x2028, 0x2029,
	0x202E, 0x205F, 0x2060, 0x2126, 0x212A, 0x3000, 0x3042, 0x4E2D, 0xA7C6, 0xAB70, 0xD7FF, 0xE000, 0xF8FF,
	0xFB00, 0xFB03, 0xFE0F, 0xFEFF, 0xFF9E, 0xFF9F, 0xFFFD, 0xFFFE, 0xFFFF, 0x10000, 0x10400, 0x1F600, 0x1F1E6,
	0x2FFFF, 0x3FFFD, 0x3FFFE, 0xE0001, 0xE0020, 0xE0100, 0xE01EF, 0xE01F0, 0xF0000, 0xFFFFD, 0x100000,
	0x10FFFD, 0x10FFFE, 0x10FFFF,
}

// randomChar draws a char, favouring the ranges where classes change.
func randomChar(random *rand.Rand) rune {
	for {
		var c rune
		switch random.IntN(6) {
		case 0:
			c = rune(random.IntN(0x80))
		case 1:
			c = rune(random.IntN(0x800))
		case 2:
			c = rune(random.IntN(0x3000))
		case 3:
			c = rune(random.IntN(0x10000))
		case 4:
			c = rune(0x10000 + random.IntN(0x20000))
		default:
			c = rune(random.IntN(0x110000))
		}
		if CharFromU32(uint32(c)).IsSome() {
			return c
		}
	}
}

// alphabets are the char pools random strings are drawn from. Small pools
// make repeats, which is what the searching and splitting code needs.
var alphabets = [][]rune{
	[]rune("ab"),
	[]rune("abc "),
	[]rune("aé"),
	[]rune("aΣσς. "),
	[]rune("a\n\r"),
	[]rune(" \t\n\v\f\r\u0085\u00a0\u1680\u2003\u2028\u2029\u3000x"),
	[]rune("abcXYZ019_-.,;:/\\'\"!?"),
	[]rune("ßİŉǅﬁΐǰẞΣ"),
	[]rune("e\u0301\u0308\u200d\ufe0f"),
	[]rune("日本語中文"),
	[]rune("😀👍🏽a"),
	{0, 1, 0x7F, 0x80, 0x7FF, 0x800, 0xD7FF, 0xE000, 0xFFFD, 0xFFFF, 0x10000, 0x10FFFF},
}

func randomString(random *rand.Rand, maxChars int) string {
	alphabet := alphabets[random.IntN(len(alphabets))]
	if random.IntN(8) == 0 {
		// Any chars at all.
		alphabet = nil
	}
	var text strings.Builder
	for n := random.IntN(maxChars + 1); n > 0; n-- {
		if alphabet == nil {
			text.WriteRune(randomChar(random))
		} else {
			text.WriteRune(alphabet[random.IntN(len(alphabet))])
		}
	}
	return text.String()
}

// fixedStrings are the edge cases every string test starts with.
var fixedStrings = []string{
	"", " ", "a", "é", "\n", "\r\n", "\r", "a\r", "\n\n", "a\nb", "a\r\nb\r\n", "a\n\rb", "a\rb\n", "\r\r\n",
	"  hello  world  ", "\t\n\v\f\r x \t\n\v\f\r", "\u00a0x\u00a0", "\u2028a\u2029", "\u3000全角\u3000",
	"\u0085next\u0085", "\u200bzero width\u200b", "\ufeffbom", "Hello, World!", "ΑΣ", "ΑΣ ", "ΑΣΑ", "Σ", "ΣΣ",
	"ΑΣ.", "Α.Σ", "Α'Σ'", "ΑΣ\u0301", "Α\u0301Σ", "aΣ", "1Σ", " Σ", "ΌΣΟΣ ΌΣΟΣ", "ΑΣ\u00adΑ", "ǅΣ", "ʰΣ",
	"Straße", "İstanbul", "ıi", "ŉ", "ǰ", "ﬁﬂﬃ", "ΐΰ", "ᾈᾘᾨ", "ǅǆǄ", "ẞß", "Ⅷⅷ", "ⓐⒶ", "𐐀𐐨", "𞤀𞤢",
	"e\u0301", "\u0301e", "\u0301\u0301", "a\u200db", "👨\u200d👩\u200d👧", "🇫🇷", "😀", "日本語", "\x00", "a\x00b",
	"\x7f", "\u0080", "߿ࠀ", "\uffff\U00010000", "\U0010ffff", "�", "\ud7ff\ue000", "tab\there",
	"quote\"s and 'apostrophes' and \\backslashes\\", "\x1b[31mred\x1b[0m", "\u202eRTL", "\ue000\uf8ff",
	"\u0378\U000e0001", "ｶﾞﾞﾟ", "0123456789", "٣٤٥", "Ⅷ½²", "aaaa", "abab", "aXbXc", "XaXbX", "XX",
	"a,b,,c,", ",", ",,", "key=value=more", "  leading", "trailing  ", "MiXeD CaSe 123", "ALLCAPS", "alllower",
	"x" + strings.Repeat("ab", 40), strings.Repeat("é", 33), strings.Repeat("a", 64) + "é" + strings.Repeat("b", 64),
	strings.Repeat("A", 15) + "Σ", strings.Repeat("A", 16) + "Σ", strings.Repeat("A", 17) + "Σ",
	strings.Repeat("a ", 20), strings.Repeat("line\n", 5), strings.Repeat("line\r\n", 5) + "last",
}

func answerStr(t *testing.T, raw json.RawMessage) any {
	request, members := decodeRequest(t, raw)
	s := request.S
	switch request.Op {
	case "str":
		charIndices := []any{}
		for _, pair := range CharIndices(s) {
			charIndices = append(charIndices, []any{pair.F0, pair.F1})
		}
		reversed := CharIndicesRev(s)
		for i, pair := range CharIndices(s) {
			if reversed[len(reversed)-1-i] != pair {
				t.Errorf("CharIndicesRev(%q) is not CharIndices reversed", s)
			}
		}
		boundaries := []any{}
		for i := 0; i <= len(s)+2; i++ {
			boundaries = append(boundaries, []any{IsCharBoundary(s, i), FloorCharBoundary(s, i), CeilCharBoundary(s, i)})
		}
		lowerInPlace, upperInPlace := s, s
		MakeAsciiLowercase(&lowerInPlace)
		MakeAsciiUppercase(&upperInPlace)
		return map[string]any{
			"chars":                  list(Chars(s)),
			"chars_rev":              list(CharsRev(s)),
			"chars_count":            CharsCount(s),
			"chars_next":             option(CharsNext(s)),
			"chars_next_back":        option(CharsNextBack(s)),
			"char_indices":           charIndices,
			"boundaries":             boundaries,
			"trim":                   Trim(s),
			"trim_start":             TrimStart(s),
			"trim_end":               TrimEnd(s),
			"trim_ascii":             TrimAscii(s),
			"trim_ascii_start":       TrimAsciiStart(s),
			"trim_ascii_end":         TrimAsciiEnd(s),
			"lines":                  list(Lines(s)),
			"split_whitespace":       list(SplitWhitespace(s)),
			"split_ascii_whitespace": list(SplitAsciiWhitespace(s)),
			"to_lowercase":           ToLowercase(s),
			"to_uppercase":           ToUppercase(s),
			"to_ascii_lowercase":     ToAsciiLowercase(s),
			"to_ascii_uppercase":     ToAsciiUppercase(s),
			"make_ascii_lowercase":   lowerInPlace,
			"make_ascii_uppercase":   upperInPlace,
			"is_ascii":               IsAscii(s),
			"encode_utf16":           list(EncodeUtf16(s)),
			"escape_debug":           EscapeDebug(s),
			"escape_default":         EscapeDefault(s),
			"escape_unicode":         EscapeUnicode(s),
			"debug":                  strDebug(s),
		}
	case "index":
		i := int(request.I)
		return []any{IsCharBoundary(s, i), FloorCharBoundary(s, i), CeilCharBoundary(s, i)}
	case "slice":
		a, b := int(request.A), int(request.B)
		return oracle.Catch(func() any {
			switch request.F {
			case "range":
				return Slice(s, a, b)
			case "from":
				return SliceFrom(s, a)
			case "to":
				return SliceTo(s, b)
			case "inclusive":
				return SliceInclusive(s, a, b)
			case "to_inclusive":
				return SliceToInclusive(s, b)
			case "get_range":
				return option(Get(s, a, b))
			case "get_from":
				return option(GetFrom(s, a))
			case "get_to":
				return option(GetTo(s, b))
			case "get_inclusive":
				return option(GetInclusive(s, a, b))
			case "get_to_inclusive":
				return option(GetToInclusive(s, b))
			}
			t.Fatalf("unknown slice request %s", raw)
			return nil
		})
	case "split_at":
		mid := int(request.Mid)
		return oracle.Catch(func() any {
			switch request.F {
			case "split_at":
				head, tail := SplitAt(s, mid)
				return []string{head, tail}
			case "split_at_checked":
				return halves(SplitAtChecked(s, mid))
			}
			t.Fatalf("unknown split_at request %s", raw)
			return nil
		})
	case "pair":
		a, b := text(t, members, "a"), text(t, members, "b")
		return map[string]any{
			"eq_ignore_ascii_case": EqIgnoreAsciiCase(a, b),
			"cmp":                  strings.Compare(a, b),
		}
	case "repeat":
		return oracle.Catch(func() any { return Repeat(s, int(request.N)) })
	case "bytes":
		a, b := unhex(t, text(t, members, "a")), unhex(t, text(t, members, "b"))
		classes := []uint32{}
		lower, upper := []byte{}, []byte{}
		for _, c := range a {
			classes = append(classes, flags(
				CharIsAscii(c),
				IsAsciiAlphabetic(c),
				IsAsciiDigit(c),
				IsAsciiAlphanumeric(c),
				IsAsciiHexdigit(c),
				IsAsciiPunctuation(c),
				IsAsciiGraphic(c),
				IsAsciiWhitespace(c),
				IsAsciiControl(c),
				IsAsciiUppercase(c),
				IsAsciiLowercase(c),
			))
			lower = append(lower, CharToAsciiLowercase(c))
			upper = append(upper, CharToAsciiUppercase(c))
		}
		return map[string]any{
			"is_ascii":             BytesIsAscii(a),
			"trim_ascii":           hex.EncodeToString(BytesTrimAscii(a)),
			"trim_ascii_start":     hex.EncodeToString(BytesTrimAsciiStart(a)),
			"trim_ascii_end":       hex.EncodeToString(BytesTrimAsciiEnd(a)),
			"to_ascii_lowercase":   hex.EncodeToString(BytesToAsciiLowercase(a)),
			"to_ascii_uppercase":   hex.EncodeToString(BytesToAsciiUppercase(a)),
			"eq_ignore_ascii_case": BytesEqIgnoreAsciiCase(a, b),
			"classes":              classes,
			"lower":                hex.EncodeToString(lower),
			"upper":                hex.EncodeToString(upper),
		}
	}
	t.Fatalf("unknown request %s", raw)
	return nil
}

// strDebug is `format!("{s:?}")`, which this package does not provide (the
// formatter in go/rs does); it is rebuilt here from charEscapeDebugExt to
// check that function with Debug's arguments on every char of the corpus.
func strDebug(s string) string {
	var text strings.Builder
	text.WriteByte('"')
	for _, c := range s {
		escaped, _ := charEscapeDebugExt(c, escapeDebugExtArgs{
			escapeGraphemeExtender: true,
			escapeSingleQuote:      false,
			escapeDoubleQuote:      true,
		})
		text.WriteString(escaped)
	}
	text.WriteByte('"')
	return text.String()
}

func TestDiffAgainstRust_str(t *testing.T) {
	oracle.Check(t, "std_str", "str", func(yield func(any)) {
		for _, s := range fixedStrings {
			yield(map[string]any{"op": "str", "s": s})
		}
		random := rand.New(rand.NewPCG(12, 1))
		for i := 0; i < 350; i++ {
			yield(map[string]any{"op": "str", "s": randomString(random, 40)})
		}

		// Indexing: every range kind, at every pair of offsets of a few
		// strings, with the offsets that are off a boundary or out of range.
		for _, s := range []string{"", "a", "aé", "é", "日本", "a😀b", "e\u0301", "\u0301", "'\"", "\x00\u00a0", "ab\U0010ffff"} {
			for _, f := range []string{"range", "from", "to", "inclusive", "to_inclusive",
				"get_range", "get_from", "get_to", "get_inclusive", "get_to_inclusive"} {
				for a := 0; a <= len(s)+2; a++ {
					for b := 0; b <= len(s)+2; b++ {
						if (strings.HasSuffix(f, "from") && b > 0) || (strings.Contains(f, "to") && a > 0) {
							continue
						}
						yield(map[string]any{"op": "slice", "f": f, "s": s, "a": a, "b": b})
					}
				}
			}
			for mid := 0; mid <= len(s)+2; mid++ {
				yield(map[string]any{"op": "split_at", "f": "split_at", "s": s, "mid": mid})
				yield(map[string]any{"op": "split_at", "f": "split_at_checked", "s": s, "mid": mid})
			}
		}
		// A usize that a Go int holds as a negative number.
		huge := []uint64{1<<64 - 1, 1<<64 - 2, 1 << 63, 1<<63 - 1}
		for _, n := range huge {
			for _, f := range []string{"range", "from", "to", "inclusive", "to_inclusive",
				"get_range", "get_from", "get_to", "get_inclusive", "get_to_inclusive"} {
				yield(map[string]any{"op": "slice", "f": f, "s": "aé", "a": n, "b": 1})
				yield(map[string]any{"op": "slice", "f": f, "s": "aé", "a": 1, "b": n})
				yield(map[string]any{"op": "slice", "f": f, "s": "aé", "a": n, "b": n})
			}
			yield(map[string]any{"op": "split_at", "f": "split_at", "s": "aé", "mid": n})
			yield(map[string]any{"op": "split_at", "f": "split_at_checked", "s": "aé", "mid": n})
			yield(map[string]any{"op": "index", "s": "aé", "i": n})
			yield(map[string]any{"op": "index", "s": "", "i": n})
		}

		pairs := [][2]string{
			{"", ""}, {"a", "A"}, {"a", "b"}, {"hello", "HELLO"}, {"HeLLo", "hEllO"}, {"a", "aa"}, {"é", "É"},
			{"é", "é"}, {"k", "K"}, {"k", "K"}, {"@", "`"}, {"[", "{"}, {"z", "Z"}, {"Straße", "STRASSE"},
			{"i", "İ"}, {"ab", "a"}, {"b", "a"}, {"ÿ", "\U00010000"}, {"\uffff", "\U00010000"},
		}
		for _, pair := range pairs {
			yield(map[string]any{"op": "pair", "a": pair[0], "b": pair[1]})
		}
		for i := 0; i < 60; i++ {
			a := randomString(random, 6)
			b := a
			if random.IntN(3) > 0 {
				b = strings.Map(func(c rune) rune {
					switch random.IntN(3) {
					case 0:
						return CharToAsciiUppercase(c)
					case 1:
						return CharToAsciiLowercase(c)
					}
					return c
				}, a)
			}
			if random.IntN(4) == 0 {
				b = randomString(random, 6)
			}
			yield(map[string]any{"op": "pair", "a": a, "b": b})
		}

		for _, s := range []string{"", "ab", "é"} {
			for _, n := range []uint64{0, 1, 2, 7, 1<<64 - 1, 1 << 63, 1 << 62} {
				yield(map[string]any{"op": "repeat", "s": s, "n": n})
			}
		}

		// The ASCII methods of bytes: every byte value, then random byte
		// strings with whitespace at the ends.
		all := make([]byte, 256)
		for i := range all {
			all[i] = byte(i)
		}
		yield(map[string]any{"op": "bytes", "a": hex.EncodeToString(all), "b": hex.EncodeToString(all)})
		edges := []string{" ", "\t", "\n", "\v", "\f", "\r", "a", "Z", "\x00", "\xff", "\x85"}
		for i := 0; i < 80; i++ {
			var a []byte
			for n := random.IntN(10); n > 0; n-- {
				a = append(a, edges[random.IntN(len(edges))]...)
			}
			b := append([]byte(nil), a...)
			for j := range b {
				if random.IntN(3) == 0 {
					b[j] ^= 0x20
				}
			}
			yield(map[string]any{"op": "bytes", "a": hex.EncodeToString(a), "b": hex.EncodeToString(b)})
		}
	}, func(raw json.RawMessage) any { return answerStr(t, raw) })
}
