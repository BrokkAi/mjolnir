package rs_test

import (
	"encoding/json"
	"math"
	"math/big"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// Differential tests of the formatter against Rust's core::fmt, through the
// oracle module std_fmt (go/port/oracle/src/std_fmt.rs).
//
// The corpora under testdata/oracle are what Rust answered when they were
// recorded:
//
//	MJ_ORACLE=record go/port/gotest.sh ./go/rs/ -run TestFmtOracle
//
// To keep the package's recorded data small, a corpus holds a deterministic
// part of what its generator produces when MJ_ORACLE=live: a quarter of the
// random values, every third value of the sweeps over powers of two and of
// ten, and fewer random specs for each edge value. MJ_ORACLE=live asks Rust
// about the whole set as well as replaying the corpus. With
// RS_FMT_ORACLE_SCALE=n as well, n times as many random values are
// generated, for a longer comparison still.

// oracleRng is a splitmix64 generator: the same sequence everywhere, whatever
// the Go release.
type oracleRng struct{ state uint64 }

func (r *oracleRng) next() uint64 {
	r.state += 0x9E3779B97F4A7C15
	z := r.state
	z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
	z = (z ^ (z >> 27)) * 0x94D049BB133111EB
	return z ^ (z >> 31)
}

func (r *oracleRng) intn(n int) int { return int(r.next() % uint64(n)) }

func oraclePick[T any](r *oracleRng, items []T) T { return items[r.intn(len(items))] }

// oracleLive reports whether the generators run to ask Rust (MJ_ORACLE=live)
// rather than to record a corpus.
func oracleLive() bool { return os.Getenv("MJ_ORACLE") == "live" }

// oracleRandoms is how many random values to generate where the whole set has
// n.
func oracleRandoms(n int) int {
	if !oracleLive() {
		return n / 4
	}
	if scale, err := strconv.Atoi(os.Getenv("RS_FMT_ORACLE_SCALE")); err == nil && scale > 0 {
		return n * scale
	}
	return n
}

// oracleStride is the step of a sweep over edge values whose whole set has
// step step.
func oracleStride(step int) int {
	if oracleLive() {
		return step
	}
	return step * 3
}

// oracleSpecs is how many random specs to format an edge value with where
// the whole set has n.
func oracleSpecs(n int) int {
	if oracleLive() {
		return n
	}
	return (n + 2) / 3
}

// The flag combinations the oracle compiled.
var (
	oracleFillAligns = []string{"", "<", "^", ">", "*<", "*^", "*>", "0<", "0^", "0>", "é<", "é^", "é>"}
	oracleSigns      = []string{"", "+", "-"}
)

// flags returns `[[fill]align][sign]['#']['0']`, plain more often than not.
func (r *oracleRng) flags() string {
	var flags string
	if r.intn(2) == 0 {
		flags += oraclePick(r, oracleFillAligns)
	}
	if r.intn(3) == 0 {
		flags += oraclePick(r, oracleSigns)
	}
	if r.intn(4) == 0 {
		flags += "#"
	}
	if r.intn(4) == 0 {
		flags += "0"
	}
	return flags
}

// oracleNone is "no width" or "no precision" in a list of choices.
const oracleNone = -1

// spec returns one spec for a `fmt` request: flags, type, width, precision,
// and how the Go side is to write the width and the precision (see
// oracleFormat).
func (r *oracleRng) spec(types []string, widths, precisions []int) []any {
	spec := []any{r.flags(), oraclePick(r, types), nil, nil, r.intn(4)}
	if width := oraclePick(r, widths); width != oracleNone {
		spec[2] = width
	}
	if precision := oraclePick(r, precisions); precision != oracleNone {
		spec[3] = precision
	}
	return spec
}

func (r *oracleRng) specs(count int, types []string, widths, precisions []int) [][]any {
	specs := make([][]any, count)
	for i := range specs {
		specs[i] = r.spec(types, widths, precisions)
	}
	return specs
}

// oracleEverySpec returns the plain spec of each type first, so that every
// value is at least formatted the ordinary way, then count random ones.
func (r *oracleRng) everySpec(count int, types []string, widths, precisions []int) [][]any {
	var specs [][]any
	for _, ty := range types {
		specs = append(specs, []any{"", ty, nil, nil, 0})
	}
	return append(specs, r.specs(count, types, widths, precisions)...)
}

func oracleFmtRequest(value any, specs [][]any) map[string]any {
	return map[string]any{"op": "fmt", "v": value, "specs": specs}
}

// oracleFormat formats value with one spec of a `fmt` request. The oracle
// always takes the width and the precision as arguments; here they are
// written into the format string as numbers (mode 0), or passed as
// positional arguments (1), as named ones (2), or with `.*` (3), which must
// all mean the same.
func oracleFormat(value any, spec []json.RawMessage) string {
	flags, ty := oracleText(spec[0]), oracleText(spec[1])
	hasWidth, hasPrecision := string(spec[2]) != "null", string(spec[3]) != "null"
	var width, precision int
	if hasWidth {
		width = int(oracleInt(spec[2]))
	}
	if hasPrecision {
		precision = int(oracleInt(spec[3]))
	}
	mode := int(oracleInt(spec[4]))
	if mode == 3 && !hasPrecision {
		mode = 0
	}
	// `{:0}` is the `0` flag, not a width of zero.
	if mode == 0 && hasWidth && width == 0 && !strings.HasSuffix(flags, "0") {
		mode = 1
	}
	if mode == 3 && hasWidth && width == 0 && !strings.HasSuffix(flags, "0") {
		mode = 2
	}

	format := "{:" + flags
	args := []any{value}
	switch mode {
	case 0:
		if hasWidth {
			format += strconv.Itoa(width)
		}
		if hasPrecision {
			format += "." + strconv.Itoa(precision)
		}
	case 1:
		if hasWidth {
			format += strconv.Itoa(len(args)) + "$"
			args = append(args, width)
		}
		if hasPrecision {
			format += "." + strconv.Itoa(len(args)) + "$"
			args = append(args, precision)
		}
	case 2:
		if hasWidth {
			format += "w$"
			args = append(args, rs.Arg("w", width))
		}
		if hasPrecision {
			format += ".p$"
			args = append(args, rs.Arg("p", precision))
		}
	default:
		if hasWidth {
			format += strconv.Itoa(width)
		}
		format += ".*"
		args = []any{precision, value}
	}
	format += ty + "}"
	return rs.Format(format, args...)
}

// oracleAnswer is the Go side of every std_fmt request.
func oracleAnswer(request json.RawMessage) any {
	var parsed struct {
		Op      string              `json:"op"`
		V       json.RawMessage     `json:"v"`
		Specs   [][]json.RawMessage `json:"specs"`
		F       string              `json:"f"`
		Args    string              `json:"args"`
		Capture []string            `json:"capture"`
	}
	if err := json.Unmarshal(request, &parsed); err != nil {
		panic(err)
	}
	switch parsed.Op {
	case "fmt":
		return oracle.Catch(func() any {
			value := oracleValue(parsed.V)
			out := make([]string, len(parsed.Specs))
			for i, spec := range parsed.Specs {
				out[i] = oracleFormat(value, spec)
			}
			return map[string]any{"out": out}
		})
	case "tmpl":
		return oracle.Catch(func() any {
			return map[string]any{"out": rs.Format(parsed.F, oracleTemplateArgs(parsed.Args, parsed.Capture)...)}
		})
	case "escaped":
		return map[string]any{"ranges": oracleCharRanges(func(c rune) bool {
			return strings.HasPrefix(rs.Format("{:?}", rs.Char(c)), `'\u{`)
		})}
	case "whitespace":
		return map[string]any{"ranges": oracleCharRanges(rs.FmtTestIsWhitespace)}
	}
	panic("no oracle op " + parsed.Op)
}

// oracleCharRanges returns the inclusive ranges of chars a predicate holds
// for. A Rust char is never a surrogate.
func oracleCharRanges(holds func(c rune) bool) [][2]uint32 {
	ranges := [][2]uint32{}
	for c := rune(0); c <= 0x10FFFF; c++ {
		if (c >= 0xD800 && c <= 0xDFFF) || !holds(c) {
			continue
		}
		if last := len(ranges) - 1; last >= 0 && ranges[last][1]+1 == uint32(c) {
			ranges[last][1] = uint32(c)
		} else {
			ranges = append(ranges, [2]uint32{uint32(c), uint32(c)})
		}
	}
	return ranges
}

func oracleCheck(t *testing.T, corpus string, generate oracle.Generate) {
	t.Helper()
	oracle.Check(t, "std_fmt", corpus, generate, oracleAnswer)
}

// Integers.

type oracleIntType struct {
	tag    string
	bits   uint
	signed bool
}

var oracleIntTypes = []oracleIntType{
	{"i8", 8, true}, {"i16", 16, true}, {"i32", 32, true}, {"i64", 64, true}, {"i128", 128, true}, {"isize", 64, true},
	{"u8", 8, false}, {"u16", 16, false}, {"u32", 32, false}, {"u64", 64, false}, {"u128", 128, false}, {"usize", 64, false},
}

func (ty oracleIntType) min() *big.Int {
	if !ty.signed {
		return big.NewInt(0)
	}
	return new(big.Int).Neg(new(big.Int).Lsh(big.NewInt(1), ty.bits-1))
}

func (ty oracleIntType) max() *big.Int {
	bits := ty.bits
	if ty.signed {
		bits--
	}
	return new(big.Int).Sub(new(big.Int).Lsh(big.NewInt(1), bits), big.NewInt(1))
}

func (ty oracleIntType) holds(n *big.Int) bool {
	return n.Cmp(ty.min()) >= 0 && n.Cmp(ty.max()) <= 0
}

// random returns a value of the type with a random number of significant
// bits, so that small and large magnitudes are equally likely.
func (ty oracleIntType) random(r *oracleRng) *big.Int {
	n := new(big.Int).SetUint64(r.next())
	n.Lsh(n, 64).Or(n, new(big.Int).SetUint64(r.next()))
	n.Rsh(n, 128-uint(r.intn(int(ty.bits)+1)))
	if ty.signed && r.intn(2) == 0 {
		n.Neg(n)
	}
	if !ty.holds(n) {
		return ty.max()
	}
	return n
}

// edges returns the values around which integer formatting branches: the
// ends of the range, the powers of ten and their neighbours, and the values
// that round up to the next power of ten in `{:.Ne}`.
func (ty oracleIntType) edges() []*big.Int {
	candidates := []*big.Int{ty.min(), ty.max(), new(big.Int).Add(ty.min(), big.NewInt(1)), new(big.Int).Sub(ty.max(), big.NewInt(1))}
	for _, small := range []int64{0, 1, -1, 2, 5, 9, 15, 25, 35, 45, 95, 99, 105, 115, 125, 995, 1005, 1015, 1025, 1250, 1350, 9995, 12345, 99995, 123456789, 1234500000} {
		candidates = append(candidates, big.NewInt(small), big.NewInt(-small))
	}
	power := big.NewInt(1)
	for i := 0; i < 39; i++ {
		power = new(big.Int).Mul(power, big.NewInt(10))
		for _, offset := range []int64{-1, 0, 1} {
			candidates = append(candidates, new(big.Int).Add(power, big.NewInt(offset)))
			candidates = append(candidates, new(big.Int).Neg(new(big.Int).Add(power, big.NewInt(offset))))
		}
		half := new(big.Int).Div(power, big.NewInt(2))
		candidates = append(candidates, half, new(big.Int).Mul(power, big.NewInt(15)), new(big.Int).Mul(power, big.NewInt(25)))
	}
	var edges []*big.Int
	seen := map[string]bool{}
	for _, candidate := range candidates {
		if ty.holds(candidate) && !seen[candidate.String()] {
			seen[candidate.String()] = true
			edges = append(edges, candidate)
		}
	}
	return edges
}

var (
	oracleIntSpecTypes  = []string{"", "?", "x?", "X?", "x", "X", "o", "b", "e", "E"}
	oracleIntWidths     = []int{oracleNone, oracleNone, 0, 1, 2, 5, 8, 12, 20, 45, 70, 135}
	oracleIntPrecisions = []int{oracleNone, oracleNone, oracleNone, 0, 1, 2, 3, 5, 10, 25, 45}
)

func TestFmtOracleIntegers(t *testing.T) {
	oracleCheck(t, "std_fmt_integers", func(yield func(any)) {
		r := &oracleRng{state: 1}
		for _, ty := range oracleIntTypes {
			for i, edge := range ty.edges() {
				specs := r.specs(oracleSpecs(6), oracleIntSpecTypes, oracleIntWidths, oracleIntPrecisions)
				if i < 4 {
					specs = r.everySpec(6, oracleIntSpecTypes, oracleIntWidths, oracleIntPrecisions)
				}
				yield(oracleFmtRequest([]any{ty.tag, edge.String()}, specs))
			}
			for i := 0; i < oracleRandoms(40); i++ {
				yield(oracleFmtRequest([]any{ty.tag, ty.random(r).String()},
					r.specs(8, oracleIntSpecTypes, oracleIntWidths, oracleIntPrecisions)))
			}
		}
		// The exponent form rounds half to even: every precision of some
		// values with fives in them.
		for _, digits := range []string{"5", "15", "25", "125", "135", "9995", "99985", "55555555555", "44444444445", "18446744073709551615", "9223372036854775807", "340282366920938463463374607431768211455"} {
			n, _ := new(big.Int).SetString(digits, 10)
			for _, ty := range oracleIntTypes {
				if !ty.holds(n) {
					continue
				}
				var specs [][]any
				for precision := 0; precision <= len(digits)+1; precision++ {
					specs = append(specs, []any{"", "e", nil, precision, precision % 4})
				}
				yield(oracleFmtRequest([]any{ty.tag, digits}, specs))
			}
		}
	})
}

// Floats.

var (
	oracleFloatSpecTypes  = []string{"", "", "?", "?", "e", "E", "x?"}
	oracleFloatWidths     = []int{oracleNone, oracleNone, oracleNone, 0, 5, 10, 25, 40}
	oracleFloatPrecisions = []int{oracleNone, oracleNone, oracleNone, oracleNone, 0, 1, 2, 3, 4, 6, 9, 15, 16, 17, 18, 20, 30, 50}
	// Long outputs, used sparingly: the corpus has a size to keep.
	oracleFloatLongPrecisions = []int{100, 330, 400, 770, 1100}
)

// oracleF64Edges returns the doubles formatting has reasons to treat apart.
func oracleF64Edges() []uint64 {
	var edges []uint64
	add := func(values ...float64) {
		for _, value := range values {
			edges = append(edges, math.Float64bits(value), math.Float64bits(-value))
		}
	}
	add(0, 1, 2, 10, 0.1, 0.2, 0.3, 0.5, 1.5, 2.5, 3.5, 0.25, 0.75, 0.125, 0.375, 0.0625,
		1e15, 1e16, 1e17, 9.999999999999998e15, 9.999999999999999e15, 1.0000000000000002e16,
		1e-4, 1e-5, 9.999999999999999e-5, 0.00010000000000000002, 1e-7, 123456789, 1.23456789e-7,
		math.MaxFloat64, math.SmallestNonzeroFloat64, 2.2250738585072014e-308, 2.225073858507201e-308,
		math.Inf(1), math.Pi, math.E, 1.0/3.0, 2.0/3.0, 100, 1000, 1e21, 1e22, 1e23, 5e-324, 1.7976931348623157e308,
		9.5, 99.5, 999.5, 0.95, 0.995, 0.9995, 9.995, 0.05, 0.005, 0.0005, 0.15, 0.25, 0.35, 0.45, 0.55,
		4.35, 8.345, 1.005, 2.675, 1.45, 5e-1, 4.5, 123456.5, 0.000001, 0.0000001, 4503599627370496, 9007199254740992,
		9007199254740993, 18446744073709551615, 1.8446744073709552e19, 0.1+0.2, 5e15, 5e16, 1.5e16)
	// NaNs with different payloads and signs.
	edges = append(edges, 0x7FF8000000000000, 0xFFF8000000000000, 0x7FF0000000000001, 0x7FFFFFFFFFFFFFFF, 0xFFF0000000000001)
	// Powers of two, where the gap to the neighbour below is half the gap
	// above, and their neighbours.
	for exponent := -1074; exponent <= 1023; exponent += oracleStride(7) {
		bits := math.Float64bits(math.Ldexp(1, exponent))
		edges = append(edges, bits, bits+1)
		if bits > 1 {
			edges = append(edges, bits-1)
		}
	}
	// Powers of ten and their neighbours.
	for exponent := -323; exponent <= 308; exponent += oracleStride(3) {
		value, _ := strconv.ParseFloat("1e"+strconv.Itoa(exponent), 64)
		bits := math.Float64bits(value)
		edges = append(edges, bits, bits+1, bits-1)
	}
	// Subnormals.
	for _, bits := range []uint64{1, 2, 3, 4, 0xF, 0x10, 0xFFFFFFFFFFFFF, 0xFFFFFFFFFFFFE, 0x8000000000000, 0x10000000000000, 0x10000000000001} {
		edges = append(edges, bits)
	}
	return edges
}

func oracleF32Edges() []uint32 {
	var edges []uint32
	add := func(values ...float32) {
		for _, value := range values {
			edges = append(edges, math.Float32bits(value), math.Float32bits(-value))
		}
	}
	add(0, 1, 2, 10, 0.1, 0.2, 0.3, 0.5, 1.5, 2.5, 0.25, 0.125, 1e15, 1e16, 1e17, 9.999999e15, 1.0000001e16,
		1e-4, 1e-5, 9.999999e-5, 0.00010000001, 123456789, math.MaxFloat32, math.SmallestNonzeroFloat32,
		1.17549435e-38, 1.1754942e-38, float32(math.Inf(1)), math.Pi, 1.0/3.0, 16777216, 16777217, 0.95, 9.5, 0.05,
		3.4028235e38, 1e-45, 8.5, 0.15, 0.35, 2.675, 1.005, 4.35)
	edges = append(edges, 0x7FC00000, 0xFFC00000, 0x7F800001, 0x7FFFFFFF)
	for exponent := -149; exponent <= 127; exponent += oracleStride(3) {
		bits := math.Float32bits(float32(math.Ldexp(1, exponent)))
		edges = append(edges, bits, bits+1)
		if bits > 1 {
			edges = append(edges, bits-1)
		}
	}
	for exponent := -45; exponent <= 38; exponent += oracleStride(1) {
		value, _ := strconv.ParseFloat("1e"+strconv.Itoa(exponent), 32)
		bits := math.Float32bits(float32(value))
		edges = append(edges, bits, bits+1, bits-1)
	}
	edges = append(edges, 1, 2, 3, 0x7FFFFF, 0x7FFFFE, 0x400000, 0x800000, 0x800001)
	return edges
}

// oracleDecimalF64 returns a double that is a short decimal: a few digits
// and an exponent, the kind of number programs print.
func (r *oracleRng) decimalF64() float64 {
	digits := 1 + r.intn(17)
	text := ""
	for i := 0; i < digits; i++ {
		text += strconv.Itoa(r.intn(10))
	}
	value, _ := strconv.ParseFloat(text+"e"+strconv.Itoa(r.intn(60)-30-digits), 64)
	if r.intn(4) == 0 {
		value = -value
	}
	return value
}

func (r *oracleRng) floatSpecs(count int) [][]any {
	specs := r.specs(count, oracleFloatSpecTypes, oracleFloatWidths, oracleFloatPrecisions)
	if r.intn(40) == 0 {
		specs = append(specs, r.spec(oracleFloatSpecTypes, []int{oracleNone}, oracleFloatLongPrecisions))
	}
	return specs
}

func TestFmtOracleF64(t *testing.T) {
	oracleCheck(t, "std_fmt_f64", func(yield func(any)) {
		r := &oracleRng{state: 2}
		for _, bits := range oracleF64Edges() {
			specs := [][]any{{"", "", nil, nil, 0}, {"", "?", nil, nil, 0}, {"", "e", nil, nil, 0}}
			yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(bits, 10)}, append(specs, r.floatSpecs(oracleSpecs(5))...)))
		}
		// The longest outputs: every digit of the smallest and the largest
		// values, and the largest precision and width a u16 holds.
		for _, bits := range []uint64{1, 0x000FFFFFFFFFFFFF, 0x0010000000000000, math.Float64bits(math.MaxFloat64), math.Float64bits(-0.1)} {
			yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(bits, 10)}, [][]any{
				{"", "", nil, 1074, 0}, {"", "?", nil, 1100, 1}, {"", "e", nil, 767, 2}, {"", "E", nil, 65535, 3},
				{"+", "", 65535, 65535, 1}, {"0", "e", 65535, nil, 2},
			}))
		}
		for i := 0; i < oracleRandoms(1500); i++ {
			// Any bit pattern: every exponent is as likely as another.
			specs := [][]any{{"", "", nil, nil, 0}, {"", "?", nil, nil, 0}}
			yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(r.next(), 10)}, append(specs, r.floatSpecs(2)...)))
		}
		for i := 0; i < oracleRandoms(900); i++ {
			specs := [][]any{{"", "", nil, nil, 0}, {"", "?", nil, nil, 0}}
			yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(math.Float64bits(r.decimalF64()), 10)},
				append(specs, r.floatSpecs(3)...)))
		}
		for i := 0; i < oracleRandoms(300); i++ {
			// Halfway cases: k + 0.5 scaled by a power of two is exact, and
			// rounds to even.
			value := (float64(r.intn(2000)) + 0.5) * math.Ldexp(1, -r.intn(6))
			var specs [][]any
			for precision := 0; precision <= 6; precision++ {
				specs = append(specs, []any{"", oraclePick(r, []string{"", "?", "e"}), nil, precision, precision % 4})
			}
			yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(math.Float64bits(value), 10)}, specs))
		}
	})
}

func TestFmtOracleF32(t *testing.T) {
	oracleCheck(t, "std_fmt_f32", func(yield func(any)) {
		r := &oracleRng{state: 3}
		for _, bits := range oracleF32Edges() {
			specs := [][]any{{"", "", nil, nil, 0}, {"", "?", nil, nil, 0}, {"", "e", nil, nil, 0}}
			yield(oracleFmtRequest([]any{"f32", strconv.FormatUint(uint64(bits), 10)}, append(specs, r.floatSpecs(oracleSpecs(5))...)))
		}
		// The longest outputs, as for f64.
		for _, bits := range []uint32{1, 0x007FFFFF, 0x00800000, math.Float32bits(math.MaxFloat32), math.Float32bits(-0.1)} {
			yield(oracleFmtRequest([]any{"f32", strconv.FormatUint(uint64(bits), 10)}, [][]any{
				{"", "", nil, 149, 0}, {"", "?", nil, 200, 1}, {"", "e", nil, 112, 2}, {"", "E", nil, 65535, 3},
				{"+", "", 65535, 65535, 1}, {"0", "e", 65535, nil, 2},
			}))
		}
		for i := 0; i < oracleRandoms(1500); i++ {
			specs := [][]any{{"", "", nil, nil, 0}, {"", "?", nil, nil, 0}}
			yield(oracleFmtRequest([]any{"f32", strconv.FormatUint(uint64(uint32(r.next())), 10)}, append(specs, r.floatSpecs(2)...)))
		}
		for i := 0; i < oracleRandoms(600); i++ {
			specs := [][]any{{"", "", nil, nil, 0}, {"", "?", nil, nil, 0}}
			value := float32(r.decimalF64())
			yield(oracleFmtRequest([]any{"f32", strconv.FormatUint(uint64(math.Float32bits(value)), 10)},
				append(specs, r.floatSpecs(3)...)))
		}
		for i := 0; i < oracleRandoms(200); i++ {
			value := (float32(r.intn(2000)) + 0.5) * float32(math.Ldexp(1, -r.intn(6)))
			var specs [][]any
			for precision := 0; precision <= 6; precision++ {
				specs = append(specs, []any{"", oraclePick(r, []string{"", "?", "e"}), nil, precision, precision % 4})
			}
			yield(oracleFmtRequest([]any{"f32", strconv.FormatUint(uint64(math.Float32bits(value)), 10)}, specs))
		}
	})
}

// Strings and chars.

// oracleChars are chars of every class `{:?}` tells apart, and some it does
// not: quotes and the backslash, the named escapes, controls, combining
// marks, invisible and format characters, white space, private use,
// unassigned and noncharacters, and wide and astral ones.
var oracleChars = []rune{
	'"', '\'', '\\', '\n', '\r', '\t', 0, 1, 0x07, 0x08, 0x0B, 0x0C, 0x1B, 0x1F, 0x7F, 0x80, 0x85, 0x9F,
	' ', 'a', 'Z', '0', '~', '{', '}', '%', 0xA0, 0xA1, 0xAD, 0xE9, 0xDF, 0x100, 0x2FF,
	0x300, 0x301, 0x308, 0x36F, 0x483, 0x488, 0x5BF, 0x610, 0x64B, 0x900, 0x93C, 0xE31, 0x1AB0, 0x20D0, 0x20DD, 0x302A, 0xFE00, 0xFE0F, 0xFE20,
	0x200B, 0x200C, 0x200D, 0x200E, 0x200F, 0x202A, 0x202E, 0x2060, 0x2061, 0x2064, 0x2066, 0x206F, 0xFEFF, 0x61C, 0x180E, 0x115F, 0x1160, 0x3164, 0xFFA0, 0x34F, 0x17B4,
	0x1680, 0x2000, 0x2003, 0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000,
	0xE000, 0xE001, 0xF8FF, 0xF0000, 0xFFFFD, 0x100000, 0x10FFFD,
	0x378, 0x379, 0x530, 0x38B, 0xFDD0, 0xFDEF, 0xFFFE, 0xFFFF, 0x1FFFE, 0x1FFFF, 0x2FFFE, 0x3FFFD, 0x3FFFE, 0x3FFFF, 0x40000, 0xE0000, 0xEFFFF, 0x10FFFE, 0x10FFFF, 0xD7FF,
	0xE0001, 0xE0020, 0xE007F, 0xE0100, 0xE01EF, 0xE01F0, 0xE0080, 0xE0FFF, 0xE1000,
	0xFF9E, 0xFF9F, 0xFF9D, 0xFFA1, 0x3042, 0x4E2D, 0xAC00, 0x1F600, 0x1F1E6, 0x1F3FB, 0x1D165, 0x1D173, 0x1BCA0, 0x13430, 0x110BD, 0x16FE4,
	0x2603, 0x2764, 0x00B5, 0x03BC, 0x20AC, 0xFFFD, 0xFFFC, 0xFFF9, 0xFFFB, 0x1D400, 0x2028,
}

// string returns a string of up to maxLen chars, mostly ordinary letters
// with the chars above mixed in.
func (r *oracleRng) string(maxLen int) string {
	length := r.intn(maxLen + 1)
	var b strings.Builder
	for i := 0; i < length; i++ {
		switch r.intn(4) {
		case 0:
			b.WriteRune(oraclePick(r, oracleChars))
		case 1:
			b.WriteRune(oraclePick(r, []rune("héllo wörld ✓ 日本語 ☃")))
		default:
			b.WriteByte(byte('a' + r.intn(26)))
		}
	}
	return b.String()
}

var (
	oracleStrSpecTypes  = []string{"", "?", "x?"}
	oracleStrWidths     = []int{oracleNone, oracleNone, 0, 1, 3, 8, 20}
	oracleStrPrecisions = []int{oracleNone, oracleNone, 0, 1, 2, 3, 5, 10}
)

func TestFmtOracleStrings(t *testing.T) {
	oracleCheck(t, "std_fmt_strings", func(yield func(any)) {
		r := &oracleRng{state: 4}
		// Which chars are escaped, and which are white space: every char.
		yield(map[string]any{"op": "escaped"})
		yield(map[string]any{"op": "whitespace"})
		for _, c := range oracleChars {
			yield(oracleFmtRequest([]any{"char", int(c)}, r.everySpec(3, oracleStrSpecTypes, oracleStrWidths, oracleStrPrecisions)))
			// The char alone, first, last and in the middle of a string: a
			// combining mark is escaped wherever it stands.
			for _, text := range []string{string(c), string(c) + "a", "a" + string(c), "a" + string(c) + "b", string(c) + string(c)} {
				yield(oracleFmtRequest([]any{"str", text}, [][]any{{"", "?", nil, nil, 0}, {"", "", nil, nil, 0}}))
			}
		}
		for _, text := range []string{"", "a", "hello", "héllo", "日本語", "a\tb", "tab\there", `back\slash`, `"quoted"`, "it's", "line\nbreak", "\r\n",
			"e\u0301", "a\u0300\u0301", "\u0301", "👨\u200d👩\u200d👧", "🇫🇷", "✓ done", "nul\x00byte", "bell\x07", "esc\x1b[0m", "del\x7f", strings.Repeat("ab", 40), strings.Repeat("é", 33)} {
			yield(oracleFmtRequest([]any{"str", text}, r.everySpec(8, oracleStrSpecTypes, oracleStrWidths, oracleStrPrecisions)))
		}
		for i := 0; i < oracleRandoms(500); i++ {
			yield(oracleFmtRequest([]any{"str", r.string(24)}, r.everySpec(4, oracleStrSpecTypes, oracleStrWidths, oracleStrPrecisions)))
		}
		for i := 0; i < oracleRandoms(200); i++ {
			// Any assigned or unassigned scalar value.
			c := rune(r.intn(0x110000))
			if c >= 0xD800 && c <= 0xDFFF {
				c = 0xFFFD
			}
			yield(oracleFmtRequest([]any{"char", int(c)}, r.everySpec(2, oracleStrSpecTypes, oracleStrWidths, oracleStrPrecisions)))
		}
		for _, b := range []bool{true, false} {
			yield(oracleFmtRequest([]any{"bool", b}, r.everySpec(10, oracleStrSpecTypes, oracleStrWidths, oracleStrPrecisions)))
		}
	})
}

// Durations.

var (
	oracleDurWidths     = []int{oracleNone, oracleNone, 0, 3, 8, 15, 30}
	oracleDurPrecisions = []int{oracleNone, oracleNone, oracleNone, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 20}
)

func oracleDurValue(nanos uint64) []any {
	return []any{"dur", strconv.FormatUint(nanos/1_000_000_000, 10), nanos % 1_000_000_000}
}

func TestFmtOracleDurations(t *testing.T) {
	oracleCheck(t, "std_fmt_durations", func(yield func(any)) {
		r := &oracleRng{state: 5}
		types := []string{"?"}
		edges := []uint64{0, 1, 9, 10, 99, 100, 999, 1000, 1001, 1500, 1999, 999_999, 1_000_000, 1_000_001, 1_500_000, 2_500_000,
			999_999_999, 1_000_000_000, 1_000_000_001, 1_500_000_000, 2_500_000_000, 1_999_999_999, 59_999_500_000, 59_999_499_999,
			1_050_000_000, 1_150_000_000, 1_250_000_000, 1_005_000_000, 1_015_000_000, 9_999_999_999, 9_500_000_000, 999_500, 999_499,
			1_000_500, 1_500, 2_500, 3_500, 999_500_000, 999_999_500, 123_456_789, 1_234_567_890, 12_345_678_901_234_567,
			9_223_372_036_854_775_807, 9_223_372_036_000_000_000, 86_400_000_000_000, 3_600_000_000_000, 100_000_000, 10_000_000,
			5_000_000_000, 500_000_000, 50_000_000, 5_000_000, 500_000, 50_000, 5_000, 500, 50, 5, 1_000_000_005, 1_000_000_050, 4_500_000_000, 5_500_000_000}
		for _, nanos := range edges {
			var specs [][]any
			for precision := 0; precision <= 10; precision++ {
				specs = append(specs, []any{"", "?", nil, precision, precision % 4})
			}
			specs = append(specs, r.everySpec(6, types, oracleDurWidths, oracleDurPrecisions)...)
			yield(oracleFmtRequest(oracleDurValue(nanos), specs))
		}
		for i := 0; i < oracleRandoms(400); i++ {
			// A random number of significant bits, then often a round
			// number: trailing zeros decide the digits printed.
			nanos := r.next() >> uint(1+r.intn(63))
			if cut := r.intn(4); cut > 0 {
				scale := uint64(math.Pow10(cut * 3))
				nanos = nanos / scale * scale
			}
			yield(oracleFmtRequest(oracleDurValue(nanos), r.everySpec(8, types, oracleDurWidths, oracleDurPrecisions)))
		}
	})
}

// Containers and derived Debug.

// oracleLeaf returns a value without parts.
func (r *oracleRng) leaf() any {
	switch r.intn(14) {
	case 0:
		return []any{"i32", strconv.Itoa(r.intn(2001) - 1000)}
	case 1:
		return []any{"u8", strconv.Itoa(r.intn(256))}
	case 2:
		return []any{"i64", strconv.FormatInt(int64(r.next()), 10)}
	case 3:
		return []any{"str", r.string(8)}
	case 4:
		return []any{"char", int(oraclePick(r, oracleChars))}
	case 5:
		return []any{"bool", r.intn(2) == 0}
	case 6:
		return []any{"f64", strconv.FormatUint(math.Float64bits(r.decimalF64()), 10)}
	case 7:
		return []any{"unit"}
	case 8:
		return oracleDurValue(r.next() >> uint(20+r.intn(40)))
	case 9:
		return []any{"none"}
	case 10:
		return []any{"Kind", r.intn(3)}
	case 11:
		return []any{"UnitStruct"}
	case 12:
		return []any{"SessionId", r.string(6)}
	default:
		return []any{"f32", strconv.FormatUint(uint64(math.Float32bits(float32(r.decimalF64()))), 10)}
	}
}

func (r *oracleRng) values(depth, maxCount int) []any {
	values := []any{}
	for i := r.intn(maxCount + 1); i > 0; i-- {
		values = append(values, r.value(depth))
	}
	return values
}

func (r *oracleRng) strings(maxCount int) []string {
	items := []string{}
	for i := r.intn(maxCount + 1); i > 0; i-- {
		items = append(items, r.string(6))
	}
	return items
}

func (r *oracleRng) stateValue(depth int) []any {
	switch r.intn(6) {
	case 0:
		return []any{"Idle"}
	case 1:
		nanos := r.next() >> uint(20+r.intn(40))
		return []any{"Running", r.intn(100000), strconv.FormatUint(nanos/1_000_000_000, 10), nanos % 1_000_000_000}
	case 2:
		return []any{"Failed", r.string(10), r.intn(200) - 100}
	case 3:
		if depth <= 0 {
			return []any{"Idle"}
		}
		return []any{"Wrapped", r.stateValue(depth - 1)}
	case 4:
		if r.intn(2) == 0 {
			return []any{"Maybe", nil}
		}
		return []any{"Maybe", r.intn(256)}
	default:
		return []any{"Payload", r.value(depth - 1)}
	}
}

func (r *oracleRng) record(depth int) map[string]any {
	scores := [][]any{}
	for i := r.intn(4); i > 0; i-- {
		scores = append(scores, []any{r.string(4), strconv.FormatUint(math.Float64bits(r.decimalF64()), 10)})
	}
	raw := []int{}
	for i := r.intn(5); i > 0; i-- {
		raw = append(raw, r.intn(256))
	}
	nanos := r.next() >> uint(20+r.intn(40))
	record := map[string]any{
		"id":           strconv.FormatUint(r.next()>>uint(r.intn(64)), 10),
		"name":         r.string(10),
		"tags":         r.strings(3),
		"parent":       nil,
		"scores":       scores,
		"kind":         r.intn(3),
		"state":        r.stateValue(depth - 1),
		"raw":          raw,
		"elapsed":      []any{strconv.FormatUint(nanos/1_000_000_000, 10), nanos % 1_000_000_000},
		"pair":         []any{r.intn(200) - 100, r.string(5)},
		"ch":           int(oraclePick(r, oracleChars)),
		"flag":         r.intn(2) == 0,
		"ratio":        strconv.FormatUint(uint64(math.Float32bits(float32(r.decimalF64()))), 10),
		"session":      r.string(8),
		"private_note": nil,
		"any":          r.value(depth - 1),
	}
	if depth > 0 && r.intn(3) == 0 {
		record["parent"] = r.record(depth - 1)
	}
	if r.intn(2) == 0 {
		record["private_note"] = r.string(8)
	}
	return record
}

// value returns a value nested at most depth levels deep.
func (r *oracleRng) value(depth int) any {
	if depth <= 0 {
		return r.leaf()
	}
	switch r.intn(30) {
	case 0, 1:
		return []any{"some", r.value(depth - 1)}
	case 2:
		return []any{"ok", r.value(depth - 1)}
	case 3:
		return []any{"err", r.intn(1000), r.string(8)}
	case 4, 5:
		return []any{"vec", r.values(depth-1, 4)}
	case 6:
		return []any{"deque", r.values(depth-1, 3)}
	case 7:
		bytes := []int{}
		for i := r.intn(6); i > 0; i-- {
			bytes = append(bytes, r.intn(256))
		}
		return []any{"bytes", bytes}
	case 8:
		numbers := []int{}
		for i := r.intn(5); i > 0; i-- {
			numbers = append(numbers, r.intn(2001)-1000)
		}
		return []any{"vec_i32", numbers}
	case 9:
		return []any{"vec_str", r.strings(4)}
	case 10:
		return []any{"arr3", []int{r.intn(100), -r.intn(100), r.intn(100000)}}
	case 11:
		return []any{"t2", r.value(depth - 1), r.value(depth - 1)}
	case 12:
		return []any{"t3", r.value(depth - 1), r.value(depth - 1), r.value(depth - 1)}
	case 13:
		return []any{"t4", r.leaf(), r.leaf(), r.value(depth - 1), r.leaf()}
	case 14:
		return []any{"t5", r.leaf(), r.leaf(), r.leaf(), r.value(depth - 1), r.leaf()}
	case 15, 16:
		entries := [][]any{}
		for i := r.intn(4); i > 0; i-- {
			entries = append(entries, []any{r.string(5), r.value(depth - 1)})
		}
		return []any{"map_str", entries}
	case 17:
		entries := [][]any{}
		for i := r.intn(4); i > 0; i-- {
			entries = append(entries, []any{strconv.Itoa(r.intn(41) - 20), r.value(depth - 1)})
		}
		return []any{"map_i64", entries}
	case 18:
		entries := [][]any{}
		for i := r.intn(4); i > 0; i-- {
			entries = append(entries, []any{r.string(4), strconv.FormatUint(math.Float64bits(r.decimalF64()), 10)})
		}
		return []any{"map_str_f64", entries}
	case 19:
		return []any{"set_str", r.strings(5)}
	case 20:
		numbers := []string{}
		for i := r.intn(6); i > 0; i-- {
			numbers = append(numbers, strconv.Itoa(r.intn(2001)-1000))
		}
		return []any{"set_i64", numbers}
	case 21:
		return []any{"box", r.value(depth - 1)}
	case 22:
		return []any{"Point", r.intn(200) - 100, r.intn(200) - 100}
	case 23:
		return []any{"Pair", r.value(depth - 1), r.value(depth - 1)}
	case 24:
		return []any{"Wrap", r.value(depth - 1)}
	case 25:
		switch r.intn(4) {
		case 0:
			return []any{"Meters", strconv.FormatUint(math.Float64bits(r.decimalF64()), 10)}
		case 1:
			return []any{"Count", r.intn(100000)}
		case 2:
			return []any{"Tags", r.strings(3)}
		default:
			return []any{"Empty"}
		}
	case 26, 27:
		return []any{"State", r.stateValue(depth)}
	case 28:
		return []any{"Record", r.record(depth)}
	default:
		return r.leaf()
	}
}

var (
	oracleDebugSpecTypes  = []string{"?", "?", "?", "x?", "X?"}
	oracleDebugWidths     = []int{oracleNone, oracleNone, oracleNone, oracleNone, 0, 4, 9}
	oracleDebugPrecisions = []int{oracleNone, oracleNone, oracleNone, oracleNone, oracleNone, 0, 2, 5}
)

func TestFmtOracleDebug(t *testing.T) {
	oracleCheck(t, "std_fmt_debug", func(yield func(any)) {
		r := &oracleRng{state: 6}
		plainAndPretty := func(value any, extra int) {
			specs := [][]any{{"", "?", nil, nil, 0}, {"#", "?", nil, nil, 0}}
			yield(oracleFmtRequest(value, append(specs, r.specs(extra, oracleDebugSpecTypes, oracleDebugWidths, oracleDebugPrecisions)...)))
		}
		// Every shape once, empty and not.
		for _, value := range []any{
			[]any{"unit"}, []any{"none"}, []any{"some", []any{"unit"}}, []any{"some", []any{"none"}}, []any{"some", []any{"some", []any{"i32", "-3"}}},
			[]any{"ok", []any{"str", "fine"}}, []any{"err", 3, "bad \"input\""},
			[]any{"vec", []any{}}, []any{"vec", []any{[]any{"vec", []any{}}}}, []any{"deque", []any{}}, []any{"bytes", []int{}}, []any{"bytes", []int{0, 127, 255}},
			[]any{"vec_str", []string{}}, []any{"arr3", []int{1, 2, 3}},
			[]any{"t2", []any{"i32", "1"}, []any{"str", "x"}}, []any{"t2", []any{"unit"}, []any{"t2", []any{"unit"}, []any{"unit"}}},
			[]any{"map_str", [][]any{}}, []any{"map_i64", [][]any{}}, []any{"set_str", []string{}}, []any{"set_i64", []string{"3", "-1", "2"}},
			[]any{"map_str", [][]any{{"b", []any{"i32", "2"}}, {"a", []any{"i32", "1"}}, {"é", []any{"none"}}, {"B", []any{"vec", []any{}}}}},
			[]any{"map_i64", [][]any{{"10", []any{"str", "ten"}}, {"-5", []any{"str", "minus"}}, {"0", []any{"unit"}}}},
			[]any{"box", []any{"box", []any{"str", "boxed"}}},
			[]any{"UnitStruct"}, []any{"Empty"}, []any{"Point", 1, -2}, []any{"Pair", []any{"unit"}, []any{"Point", 0, 0}},
			[]any{"Wrap", []any{"Wrap", []any{"UnitStruct"}}}, []any{"SessionId", "abc"}, []any{"Meters", strconv.FormatUint(math.Float64bits(1.5), 10)},
			[]any{"Count", 7}, []any{"Tags", []string{"a", "b"}}, []any{"Tags", []string{}}, []any{"Kind", 0}, []any{"Kind", 2},
			[]any{"State", []any{"Idle"}}, []any{"State", []any{"Running", 42, "1", 500000000}}, []any{"State", []any{"Failed", "boom", -1}},
			[]any{"State", []any{"Wrapped", []any{"Wrapped", []any{"Idle"}}}}, []any{"State", []any{"Maybe", nil}}, []any{"State", []any{"Maybe", 9}},
			[]any{"State", []any{"Payload", []any{"vec", []any{[]any{"State", []any{"Idle"}}}}}},
		} {
			plainAndPretty(value, 6)
		}
		for i := 0; i < oracleRandoms(60); i++ {
			plainAndPretty([]any{"Record", r.record(2)}, 2)
		}
		for i := 0; i < oracleRandoms(500); i++ {
			plainAndPretty(r.value(1+r.intn(3)), 3)
		}
		// Display through a pointer.
		for i := 0; i < oracleRandoms(40); i++ {
			leaf := oraclePick(r, []any{[]any{"i32", "-12"}, []any{"str", "héllo"}, []any{"f64", strconv.FormatUint(math.Float64bits(2.5), 10)}, []any{"char", int('x')}, []any{"bool", true}})
			yield(oracleFmtRequest([]any{"box", leaf}, r.everySpec(4, []string{"", "?"}, oracleStrWidths, oracleStrPrecisions)))
		}
	})
}

// Whole format strings.

// oraclePool holds the arguments of the format strings in `template` of
// go/port/oracle/src/std_fmt.rs, under the names they have there.
var oraclePool = map[string]any{
	"a":     int32(13),
	"b":     uint8(14),
	"neg":   int64(-7),
	"s":     "héllo",
	"x":     float64(1.5),
	"y":     float32(-0.25),
	"w":     7,
	"p":     2,
	"c":     rs.Char('z'),
	"name":  "World",
	"list":  []int32{1, 2, 3},
	"opt":   rs.Some("x"),
	"shown": oracleShown("shown"),
}

// oracleShown prints the flags it is formatted with.
type oracleShown string

func (s oracleShown) FmtDisplay(f *rs.Formatter) error {
	return f.WriteFmt(
		"<{} fill={:?} align={:?} width={:?} precision={:?} plus={} minus={} alt={} zero={}>",
		string(s),
		rs.Char(f.Fill()),
		f.Align(),
		f.Width(),
		f.Precision(),
		f.SignPlus(),
		f.SignMinus(),
		f.Alternate(),
		f.SignAwareZeroPad(),
	)
}

func (s oracleShown) FmtDebug(f *rs.Formatter) error {
	width := f.Width()
	return f.DebugStruct("Shown").Field("name", string(s)).Field("width", width).FinishNonExhaustive()
}

// oracleTemplateArgs builds the arguments of a `tmpl` request: args is the
// argument list as the Rust source has it (`a, n = b`), and capture names
// the variables the format string takes from the scope, which Go passes by
// name.
func oracleTemplateArgs(args string, capture []string) []any {
	var out []any
	if args != "" {
		for _, arg := range strings.Split(args, ", ") {
			if name, value, named := strings.Cut(arg, " = "); named {
				out = append(out, rs.Arg(name, oraclePool[value]))
			} else {
				out = append(out, oraclePool[arg])
			}
		}
	}
	for _, name := range capture {
		out = append(out, rs.Arg(name, oraclePool[name]))
	}
	return out
}

// oracleTemplates lists the format strings of `template` in std_fmt.rs:
// the string, its arguments there, and what it captures.
var oracleTemplates = [][3]string{
	{"", "", ""},
	{"plain text", "", ""},
	{"{{}}", "", ""},
	{"{{{}}}", "a", ""},
	{"}}{{", "", ""},
	{"a{{b}}c{}d", "a", ""},
	{"{}", "a", ""},
	{"{} {}", "a, b", ""},
	{"{0} {0}", "a", ""},
	{"{1} {0}", "a, b", ""},
	{"{} {0} {}", "a, b", ""},
	{"{0} {} {1} {}", "a, b", ""},
	{"{name}", "", "name"},
	{"{name} {}", "a", "name"},
	{"{} {name} {}", "a, b", "name"},
	{"{name}{name}", "", "name"},
	{"{n}", "n = a", ""},
	{"{} {n} {0}", "b, n = a", ""},
	{"{0} {1}", "b, n = a", ""},
	{"{} {}", "b, n = a", ""},
	{"{ }", "a", ""},
	{"{0 }", "a", ""},
	{"{name }", "", "name"},
	{"{:x }", "a", ""},
	{"{0:x }", "a", ""},
	{"{:5}|", "a", ""},
	{"{:<5}|", "a", ""},
	{"{:^5}|", "a", ""},
	{"{:>5}|", "a", ""},
	{"{:*<5}|", "a", ""},
	{"{:}<5}|", "a", ""},
	{"{:{<5}|", "a", ""},
	{"{:>>5}|", "a", ""},
	{"{:<<5}|", "a", ""},
	{"{:^^5}|", "a", ""},
	{"{:é^6}|", "a", ""},
	{"{:🎉>4}|", "a", ""},
	{"{:0<5}|", "a", ""},
	{"{:+}", "a", ""},
	{"{:+}", "neg", ""},
	{"{:-}", "a", ""},
	{"{:#x}", "a", ""},
	{"{:#X}", "a", ""},
	{"{:#o}", "a", ""},
	{"{:#b}", "a", ""},
	{"{:05}", "neg", ""},
	{"{:+05}", "a", ""},
	{"{:#06x}", "a", ""},
	{"{:<#06x}|", "a", ""},
	{"{:0}", "a", ""},
	{"{:00}", "a", ""},
	{"{:01}", "a", ""},
	{"{:010}", "a", ""},
	{"{:w$}|", "a", "w"},
	{"{:1$}|", "a, w", ""},
	{"{:0$}|", "w", ""},
	{"{0:1$}|", "a, w", ""},
	{"{:w$.p$}|", "x", "w,p"},
	{"{:1$.2$}|", "x, w, p", ""},
	{"{:.*}|", "p, x", ""},
	{"{:.*} {}", "p, x, a", ""},
	{"{} {:.*} {}", "a, p, x, b", ""},
	{"{1:.*}", "p, x", ""},
	{"{0:.*}", "w", ""},
	{"{:.*} {:.*}", "p, x, w, x", ""},
	{"{:w$.*}|", "p, x", "w"},
	{"{name:.*}|", "p", "name"},
	{"{:.p$}|", "s", "p"},
	{"{:.0}|", "s", ""},
	{"{:.3}|", "s", ""},
	{"{:8.3}|", "s", ""},
	{"{:>8.3}|", "s", ""},
	{"{:^w$.p$}|", "s", "w,p"},
	{"{:?}", "s", ""},
	{"{:#?}", "list", ""},
	{"{:x?}", "list", ""},
	{"{:X?}", "list", ""},
	{"{:#x?}", "list", ""},
	{"{:02x?}", "list", ""},
	{"{:#04X?}", "list", ""},
	{"{:?} {:?}", "opt, c", ""},
	{"{:e}", "x", ""},
	{"{:E}", "x", ""},
	{"{:e}", "a", ""},
	{"{:.1e}", "y", ""},
	{"{:10.3e}|", "x", ""},
	{"{:b}", "b", ""},
	{"{:o}", "b", ""},
	{"{}{}{}{}", "a, b, c, s", ""},
	{"{3}{2}{1}{0}", "a, b, c, s", ""},
	{"{s} {a:>w$} {x:.p$}", "", "s,a,w,x,p"},
	{"{a}{b}{c}", "", "a,b,c"},
	{"{:>1$} {:>1$}", "a, w", ""},
	{"{0:>2$} {1:>2$}", "a, b, w", ""},
	{"{:width$}|", "a, width = w", ""},
	{"{:.prec$}|", "x, prec = p", ""},
	{"{:width$.prec$}|", "x, width = w, prec = p", ""},
	{"{x:w$.p$}|{y:w$.p$}|", "", "x,w,p,y"},
	{"{_x}", "_x = a", ""},
	{"{x1} {x_2}", "x1 = a, x_2 = b", ""},
	{"{é}", "é = a", ""},
	{"{shown}", "", "shown"},
	{"{shown:>10.3}", "", "shown"},
	{"{shown:*^+#012.4}", "", "shown"},
	{"{shown:<-w$.p$}", "", "shown,w,p"},
	{"{shown:?}", "", "shown"},
	{"{shown:9?}", "", "shown"},
	{"{shown:#?}", "", "shown"},
	{"{shown:.*}", "p", "shown"},
	{"{shown:0$}", "w", "shown"},
	{"tab\there\nnewline {} \"quoted\" \\ end", "a", ""},
	{"unicode ✓ {} ☃", "s", ""},
}

func TestFmtOracleTemplates(t *testing.T) {
	oracleCheck(t, "std_fmt_templates", func(yield func(any)) {
		for _, template := range oracleTemplates {
			request := map[string]any{"op": "tmpl", "f": template[0], "args": template[1]}
			if template[2] != "" {
				request["capture"] = strings.Split(template[2], ",")
			}
			yield(request)
		}
		// A width or a precision taken from an argument must fit in a u16;
		// Rust panics when it does not.
		yield(oracleFmtRequest([]any{"i32", "1"}, [][]any{{"", "", 65535, nil, 1}}))
		yield(oracleFmtRequest([]any{"i32", "1"}, [][]any{{"", "", 65536, nil, 1}}))
		yield(oracleFmtRequest([]any{"str", "abc"}, [][]any{{"", "", nil, 65536, 2}}))
		yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(math.Float64bits(1.5), 10)}, [][]any{{"", "", nil, 70000, 3}}))
		yield(oracleFmtRequest([]any{"f64", strconv.FormatUint(math.Float64bits(1.5), 10)}, [][]any{{"", "e", 65535, 65535, 1}}))
	})
}
