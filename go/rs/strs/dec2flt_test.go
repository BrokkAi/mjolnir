package strs

import (
	"math"
	"math/big"
	"math/rand/v2"
	"strconv"
	"strings"
	"testing"
)

// floatSyntax is what the float grammar accepts and refuses.
var floatSyntax = []string{
	"", "+", "-", ".", "+.", "-.", "e5", "E5", "1e", "1E", "1e+", "1e-", ".e1", "1.e1", "1.E-1", ".5", "5.", "+.5",
	"-5.", "+5.e1", "0x10", "0x1p3", "0X1P3", "1_0", "1__0", "_1", " 1", "1 ", "1 e5", "\t1", "1\n",
	"inf", "-inf", "+inf", "INF", "Inf", "iNf", "infinity", "-INFINITY", "Infinity", "+infinity", "iNfInItY",
	"infinit", "infinityy", "infinite", "in", "i", "infi", "infin", "nan", "NaN", "-NaN", "+nan", "NAN", "nAn", "nan1",
	"NaN1", "na", "nanx", "nan(1)", "-", "--inf", "+-inf", "inf-", "inf\x00\x00\x00\x00\x00", "nan\x00", "\x00nan",
	"ınf", "İNF", "ｉｎｆ", "1e5e5", "1.2.3", "1..2", "--1", "+-1", "-+1", "++1", "1-", "1+", "1e5.", "1e5.5", "1f",
	"1.0f32", "1.0f64", "1d", "1e١", "١", "1,5", "1e 5", "1e+ 5", "1e+-5", "1e++5", "0", "-0", "+0", "0.0", "-0.0",
	"0e0", "-0e0", "0e99999", "-0e-99999", "-0e99999999999999999999", ".0", "0.", "-.0", "-0.", "00", "007", "-007.500",
	"1e0", "1E0", "1e+0", "1e-0", "1e00000000000000000001", "1e-00000000000000000001", "1e99999999999999999999",
	"1e-99999999999999999999", "0e99999999999", "1e400", "-1e400", "1e-400", "-1e-400", "1e65535", "1e65536",
	"1e655359", "1e655360", "1e-65535", "1e-65536", "1e-655360", "0.e5", "0.0e-5", "é", "1é", "1\u00a0", "１",
	"1.0e", "1.0e+", "e", "E", "+e5", "-e5", ".e", "-.e5", "1e5 ", "0b1", "1/2", "1%", "$1", "1.5.e5", "TRUE", "null",
}

// floatExact is the exact decimal expansion of m * 2^e.
func floatExact(m *big.Int, e int) string {
	if e >= 0 {
		return new(big.Int).Lsh(m, uint(e)).String()
	}
	// m / 2^k = m * 5^k / 10^k
	k := -e
	scaled := new(big.Int).Exp(big.NewInt(5), big.NewInt(int64(k)), nil)
	scaled.Mul(scaled, m)
	digits := scaled.String()
	if len(digits) <= k {
		digits = strings.Repeat("0", k-len(digits)+1) + digits
	}
	whole, fraction := digits[:len(digits)-k], strings.TrimRight(digits[len(digits)-k:], "0")
	if fraction == "" {
		return whole
	}
	return whole + "." + fraction
}

// floatMidpoint64 is the decimal number exactly halfway between the positive
// finite float64 with the given bits and the next one up.
func floatMidpoint64(word uint64) string {
	mantissa := word & (1<<52 - 1)
	exponent := int(word >> 52 & 0x7FF)
	if exponent == 0 {
		exponent = 1
	} else {
		mantissa |= 1 << 52
	}
	m := new(big.Int).SetUint64(mantissa)
	m.Lsh(m, 1).Add(m, big.NewInt(1))
	return floatExact(m, exponent-1075-1)
}

// floatMidpoint32 is the same for a float32.
func floatMidpoint32(word uint32) string {
	mantissa := uint64(word & (1<<23 - 1))
	exponent := int(word >> 23 & 0xFF)
	if exponent == 0 {
		exponent = 1
	} else {
		mantissa |= 1 << 23
	}
	m := new(big.Int).SetUint64(mantissa)
	m.Lsh(m, 1).Add(m, big.NewInt(1))
	return floatExact(m, exponent-150-1)
}

// floatNudge changes the last digit of a decimal by one, up or down, which
// moves a halfway case to just one side.
func floatNudge(s string, up bool) string {
	if up {
		if strings.Contains(s, ".") {
			return s + "1"
		}
		return s + ".000000000000000000001"
	}
	// One below: decrement the last nonzero digit and append nines.
	i := len(s) - 1
	for i >= 0 && (s[i] == '0' || s[i] == '.') {
		i--
	}
	if i < 0 {
		return s
	}
	lowered := s[:i] + string(s[i]-1) + s[i+1:]
	if !strings.Contains(lowered, ".") {
		lowered += "."
		// Borrowing across trailing zeros of an integer: 1000 - tiny.
		lowered = strings.TrimRight(s[:i]+string(s[i]-1), "") + strings.Repeat("9", len(s)-i-1) + "."
	}
	return lowered + "99999999999999999999"
}

// floatValues are the numbers the conversion's paths turn on.
func floatValues() []string {
	values := []string{
		"1", "2", "3", "10", "0.1", "0.2", "0.3", "0.5", "1.5", "2.5", "3.14", "-3.14", "2.5E10", "2.5e-10", "123456789",
		"1e22", "1e23", "1e24", "1e15", "1e16", "9007199254740992", "9007199254740993", "9007199254740994",
		"9007199254740995", "9007199254740993.0", "9007199254740992.999", "9007199254740993.00000000000000001",
		"9007199254740993e0", "900719925474099.3e1", "16777216", "16777217", "16777218", "16777219", "16777217.0",
		"16777217.000000001", "16777216.999999999", "33554434", "33554435", "0.000001", "123456.789e-3", "1e-22",
		"1e-23", "4.9e-324", "5e-324", "4.9406564584124654e-324", "2.4703282292062327e-324", "2.4703282292062328e-324",
		"2.47e-324", "2.5e-324", "1e-323", "1e-324", "1e-325", "2.2250738585072014e-308", "2.2250738585072011e-308",
		"2.2250738585072012e-308", "2.225073858507201e-308", "4.4501477170144023e-308", "1.7976931348623157e308",
		"1.7976931348623158e308", "1.7976931348623159e308", "1.797693134862315807e308", "1.797693134862315808e308",
		"1.8e308", "2e308", "1e308", "1e309", "179769313486231570000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
		"3.4028235e38", "3.4028236e38", "3.40282356e38", "3.4028235677973366e38", "3.4028235677973367e38", "3.5e38",
		"1e38", "1e39", "1.4e-45", "1e-45", "7e-46", "7.006492321624085e-46", "7.0064923216240854e-46", "8e-46",
		"1.17549435e-38", "1.1754942e-38", "1.1754943e-38", "6.9294956446009195e15", "3.0102449725576e-310",
		"8.98846567431158e307", "1.0000000000000002", "1.00000000000000011102230246251565404236316680908203125",
		"1.00000000000000011102230246251565404236316680908203124", "1.00000000000000011102230246251565404236316680908203126",
		"0.1000000000000000055511151231257827021181583404541015625", "123456789012345678901234567890",
		"12345678901234567890", "1234567890123456789", "99999999999999999999", "18446744073709551615",
		"18446744073709551616", "0.00000000000000000000000000000000000000000001", "1.1920928955078125e-07",
		"7.038531e-26", "1.00000017881393432617187499", "1.000000178813934326171875", "1.00000017881393432617187501",
		"100000000000000016777215", "100000000000000016777216", "1e-320", "2.2250738585072013e-308",
		"1090544144181609348835077142190", "4.35e-10", "0.9999999999999999", "0.99999999999999999", "0.999999999999999999999",
		"9223372036854775807", "9223372036854775808", "9223372036854775809", "1152921504606846976",
		strings.Repeat("9", 800), strings.Repeat("9", 768) + "1", strings.Repeat("9", 767), "0." + strings.Repeat("9", 800),
		"1" + strings.Repeat("0", 400), "0." + strings.Repeat("0", 400) + "1", "1" + strings.Repeat("0", 767) + "1e-767",
		"1" + strings.Repeat("0", 769) + "1e-769", "0." + strings.Repeat("0", 300) + strings.Repeat("123456789", 90),
		strings.Repeat("0", 500) + "1." + strings.Repeat("0", 500) + "1", strings.Repeat("0", 30) + "." + strings.Repeat("0", 30),
		"1" + strings.Repeat("0", 18), "1" + strings.Repeat("0", 19), "1" + strings.Repeat("0", 20),
		"0." + strings.Repeat("0", 18) + "1", "1234567890123456789.0123456789", "0.0000001234567890123456789012",
		"00000000000000000000000001", "0.000000000000000000000000010", "1.0000000000000000000000000000000000000000",
	}
	for exponent := -30; exponent <= 40; exponent++ {
		values = append(values, "1e"+strconv.Itoa(exponent), "9007199254740993e"+strconv.Itoa(exponent),
			"16777217e"+strconv.Itoa(exponent), "123456789012345678e"+strconv.Itoa(exponent))
	}
	// The halfway points around a few floats of each width, and each side
	// of them.
	for _, word := range []uint64{
		math.Float64bits(1), math.Float64bits(1) - 1, math.Float64bits(2) - 1, math.Float64bits(0.1), math.Float64bits(1e22),
		math.Float64bits(1e23), math.Float64bits(123456.789), 1, 2, 0, 1<<52 - 1, 1 << 52, 1<<52 + 1,
		math.Float64bits(math.MaxFloat64) - 1, math.Float64bits(math.MaxFloat64), math.Float64bits(9007199254740992),
		math.Float64bits(5e-324), math.Float64bits(1e-310), math.Float64bits(3e100), math.Float64bits(7e-200),
	} {
		mid := floatMidpoint64(word)
		values = append(values, mid, floatNudge(mid, true), floatNudge(mid, false))
	}
	for _, word := range []uint32{
		math.Float32bits(1), math.Float32bits(1) - 1, math.Float32bits(16777216), math.Float32bits(0.1), 1, 0, 1<<23 - 1,
		1 << 23, math.Float32bits(math.MaxFloat32) - 1, math.Float32bits(math.MaxFloat32), math.Float32bits(1e10),
		math.Float32bits(1e-10), math.Float32bits(3e30), math.Float32bits(1e-40),
	} {
		mid := floatMidpoint32(word)
		values = append(values, mid, floatNudge(mid, true), floatNudge(mid, false))
	}
	return values
}

// floatSaturation are inputs long enough to show where Rust stops reading an
// exponent (at 0x10000), which no other correctly rounding parser shares.
func floatSaturation() []string {
	zeros := func(n int) string { return strings.Repeat("0", n) }
	return []string{
		"0." + zeros(9999) + "1e10000",
		"0." + zeros(9999) + "1e100000",
		"0." + zeros(65535) + "1e65536",
		"0." + zeros(69999) + "1e70000",
		"0." + zeros(69999) + "1e700000",
		"1" + zeros(70000) + "e-70000",
		"1" + zeros(70000) + "e-700000",
		zeros(70000) + "1e0",
	}
}

func randomDigits(random *rand.Rand, n int) string {
	var text strings.Builder
	for ; n > 0; n-- {
		text.WriteByte(byte('0' + random.IntN(10)))
	}
	return text.String()
}

// randomFloatText draws a float literal from the families the conversion
// treats differently.
func randomFloatText(random *rand.Rand) string {
	sign := []string{"", "", "", "-", "+"}[random.IntN(5)]
	exponent := func(limit int) string {
		if random.IntN(3) == 0 {
			return ""
		}
		return "eE"[random.IntN(2):][:1] + []string{"", "+", "-"}[random.IntN(3)] + strconv.Itoa(random.IntN(limit))
	}
	switch random.IntN(12) {
	case 0: // short decimals: the fast path
		text := randomDigits(random, 1+random.IntN(5))
		if random.IntN(2) == 0 {
			text += "." + randomDigits(random, random.IntN(6))
		}
		return sign + text + exponent(30)
	case 1: // 15 to 21 significant digits: Eisel-Lemire
		digits := randomDigits(random, 15+random.IntN(7))
		at := random.IntN(len(digits) + 1)
		return sign + digits[:at] + "." + digits[at:] + exponent(330)
	case 2: // exact halfway between two float64
		return sign + floatMidpoint64(random.Uint64()&(1<<63-1)%math.Float64bits(math.Inf(1)))
	case 3: // just off halfway
		mid := floatMidpoint64(random.Uint64() & (1<<63 - 1) % math.Float64bits(math.Inf(1)))
		return sign + floatNudge(mid, random.IntN(2) == 0)
	case 4: // exact halfway between two float32, and just off
		mid := floatMidpoint32(random.Uint32() & (1<<31 - 1) % math.Float32bits(float32(math.Inf(1))))
		if random.IntN(2) == 0 {
			return sign + mid
		}
		return sign + floatNudge(mid, random.IntN(2) == 0)
	case 5: // around the smallest and the largest values
		return sign + []string{"4.9", "2.47", "2.4703282292062327", "2.2250738585072", "1.7976931348623", "3.40282", "1.4", "7.006492"}[random.IntN(8)] +
			randomDigits(random, random.IntN(6)) + "e" + []string{"-324", "-323", "-308", "-307", "308", "38", "-45", "-46", "-38"}[random.IntN(9)]
	case 6: // a float64 printed and read back
		value := math.Float64frombits(random.Uint64())
		if math.IsNaN(value) || math.IsInf(value, 0) {
			return "1"
		}
		return strconv.FormatFloat(value, "eg"[random.IntN(2)], []int{-1, 17, 16, 20}[random.IntN(4)], 64)
	case 7: // a float32 printed and read back
		value := math.Float32frombits(random.Uint32())
		if value != value || math.IsInf(float64(value), 0) {
			return "1"
		}
		return strconv.FormatFloat(float64(value), "eg"[random.IntN(2)], []int{-1, 9, 8, 12}[random.IntN(4)], 32)
	case 8: // long mantissas: the big-decimal fallback
		digits := randomDigits(random, 30+random.IntN(400))
		at := random.IntN(len(digits) + 1)
		return sign + digits[:at] + "." + digits[at:] + exponent(400)
	case 9: // leading and trailing zeros around a few digits
		return sign + strings.Repeat("0", random.IntN(25)) + randomDigits(random, random.IntN(4)) + "." +
			strings.Repeat("0", random.IntN(25)) + randomDigits(random, 1+random.IntN(22)) + strings.Repeat("0", random.IntN(25)) + exponent(60)
	case 10: // exponents that cancel a long fraction
		n := random.IntN(340)
		return sign + "0." + strings.Repeat("0", n) + randomDigits(random, 1+random.IntN(20)) + "e" + strconv.Itoa(n+random.IntN(5)-2)
	default: // integers of 16 to 40 digits
		return sign + randomDigits(random, 16+random.IntN(25)) + exponent(20)
	}
}

// strconv.ParseFloat rounds correctly too, so on input of ordinary size the
// port must return the very same bits. (On absurdly long input the two cut
// off the exponent at different places; the oracle corpus covers that.)
func TestDec2fltAgreesWithStrconv(t *testing.T) {
	random := rand.New(rand.NewPCG(18, 1))
	inputs := append(floatValues(), "0", "-0", "1e400", "-1e400", "1e-400")
	count := 400000
	if testing.Short() {
		count = 20000
	}
	failures := 0
	check := func(s string) {
		want64, err := strconv.ParseFloat(s, 64)
		if err != nil && err.(*strconv.NumError).Err != strconv.ErrRange {
			t.Fatalf("strconv rejects %q: %v", s, err)
		}
		want32, _ := strconv.ParseFloat(s, 32)
		got64, err64 := ParseF64(s)
		got32, err32 := ParseF32(s)
		if err64 != nil || err32 != nil ||
			math.Float64bits(got64) != math.Float64bits(want64) ||
			math.Float32bits(got32) != math.Float32bits(float32(want32)) {
			failures++
			if failures <= 10 {
				t.Errorf("%q: f64 %x (%v), strconv %x; f32 %x (%v), strconv %x", s,
					math.Float64bits(got64), err64, math.Float64bits(want64),
					math.Float32bits(got32), err32, math.Float32bits(float32(want32)))
			}
		}
	}
	for _, s := range inputs {
		check(s)
	}
	for i := 0; i < count; i++ {
		check(randomFloatText(random))
	}
	if failures > 10 {
		t.Errorf("%d more inputs differ", failures-10)
	}
}

// Port of coretests 1.99.1 tests/num/dec2flt/mod.rs (the tests that go
// through `str::parse`).

func dec2fltOk(t *testing.T, s string, want float64) {
	t.Helper()
	got, err := ParseF64(s)
	if err != nil || math.Float64bits(got) != math.Float64bits(want) {
		t.Errorf("ParseF64(%q) = %v, %v; want %v", s, got, err, want)
	}
	got32, err := ParseF32(s)
	if err != nil || math.Float32bits(got32) != math.Float32bits(float32(want)) {
		// The f32 expectation only holds where narrowing the f64 is exact
		// rounding, which is every case of these tests.
		want32, _ := strconv.ParseFloat(s, 32)
		if math.Float32bits(got32) != math.Float32bits(float32(want32)) {
			t.Errorf("ParseF32(%q) = %v, %v; want %v", s, got32, err, float32(want32))
		}
	}
}

func TestDec2flt_ordinary(t *testing.T) {
	dec2fltOk(t, "1.0", 1.0)
	dec2fltOk(t, "3e-5", 3e-5)
	dec2fltOk(t, "0.1", 0.1)
	dec2fltOk(t, "12345.", 12345.)
	dec2fltOk(t, "0.9999999", 0.9999999)
	dec2fltOk(t, "2.2250738585072014e-308", 2.2250738585072014e-308)
}

func TestDec2flt_special_code_paths(t *testing.T) {
	dec2fltOk(t, "36893488147419103229.0", 36893488147419103229.0) // 2^65 - 3, triggers half-to-even with even significand
	dec2fltOk(t, "101e-33", 101e-33)                               // Triggers the tricky underflow case in AlgorithmM (for f32)
	dec2fltOk(t, "1e23", 1e23)                                     // Triggers AlgorithmR
	dec2fltOk(t, "2075e23", 2075e23)                               // Triggers another path through AlgorithmR
	dec2fltOk(t, "8713e-23", 8713e-23)                             // ... and yet another.
}

func TestDec2flt_large(t *testing.T) {
	dec2fltOk(t, "1e300", 1e300)
	dec2fltOk(t, "123456789.34567e250", 123456789.34567e250)
	dec2fltOk(t, "943794359898089732078308743689303290943794359843568973207830874368930329.", 943794359898089732078308743689303290943794359843568973207830874368930329.)
}

func TestDec2flt_subnormals(t *testing.T) {
	dec2fltOk(t, "5e-324", 5e-324)
	dec2fltOk(t, "91e-324", 91e-324)
	dec2fltOk(t, "1e-322", 1e-322)
	dec2fltOk(t, "13245643e-320", 13245643e-320)
	dec2fltOk(t, "2.22507385851e-308", 2.22507385851e-308)
	dec2fltOk(t, "2.1e-308", 2.1e-308)
	dec2fltOk(t, "4.9406564584124654e-324", 4.9406564584124654e-324)
}

func TestDec2flt_infinity(t *testing.T) {
	dec2fltOk(t, "1e400", math.Inf(1))
	dec2fltOk(t, "1e309", math.Inf(1))
	dec2fltOk(t, "2e308", math.Inf(1))
	dec2fltOk(t, "1.7976931348624e308", math.Inf(1))
}

func TestDec2flt_zero(t *testing.T) {
	dec2fltOk(t, "0", 0.0)
	dec2fltOk(t, "0.0", 0.0)
	dec2fltOk(t, "1e-325", 0.0)
	dec2fltOk(t, "1e-326", 0.0)
	dec2fltOk(t, "1e-500", 0.0)
}

func TestDec2flt_many_digits(t *testing.T) {
	dec2fltOk(t, "1.175494140627517859246175898662808184331245864732796240031385942718174675986064769972472277004271745681762695312500000000000000000e-38", 1.175494140627517859246175898662808184331245864732796240031385942718174675986064769972472277004271745681762695312500000000000000000e-38)
}

func TestDec2flt_fast_path_correct(t *testing.T) {
	// This number triggers the fast path and is handled incorrectly when compiling on
	// x86 without SSE2 (i.e., using the x87 FPU stack).
	dec2fltOk(t, "1.448997445238699", 1.448997445238699)
}

func TestDec2flt_lonely_dot(t *testing.T) {
	if _, err := ParseF64("."); err == nil {
		t.Error(`"." parses`)
	}
}

func TestDec2flt_exponentiated_dot(t *testing.T) {
	if _, err := ParseF64(".e0"); err == nil {
		t.Error(`".e0" parses`)
	}
}

func TestDec2flt_lonely_sign(t *testing.T) {
	for _, s := range []string{"+", "-"} {
		if _, err := ParseF64(s); err == nil {
			t.Errorf("%q parses", s)
		}
	}
}

func TestDec2flt_whitespace(t *testing.T) {
	if _, err := ParseF64(" 1.0"); err == nil {
		t.Error(`" 1.0" parses`)
	}
	if _, err := ParseF64("1.0 "); err == nil {
		t.Error(`"1.0 " parses`)
	}
}

func TestDec2flt_nan(t *testing.T) {
	for _, s := range []string{"NaN", "nan", "+nan", "-NAN"} {
		value, err := ParseF64(s)
		if err != nil || !math.IsNaN(value) {
			t.Errorf("ParseF64(%q) = %v, %v", s, value, err)
		}
		value32, err := ParseF32(s)
		if err != nil || value32 == value32 {
			t.Errorf("ParseF32(%q) = %v, %v", s, value32, err)
		}
	}
}

func TestDec2flt_inf(t *testing.T) {
	for s, want := range map[string]float64{"inf": math.Inf(1), "-inf": math.Inf(-1), "+infinity": math.Inf(1), "-INFINITY": math.Inf(-1)} {
		if value, err := ParseF64(s); err != nil || value != want {
			t.Errorf("ParseF64(%q) = %v, %v", s, value, err)
		}
		if value, err := ParseF32(s); err != nil || float64(value) != want {
			t.Errorf("ParseF32(%q) = %v, %v", s, value, err)
		}
	}
}

func TestDec2flt_massive_exponent(t *testing.T) {
	max := strconv.FormatInt(math.MaxInt64, 10)
	dec2fltOk(t, "1e"+max+"000", math.Inf(1))
	dec2fltOk(t, "1e-"+max+"000", 0.0)
	dec2fltOk(t, "1e"+max+"000", math.Inf(1))
}
