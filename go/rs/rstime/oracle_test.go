package rstime

import (
	"encoding/json"
	"math"
	"math/rand/v2"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests ask Rust (go/port/oracle/src/std_time.rs) what it
// does with the same inputs. Where a Rust result can be past the end of the
// port's range, the oracle replies in the form the port must produce; see the
// module's documentation.

const oracleModule = "std_time"

func decodeRequest[T any](t *testing.T, request json.RawMessage) T {
	t.Helper()
	var decoded T
	if err := json.Unmarshal(request, &decoded); err != nil {
		t.Fatalf("request does not decode: %v\n%s", err, request)
	}
	return decoded
}

// seeded returns a generator with a fixed seed, so a corpus is reproducible.
func seeded(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x9E3779B97F4A7C15))
}

// edgeSecs and edgeNanos are the values Duration's Debug algorithm branches
// on: unit boundaries, rounding ties, runs of nines, and the ends of the
// port's and of Rust's ranges.
var edgeSecs = []uint64{
	0, 1, 2, 7, 9, 10, 59, 99, 999, 1000, 123456789, maxSecs, maxSecs + 1,
	math.MaxUint32, math.MaxUint64 - 1, math.MaxUint64,
}

var edgeNanos = []uint32{
	0, 1, 5, 9, 10, 49, 50, 99, 100, 499, 500, 501, 999, 1000, 1499, 1500, 1501, 2500, 9999,
	10000, 99999, 100000, 499999, 500000, 500001, 999999, 1000000, 1499999, 1500000, 2500000,
	9999999, 49999999, 50000000, 99999999, 100000000, 104990000, 105000000, 123456789, 449999999,
	450000000, 499999999, 500000000, 500000001, 550000000, 949999999, 950000000, 989999999,
	995000000, 999499999, 999500000, 999949999, 999950000, 999999499, 999999500, 999999994,
	999999995, 999999999,
}

var debugFormats = []string{
	"plain", "alt", "prec", "width", "width_prec", "left", "right", "center", "left_prec",
	"right_prec", "center_prec", "fill_left", "fill_right", "fill_center", "plus", "plus_prec",
	"plus_width", "plus_right", "zero", "zero_prec",
}

type debugRequest struct {
	Op    string `json:"op"`
	Secs  uint64 `json:"secs"`
	Nanos uint32 `json:"nanos"`
	Fmt   string `json:"fmt"`
	Width int    `json:"width"`
	Prec  int    `json:"prec"`
}

// debugFormats maps the oracle's `fmt` names to the format strings the
// oracle formats with, width and precision being the named arguments w and
// p.
var debugFormatStrings = map[string]string{
	"plain": "{:?}", "alt": "{:#?}", "prec": "{:.p$?}", "width": "{:w$?}", "width_prec": "{:w$.p$?}",
	"left": "{:<w$?}", "right": "{:>w$?}", "center": "{:^w$?}", "left_prec": "{:<w$.p$?}",
	"right_prec": "{:>w$.p$?}", "center_prec": "{:^w$.p$?}", "fill_left": "{:*<w$?}",
	"fill_right": "{:*>w$.p$?}", "fill_center": "{:é^w$.p$?}", "plus": "{:+?}", "plus_prec": "{:+.p$?}",
	"plus_width": "{:+w$?}", "plus_right": "{:>+w$.p$?}", "zero": "{:0w$?}", "zero_prec": "{:0w$.p$?}",
}

// debugText formats the request's duration as the oracle does, through the
// formatter, which is where Duration's Debug lives.
func debugText(t *testing.T, request debugRequest) string {
	t.Helper()
	format, ok := debugFormatStrings[request.Fmt]
	if !ok {
		t.Fatalf("unknown format %q", request.Fmt)
	}
	args := []any{DurationNew(request.Secs, request.Nanos)}
	if strings.Contains(format, "w$") {
		args = append(args, rs.Arg("w", request.Width))
	}
	if strings.Contains(format, "p$") {
		args = append(args, rs.Arg("p", request.Prec))
	}
	return rs.Format(format, args...)
}

// inGoRange reports whether a time.Duration holds secs seconds and nanos
// nanoseconds. Durations beyond that are the formatter's to test (go/rs).
func inGoRange(secs uint64, nanos uint32) bool {
	return secs < maxSecs || (secs == maxSecs && nanos <= maxSecsNanos)
}

// randomNanos builds nanosecond counts whose digits exercise rounding:
// mostly nines, fives and zeros with the odd other digit.
func randomNanos(random *rand.Rand) uint32 {
	if random.IntN(4) == 0 {
		return uint32(random.IntN(nanosPerSec))
	}
	digits := []byte("000000000")
	for i := range digits {
		switch random.IntN(6) {
		case 0, 1:
			digits[i] = '9'
		case 2:
			digits[i] = '5'
		case 3:
			digits[i] = '0'
		case 4:
			digits[i] = '4'
		default:
			digits[i] = byte('0' + random.IntN(10))
		}
	}
	value, _ := strconv.ParseUint(string(digits), 10, 32)
	return uint32(value)
}

func randomSecs(random *rand.Rand) uint64 {
	switch random.IntN(5) {
	case 0:
		return 0
	case 1:
		return uint64(random.IntN(1000))
	case 2:
		return edgeSecs[random.IntN(len(edgeSecs))]
	case 3:
		// Runs of nines carry on rounding.
		return []uint64{9, 99, 999, 9999, 999999999, 9999999999999999999}[random.IntN(6)]
	default:
		return random.Uint64() >> uint(random.IntN(64))
	}
}

func TestDiffDurationDebug(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_debug", func(yieldAny func(any)) {
		yield := func(request debugRequest) {
			if inGoRange(request.Secs, request.Nanos) {
				yieldAny(request)
			}
		}
		precisions := []int{0, 1, 2, 3, 5, 8, 9, 10, 12}
		widths := []int{0, 1, 5, 9, 12, 20, 21, 22, 24, 30}
		// Every edge value, unformatted and at each precision in turn.
		turn := 0
		for _, secs := range edgeSecs {
			for _, nanos := range edgeNanos {
				yield(debugRequest{Op: "dur_debug", Secs: secs, Nanos: nanos, Fmt: "plain"})
				yield(debugRequest{Op: "dur_debug", Secs: secs, Nanos: nanos, Fmt: "prec", Prec: precisions[turn%len(precisions)]})
				turn++
			}
		}
		// Every format on a spread of the edge values.
		for index, format := range debugFormats {
			for step := 0; step < 40; step++ {
				secs := edgeSecs[(index*7+step*3)%len(edgeSecs)]
				nanos := edgeNanos[(index*11+step*5)%len(edgeNanos)]
				if step%3 == 0 {
					secs = 0
				}
				yield(debugRequest{
					Op: "dur_debug", Secs: secs, Nanos: nanos, Fmt: format,
					Width: widths[(index+step)%len(widths)], Prec: precisions[(index*3+step)%len(precisions)],
				})
			}
		}
		random := seeded(1)
		for range 1500 {
			yield(debugRequest{
				Op: "dur_debug", Secs: randomSecs(random), Nanos: randomNanos(random),
				Fmt:   debugFormats[random.IntN(len(debugFormats))],
				Width: random.IntN(40), Prec: random.IntN(14),
			})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[debugRequest](t, raw)
		return map[string]string{"text": debugText(t, request)}
	})
}

type bitsRequest struct {
	Op   string `json:"op"`
	Bits uint64 `json:"bits"`
}

func floatAnswer(secs uint64, nanos uint32, err error) any {
	if err != nil {
		return map[string]string{"err": err.Error()}
	}
	return map[string]any{"ok": []uint64{secs, uint64(nanos)}, "sat": int64(fromParts(secs, nanos))}
}

// interestingFloats are the values try_from_secs branches on: zeros, the
// smallest nanosecond fractions and their ties, whole seconds, the ends of
// the port's and of Rust's ranges, and the values that are not durations.
func interestingFloats() []float64 {
	values := []float64{
		0, math.Copysign(0, -1), math.NaN(), -math.NaN(), math.Inf(1), math.Inf(-1),
		math.SmallestNonzeroFloat64, -math.SmallestNonzeroFloat64, math.MaxFloat64, -math.MaxFloat64,
		1, 0.5, 0.25, 1.5, 2.5, 1e-9, 0.5e-9, 1.5e-9, 2.5e-9, 3.5e-9, 0.49e-9, 0.51e-9, 1e-10,
		0.999999999, 0.9999999994, 0.9999999995, 0.9999999996, 0.99999999949999, 1.9999999995,
		2.9999999995, 123456789.123456789, 4.0e9, 9223372036.854775807, 9223372036.854775808,
		9.3e9, 1e12, 1e15, 1e18, 1.8446744073709552e19, 1.8446744073709550e19, 3e19, 1e300,
		-1e-300, -1e-9, -0.5e-9, -1, 4503599627370496, 4503599627370497, 9007199254740992,
		9007199254740993, 4503599627370495.5, 2251799813685247.75,
	}
	for exponent := -80; exponent <= 70; exponent++ {
		power := math.Ldexp(1, exponent)
		values = append(values, power, math.Nextafter(power, math.Inf(1)), math.Nextafter(power, 0), 1.5*power)
	}
	return values
}

func TestDiffDurationFromSecsF64(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_from_secs_f64", func(yield func(any)) {
		for _, value := range interestingFloats() {
			yield(bitsRequest{Op: "dur_from_f64", Bits: math.Float64bits(value)})
		}
		random := seeded(2)
		for range 1500 {
			var raw uint64
			switch random.IntN(4) {
			case 0:
				// Any bit pattern at all.
				raw = random.Uint64()
			case 1:
				// An exact multiple of half a nanosecond: a rounding tie.
				raw = math.Float64bits(float64(random.Uint64N(1<<40)) * 0.5e-9)
			default:
				// A positive value across the exponents the conversion
				// distinguishes.
				exponent := uint64(1023 - 36 + random.IntN(104))
				raw = exponent<<52 | random.Uint64()&(1<<52-1)
			}
			yield(bitsRequest{Op: "dur_from_f64", Bits: raw})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[bitsRequest](t, raw)
		return floatAnswer(DurationTryFromSecsF64Parts(math.Float64frombits(request.Bits)))
	})
}

func TestDiffDurationFromSecsF32(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_from_secs_f32", func(yield func(any)) {
		for _, value := range interestingFloats() {
			yield(bitsRequest{Op: "dur_from_f32", Bits: uint64(math.Float32bits(float32(value)))})
		}
		random := seeded(3)
		for range 1200 {
			var raw uint32
			if random.IntN(4) == 0 {
				raw = random.Uint32()
			} else {
				exponent := uint32(127 - 36 + random.IntN(104))
				raw = exponent<<23 | random.Uint32()&(1<<23-1)
			}
			yield(bitsRequest{Op: "dur_from_f32", Bits: uint64(raw)})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[bitsRequest](t, raw)
		return floatAnswer(DurationTryFromSecsF32Parts(math.Float32frombits(uint32(request.Bits))))
	})
}

// okOrPanic runs f and replies as the oracle does: {"ok": value}, or
// {"panic": message} when f panics.
func okOrPanic(f func() any) any {
	return oracle.Catch(func() any { return map[string]any{"ok": f()} })
}

// interestingCounts are counts of nanoseconds (or of seconds, millis and so
// on for the constructors) around every unit and the ends of the ranges.
func interestingCounts() []uint64 {
	values := []uint64{0, 1, 2, 999, 1000, 1001, 999_999, 1_000_000, 1_000_001, 999_999_999, 1_000_000_000, 1_000_000_001}
	for _, base := range []uint64{
		math.MaxInt64 / 1_000_000_000, math.MaxInt64 / 1_000_000, math.MaxInt64 / 1_000, math.MaxInt64,
		math.MaxUint64 / 3600, math.MaxUint64 / 60, math.MaxUint32, math.MaxUint64 - 1,
	} {
		values = append(values, base-1, base, base+1)
	}
	return append(values, math.MaxUint64)
}

type ctorRequest struct {
	Op    string `json:"op"`
	Value uint64 `json:"value"`
	Nanos uint32 `json:"nanos"`
}

func TestDiffDurationConstructors(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_constructors", func(yield func(any)) {
		nanos := []uint32{0, 1, 999_999_999, 1_000_000_000, 1_000_000_001, 2_000_000_000, math.MaxUint32}
		for index, value := range interestingCounts() {
			yield(ctorRequest{Op: "dur_ctor", Value: value, Nanos: nanos[index%len(nanos)]})
			yield(ctorRequest{Op: "dur_ctor", Value: value, Nanos: nanos[(index+3)%len(nanos)]})
		}
		random := seeded(4)
		for range 400 {
			yield(ctorRequest{Op: "dur_ctor", Value: random.Uint64() >> uint(random.IntN(64)), Nanos: random.Uint32() >> uint(random.IntN(32))})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[ctorRequest](t, raw)
		return map[string]any{
			"from_secs":   int64(DurationFromSecs(request.Value)),
			"from_millis": int64(DurationFromMillis(request.Value)),
			"from_micros": int64(DurationFromMicros(request.Value)),
			"from_nanos":  int64(DurationFromNanos(request.Value)),
			"from_mins":   okOrPanic(func() any { return int64(DurationFromMins(request.Value)) }),
			"from_hours":  okOrPanic(func() any { return int64(DurationFromHours(request.Value)) }),
			"new":         okOrPanic(func() any { return int64(DurationNew(request.Value, request.Nanos)) }),
		}
	})
}

type nanosRequest struct {
	Op    string `json:"op"`
	Nanos uint64 `json:"nanos"`
}

// randomGoNanos is a duration inside the port's range, weighted towards the
// interesting sizes.
func randomGoNanos(random *rand.Rand) uint64 {
	switch random.IntN(6) {
	case 0:
		return uint64(random.IntN(2000))
	case 1:
		return random.Uint64N(2_000_000_000)
	case 2:
		return math.MaxInt64 - random.Uint64N(2_000_000_000)
	case 3:
		return math.MaxInt64 / (1 + random.Uint64N(8))
	default:
		return random.Uint64N(math.MaxInt64) >> uint(random.IntN(63))
	}
}

func goRangeCounts() []uint64 {
	var values []uint64
	for _, value := range interestingCounts() {
		if value <= math.MaxInt64 {
			values = append(values, value)
		}
	}
	return values
}

func TestDiffDurationMethods(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_methods", func(yield func(any)) {
		for _, value := range goRangeCounts() {
			yield(nanosRequest{Op: "dur_methods", Nanos: value})
		}
		random := seeded(5)
		for range 800 {
			yield(nanosRequest{Op: "dur_methods", Nanos: randomGoNanos(random)})
		}
	}, func(raw json.RawMessage) any {
		d := time.Duration(decodeRequest[nanosRequest](t, raw).Nanos)
		return map[string]any{
			"as_secs":       DurationAsSecs(d),
			"subsec_millis": DurationSubsecMillis(d),
			"subsec_micros": DurationSubsecMicros(d),
			"subsec_nanos":  DurationSubsecNanos(d),
			"as_millis":     DurationAsMillis(d),
			"as_micros":     DurationAsMicros(d),
			"as_nanos":      DurationAsNanos(d),
			"as_secs_f64":   strconv.FormatUint(math.Float64bits(DurationAsSecsF64(d)), 10),
			"as_secs_f32":   math.Float32bits(DurationAsSecsF32(d)),
			"as_millis_f64": strconv.FormatUint(math.Float64bits(DurationAsMillisF64(d)), 10),
			"as_millis_f32": math.Float32bits(DurationAsMillisF32(d)),
			"is_zero":       DurationIsZero(d),
			"debug":         DurationDebug(d),
		}
	})
}

type binaryRequest struct {
	Op string `json:"op"`
	A  uint64 `json:"a"`
	B  uint64 `json:"b"`
}

// optionAnswer is an Option<Duration> as the oracle replies with it: the
// nanoseconds, or null.
func optionAnswer(option rs.Option[time.Duration]) any {
	if d, ok := option.Get(); ok {
		return int64(d)
	}
	return nil
}

// floatBits32 is an f32 by its bits, with every NaN as Rust's 0.0 / 0.0.
func floatBits32(value float32) uint32 { return math.Float32bits(value) }

func TestDiffDurationBinary(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_binary", func(yield func(any)) {
		counts := goRangeCounts()
		for i, a := range counts {
			for j, b := range counts {
				if (i+j)%3 == 0 || i == j {
					yield(binaryRequest{Op: "dur_binary", A: a, B: b})
				}
			}
		}
		random := seeded(6)
		for range 1000 {
			yield(binaryRequest{Op: "dur_binary", A: randomGoNanos(random), B: randomGoNanos(random)})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[binaryRequest](t, raw)
		a, b := time.Duration(request.A), time.Duration(request.B)
		order := 0
		if a < b {
			order = -1
		} else if a > b {
			order = 1
		}
		return map[string]any{
			"checked_add":      optionAnswer(DurationCheckedAdd(a, b)),
			"checked_sub":      optionAnswer(DurationCheckedSub(a, b)),
			"saturating_add":   int64(DurationSaturatingAdd(a, b)),
			"saturating_sub":   int64(DurationSaturatingSub(a, b)),
			"abs_diff":         int64(DurationAbsDiff(a, b)),
			"add":              okOrPanic(func() any { return int64(DurationAdd(a, b)) }),
			"sub":              okOrPanic(func() any { return int64(DurationSub(a, b)) }),
			"div_duration_f64": strconv.FormatUint(math.Float64bits(DurationDivDurationF64(a, b)), 10),
			"div_duration_f32": floatBits32(DurationDivDurationF32(a, b)),
			"cmp":              order,
		}
	})
}

type scalarRequest struct {
	Op  string `json:"op"`
	A   uint64 `json:"a"`
	Rhs uint32 `json:"rhs"`
}

func TestDiffDurationScalar(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_scalar", func(yield func(any)) {
		scalars := []uint32{0, 1, 2, 3, 7, 10, 100, 1000, 4000, 1_000_000, 1_000_000_000, math.MaxUint32 - 1, math.MaxUint32}
		for _, a := range goRangeCounts() {
			for _, rhs := range scalars {
				yield(scalarRequest{Op: "dur_scalar", A: a, Rhs: rhs})
			}
		}
		random := seeded(7)
		for range 800 {
			yield(scalarRequest{Op: "dur_scalar", A: randomGoNanos(random), Rhs: random.Uint32() >> uint(random.IntN(32))})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[scalarRequest](t, raw)
		a := time.Duration(request.A)
		return map[string]any{
			"checked_mul":    optionAnswer(DurationCheckedMul(a, request.Rhs)),
			"saturating_mul": int64(DurationSaturatingMul(a, request.Rhs)),
			"checked_div":    optionAnswer(DurationCheckedDiv(a, request.Rhs)),
			"mul":            okOrPanic(func() any { return int64(DurationMul(a, request.Rhs)) }),
			"div":            okOrPanic(func() any { return int64(DurationDiv(a, request.Rhs)) }),
		}
	})
}

type floatRequest struct {
	Op     string `json:"op"`
	A      uint64 `json:"a"`
	Bits   uint64 `json:"bits"`
	Bits32 uint32 `json:"bits32"`
}

// floatOperation replies for an operation that goes through
// Duration::from_secs_f64: the exact seconds and nanoseconds Rust arrives at,
// and the duration the port's function returns.
func floatOperation(public func() time.Duration, exact func() (uint64, uint32, error)) any {
	return okOrPanic(func() any {
		d := public()
		secs, nanos, err := exact()
		if err != nil {
			panic("the exact computation failed where the function did not: " + err.Error())
		}
		return map[string]any{"parts": []uint64{secs, uint64(nanos)}, "sat": int64(d)}
	})
}

func TestDiffDurationFloat(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_float", func(yield func(any)) {
		factors := []float64{
			0, math.Copysign(0, -1), 1, 0.5, 2, 1.5, 3, 0.1, 1e-9, 1e9, 1e-18, 1e18, 1e30, -1, -1e-30,
			math.NaN(), math.Inf(1), math.Inf(-1), math.SmallestNonzeroFloat64, math.MaxFloat64, 0.3333333333333333,
		}
		for _, a := range goRangeCounts() {
			for _, factor := range factors {
				yield(floatRequest{Op: "dur_float", A: a, Bits: math.Float64bits(factor), Bits32: math.Float32bits(float32(factor))})
			}
		}
		random := seeded(8)
		for range 800 {
			exponent := uint64(1023 - 40 + random.IntN(80))
			wide := exponent<<52 | random.Uint64()&(1<<52-1)
			exponent32 := uint32(127 - 40 + random.IntN(80))
			narrow := exponent32<<23 | random.Uint32()&(1<<23-1)
			if random.IntN(10) == 0 {
				wide |= 1 << 63
				narrow |= 1 << 31
			}
			yield(floatRequest{Op: "dur_float", A: randomGoNanos(random), Bits: wide, Bits32: narrow})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[floatRequest](t, raw)
		a := time.Duration(request.A)
		secs, nanos := toParts(a)
		wide := math.Float64frombits(request.Bits)
		narrow := math.Float32frombits(request.Bits32)
		return map[string]any{
			"mul_f64": floatOperation(
				func() time.Duration { return DurationMulF64(a, wide) },
				func() (uint64, uint32, error) { return mulF64(secs, nanos, wide) }),
			"div_f64": floatOperation(
				func() time.Duration { return DurationDivF64(a, wide) },
				func() (uint64, uint32, error) { return divF64(secs, nanos, wide) }),
			"mul_f32": floatOperation(
				func() time.Duration { return DurationMulF32(a, narrow) },
				func() (uint64, uint32, error) { return mulF64(secs, nanos, float64(narrow)) }),
			"div_f32": floatOperation(
				func() time.Duration { return DurationDivF32(a, narrow) },
				func() (uint64, uint32, error) { return divF64(secs, nanos, float64(narrow)) }),
			"from_secs_f64": floatOperation(
				func() time.Duration { return DurationFromSecsF64(wide) },
				func() (uint64, uint32, error) { return DurationTryFromSecsF64Parts(wide) }),
			"from_secs_f32": floatOperation(
				func() time.Duration { return DurationFromSecsF32(narrow) },
				func() (uint64, uint32, error) { return DurationTryFromSecsF32Parts(narrow) }),
		}
	})
}

type sumRequest struct {
	Op    string   `json:"op"`
	Items []uint64 `json:"items"`
}

func TestDiffDurationSum(t *testing.T) {
	oracle.Check(t, oracleModule, "duration_sum", func(yield func(any)) {
		yield(sumRequest{Op: "dur_sum", Items: []uint64{}})
		yield(sumRequest{Op: "dur_sum", Items: []uint64{math.MaxInt64}})
		yield(sumRequest{Op: "dur_sum", Items: []uint64{math.MaxInt64, 0}})
		yield(sumRequest{Op: "dur_sum", Items: []uint64{math.MaxInt64, 1}})
		yield(sumRequest{Op: "dur_sum", Items: []uint64{math.MaxInt64 - 1, 1}})
		yield(sumRequest{Op: "dur_sum", Items: []uint64{1_999_999_999, 2_999_999_999, 999_999_999, 999_999_999, 999_999_999, 5_000_000_000}})
		random := seeded(9)
		for range 200 {
			items := make([]uint64, random.IntN(6))
			for i := range items {
				items[i] = randomGoNanos(random) >> uint(random.IntN(3))
			}
			yield(sumRequest{Op: "dur_sum", Items: items})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[sumRequest](t, raw)
		durations := make([]time.Duration, len(request.Items))
		for i, item := range request.Items {
			durations[i] = time.Duration(item)
		}
		return oracle.Catch(func() any { return map[string]any{"ok": int64(DurationSum(durations))} })
	})
}

type systemTimeRequest struct {
	Op     string `json:"op"`
	ASecs  int64  `json:"a_secs"`
	ANanos uint32 `json:"a_nanos"`
	BSecs  int64  `json:"b_secs"`
	BNanos uint32 `json:"b_nanos"`
	D      uint64 `json:"d"`
}

// timeParts is a SystemTime as the oracle replies with it: seconds (negative
// before the epoch) and nanoseconds past them.
func timeParts(option rs.Option[time.Time]) any {
	t, ok := option.Get()
	if !ok {
		return nil
	}
	return []int64{t.Unix(), int64(t.Nanosecond())}
}

func TestDiffSystemTime(t *testing.T) {
	if rs.CfgWindows {
		t.Skip("the oracle's SystemTime is the Unix one; a Windows SystemTime counts 100ns intervals")
	}
	oracle.Check(t, oracleModule, "system_time", func(yield func(any)) {
		secs := []int64{0, 1, -1, 2, -2, 59, 1_000_000, -1_000_000, 1_700_000_000, 1_800_000_000, -62_135_596_800, 253_402_300_799, 4_000_000_000, -4_000_000_000}
		nanos := []uint32{0, 1, 500_000_000, 999_999_999, 250_000_000, 100, 99}
		durations := []uint64{0, 1, 999_999_999, 1_000_000_000, 1_000_000_001, 1_500_000_000, 86_400_000_000_000, 31_536_000_000_000_000, math.MaxInt64 - 1}
		turn := 0
		for _, aSecs := range secs {
			for _, bSecs := range secs {
				yield(systemTimeRequest{
					Op: "systime", ASecs: aSecs, ANanos: nanos[turn%len(nanos)], BSecs: bSecs,
					BNanos: nanos[(turn/len(nanos)+turn)%len(nanos)], D: durations[turn%len(durations)],
				})
				turn++
			}
		}
		random := seeded(10)
		for range 800 {
			yield(systemTimeRequest{
				Op:    "systime",
				ASecs: random.Int64N(8_000_000_000) - 4_000_000_000, ANanos: uint32(random.IntN(nanosPerSec)),
				BSecs: random.Int64N(8_000_000_000) - 4_000_000_000, BNanos: uint32(random.IntN(nanosPerSec)),
				D: min(randomGoNanos(random), math.MaxInt64-1),
			})
		}
	}, func(raw json.RawMessage) any {
		request := decodeRequest[systemTimeRequest](t, raw)
		a := time.Unix(request.ASecs, int64(request.ANanos)).UTC()
		b := time.Unix(request.BSecs, int64(request.BNanos)).UTC()
		d := time.Duration(request.D)
		var since any
		if duration, err := SystemTimeDurationSince(a, b); err == nil {
			since = map[string]any{"ok": int64(duration)}
		} else {
			since = map[string]any{"err": int64(err.(*SystemTimeError).Duration()), "display": err.Error()}
		}
		return map[string]any{
			"duration_since": since,
			"checked_add":    timeParts(SystemTimeCheckedAdd(a, d)),
			"checked_sub":    timeParts(SystemTimeCheckedSub(a, d)),
			"cmp":            SystemTimeCmp(a, b),
			"debug":          SystemTimeDebug(a),
		}
	})
}

type textRequest struct {
	Op  string `json:"op"`
	Key string `json:"key"`
}

// timeTexts are the fixed texts of std::time that do not go through the
// formatter: the Display of its errors and its panic messages.
var timeTexts = map[string]func() string{
	"systemtime_error_display": func() string {
		_, err := SystemTimeDurationSince(UnixEpoch, SystemTimeAdd(UnixEpoch, DurationFromMillis(1500)))
		return err.Error()
	},
	"try_from_float_negative_display": func() string {
		_, err := DurationTryFromSecsF64(-1.0)
		return err.Error()
	},
	"try_from_float_overflow_display": func() string {
		_, err := DurationTryFromSecsF64(math.NaN())
		return err.Error()
	},
	"instant_add_overflow_panic":            func() string { InstantNow().Add(DurationMax); return "" },
	"instant_sub_overflow_panic":            func() string { InstantNow().Sub(DurationMax); return "" },
	"systemtime_add_overflow_panic":         func() string { SystemTimeAdd(SystemTimeNow(), DurationMax); return "" },
	"systemtime_sub_overflow_panic":         func() string { SystemTimeSub(SystemTimeNow(), DurationMax); return "" },
	"duration_add_overflow_panic":           func() string { DurationAdd(DurationMax, DurationFromNanos(1)); return "" },
	"duration_sub_overflow_panic":           func() string { DurationSub(DurationZero, DurationFromNanos(1)); return "" },
	"duration_mul_overflow_panic":           func() string { DurationMul(DurationMax, 2); return "" },
	"duration_div_zero_panic":               func() string { DurationDiv(DurationFromSecs(1), 0); return "" },
	"duration_new_overflow_panic":           func() string { DurationNew(math.MaxUint64, 1_000_000_000); return "" },
	"duration_from_mins_overflow_panic":     func() string { DurationFromMins(math.MaxUint64); return "" },
	"duration_from_hours_overflow_panic":    func() string { DurationFromHours(math.MaxUint64); return "" },
	"duration_sum_overflow_panic":           func() string { DurationSum([]time.Duration{DurationMax, DurationMax}); return "" },
	"duration_from_secs_f64_negative_panic": func() string { DurationFromSecsF64(-1.0); return "" },
	"duration_from_secs_f64_nan_panic":      func() string { DurationFromSecsF64(math.NaN()); return "" },
	"unix_epoch_debug":                      func() string { return SystemTimeDebug(UnixEpoch) },
}

// textAnswer replies with the text a key's function returns or panics with.
func textAnswer(texts map[string]func() string, key string) any {
	produce, ok := texts[key]
	if !ok {
		return map[string]string{"unknown key": key}
	}
	reply := oracle.Catch(func() any { return map[string]string{"text": produce()} })
	if panicked, ok := reply.(map[string]string); ok {
		if message, ok := panicked["panic"]; ok {
			return map[string]string{"text": message}
		}
	}
	return reply
}

func sortedKeys(texts map[string]func() string) []string {
	keys := make([]string, 0, len(texts))
	for key := range texts {
		keys = append(keys, key)
	}
	for i := 1; i < len(keys); i++ {
		for j := i; j > 0 && keys[j] < keys[j-1]; j-- {
			keys[j], keys[j-1] = keys[j-1], keys[j]
		}
	}
	return keys
}

func TestDiffTimeText(t *testing.T) {
	if rs.CfgWindows {
		t.Skip("the oracle's SystemTime Debug text is the Unix one")
	}
	oracle.Check(t, oracleModule, "time_text", func(yield func(any)) {
		for _, key := range sortedKeys(timeTexts) {
			yield(textRequest{Op: "text", Key: key})
		}
	}, func(raw json.RawMessage) any {
		return textAnswer(timeTexts, decodeRequest[textRequest](t, raw).Key)
	})
}

// formattedTexts are the texts that go through rs's formatter.
var formattedTexts = map[string]func() string{
	"systemtime_error_debug": func() string {
		_, err := SystemTimeDurationSince(UnixEpoch, SystemTimeAdd(UnixEpoch, DurationFromMillis(1500)))
		return rs.DebugString(err)
	},
	"systemtime_error_padded": func() string {
		_, err := SystemTimeDurationSince(UnixEpoch, SystemTimeAdd(UnixEpoch, DurationFromMillis(1500)))
		return rs.Format("[{:>50}]", err)
	},
	"systemtime_duration_since_unwrap_panic": func() string {
		rs.Unwrap(SystemTimeDurationSince(UnixEpoch, SystemTimeAdd(UnixEpoch, DurationFromMillis(1500))))
		return ""
	},
	"try_from_float_negative_debug": func() string {
		_, err := DurationTryFromSecsF64(-1.0)
		return rs.DebugString(err)
	},
	"try_from_float_overflow_debug": func() string {
		_, err := DurationTryFromSecsF64(math.NaN())
		return rs.DebugString(err)
	},
	"try_from_float_padded": func() string {
		_, err := DurationTryFromSecsF64(-1.0)
		return rs.Format("[{:>70}]", err)
	},
	"instant_debug_shape": func() string { return digitsAsN(rs.DebugString(InstantNow())) },
}

// digitsAsN replaces every run of digits with N, as the oracle does for text
// that holds a value that differs from run to run.
func digitsAsN(text string) string {
	out := make([]byte, 0, len(text))
	inDigits := false
	for i := 0; i < len(text); i++ {
		if text[i] >= '0' && text[i] <= '9' {
			if !inDigits {
				out = append(out, 'N')
			}
			inDigits = true
			continue
		}
		inDigits = false
		out = append(out, text[i])
	}
	return string(out)
}

func TestDiffTimeFormattedText(t *testing.T) {
	if rs.CfgWindows {
		t.Skip("the oracle's Instant Debug text is the Unix one")
	}
	// The corpus is recorded whether or not the formatter exists; it is
	// compared once it does.
	if !formatterReady() && !recording() {
		t.Skip("rs.Format is not ported yet; FmtDisplay and FmtDebug cannot be checked")
	}
	oracle.Check(t, oracleModule, "time_formatted_text", func(yield func(any)) {
		for _, key := range sortedKeys(formattedTexts) {
			yield(textRequest{Op: "text", Key: key})
		}
	}, func(raw json.RawMessage) any {
		if !formatterReady() {
			// Recording only: there is nothing to compare with yet.
			t.Skip("rs.Format is not ported yet; the corpus was recorded and not compared")
		}
		return textAnswer(formattedTexts, decodeRequest[textRequest](t, raw).Key)
	})
}
