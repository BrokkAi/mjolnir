package strs

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"math/big"
	"math/rand/v2"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

func answerUtf8(t *testing.T, raw json.RawMessage) any {
	request, members := decodeRequest(t, raw)
	switch request.Op {
	case "from_utf8":
		b := unhex(t, text(t, members, "b"))
		var strReply, stringReply any
		if s, err := FromUtf8(b); err != nil {
			utf8Error := err.(Utf8Error)
			strReply = map[string]any{
				"err":         err.Error(),
				"debug":       utf8Error.debugString(),
				"valid_up_to": utf8Error.ValidUpTo(),
				"error_len":   option(utf8Error.ErrorLen()),
			}
		} else {
			strReply = map[string]any{"ok": s}
		}
		if s, err := StringFromUtf8(b); err != nil {
			fromUtf8Error := err.(*FromUtf8Error)
			if lossy := fromUtf8Error.IntoUtf8Lossy(); lossy != FromUtf8Lossy(b) {
				t.Errorf("IntoUtf8Lossy of %x is %q", b, lossy)
			}
			stringReply = map[string]any{
				"err":         err.Error(),
				"debug":       fromUtf8Error.debugString(),
				"bytes":       hex.EncodeToString(fromUtf8Error.IntoBytes()),
				"valid_up_to": fromUtf8Error.Utf8Error().ValidUpTo(),
				"error_len":   option(fromUtf8Error.Utf8Error().ErrorLen()),
			}
		} else {
			stringReply = map[string]any{"ok": s}
		}
		chunks := []any{}
		for _, chunk := range Utf8Chunks(b) {
			chunks = append(chunks, []string{chunk.Valid(), hex.EncodeToString(chunk.Invalid())})
		}
		return map[string]any{
			"str":    strReply,
			"string": stringReply,
			"lossy":  FromUtf8Lossy(b),
			"chunks": chunks,
		}
	case "utf16":
		return map[string]any{
			"from_utf16": result(FromUtf16(request.U)),
			"lossy":      FromUtf16Lossy(request.U),
		}
	}
	t.Fatalf("unknown request %s", raw)
	return nil
}

func TestDiffAgainstRust_utf8(t *testing.T) {
	oracle.Check(t, "std_str", "utf8", func(yield func(any)) {
		fixed := []string{
			"", "00", "41", "7f", "80", "bf", "c0", "c1", "c2", "df", "e0", "ef", "f0", "f4", "f5", "ff",
			"c280", "c2bf", "c27f", "c2c0", "dfbf", "c080", "c1bf",
			"e0a080", "e09f80", "e0a0", "e0", "e0a07f", "e0807f", "e1", "e180", "e18080", "e1807f", "e17f80",
			"ed9fbf", "eda080", "edbfbf", "ed", "eda0", "ee8080", "efbfbf", "efbf", "efbfbd",
			"f0908080", "f08f8080", "f090", "f09080", "f0", "f09f9880", "f09f98", "f09f", "f09f987f",
			"f48fbfbf", "f4908080", "f48f", "f48fbf", "f5808080", "f8888080", "fc80808080", "feff", "fffe",
			"68656c6c6fff", "ff68656c6c6f", "68ff65ff6c", "e282", "e28261", "e282ac", "41e282", "f09f98f09f9880",
			"c2c2c2", "e0a0e0a0", "f0f0f0f0", "808080", "61c3a962e697a563f09f988064", "edb080eda080",
			"c3a9c3", "c3a9e2", "c3a9f0", "3132338061", "e38182e38184e3", strings.Repeat("61", 40) + "ff" + strings.Repeat("62", 40),
			strings.Repeat("c3a9", 30) + "c3", strings.Repeat("ff", 20), strings.Repeat("f09f98", 5),
		}
		for _, b := range fixed {
			yield(map[string]any{"op": "from_utf8", "b": b})
		}
		// Random byte strings over the bytes the validation branches on, and
		// valid text with bytes changed, removed or cut.
		random := rand.New(rand.NewPCG(13, 1))
		dangerous := []byte{0x00, 0x41, 0x7f, 0x80, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc0, 0xc1, 0xc2, 0xdf, 0xe0, 0xe1,
			0xec, 0xed, 0xee, 0xef, 0xf0, 0xf1, 0xf3, 0xf4, 0xf5, 0xf7, 0xf8, 0xff}
		for i := 0; i < 500; i++ {
			b := make([]byte, random.IntN(9))
			for j := range b {
				b[j] = dangerous[random.IntN(len(dangerous))]
			}
			yield(map[string]any{"op": "from_utf8", "b": hex.EncodeToString(b)})
		}
		for i := 0; i < 400; i++ {
			b := []byte(randomString(random, 12))
			for n := random.IntN(4); n > 0 && len(b) > 0; n-- {
				at := random.IntN(len(b))
				switch random.IntN(4) {
				case 0:
					b[at] = dangerous[random.IntN(len(dangerous))]
				case 1:
					b = append(b[:at], b[at+1:]...)
				case 2:
					b = b[:at]
				default:
					b = append(b[:at], append([]byte{dangerous[random.IntN(len(dangerous))]}, b[at:]...)...)
				}
			}
			yield(map[string]any{"op": "from_utf8", "b": hex.EncodeToString(b)})
		}

		units := []uint16{0x41, 0xe9, 0x20ac, 0xd7ff, 0xd800, 0xd83d, 0xdbff, 0xdc00, 0xde00, 0xdfff, 0xe000, 0xfffd, 0xffff, 0}
		yield(map[string]any{"op": "utf16", "u": []uint16{}})
		for _, a := range units {
			yield(map[string]any{"op": "utf16", "u": []uint16{a}})
			for _, b := range units {
				yield(map[string]any{"op": "utf16", "u": []uint16{a, b}})
			}
		}
		for i := 0; i < 200; i++ {
			u := make([]uint16, random.IntN(8))
			for j := range u {
				u[j] = units[random.IntN(len(units))]
			}
			yield(map[string]any{"op": "utf16", "u": u})
		}
	}, func(raw json.RawMessage) any { return answerUtf8(t, raw) })
}

// answerPattern runs every pattern method the way the oracle's
// pattern_reply! does, with the spelling of the method for the kind of
// pattern in the request.
func answerPattern(t *testing.T, raw json.RawMessage) any {
	request, _ := decodeRequest(t, raw)
	s, n, to := request.S, int(request.N), request.To
	pat := request.Pat
	reply := map[string]any{}
	switch {
	case pat.S != nil:
		p := *pat.S
		reply["contains"] = Contains(s, p)
		reply["starts_with"] = StartsWith(s, p)
		reply["ends_with"] = EndsWith(s, p)
		reply["find"] = option(Find(s, p))
		reply["rfind"] = option(Rfind(s, p))
		reply["split"] = list(Split(s, p))
		reply["split_inclusive"] = list(SplitInclusive(s, p))
		reply["rsplit"] = list(Rsplit(s, p))
		reply["split_terminator"] = list(SplitTerminator(s, p))
		reply["rsplit_terminator"] = list(RsplitTerminator(s, p))
		reply["splitn"] = list(Splitn(s, n, p))
		reply["rsplitn"] = list(Rsplitn(s, n, p))
		reply["split_once"] = halves(SplitOnce(s, p))
		reply["rsplit_once"] = halves(RsplitOnce(s, p))
		reply["matches"] = list(Matches(s, p))
		reply["rmatches"] = list(Rmatches(s, p))
		reply["match_indices"] = indexPairs(MatchIndices(s, p))
		reply["rmatch_indices"] = indexPairs(RmatchIndices(s, p))
		reply["trim_start_matches"] = TrimStartMatches(s, p)
		reply["trim_end_matches"] = TrimEndMatches(s, p)
		reply["strip_prefix"] = option(StripPrefix(s, p))
		reply["strip_suffix"] = option(StripSuffix(s, p))
		reply["replace"] = Replace(s, p, to)
		reply["replacen"] = Replacen(s, p, to, n)
	case pat.C != nil:
		p := *pat.C
		reply["contains"] = ContainsChar(s, p)
		reply["starts_with"] = StartsWithChar(s, p)
		reply["ends_with"] = EndsWithChar(s, p)
		reply["find"] = option(FindChar(s, p))
		reply["rfind"] = option(RfindChar(s, p))
		reply["split"] = list(SplitChar(s, p))
		reply["split_inclusive"] = list(SplitInclusiveChar(s, p))
		reply["rsplit"] = list(RsplitChar(s, p))
		reply["split_terminator"] = list(SplitTerminatorChar(s, p))
		reply["rsplit_terminator"] = list(RsplitTerminatorChar(s, p))
		reply["splitn"] = list(SplitnChar(s, n, p))
		reply["rsplitn"] = list(RsplitnChar(s, n, p))
		reply["split_once"] = halves(SplitOnceChar(s, p))
		reply["rsplit_once"] = halves(RsplitOnceChar(s, p))
		reply["matches"] = list(MatchesChar(s, p))
		reply["rmatches"] = list(RmatchesChar(s, p))
		reply["match_indices"] = indexPairs(MatchIndicesChar(s, p))
		reply["rmatch_indices"] = indexPairs(RmatchIndicesChar(s, p))
		reply["trim_matches"] = TrimMatchesChar(s, p)
		reply["trim_start_matches"] = TrimStartMatchesChar(s, p)
		reply["trim_end_matches"] = TrimEndMatchesChar(s, p)
		reply["strip_prefix"] = option(StripPrefixChar(s, p))
		reply["strip_suffix"] = option(StripSuffixChar(s, p))
		reply["replace"] = ReplaceChar(s, p, to)
		reply["replacen"] = ReplacenChar(s, p, to, n)
	case pat.F != nil:
		p := predicate(t, *pat.F)
		reply["contains"] = ContainsFn(s, p)
		reply["starts_with"] = StartsWithFn(s, p)
		reply["ends_with"] = EndsWithFn(s, p)
		reply["find"] = option(FindFn(s, p))
		reply["rfind"] = option(RfindFn(s, p))
		reply["split"] = list(SplitFn(s, p))
		reply["split_inclusive"] = list(SplitInclusiveFn(s, p))
		reply["rsplit"] = list(RsplitFn(s, p))
		reply["split_terminator"] = list(SplitTerminatorFn(s, p))
		reply["rsplit_terminator"] = list(RsplitTerminatorFn(s, p))
		reply["splitn"] = list(SplitnFn(s, n, p))
		reply["rsplitn"] = list(RsplitnFn(s, n, p))
		reply["split_once"] = halves(SplitOnceFn(s, p))
		reply["rsplit_once"] = halves(RsplitOnceFn(s, p))
		reply["matches"] = list(MatchesFn(s, p))
		reply["rmatches"] = list(RmatchesFn(s, p))
		reply["match_indices"] = indexPairs(MatchIndicesFn(s, p))
		reply["rmatch_indices"] = indexPairs(RmatchIndicesFn(s, p))
		reply["trim_matches"] = TrimMatchesFn(s, p)
		reply["trim_start_matches"] = TrimStartMatchesFn(s, p)
		reply["trim_end_matches"] = TrimEndMatchesFn(s, p)
		reply["strip_prefix"] = option(StripPrefixFn(s, p))
		reply["strip_suffix"] = option(StripSuffixFn(s, p))
		reply["replace"] = ReplaceFn(s, p, to)
		reply["replacen"] = ReplacenFn(s, p, to, n)
	default:
		// A set of chars. A request for the empty set has `"cs": []`.
		p := pat.Cs
		reply["contains"] = ContainsChars(s, p)
		reply["starts_with"] = StartsWithChars(s, p)
		reply["ends_with"] = EndsWithChars(s, p)
		reply["find"] = option(FindChars(s, p))
		reply["rfind"] = option(RfindChars(s, p))
		reply["split"] = list(SplitChars(s, p))
		reply["split_inclusive"] = list(SplitInclusiveChars(s, p))
		reply["rsplit"] = list(RsplitChars(s, p))
		reply["split_terminator"] = list(SplitTerminatorChars(s, p))
		reply["rsplit_terminator"] = list(RsplitTerminatorChars(s, p))
		reply["splitn"] = list(SplitnChars(s, n, p))
		reply["rsplitn"] = list(RsplitnChars(s, n, p))
		reply["split_once"] = halves(SplitOnceChars(s, p))
		reply["rsplit_once"] = halves(RsplitOnceChars(s, p))
		reply["matches"] = list(MatchesChars(s, p))
		reply["rmatches"] = list(RmatchesChars(s, p))
		reply["match_indices"] = indexPairs(MatchIndicesChars(s, p))
		reply["rmatch_indices"] = indexPairs(RmatchIndicesChars(s, p))
		reply["trim_matches"] = TrimMatchesChars(s, p)
		reply["trim_start_matches"] = TrimStartMatchesChars(s, p)
		reply["trim_end_matches"] = TrimEndMatchesChars(s, p)
		reply["strip_prefix"] = option(StripPrefixChars(s, p))
		reply["strip_suffix"] = option(StripSuffixChars(s, p))
		reply["replace"] = ReplaceChars(s, p, to)
		reply["replacen"] = ReplacenChars(s, p, to, n)
	}
	return reply
}

func TestDiffAgainstRust_pattern(t *testing.T) {
	oracle.Check(t, "std_str", "pattern", func(yield func(any)) {
		ask := func(s string, pat map[string]any, n uint64, to string) {
			yield(map[string]any{"op": "pat", "s": s, "pat": pat, "n": n, "to": to})
		}
		str := func(p string) map[string]any { return map[string]any{"s": p} }
		chr := func(c rune) map[string]any { return map[string]any{"c": c} }
		set := func(cs ...rune) map[string]any {
			if cs == nil {
				cs = []rune{}
			}
			return map[string]any{"cs": cs}
		}
		fn := func(name string) map[string]any { return map[string]any{"f": name} }

		// The cases of the documentation and of the std tests, and the
		// corners: empty haystack, empty needle, matches at the ends,
		// adjacent matches, a needle longer than the haystack, overlaps.
		haystacks := []string{
			"", "a", "X", "aXb", "XaX", "XX", "aXXb", "Mary had a little lamb", "lion::tiger::leopard", "abcXdefXghi",
			"A.B.", "A..B..", "aaa", "aaaa", "ababa", "abababab", "aabaaabaaaab", "é", "éé", "aéa", "日本語", "😀a😀",
			"a\nb\n", "  a  b  ", "\u00a0a\u2003", "ΑΣ", "x=1;y=2;", "ꁁaꁁ", "\U0010ffff\uffff",
		}
		needles := []string{"", "a", "X", "XX", "aa", "aaa", "ab", "aba", "abab", "é", "::", ".", "aab", "ba", "日", "語",
			"😀", "ꁁ", "little", "Mary had a little lamb", "Mary had a little lamb!", "éa", "\n"}
		for _, s := range haystacks {
			for _, p := range needles {
				ask(s, str(p), 2, "-")
			}
			for _, c := range []rune{'a', 'X', 'é', '.', ':', ' ', '\n', '日', '😀', 'ꁁ', 0xa0, 0x10ffff} {
				ask(s, chr(c), 2, "-")
			}
			ask(s, set(), 2, "-")
			ask(s, set('a'), 2, "-")
			ask(s, set('X', '.'), 2, "-")
			ask(s, set('a', 'é', '日', '😀'), 3, "")
			for _, f := range []string{"is_whitespace", "is_alphabetic", "is_uppercase", "not_ascii", "always", "never"} {
				ask(s, fn(f), 2, "-")
			}
		}
		// Every count around the number of parts, for splitn, rsplitn and
		// replacen. A count above any length is a usize that a Go int holds
		// as a negative number.
		for _, n := range []uint64{0, 1, 2, 3, 4, 5, 6, 1<<64 - 1, 1 << 63} {
			ask("aXbXcXd", chr('X'), n, "--")
			ask("aXbXcXd", str("X"), n, "")
			ask("aXbXcXd", str(""), n, "é")
			ask("XaXbX", set('X', 'a'), n, "<>")
			ask("", str(""), n, "x")
			ask("", chr('X'), n, "x")
			ask("aaaa", str("aa"), n, "b")
			ask(" a  b ", fn("is_whitespace"), n, "_")
		}

		// Random haystacks and needles from small alphabets, so that needles
		// occur, repeat and overlap, and have periods for the two-way search
		// to find.
		random := rand.New(rand.NewPCG(14, 1))
		small := [][]rune{[]rune("ab"), []rune("abc"), []rune("aé"), []rune("a日😀"), []rune("ab \n")}
		draw := func(alphabet []rune, maxChars int) string {
			var text strings.Builder
			for n := random.IntN(maxChars + 1); n > 0; n-- {
				text.WriteRune(alphabet[random.IntN(len(alphabet))])
			}
			return text.String()
		}
		for i := 0; i < 420; i++ {
			alphabet := small[random.IntN(len(small))]
			s := draw(alphabet, 14)
			n := uint64(random.IntN(5))
			to := draw(alphabet, 2)
			switch random.IntN(6) {
			case 0, 1, 2:
				p := draw(alphabet, 4)
				if random.IntN(3) == 0 && len(s) > 0 {
					// A needle taken from the haystack, which must occur.
					chars := []rune(s)
					from := random.IntN(len(chars))
					p = string(chars[from : from+random.IntN(len(chars)-from+1)])
				}
				ask(s, str(p), n, to)
			case 3:
				ask(s, chr(alphabet[random.IntN(len(alphabet))]), n, to)
			case 4:
				ask(s, set(alphabet[random.IntN(len(alphabet))], alphabet[random.IntN(len(alphabet))]), n, to)
			default:
				ask(s, fn([]string{"is_whitespace", "is_alphabetic", "not_ascii", "is_lowercase"}[random.IntN(4)]), n, to)
			}
		}
		// Long periodic needles in long haystacks.
		for i := 0; i < 40; i++ {
			unit := draw([]rune("ab"), 5) + "a"
			needle := strings.Repeat(unit, 1+random.IntN(4)) + draw([]rune("ab"), 3)
			s := strings.Repeat(unit, random.IntN(12)) + draw([]rune("abc"), 6) + strings.Repeat(unit, random.IntN(12))
			ask(s, str(needle), uint64(random.IntN(4)), "·")
		}
	}, func(raw json.RawMessage) any { return answerPattern(t, raw) })
}

func answerString(t *testing.T, raw json.RawMessage) any {
	request, _ := decodeRequest(t, raw)
	a, b := int(request.A), int(request.B)
	return oracle.Catch(func() any {
		s := request.S
		var ret any
		switch request.F {
		case "truncate":
			Truncate(&s, a)
		case "pop":
			ret = option(Pop(&s))
		case "remove":
			ret = Remove(&s, a)
		case "insert":
			Insert(&s, a, request.C)
		case "insert_str":
			InsertStr(&s, a, request.With)
		case "drain":
			ret = Drain(&s, a, b)
		case "retain":
			Retain(&s, predicate(t, request.Keep))
		case "split_off":
			ret = SplitOff(&s, a)
		case "replace_range":
			ReplaceRange(&s, a, b, request.With)
		default:
			t.Fatalf("unknown string request %s", raw)
		}
		return map[string]any{"s": s, "ret": ret}
	})
}

func TestDiffAgainstRust_string(t *testing.T) {
	oracle.Check(t, "std_str", "string", func(yield func(any)) {
		huge := []uint64{1<<64 - 1, 1 << 63}
		for _, s := range []string{"", "a", "aé", "éa", "日本", "a😀b", "e\u0301x", "ab"} {
			var offsets []uint64
			for i := 0; i <= len(s)+2; i++ {
				offsets = append(offsets, uint64(i))
			}
			offsets = append(offsets, huge...)
			yield(map[string]any{"op": "string", "f": "pop", "s": s})
			for _, keep := range []string{"always", "never", "is_ascii", "not_ascii", "is_alphabetic", "is_whitespace"} {
				yield(map[string]any{"op": "string", "f": "retain", "s": s, "keep": keep})
			}
			for _, a := range offsets {
				yield(map[string]any{"op": "string", "f": "truncate", "s": s, "a": a})
				yield(map[string]any{"op": "string", "f": "remove", "s": s, "a": a})
				yield(map[string]any{"op": "string", "f": "split_off", "s": s, "a": a})
				for _, c := range []rune{'x', 'é', '日', '😀'} {
					yield(map[string]any{"op": "string", "f": "insert", "s": s, "a": a, "c": c})
				}
				for _, with := range []string{"", "x", "éé"} {
					yield(map[string]any{"op": "string", "f": "insert_str", "s": s, "a": a, "with": with})
				}
				for _, b := range offsets {
					yield(map[string]any{"op": "string", "f": "drain", "s": s, "a": a, "b": b})
					yield(map[string]any{"op": "string", "f": "replace_range", "s": s, "a": a, "b": b, "with": "é"})
					yield(map[string]any{"op": "string", "f": "replace_range", "s": s, "a": a, "b": b, "with": ""})
				}
			}
		}
		random := rand.New(rand.NewPCG(15, 1))
		for i := 0; i < 150; i++ {
			s := randomString(random, 10)
			keep := []string{"is_whitespace", "is_alphabetic", "is_uppercase", "is_ascii", "is_control", "is_numeric"}[random.IntN(6)]
			yield(map[string]any{"op": "string", "f": "retain", "s": s, "keep": keep})
			yield(map[string]any{"op": "string", "f": "pop", "s": s})
		}
	}, func(raw json.RawMessage) any { return answerString(t, raw) })
}

func u128Text(hi, lo uint64) string {
	value := new(big.Int).SetUint64(hi)
	value.Lsh(value, 64).Or(value, new(big.Int).SetUint64(lo))
	return value.String()
}

func i128Text(hi int64, lo uint64) string {
	value := big.NewInt(hi)
	value.Lsh(value, 64).Or(value, new(big.Int).SetUint64(lo))
	return value.String()
}

// intResult is the oracle's `int`: the value in decimal, or the error.
func intResult[T any](value T, err error) any {
	if err != nil {
		return errorReply(err)
	}
	return map[string]any{"ok": fmt.Sprint(value)}
}

func int128Result(text string, err error) any {
	if err != nil {
		return errorReply(err)
	}
	return map[string]any{"ok": text}
}

// usizeResult is a usize reply: this port holds a usize in an int, so the
// values above the largest int, which Rust parses, are a documented
// PosOverflow here. The comparison accepts exactly that difference.
func usizeResult(value int, err error, exact func() (uint64, error)) any {
	if wide, wideErr := exact(); wideErr == nil && wide > math.MaxInt {
		parseErr, ok := err.(*ParseIntError)
		if !ok || parseErr.Kind() != IntErrorKind_PosOverflow {
			return map[string]any{"wrong": fmt.Sprint(value, err)}
		}
		return map[string]any{"ok": strconv.FormatUint(wide, 10)}
	}
	return intResult(value, err)
}

func answerParse(t *testing.T, raw json.RawMessage) any {
	request, members := decodeRequest(t, raw)
	s := request.S
	switch request.Op {
	case "parse":
		switch text(t, members, "t") {
		case "ints":
			u128Hi, u128Lo, u128Err := ParseU128Parts(s)
			i128Hi, i128Lo, i128Err := ParseI128Parts(s)
			usize, usizeErr := ParseUsize(s)
			return map[string]any{
				"u8":    intResult(ParseU8(s)),
				"u16":   intResult(ParseU16(s)),
				"u32":   intResult(ParseU32(s)),
				"u64":   intResult(ParseU64(s)),
				"u128":  int128Result(u128Text(u128Hi, u128Lo), u128Err),
				"usize": usizeResult(usize, usizeErr, func() (uint64, error) { return ParseU64(s) }),
				"i8":    intResult(ParseI8(s)),
				"i16":   intResult(ParseI16(s)),
				"i32":   intResult(ParseI32(s)),
				"i64":   intResult(ParseI64(s)),
				"i128":  int128Result(i128Text(i128Hi, i128Lo), i128Err),
				"isize": intResult(ParseIsize(s)),
			}
		case "floats":
			f32, err32 := ParseF32(s)
			f64, err64 := ParseF64(s)
			reply := map[string]any{}
			if err32 != nil {
				reply["f32"] = errorReply(err32)
			} else {
				reply["f32"] = map[string]any{"ok": fmt.Sprintf("%08x", math.Float32bits(f32))}
			}
			if err64 != nil {
				reply["f64"] = errorReply(err64)
			} else {
				reply["f64"] = map[string]any{"ok": fmt.Sprintf("%016x", math.Float64bits(f64))}
			}
			return reply
		case "misc":
			return map[string]any{
				"bool": result(ParseBool(s)),
				"char": result(ParseChar(s)),
			}
		}
	case "radix":
		radix := request.Radix
		return oracle.Catch(func() any {
			u128Hi, u128Lo, u128Err := U128FromStrRadixParts(s, radix)
			i128Hi, i128Lo, i128Err := I128FromStrRadixParts(s, radix)
			usize, usizeErr := UsizeFromStrRadix(s, radix)
			return map[string]any{
				"u8":    intResult(U8FromStrRadix(s, radix)),
				"u16":   intResult(U16FromStrRadix(s, radix)),
				"u32":   intResult(U32FromStrRadix(s, radix)),
				"u64":   intResult(U64FromStrRadix(s, radix)),
				"u128":  int128Result(u128Text(u128Hi, u128Lo), u128Err),
				"usize": usizeResult(usize, usizeErr, func() (uint64, error) { return U64FromStrRadix(s, radix) }),
				"i8":    intResult(I8FromStrRadix(s, radix)),
				"i16":   intResult(I16FromStrRadix(s, radix)),
				"i32":   intResult(I32FromStrRadix(s, radix)),
				"i64":   intResult(I64FromStrRadix(s, radix)),
				"i128":  int128Result(i128Text(i128Hi, i128Lo), i128Err),
				"isize": intResult(IsizeFromStrRadix(s, radix)),
			}
		})
	}
	t.Fatalf("unknown request %s", raw)
	return nil
}

// integerEdges are the values each integer type's range ends at.
var integerEdges = []string{
	"0", "1", "9", "10", "99", "100", "126", "127", "128", "129", "254", "255", "256", "257", "999", "1000",
	"32767", "32768", "32769", "65535", "65536", "65537", "99999", "2147483647", "2147483648", "2147483649",
	"4294967295", "4294967296", "4294967297", "9999999999", "9223372036854775807", "9223372036854775808",
	"9223372036854775809", "18446744073709551615", "18446744073709551616", "18446744073709551617",
	"99999999999999999999", "170141183460469231731687303715884105727", "170141183460469231731687303715884105728",
	"170141183460469231731687303715884105729", "340282366920938463463374607431768211455",
	"340282366920938463463374607431768211456", "340282366920938463463374607431768211457",
	"999999999999999999999999999999999999999", "1000000000000000000000000000000000000000",
	"99999999999999999999999999999999999999999999999999",
}

func TestDiffAgainstRust_parse_int(t *testing.T) {
	oracle.Check(t, "std_str", "parse_int", func(yield func(any)) {
		ints := func(s string) { yield(map[string]any{"op": "parse", "t": "ints", "s": s}) }
		misc := func(s string) { yield(map[string]any{"op": "parse", "t": "misc", "s": s}) }

		// Integers: the edges of every type with each sign and with leading
		// zeros, then everything Rust refuses.
		for _, edge := range integerEdges {
			for _, prefix := range []string{"", "+", "-", "000", "-000", "+0"} {
				ints(prefix + edge)
			}
		}
		for _, s := range []string{
			"", "+", "-", "+-", "-+", "++1", "--1", "+-1", "-+1", " 1", "1 ", " ", "1_000", "_1", "0x10", "0b1", "0o7",
			"1e3", "1.0", "1.", ".1", "12a", "a12", "1a2", "a", "-a", "+a", "１２３", "٣", "1\u00a0", "\u00a01", "1é",
			"1\x00", "\x001", "0-1", "1-", "1+", "256x", "99x", "999x", "-129x", "-99x", "9999999999999999999x",
			"99999999999999999999x", "340282366920938463463374607431768211455x",
			"340282366920938463463374607431768211456x", "x340282366920938463463374607431768211456",
			"-170141183460469231731687303715884105728x", "-170141183460469231731687303715884105729x",
			"18446744073709551615x", "184467440737095516150", "1844674407370955161x", "inf", "NaN", "true",
		} {
			ints(s)
		}
		random := rand.New(rand.NewPCG(16, 1))
		digits := func(n int) string {
			var text strings.Builder
			for ; n > 0; n-- {
				text.WriteByte(byte('0' + random.IntN(10)))
			}
			return text.String()
		}
		for i := 0; i < 400; i++ {
			s := []string{"", "", "+", "-"}[random.IntN(4)] + digits(1+random.IntN(42))
			if random.IntN(8) == 0 {
				junk := []string{"x", " ", "_", "-", "+", ".", "é", "A"}[random.IntN(8)]
				at := random.IntN(len(s) + 1)
				s = s[:at] + junk + s[at:]
			}
			ints(s)
		}

		// from_str_radix, in the radixes that take each loop of the parser,
		// with digits of both cases and the first char that is not a digit.
		for _, radix := range []uint32{2, 3, 8, 10, 16, 35, 36} {
			alphabet := "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
			for _, s := range []string{"", "+", "-", "0", "-0", "1", "-1", "z", "Z", "zz", "-zz", "10", "7f", "7F", "80",
				"-80", "-81", "ff", "FF", "100", "-100", "7fff", "8000", "-8000", "-8001", "ffff", "10000", "7fffffff",
				"80000000", "-80000000", "-80000001", "ffffffff", "100000000", "7fffffffffffffff", "8000000000000000",
				"-8000000000000000", "-8000000000000001", "ffffffffffffffff", "10000000000000000",
				"7fffffffffffffffffffffffffffffff", "80000000000000000000000000000000",
				"-80000000000000000000000000000000", "-80000000000000000000000000000001",
				"ffffffffffffffffffffffffffffffff", "100000000000000000000000000000000", "g", "G", "y", "9", "2", "8",
				strings.Repeat("1", 7), strings.Repeat("1", 8), strings.Repeat("1", 9), strings.Repeat("1", 63),
				strings.Repeat("1", 64), strings.Repeat("1", 65), strings.Repeat("1", 127), strings.Repeat("1", 128),
				strings.Repeat("1", 129), strings.Repeat("z", 12), strings.Repeat("z", 13), strings.Repeat("z", 25),
				"12 ", "1_2", "+-1", "é", "1é"} {
				yield(map[string]any{"op": "radix", "s": s, "radix": radix})
			}
			for i := 0; i < 25; i++ {
				var text strings.Builder
				if random.IntN(3) == 0 {
					text.WriteByte("+-"[random.IntN(2)])
				}
				for n := 1 + random.IntN(30); n > 0; n-- {
					text.WriteByte(alphabet[random.IntN(int(min(radix+2, 62)))])
				}
				yield(map[string]any{"op": "radix", "s": text.String(), "radix": radix})
			}
		}
		for _, radix := range []uint32{0, 1, 37, 100, 1<<32 - 1} {
			yield(map[string]any{"op": "radix", "s": "1", "radix": radix})
			yield(map[string]any{"op": "radix", "s": "", "radix": radix})
		}

		for _, s := range []string{"", "true", "false", "True", "FALSE", "true ", " false", "t", "f", "1", "0", "yes",
			"truefalse", "a", "é", "ab", "😀", "e\u0301", "\u0301", "\x00", "\U0010ffff", "aa", "日本"} {
			misc(s)
		}

	}, func(raw json.RawMessage) any { return answerParse(t, raw) })
}

func TestDiffAgainstRust_parse_float(t *testing.T) {
	oracle.Check(t, "std_str", "parse_float", func(yield func(any)) {
		floats := func(s string) { yield(map[string]any{"op": "parse", "t": "floats", "s": s}) }
		for _, s := range floatSyntax {
			floats(s)
		}
		for _, s := range floatValues() {
			floats(s)
		}
		for _, s := range floatSaturation() {
			floats(s)
		}
		random := rand.New(rand.NewPCG(17, 1))
		for i := 0; i < 2500; i++ {
			floats(randomFloatText(random))
		}
	}, func(raw json.RawMessage) any { return answerParse(t, raw) })
}
