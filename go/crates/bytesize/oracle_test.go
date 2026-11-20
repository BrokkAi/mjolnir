package bytesize

import (
	"encoding/json"
	"math"
	"math/rand/v2"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests ask the real crate (go/port/oracle, module
// `bytesize`) to print, parse and format byte sizes, and hold this package to
// the same answers. The corpora are recorded in testdata/oracle:
//
//   - bytesize: every display style and conversion of a byte count, parsing
//     (`text.parse::<ByteSize>()`, which is what Mjolnir calls) and unit
//     parsing. The Go side answers without rs.Format.
//   - fmt: what goes through rs.Formatter: `{}` with width, fill, alignment
//     and precision for the size and each display style, the Debug forms,
//     and the parse error as a String.
//
// MJ_ORACLE_SCALE multiplies the number of generated random cases. It is for
// a longer run against the live oracle (`MJ_ORACLE=live MJ_ORACLE_SCALE=50
// ...`); the recorded corpus is made with the default of 1.
func oracleScale() int {
	scale, err := strconv.Atoi(os.Getenv("MJ_ORACLE_SCALE"))
	if err != nil || scale < 1 {
		return 1
	}
	return scale
}

type oracleRequest struct {
	Op    string `json:"op"`
	Bytes uint64 `json:"bytes"`
	Text  string `json:"text"`
	Spec  string `json:"spec"`
	What  string `json:"what"`
}

func oracleDecode(t *testing.T, raw json.RawMessage) oracleRequest {
	var request oracleRequest
	if err := json.Unmarshal(raw, &request); err != nil {
		t.Fatalf("decode %s: %v", raw, err)
	}
	return request
}

// oracleStyles are the display() styles, by the names the oracle uses.
var oracleStyles = []string{"iec", "iec_short", "si", "si_short", "iec_bits", "si_bits"}

func oracleStyle(size ByteSize, style string) *Display {
	switch style {
	case "iec":
		return size.Display().Iec()
	case "iec_short":
		return size.Display().IecShort()
	case "si":
		return size.Display().Si()
	case "si_short":
		return size.Display().SiShort()
	case "iec_bits":
		return size.Display().IecBits()
	case "si_bits":
		return size.Display().SiBits()
	default:
		panic(rs.Panicf("unknown style {}", style))
	}
}

func oracleDisplay(size ByteSize, formatter bool) any {
	reply := map[string]any{
		"as_u64":          size.AsU64(),
		"as_kb":           math.Float64bits(size.AsKb()),
		"as_kib":          math.Float64bits(size.AsKib()),
		"as_mb":           math.Float64bits(size.AsMb()),
		"as_mib":          math.Float64bits(size.AsMib()),
		"as_gb":           math.Float64bits(size.AsGb()),
		"as_gib":          math.Float64bits(size.AsGib()),
		"as_tb":           math.Float64bits(size.AsTb()),
		"as_tib":          math.Float64bits(size.AsTib()),
		"as_pb":           math.Float64bits(size.AsPb()),
		"as_pib":          math.Float64bits(size.AsPib()),
		"as_eb":           math.Float64bits(size.AsEb()),
		"as_eib":          math.Float64bits(size.AsEib()),
		"to_string":       size.String(),
		"default_display": size.Display().String(),
		"debug":           size.debugString(),
	}
	if formatter {
		reply["to_string"] = rs.ToString(size)
		reply["default_display"] = rs.ToString(size.Display())
		reply["debug"] = rs.DebugString(size)
	}
	for _, style := range oracleStyles {
		if formatter {
			reply[style] = rs.ToString(oracleStyle(size, style))
		} else {
			reply[style] = oracleStyle(size, style).String()
		}
	}
	return reply
}

func oracleParse(text string, formatter bool) any {
	size, err := ByteSizeFromStr(text)
	if err != nil {
		if formatter {
			return map[string]any{"err": rs.ToString(err), "err_debug": rs.DebugString(err)}
		}
		return map[string]any{"err": err.Error(), "err_debug": err.(parseError).debugString()}
	}
	return map[string]any{"ok": uint64(size)}
}

func oracleUnit(text string, formatter bool) any {
	unit, err := UnitFromStr(text)
	if err != nil {
		if formatter {
			return map[string]any{"err": rs.ToString(err), "err_debug": rs.DebugString(err)}
		}
		return map[string]any{"err": err.Error(), "err_debug": err.(UnitParseError).debugString()}
	}
	name := rs.VariantOf(unit).RustName
	if formatter {
		name = rs.DebugString(unit)
	}
	return map[string]any{"ok": name, "factor": unit.Mul(1)}
}

// oracleSpecs are the format specs the oracle's `format` knows.
var oracleSpecs = []string{
	"{}", "{:10}", "{:<10}", "{:>10}", "{:^10}", "{:->10}", "{:-<10}", "{:-^10}", "{:^11}", "{:3}", "{:.0}", "{:.1}",
	"{:.2}", "{:.3}", "{:.5}", "{:.20}", "{:>12.5}", "{:<12.5}", "{:12.5}", "{:-^15.5}", "{:>10.0}", "{:3.5}",
	"{:30.20}", "{:012}", "{:+}", "{:#}", "{:é^14}", "{:🦀>12}", "{:?}", "{:#?}", "{:>40?}", "{:.1?}",
}

func oracleFormat(request oracleRequest) any {
	if !slices.Contains(oracleSpecs, request.Spec) {
		panic(rs.Panicf("the oracle does not know the spec {}", request.Spec))
	}
	size := ByteSize(request.Bytes)
	switch request.What {
	case "", "size":
		return map[string]any{"text": rs.Format(request.Spec, size)}
	case "parse_error":
		_, err := ByteSizeFromStr(request.Text)
		if err == nil {
			return map[string]any{"invalid": true}
		}
		return map[string]any{"text": rs.Format(request.Spec, err)}
	default:
		return map[string]any{"text": rs.Format(request.Spec, oracleStyle(size, request.What))}
	}
}

func oracleAnswer(t *testing.T, formatter bool) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		request := oracleDecode(t, raw)
		return oracle.Catch(func() any {
			switch request.Op {
			case "display":
				return oracleDisplay(ByteSize(request.Bytes), formatter)
			case "parse":
				return oracleParse(request.Text, formatter)
			case "unit":
				return oracleUnit(request.Text, formatter)
			case "format":
				if !formatter {
					t.Fatalf("format requests belong in the fmt corpus")
				}
				return oracleFormat(request)
			default:
				t.Fatalf("unknown op %q", request.Op)
				return nil
			}
		})
	}
}

// oracleSizes are byte counts at the edges the display branches on: below
// and at each threshold (125 and 128 for bits), around every power of 1000
// and 1024, at the values that round up into the next unit (1023.95 KiB
// prints as 1024.0 KiB), at the exact halves that round to even, at 2^53,
// and at u64::MAX.
func oracleSizes() []uint64 {
	sizes := []uint64{}
	for b := range uint64(140) {
		sizes = append(sizes, b)
	}
	for _, unit := range []uint64{KB, KIB} {
		power := uint64(1)
		for exp := 1; exp <= 6; exp++ {
			power *= unit
			for _, delta := range []int64{-3, -2, -1, 0, 1, 2, 3} {
				sizes = append(sizes, uint64(int64(power)+delta))
			}
			for _, multiple := range []float64{1.05, 1.15, 1.25, 1.35, 1.45, 1.5, 1.55, 1.95, 9.95, 99.95, 999.95, 1023.95, 1023.94, 1023.96, 999.94, 999.96} {
				value := multiple * float64(power)
				if value < 1.8e19 {
					sizes = append(sizes, uint64(value), uint64(value)+1, uint64(value)-1)
				}
			}
		}
	}
	sizes = append(sizes,
		1<<53-1, 1<<53, 1<<53+1, 1<<63-1, 1<<63, 1<<63+1, math.MaxUint64, math.MaxUint64-1, math.MaxUint64/8,
		math.MaxUint64/8+1, 1908*MIB, 1907*MIB, 518*GIB, 518*GB, 301*KB, 419*MB, 815*TB, 609*PB, 15*EIB,
		2873872874893, 999_999_999_999_996, 1_125_899_906_842_621, 1_125_899_906_842_623, 16*EB, 18*EB,
		1_152_921_504_606_846_975,
	)
	return sizes
}

// oracleTexts are size strings written out: the crate's own tests, the
// sizes Mjolnir's configuration accepts, every edge of the number and unit
// grammar, and the whitespace the parser skips between them.
func oracleTexts() []string {
	texts := []string{
		"", "0", "500", "1K", "1Ki", "1.5Ki", "1KiB", "1.5KiB", "3 MB", "4 MiB", "6 GB", "4 GiB", "88TB", "521TiB",
		"8 PB", "8P", "12 PiB", "9007199254740993B", "9007199254740993", "a124GB", "1.3 42.0 B", "1.3 ... B",
		"1 000 B", "128GB", "128.000 GiB", "128000000000", "128.0 GiB",
		"20GB", "20GiB", "100GiB", "8GiB", "8 GiB", "512MiB", "1.5TB", "none", "None", " 8GiB", "8GiB ", "8gib",
		"8G", "8g", "8Gi", "8GIB", "8gB", "8 gb", "8\tGB", "8\nGB", "8  GB", "8 G B",
		".", "..", "1.", ".5", ".5K", "1.2.3", "1.2.3K", "00012", "00012K", "+5", "-5", "+5KB", "-5KB", "+", "-",
		"1e3", "1E", "1E3", "1e", "0x10", "١٢", "１２", "1_000", "1,5K", "1.5 K", "1.5\u00a0K", "5 b", "5 B ",
		"18446744073709551615", "18446744073709551616", "18446744073709551615B", "18446744073709551616B",
		"18446744073709551615KB", "99999999999999999999", "99999999999999999999B", "18446744073709551615.0",
		"18446744073709551615.0B", "16EiB", "15EiB", "15.99999999EiB", "15.999999999999999EiB", "16.0EiB",
		"18EB", "18.4467440737EB", "18.45EB", "19EB", "17.5EiB", "0.5B", "0.9999B", "1.9999B", "0.001KB",
		".0000000000000000000001EB", "1." + strings.Repeat("9", 30) + "KiB", strings.Repeat("1", 400) + "B",
		strings.Repeat("0", 400) + "5B", strings.Repeat("0", 400) + "5", "0." + strings.Repeat("0", 400) + "1EB",
		strings.Repeat("9", 20) + "." + strings.Repeat("9", 20) + "B", "1" + strings.Repeat("0", 309) + ".0B",
		"4.4", "4.4 ", "4.4 \u3000", "4.45KB", "4.4.KB", "4..4KB", "4.K", "4.KiB",
		"5\u212aB", "5\u212a", "5ｋB", "5KĪB", "5KiB\u0301", "5é", "5😀B", "5e3", "5E3", "5bytes", "5byte", "5KBi",
		"5iB", "5KiBB", "5 KB ", "5  KB", "5\u200bKB", "5\ufeffKB", "5\u180eKB", "5\u2060KB", "5ab€", "5a€b",
		"5€€", "5😀", "5a😀", "5ab😀", "5abc😀", "5\U0010ffffx", "5abcd", "5ab\x00", "5\x00", "5\u0085KB",
		"5\"KB\"", "5'KB'", "5\\KB",
	}
	// Each unit spelling in several letter cases.
	for _, unit := range []string{"b", "k", "kb", "m", "mb", "g", "gb", "t", "tb", "p", "pb", "e", "eb", "ki",
		"kib", "mi", "mib", "gi", "gib", "ti", "tib", "pi", "pib", "ei", "eib"} {
		texts = append(texts, "3"+unit, "3"+strings.ToUpper(unit), "2.5 "+unit, "1 "+oracleTitle(unit))
	}
	// Every char of White_Space, and some that look like it but are not,
	// between the number and the unit.
	for _, space := range []rune{0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x20, 0x85, 0xA0, 0x1680, 0x2000, 0x2001, 0x2002,
		0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000,
		0x180E, 0x200B, 0x200C, 0x200D, 0x2060, 0xFEFF, 0x1C, 0x1D, 0x1E, 0x1F} {
		texts = append(texts, "7"+string(space)+"MiB", "7.5"+string(space)+string(space)+"k", "7"+string(space))
	}
	return texts
}

// oracleTitle upper-cases the first letter of an ASCII unit.
func oracleTitle(unit string) string {
	return strings.ToUpper(unit[:1]) + unit[1:]
}

// oracleRandomText builds a size string from random parts: a number, maybe
// a fraction, maybe whitespace, a unit or something like one, and
// sometimes junk.
func oracleRandomText(random *rand.Rand) string {
	pick := func(items []string) string { return items[random.IntN(len(items))] }
	var text strings.Builder
	text.WriteString(pick([]string{"", "0", "1", "5", "12", "999", "1024", "18446744073709551615",
		"18446744073709551616", "9007199254740993", "00", "007", "+1", "-1", " 1", "1.5", ".5", "1.", ".", "1.2.3",
		"0.001", "123456789012345678901234567890", "3.14159265358979323846"}))
	text.WriteString(pick([]string{"", "", "", " ", "  ", "\t", "\u00a0", "\u3000", "\n", "\u200b"}))
	text.WriteString(pick([]string{"", "B", "b", "K", "k", "KB", "kB", "Kb", "KiB", "kib", "Ki", "M", "MB", "MiB",
		"G", "GB", "GiB", "gib", "T", "TB", "TiB", "P", "PB", "PiB", "E", "EB", "EiB", "eib", "Z", "ZB", "ZiB", "bytes",
		"KBB", "iB", "\u212aB", "é", "😀", "Kiß"}))
	if random.IntN(8) == 0 {
		text.WriteString(pick([]string{" ", "x", ".", "5", "B", "\x00", "é", "  KB"}))
	}
	return text.String()
}

// oracleRandomSize is a byte count of random magnitude: as many low bits
// as a random length from 0 to 64.
func oracleRandomSize(random *rand.Rand) uint64 {
	length := random.IntN(65)
	if length == 0 {
		return 0
	}
	return random.Uint64() >> (64 - length)
}

func oracleRandom(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x6f7261636c65))
}

func TestDiffAgainstRust(t *testing.T) {
	oracle.Check(t, "bytesize", "bytesize", func(yield func(any)) {
		random := oracleRandom(1)
		for _, size := range oracleSizes() {
			yield(map[string]any{"op": "display", "bytes": size})
		}
		for range 300 * oracleScale() {
			yield(map[string]any{"op": "display", "bytes": oracleRandomSize(random)})
			yield(map[string]any{"op": "display", "bytes": quickcheckU64(random)})
		}

		for _, text := range oracleTexts() {
			yield(map[string]any{"op": "parse", "text": text})
		}
		// The displayed form of each edge size parses back (or not) as Rust
		// says.
		for _, size := range oracleSizes() {
			yield(map[string]any{"op": "parse", "text": ByteSize(size).String()})
			yield(map[string]any{"op": "parse", "text": ByteSize(size).Display().Si().String()})
			yield(map[string]any{"op": "parse", "text": ByteSize(size).Display().IecShort().String()})
		}
		for range 1500 * oracleScale() {
			yield(map[string]any{"op": "parse", "text": oracleRandomText(random)})
		}
		for range 300 * oracleScale() {
			yield(map[string]any{"op": "parse", "text": quickcheckString(random)})
		}

		for _, text := range oracleTexts() {
			number := parseTakeWhile(text, func(c rune) bool { return ('0' <= c && c <= '9') || c == '.' })
			yield(map[string]any{"op": "unit", "text": strings.TrimLeft(text[len(number):], " ")})
		}
		for _, text := range []string{"", "b", "B", "kb", "KB", "kB", "Kb", "kib", "KIB", "kIb", "KiB", "e", "E",
			"eib", "EIB", "Ei", "iB", "gibibyte", "foo", "foob", "foobar", "a€", "ab€", "abc€", "€€", "😀", "a😀",
			"ab😀", "abc😀", "abcd", "\u212a", "\u212aB", "ｋ", "ß", " k", "k ", "kb\x00", "\x00", "k\u0301"} {
			yield(map[string]any{"op": "unit", "text": text})
		}
	}, oracleAnswer(t, false))
}

// TestDiffFmtAgainstRust compares what goes through rs.Formatter. Until the
// formatter exists it is skipped, except when recording.
func TestDiffFmtAgainstRust(t *testing.T) {
	if os.Getenv("MJ_ORACLE") != "record" {
		requireFormatter(t)
	}
	oracle.Check(t, "bytesize", "fmt", func(yield func(any)) {
		random := oracleRandom(2)
		sizes := []uint64{0, 1, 99, 124, 125, 127, 128, 357, 999, 1000, 1023, 1024, 1280, 1792, 1_048_575,
			1908 * MIB, 518 * GIB, 1_125_899_906_842_623, 1 << 53, 15 * EIB, math.MaxUint64}
		for _, size := range sizes {
			for _, spec := range oracleSpecs {
				yield(map[string]any{"op": "format", "bytes": size, "spec": spec})
			}
		}
		for _, size := range []uint64{0, 127, 128, 1000, 1024, 1908 * MIB, math.MaxUint64} {
			for _, style := range oracleStyles {
				for _, spec := range oracleSpecs {
					yield(map[string]any{"op": "format", "bytes": size, "what": style, "spec": spec})
				}
			}
		}
		for _, text := range []string{"", "x", "5 KiBB", "5\u200bKB", "-5", "5\"KB\"", "8GiB"} {
			for _, spec := range oracleSpecs {
				yield(map[string]any{"op": "format", "what": "parse_error", "text": text, "spec": spec})
			}
		}
		// The formatter-free answers again, through the formatter.
		for _, size := range sizes {
			yield(map[string]any{"op": "display", "bytes": size})
		}
		for range 100 * oracleScale() {
			yield(map[string]any{"op": "display", "bytes": oracleRandomSize(random)})
			yield(map[string]any{"op": "parse", "text": oracleRandomText(random)})
		}
		for _, text := range oracleTexts() {
			yield(map[string]any{"op": "parse", "text": text})
			number := parseTakeWhile(text, func(c rune) bool { return ('0' <= c && c <= '9') || c == '.' })
			yield(map[string]any{"op": "unit", "text": strings.TrimLeft(text[len(number):], " ")})
		}
	}, oracleAnswer(t, true))
}
