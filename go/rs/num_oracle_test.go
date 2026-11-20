package rs_test

import (
	"encoding/json"
	"fmt"
	"math"
	"math/big"
	"math/rand/v2"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The tests in this file hold the number helpers (num_*.go) against Rust
// through the oracle module std_num (go/port/oracle/src/std_num.rs).
// Integers travel as decimal strings and floats as the decimal string of
// their bits.

// numRequest is one request to std_num.
type numRequest struct {
	Op    string `json:"op"`
	Ty    string `json:"ty,omitempty"`
	F     string `json:"f,omitempty"`
	A     string `json:"a,omitempty"`
	B     string `json:"b,omitempty"`
	C     string `json:"c,omitempty"`
	S     string `json:"s,omitempty"`
	U     string `json:"u,omitempty"`
	N     int64  `json:"n,omitempty"`
	Mode  string `json:"mode,omitempty"`
	From  string `json:"from,omitempty"`
	To    string `json:"to,omitempty"`
	Bits  string `json:"bits,omitempty"`
	Hex   string `json:"hex,omitempty"`
	Go    string `json:"go,omitempty"`
	Tol   int64  `json:"tol,omitempty"`
	Radix uint32 `json:"radix,omitempty"`
}

func decodeNumRequest(t *testing.T, raw json.RawMessage) numRequest {
	t.Helper()
	var request numRequest
	if err := json.Unmarshal(raw, &request); err != nil {
		t.Fatalf("request does not decode: %v\n%s", err, raw)
	}
	return request
}

type reply = map[string]any

func val[T any](v T) reply { return reply{"r": fmt.Sprint(v)} }

func opt[T any](o rs.Option[T]) reply {
	if v, ok := o.Get(); ok {
		return reply{"r": fmt.Sprint(v)}
	}
	return reply{"r": nil}
}

func ovf[T any](v T, overflowed bool) reply {
	return reply{"r": fmt.Sprint(v), "o": overflowed}
}

func flag(b bool) reply { return reply{"r": b} }

func panicReply(message string) reply { return reply{"panic": message} }

func unknownRequest(request numRequest) reply {
	return reply{"go_error": fmt.Sprintf("the Go test does not answer %+v", request)}
}

// modal answers a method whose overflow behaviour follows the build
// profile. The request names the profile; when it is the one this test
// binary was built with, the method itself answers, and otherwise the
// expected answer is given from the checked and wrapping forms, so that the
// recorded corpus holds under both builds.
func modal(mode string, method, debug, release func() any) any {
	switch {
	case (mode == "debug") == rs.DebugAssertions:
		return method()
	case mode == "debug":
		return debug()
	default:
		return release()
	}
}

func bigInt(text string) *big.Int {
	if text == "" {
		return new(big.Int)
	}
	value, ok := new(big.Int).SetString(text, 10)
	if !ok {
		panic("not an integer: " + text)
	}
	return value
}

type anyInteger interface {
	~int | ~int8 | ~int16 | ~int32 | ~int64 | ~uint | ~uint8 | ~uint16 | ~uint32 | ~uint64 | ~uintptr
}

func isSigned[T anyInteger]() bool {
	var zero T
	return zero-1 < 0
}

// parse reads a decimal string as a T, wrapping as `as` would.
func parse[T anyInteger](text string) T {
	value := bigInt(text)
	if value.Sign() < 0 {
		return T(value.Int64())
	}
	return T(value.Uint64())
}

func parseU128(text string) rs.U128 {
	value := bigInt(text)
	if value.Sign() < 0 {
		value.Add(value, new(big.Int).Lsh(big.NewInt(1), 128))
	}
	lo := new(big.Int).And(value, new(big.Int).SetUint64(math.MaxUint64)).Uint64()
	hi := new(big.Int).Rsh(value, 64).Uint64()
	return rs.U128FromParts(hi, lo)
}

func parseI128(text string) rs.I128 { return rs.CastU128ToI128(parseU128(text)) }

func bigU128(x rs.U128) *big.Int {
	value := new(big.Int).SetUint64(x.Hi())
	value.Lsh(value, 64)
	return value.Or(value, new(big.Int).SetUint64(x.Lo()))
}

func bigI128(x rs.I128) *big.Int {
	value := bigU128(rs.CastI128ToU128(x))
	if x.Hi()>>63 != 0 {
		value.Sub(value, new(big.Int).Lsh(big.NewInt(1), 128))
	}
	return value
}

func valU128(x rs.U128) reply { return reply{"r": bigU128(x).String()} }

func valI128(x rs.I128) reply { return reply{"r": bigI128(x).String()} }

func optU128(o rs.Option[rs.U128]) reply {
	if v, ok := o.Get(); ok {
		return valU128(v)
	}
	return reply{"r": nil}
}

func optI128(o rs.Option[rs.I128]) reply {
	if v, ok := o.Get(); ok {
		return valI128(v)
	}
	return reply{"r": nil}
}

// ---------------------------------------------------------------------------
// Operand generation

type intType struct {
	name   string
	bits   uint
	signed bool
}

var fixedIntTypes = []intType{
	{"u8", 8, false}, {"u16", 16, false}, {"u32", 32, false}, {"u64", 64, false},
	{"i8", 8, true}, {"i16", 16, true}, {"i32", 32, true}, {"i64", 64, true},
}

var (
	isizeType = intType{"isize", 64, true}
	usizeType = intType{"usize", 64, false}
	u128Type  = intType{"u128", 128, false}
	i128Type  = intType{"i128", 128, true}
)

var allIntTypes = append(append([]intType{}, fixedIntTypes...), usizeType, isizeType, u128Type, i128Type)

func (t intType) min() *big.Int {
	if !t.signed {
		return new(big.Int)
	}
	return new(big.Int).Neg(new(big.Int).Lsh(big.NewInt(1), t.bits-1))
}

func (t intType) max() *big.Int {
	width := t.bits
	if t.signed {
		width--
	}
	return new(big.Int).Sub(new(big.Int).Lsh(big.NewInt(1), width), big.NewInt(1))
}

func (t intType) holds(v *big.Int) bool {
	return v.Cmp(t.min()) >= 0 && v.Cmp(t.max()) <= 0
}

// unsigned is the unsigned type of the same width, and signedOf the signed
// one.
func (t intType) unsigned() intType { return intType{name: "u", bits: t.bits} }

func (t intType) signedOf() intType { return intType{name: "i", bits: t.bits, signed: true} }

// edges are the values an integer method is most likely to get wrong.
func (t intType) edges() []*big.Int {
	offset := func(base *big.Int, delta int64) *big.Int {
		return new(big.Int).Add(base, big.NewInt(delta))
	}
	half := new(big.Int).Lsh(big.NewInt(1), t.bits/2)
	top := new(big.Int).Lsh(big.NewInt(1), t.bits-2)
	candidates := []*big.Int{
		big.NewInt(0), big.NewInt(1), big.NewInt(2), big.NewInt(3), big.NewInt(7), big.NewInt(10),
		big.NewInt(-1), big.NewInt(-2), big.NewInt(-3), big.NewInt(-10),
		offset(half, -1), half, offset(half, 1), new(big.Int).Neg(half),
		offset(top, -1), top, offset(top, 1), new(big.Int).Neg(top),
		t.max(), offset(t.max(), -1), offset(t.max(), -2),
		t.min(), offset(t.min(), 1), offset(t.min(), 2),
		new(big.Int).Rsh(t.max(), 1), offset(new(big.Int).Rsh(t.max(), 1), 1),
	}
	var edges []*big.Int
	seen := map[string]bool{}
	for _, candidate := range candidates {
		if t.holds(candidate) && !seen[candidate.String()] {
			seen[candidate.String()] = true
			edges = append(edges, candidate)
		}
	}
	return edges
}

// core are the few values every binary method is tried on in pairs.
func (t intType) core() []*big.Int {
	offset := func(base *big.Int, delta int64) *big.Int {
		return new(big.Int).Add(base, big.NewInt(delta))
	}
	if t.signed {
		return []*big.Int{big.NewInt(0), big.NewInt(1), big.NewInt(-1), big.NewInt(2), big.NewInt(-2),
			t.max(), offset(t.max(), -1), t.min(), offset(t.min(), 1)}
	}
	return []*big.Int{big.NewInt(0), big.NewInt(1), big.NewInt(2), big.NewInt(3),
		t.max(), offset(t.max(), -1), new(big.Int).Lsh(big.NewInt(1), t.bits-1)}
}

// random is a value of a random magnitude: half of them small, so that
// operations meet operands of every size.
func (t intType) random(rng *rand.Rand) *big.Int {
	width := t.bits
	if t.signed {
		width--
	}
	bitLength := uint(rng.IntN(int(width) + 1))
	value := new(big.Int)
	for index := uint(0); index < bitLength; index++ {
		if rng.IntN(2) == 1 {
			value.SetBit(value, int(index), 1)
		}
	}
	if bitLength > 0 && rng.IntN(4) != 0 {
		value.SetBit(value, int(bitLength-1), 1)
	}
	if t.signed && rng.IntN(2) == 1 {
		value.Neg(value)
		if rng.IntN(8) == 0 {
			value.Sub(value, big.NewInt(1))
		}
	}
	if !t.holds(value) {
		return t.max()
	}
	return value
}

// pairs are operand pairs for a binary method: the pairs that overflow or
// trap, a sample of the core values crossed with each other, and random
// values.
func (t intType) pairs(rng *rand.Rand, sample, random int) [][2]*big.Int {
	one, zero := big.NewInt(1), big.NewInt(0)
	pairs := [][2]*big.Int{
		{t.max(), t.max()}, {zero, zero}, {t.max(), one}, {one, zero}, {t.max(), zero},
		{zero, one}, {t.min(), t.min()}, {t.min(), one}, {zero, t.max()},
	}
	if t.signed {
		minusOne := big.NewInt(-1)
		pairs = append(pairs, [2]*big.Int{t.min(), minusOne}, [2]*big.Int{minusOne, t.min()},
			[2]*big.Int{t.max(), minusOne}, [2]*big.Int{t.min(), t.max()}, [2]*big.Int{t.max(), t.min()},
			[2]*big.Int{big.NewInt(-7), big.NewInt(2)}, [2]*big.Int{big.NewInt(7), big.NewInt(-2)},
			[2]*big.Int{big.NewInt(-7), big.NewInt(-2)})
	}
	core := t.core()
	for index := 0; index < sample; index++ {
		pairs = append(pairs, [2]*big.Int{core[rng.IntN(len(core))], core[rng.IntN(len(core))]})
	}
	edges := t.edges()
	for index := 0; index < sample/2; index++ {
		pairs = append(pairs, [2]*big.Int{edges[rng.IntN(len(edges))], edges[rng.IntN(len(edges))]})
	}
	for index := 0; index < random; index++ {
		pairs = append(pairs, [2]*big.Int{t.random(rng), t.random(rng)})
	}
	return pairs
}

// values are operands for a unary method.
func (t intType) values(rng *rand.Rand, random int) []*big.Int {
	values := t.edges()
	for index := 0; index < random; index++ {
		values = append(values, t.random(rng))
	}
	return values
}

// counts are shift counts and exponents around the widths of the types.
func (t intType) counts() []int64 {
	width := int64(t.bits)
	return []int64{0, 1, 2, 3, width - 1, width, width + 1, 2 * width, 2*width + 3, 31, 32, 33, 63, 64, 65,
		127, 128, 129, math.MaxUint32}
}

// ---------------------------------------------------------------------------
// Integer methods of the fixed-width types and of isize

var (
	binaryMethods = []string{
		"checked_add", "checked_sub", "checked_mul", "checked_div", "checked_rem", "checked_div_euclid",
		"checked_rem_euclid", "checked_ilog", "saturating_add", "saturating_sub", "saturating_mul",
		"saturating_div", "wrapping_add", "wrapping_sub", "wrapping_mul", "wrapping_div", "wrapping_rem",
		"overflowing_add", "overflowing_sub", "overflowing_mul", "overflowing_div", "overflowing_rem",
		"abs_diff", "ilog", "div_euclid", "rem_euclid",
	}
	unaryMethods = []string{
		"checked_neg", "checked_ilog2", "checked_ilog10", "wrapping_neg", "overflowing_neg", "isqrt",
		"ilog2", "ilog10", "count_ones", "count_zeros", "leading_zeros", "trailing_zeros", "leading_ones",
		"trailing_ones", "swap_bytes", "reverse_bits",
	}
	countMethods = []string{
		"checked_shl", "checked_shr", "wrapping_shl", "wrapping_shr", "overflowing_shl", "overflowing_shr",
		"rotate_left", "rotate_right",
	}
	powMethods            = []string{"checked_pow", "saturating_pow", "wrapping_pow", "overflowing_pow"}
	unsignedUnaryMethods  = []string{"checked_next_power_of_two", "is_power_of_two"}
	unsignedBinaryMethods = []string{"checked_next_multiple_of", "div_ceil", "is_multiple_of"}
	unsignedMixedMethods  = []string{
		"checked_add_signed", "checked_sub_signed", "saturating_add_signed", "saturating_sub_signed",
		"wrapping_add_signed",
	}
	signedUnaryMethods = []string{
		"checked_abs", "checked_isqrt", "saturating_neg", "saturating_abs", "wrapping_abs",
		"overflowing_abs", "unsigned_abs", "signum",
	}
	signedMixedMethods = []string{
		"checked_add_unsigned", "checked_sub_unsigned", "saturating_add_unsigned", "saturating_sub_unsigned",
	}
	modes = []string{"debug", "release"}
)

// powCases are bases and exponents whose powers sit on both sides of
// overflow.
func (t intType) powCases(rng *rand.Rand) [][2]*big.Int {
	bases := []*big.Int{big.NewInt(0), big.NewInt(1), big.NewInt(2), big.NewInt(3), big.NewInt(10),
		big.NewInt(-1), big.NewInt(-2), big.NewInt(-3), t.max(), t.min(),
		new(big.Int).Lsh(big.NewInt(1), t.bits/2), new(big.Int).Neg(new(big.Int).Lsh(big.NewInt(1), t.bits/2))}
	exponents := []int64{0, 1, 2, 3, int64(t.bits) - 2, int64(t.bits) - 1, int64(t.bits), int64(t.bits) + 1, 5, 31, 64, 1000}
	var cases [][2]*big.Int
	for _, base := range bases {
		if !t.holds(base) {
			continue
		}
		for index := 0; index < 5; index++ {
			cases = append(cases, [2]*big.Int{base, big.NewInt(exponents[rng.IntN(len(exponents))])})
		}
	}
	for index := 0; index < 6; index++ {
		cases = append(cases, [2]*big.Int{t.random(rng), big.NewInt(int64(rng.IntN(int(t.bits) + 2)))})
	}
	// The powers nearest the bounds: 2^(bits-1) and 2^bits, 3^k and 10^k.
	for _, base := range []int64{2, 3, 7, 10, -2, -3, -10} {
		if !t.signed && base < 0 {
			continue
		}
		power := big.NewInt(1)
		magnitude := big.NewInt(base)
		exponent := int64(0)
		for t.holds(power) {
			power.Mul(power, magnitude)
			exponent++
		}
		for _, e := range []int64{exponent - 1, exponent, exponent + 1} {
			if e >= 0 {
				cases = append(cases, [2]*big.Int{big.NewInt(base), big.NewInt(e)})
			}
		}
	}
	return cases
}

func generateIntMethods(types []intType, wide bool) oracle.Generate {
	return func(yield func(any)) {
		rng := rand.New(rand.NewPCG(0x6e756d, 0x696e74))
		for _, t := range types {
			emit := func(request numRequest) {
				request.Op, request.Ty = "int", t.name
				yield(request)
			}
			for _, method := range binaryMethods {
				for _, pair := range t.pairs(rng, 8, 5) {
					emit(numRequest{F: method, A: pair[0].String(), B: pair[1].String()})
				}
			}
			for _, method := range unaryMethods {
				for _, value := range t.values(rng, 4) {
					emit(numRequest{F: method, A: value.String()})
				}
			}
			for _, method := range countMethods {
				for _, count := range t.counts() {
					emit(numRequest{F: method, A: t.edges()[rng.IntN(len(t.edges()))].String(), N: count})
					emit(numRequest{F: method, A: t.random(rng).String(), N: count})
				}
			}
			for _, method := range powMethods {
				for _, c := range t.powCases(rng) {
					emit(numRequest{F: method, A: c[0].String(), N: c[1].Int64()})
				}
			}
			for _, mode := range modes {
				for _, c := range t.powCases(rng) {
					emit(numRequest{F: "pow", A: c[0].String(), N: c[1].Int64(), Mode: mode})
				}
			}
			// clamp, with bounds in order and out of order.
			for index := 0; index < 14; index++ {
				edges := t.edges()
				a, b, c := edges[rng.IntN(len(edges))], edges[rng.IntN(len(edges))], edges[rng.IntN(len(edges))]
				if index%2 == 0 && b.Cmp(c) > 0 {
					b, c = c, b
				}
				emit(numRequest{F: "clamp", A: a.String(), B: b.String(), C: c.String()})
			}
			if t.signed {
				for _, method := range signedUnaryMethods {
					for _, value := range t.values(rng, 4) {
						emit(numRequest{F: method, A: value.String()})
					}
				}
				for _, mode := range modes {
					for _, value := range t.values(rng, 2) {
						emit(numRequest{F: "abs", A: value.String(), Mode: mode})
						if wide {
							emit(numRequest{F: "neg", A: value.String(), Mode: mode})
						}
					}
				}
				unsigned := t.unsigned()
				for _, method := range signedMixedMethods {
					for _, value := range t.values(rng, 2) {
						emit(numRequest{F: method, A: value.String(), U: unsigned.edges()[rng.IntN(len(unsigned.edges()))].String()})
						emit(numRequest{F: method, A: value.String(), U: unsigned.random(rng).String()})
					}
				}
			} else {
				for _, method := range unsignedUnaryMethods {
					for _, value := range t.values(rng, 4) {
						emit(numRequest{F: method, A: value.String()})
					}
				}
				for _, mode := range modes {
					for _, value := range t.values(rng, 2) {
						emit(numRequest{F: "next_power_of_two", A: value.String(), Mode: mode})
					}
					for _, pair := range t.pairs(rng, 6, 4) {
						emit(numRequest{F: "next_multiple_of", A: pair[0].String(), B: pair[1].String(), Mode: mode})
					}
				}
				for _, method := range unsignedBinaryMethods {
					for _, pair := range t.pairs(rng, 8, 5) {
						emit(numRequest{F: method, A: pair[0].String(), B: pair[1].String()})
					}
				}
				signed := t.signedOf()
				for _, method := range unsignedMixedMethods {
					for _, value := range t.values(rng, 2) {
						emit(numRequest{F: method, A: value.String(), S: signed.edges()[rng.IntN(len(signed.edges()))].String()})
						emit(numRequest{F: method, A: value.String(), S: signed.random(rng).String()})
					}
				}
			}
			if wide {
				for _, method := range []string{"div", "rem", "bitand", "bitor", "bitxor", "cmp", "min", "max"} {
					for _, pair := range t.pairs(rng, 8, 6) {
						emit(numRequest{F: method, A: pair[0].String(), B: pair[1].String()})
					}
				}
				for _, value := range t.values(rng, 4) {
					emit(numRequest{F: "not", A: value.String()})
				}
				for _, mode := range modes {
					for _, method := range []string{"add", "sub", "mul"} {
						for _, pair := range t.pairs(rng, 8, 6) {
							emit(numRequest{F: method, A: pair[0].String(), B: pair[1].String(), Mode: mode})
						}
					}
					for _, method := range []string{"shl", "shr"} {
						for _, count := range t.counts() {
							emit(numRequest{F: method, A: t.random(rng).String(), N: count, Mode: mode})
							emit(numRequest{F: method, A: t.edges()[rng.IntN(len(t.edges()))].String(), N: count, Mode: mode})
						}
					}
				}
			}
		}
	}
}

// fixedMethod answers the methods every fixed-width integer type has.
func fixedMethod[T rs.FixedInt](request numRequest) (any, bool) {
	a, b, c := parse[T](request.A), parse[T](request.B), parse[T](request.C)
	n := uint32(request.N)
	switch request.F {
	case "checked_add":
		return opt(rs.CheckedAdd(a, b)), true
	case "checked_sub":
		return opt(rs.CheckedSub(a, b)), true
	case "checked_mul":
		return opt(rs.CheckedMul(a, b)), true
	case "checked_div":
		return opt(rs.CheckedDiv(a, b)), true
	case "checked_rem":
		return opt(rs.CheckedRem(a, b)), true
	case "checked_div_euclid":
		return opt(rs.CheckedDivEuclid(a, b)), true
	case "checked_rem_euclid":
		return opt(rs.CheckedRemEuclid(a, b)), true
	case "checked_neg":
		return opt(rs.CheckedNeg(a)), true
	case "checked_pow":
		return opt(rs.CheckedPow(a, n)), true
	case "checked_shl":
		return opt(rs.CheckedShl(a, n)), true
	case "checked_shr":
		return opt(rs.CheckedShr(a, n)), true
	case "checked_ilog2":
		return opt(rs.CheckedIlog2(a)), true
	case "checked_ilog10":
		return opt(rs.CheckedIlog10(a)), true
	case "checked_ilog":
		return opt(rs.CheckedIlog(a, b)), true
	case "saturating_add":
		return val(rs.SaturatingAdd(a, b)), true
	case "saturating_sub":
		return val(rs.SaturatingSub(a, b)), true
	case "saturating_mul":
		return val(rs.SaturatingMul(a, b)), true
	case "saturating_pow":
		return val(rs.SaturatingPow(a, n)), true
	case "saturating_div":
		return val(rs.SaturatingDiv(a, b)), true
	case "wrapping_add":
		return val(rs.WrappingAdd(a, b)), true
	case "wrapping_sub":
		return val(rs.WrappingSub(a, b)), true
	case "wrapping_mul":
		return val(rs.WrappingMul(a, b)), true
	case "wrapping_neg":
		return val(rs.WrappingNeg(a)), true
	case "wrapping_pow":
		return val(rs.WrappingPow(a, n)), true
	case "wrapping_shl":
		return val(rs.WrappingShl(a, n)), true
	case "wrapping_shr":
		return val(rs.WrappingShr(a, n)), true
	case "wrapping_div":
		return val(rs.WrappingDiv(a, b)), true
	case "wrapping_rem":
		return val(rs.WrappingRem(a, b)), true
	case "overflowing_add":
		return ovf(rs.OverflowingAdd(a, b)), true
	case "overflowing_sub":
		return ovf(rs.OverflowingSub(a, b)), true
	case "overflowing_mul":
		return ovf(rs.OverflowingMul(a, b)), true
	case "overflowing_neg":
		return ovf(rs.OverflowingNeg(a)), true
	case "overflowing_pow":
		return ovf(rs.OverflowingPow(a, n)), true
	case "overflowing_shl":
		return ovf(rs.OverflowingShl(a, n)), true
	case "overflowing_shr":
		return ovf(rs.OverflowingShr(a, n)), true
	case "overflowing_div":
		return ovf(rs.OverflowingDiv(a, b)), true
	case "overflowing_rem":
		return ovf(rs.OverflowingRem(a, b)), true
	case "pow":
		return modal(request.Mode,
			func() any { return val(rs.Pow(a, n)) },
			func() any {
				if power, ok := rs.CheckedPow(a, n).Get(); ok {
					return val(power)
				}
				return panicReply("attempt to exponentiate with overflow")
			},
			func() any { return val(rs.WrappingPow(a, n)) }), true
	case "isqrt":
		return val(rs.Isqrt(a)), true
	case "ilog2":
		return val(rs.Ilog2(a)), true
	case "ilog10":
		return val(rs.Ilog10(a)), true
	case "ilog":
		return val(rs.Ilog(a, b)), true
	case "div_euclid":
		return val(rs.DivEuclid(a, b)), true
	case "rem_euclid":
		return val(rs.RemEuclid(a, b)), true
	case "clamp":
		return val(rs.Clamp(a, b, c)), true
	case "count_ones":
		return val(rs.CountOnes(a)), true
	case "count_zeros":
		return val(rs.CountZeros(a)), true
	case "leading_zeros":
		return val(rs.LeadingZeros(a)), true
	case "trailing_zeros":
		return val(rs.TrailingZeros(a)), true
	case "leading_ones":
		return val(rs.LeadingOnes(a)), true
	case "trailing_ones":
		return val(rs.TrailingOnes(a)), true
	case "rotate_left":
		return val(rs.RotateLeft(a, n)), true
	case "rotate_right":
		return val(rs.RotateRight(a, n)), true
	case "swap_bytes":
		return val(rs.SwapBytes(a)), true
	case "reverse_bits":
		return val(rs.ReverseBits(a)), true
	}
	return nil, false
}

func unsignedMethod[T rs.UnsignedInt](request numRequest) any {
	if answer, ok := fixedMethod[T](request); ok {
		return answer
	}
	a, b := parse[T](request.A), parse[T](request.B)
	s := parse[int64](request.S)
	switch request.F {
	case "abs_diff":
		return val(rs.AbsDiff(a, b))
	case "checked_next_power_of_two":
		return opt(rs.CheckedNextPowerOfTwo(a))
	case "checked_next_multiple_of":
		return opt(rs.CheckedNextMultipleOf(a, b))
	case "checked_add_signed":
		return opt(rs.CheckedAddSigned(a, s))
	case "checked_sub_signed":
		return opt(rs.CheckedSubSigned(a, s))
	case "saturating_add_signed":
		return val(rs.SaturatingAddSigned(a, s))
	case "saturating_sub_signed":
		return val(rs.SaturatingSubSigned(a, s))
	case "wrapping_add_signed":
		return val(rs.WrappingAddSigned(a, s))
	case "div_ceil":
		return val(rs.DivCeil(a, b))
	case "is_power_of_two":
		return flag(rs.IsPowerOfTwo(a))
	case "is_multiple_of":
		return flag(rs.IsMultipleOf(a, b))
	case "next_power_of_two":
		return modal(request.Mode,
			func() any { return val(rs.NextPowerOfTwo(a)) },
			func() any {
				if power, ok := rs.CheckedNextPowerOfTwo(a).Get(); ok {
					return val(power)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any { return val(rs.CheckedNextPowerOfTwo(a).UnwrapOr(0)) })
	case "next_multiple_of":
		return modal(request.Mode,
			func() any { return val(rs.NextMultipleOf(a, b)) },
			func() any {
				if b == 0 {
					return panicReply("attempt to calculate the remainder with a divisor of zero")
				}
				if multiple, ok := rs.CheckedNextMultipleOf(a, b).Get(); ok {
					return val(multiple)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any {
				if b == 0 {
					return panicReply("attempt to calculate the remainder with a divisor of zero")
				}
				if a%b == 0 {
					return val(a)
				}
				return val(a + (b - a%b))
			})
	}
	return unknownRequest(request)
}

func signedMethod[T rs.SignedInt](request numRequest) any {
	if answer, ok := fixedMethod[T](request); ok {
		return answer
	}
	a, b := parse[T](request.A), parse[T](request.B)
	u := parse[uint64](request.U)
	// The unsigned results are of a's width in Rust and uint64 here.
	narrow := func(v uint64) reply { return val(v) }
	switch request.F {
	case "abs_diff":
		return narrow(rs.AbsDiffSigned(a, b))
	case "unsigned_abs":
		return narrow(rs.UnsignedAbs(a))
	case "checked_abs":
		return opt(rs.CheckedAbs(a))
	case "checked_isqrt":
		return opt(rs.CheckedIsqrt(a))
	case "checked_add_unsigned":
		return opt(rs.CheckedAddUnsigned(a, u))
	case "checked_sub_unsigned":
		return opt(rs.CheckedSubUnsigned(a, u))
	case "saturating_add_unsigned":
		return val(rs.SaturatingAddUnsigned(a, u))
	case "saturating_sub_unsigned":
		return val(rs.SaturatingSubUnsigned(a, u))
	case "saturating_neg":
		return val(rs.SaturatingNeg(a))
	case "saturating_abs":
		return val(rs.SaturatingAbs(a))
	case "wrapping_abs":
		return val(rs.WrappingAbs(a))
	case "overflowing_abs":
		return ovf(rs.OverflowingAbs(a))
	case "signum":
		return val(rs.Signum(a))
	case "abs":
		return modal(request.Mode,
			func() any { return val(rs.Abs(a)) },
			func() any {
				if absolute, ok := rs.CheckedAbs(a).Get(); ok {
					return val(absolute)
				}
				return panicReply("attempt to negate with overflow")
			},
			func() any { return val(rs.WrappingAbs(a)) })
	}
	return unknownRequest(request)
}

// isizeMethod answers for isize through the Isize functions.
func isizeMethod(request numRequest) any {
	a, b, c := parse[int](request.A), parse[int](request.B), parse[int](request.C)
	u := parse[int](request.U)
	n := uint32(request.N)
	switch request.F {
	case "checked_add":
		return opt(rs.IsizeCheckedAdd(a, b))
	case "checked_sub":
		return opt(rs.IsizeCheckedSub(a, b))
	case "checked_mul":
		return opt(rs.IsizeCheckedMul(a, b))
	case "checked_div":
		return opt(rs.IsizeCheckedDiv(a, b))
	case "checked_rem":
		return opt(rs.IsizeCheckedRem(a, b))
	case "checked_div_euclid":
		return opt(rs.IsizeCheckedDivEuclid(a, b))
	case "checked_rem_euclid":
		return opt(rs.IsizeCheckedRemEuclid(a, b))
	case "checked_neg":
		return opt(rs.IsizeCheckedNeg(a))
	case "checked_pow":
		return opt(rs.IsizeCheckedPow(a, n))
	case "checked_shl":
		return opt(rs.IsizeCheckedShl(a, n))
	case "checked_shr":
		return opt(rs.IsizeCheckedShr(a, n))
	case "checked_ilog2":
		return opt(rs.IsizeCheckedIlog2(a))
	case "checked_ilog10":
		return opt(rs.IsizeCheckedIlog10(a))
	case "checked_ilog":
		return opt(rs.IsizeCheckedIlog(a, b))
	case "saturating_add":
		return val(rs.IsizeSaturatingAdd(a, b))
	case "saturating_sub":
		return val(rs.IsizeSaturatingSub(a, b))
	case "saturating_mul":
		return val(rs.IsizeSaturatingMul(a, b))
	case "saturating_pow":
		return val(rs.IsizeSaturatingPow(a, n))
	case "saturating_div":
		return val(rs.IsizeSaturatingDiv(a, b))
	case "wrapping_add":
		return val(rs.IsizeWrappingAdd(a, b))
	case "wrapping_sub":
		return val(rs.IsizeWrappingSub(a, b))
	case "wrapping_mul":
		return val(rs.IsizeWrappingMul(a, b))
	case "wrapping_neg":
		return val(rs.IsizeWrappingNeg(a))
	case "wrapping_pow":
		return val(rs.IsizeWrappingPow(a, n))
	case "wrapping_shl":
		return val(rs.IsizeWrappingShl(a, n))
	case "wrapping_shr":
		return val(rs.IsizeWrappingShr(a, n))
	case "wrapping_div":
		return val(rs.IsizeWrappingDiv(a, b))
	case "wrapping_rem":
		return val(rs.IsizeWrappingRem(a, b))
	case "overflowing_add":
		return ovf(rs.IsizeOverflowingAdd(a, b))
	case "overflowing_sub":
		return ovf(rs.IsizeOverflowingSub(a, b))
	case "overflowing_mul":
		return ovf(rs.IsizeOverflowingMul(a, b))
	case "overflowing_neg":
		return ovf(rs.IsizeOverflowingNeg(a))
	case "overflowing_pow":
		return ovf(rs.IsizeOverflowingPow(a, n))
	case "overflowing_shl":
		return ovf(rs.IsizeOverflowingShl(a, n))
	case "overflowing_shr":
		return ovf(rs.IsizeOverflowingShr(a, n))
	case "overflowing_div":
		return ovf(rs.IsizeOverflowingDiv(a, b))
	case "overflowing_rem":
		return ovf(rs.IsizeOverflowingRem(a, b))
	case "pow":
		return modal(request.Mode,
			func() any { return val(rs.IsizePow(a, n)) },
			func() any {
				if power, ok := rs.IsizeCheckedPow(a, n).Get(); ok {
					return val(power)
				}
				return panicReply("attempt to exponentiate with overflow")
			},
			func() any { return val(rs.IsizeWrappingPow(a, n)) })
	case "isqrt":
		return val(rs.IsizeIsqrt(a))
	case "ilog2":
		return val(rs.IsizeIlog2(a))
	case "ilog10":
		return val(rs.IsizeIlog10(a))
	case "ilog":
		return val(rs.IsizeIlog(a, b))
	case "div_euclid":
		return val(rs.IsizeDivEuclid(a, b))
	case "rem_euclid":
		return val(rs.IsizeRemEuclid(a, b))
	case "clamp":
		return val(rs.Clamp(a, b, c))
	case "count_ones":
		return val(rs.CountOnes(a))
	case "count_zeros":
		return val(rs.CountZeros(a))
	case "leading_zeros":
		return val(rs.LeadingZeros(a))
	case "trailing_zeros":
		return val(rs.TrailingZeros(a))
	case "leading_ones":
		return val(rs.LeadingOnes(a))
	case "trailing_ones":
		return val(rs.TrailingOnes(a))
	case "rotate_left":
		return val(rs.RotateLeft(a, n))
	case "rotate_right":
		return val(rs.RotateRight(a, n))
	case "swap_bytes":
		return val(rs.SwapBytes(a))
	case "reverse_bits":
		return val(rs.ReverseBits(a))
	case "abs_diff":
		return val(rs.IsizeAbsDiff(a, b))
	case "unsigned_abs":
		return val(rs.IsizeUnsignedAbs(a))
	case "checked_abs":
		return opt(rs.IsizeCheckedAbs(a))
	case "checked_isqrt":
		return opt(rs.IsizeCheckedIsqrt(a))
	case "checked_add_unsigned":
		return opt(rs.IsizeCheckedAddUnsigned(a, u))
	case "checked_sub_unsigned":
		return opt(rs.IsizeCheckedSubUnsigned(a, u))
	case "saturating_add_unsigned":
		return val(rs.IsizeSaturatingAddUnsigned(a, u))
	case "saturating_sub_unsigned":
		return val(rs.IsizeSaturatingSubUnsigned(a, u))
	case "saturating_neg":
		return val(rs.IsizeSaturatingNeg(a))
	case "saturating_abs":
		return val(rs.IsizeSaturatingAbs(a))
	case "wrapping_abs":
		return val(rs.IsizeWrappingAbs(a))
	case "overflowing_abs":
		return ovf(rs.IsizeOverflowingAbs(a))
	case "signum":
		return val(rs.IsizeSignum(a))
	case "abs":
		return modal(request.Mode,
			func() any { return val(rs.IsizeAbs(a)) },
			func() any {
				if absolute, ok := rs.IsizeCheckedAbs(a).Get(); ok {
					return val(absolute)
				}
				return panicReply("attempt to negate with overflow")
			},
			func() any { return val(rs.IsizeWrappingAbs(a)) })
	}
	return unknownRequest(request)
}

func answerIntMethod(request numRequest) any {
	switch request.Ty {
	case "u8":
		return unsignedMethod[uint8](request)
	case "u16":
		return unsignedMethod[uint16](request)
	case "u32":
		return unsignedMethod[uint32](request)
	case "u64":
		return unsignedMethod[uint64](request)
	case "i8":
		return signedMethod[int8](request)
	case "i16":
		return signedMethod[int16](request)
	case "i32":
		return signedMethod[int32](request)
	case "i64":
		return signedMethod[int64](request)
	case "isize":
		return isizeMethod(request)
	case "u128":
		return u128Method(request)
	case "i128":
		return i128Method(request)
	}
	return unknownRequest(request)
}

func TestNumIntMethodsAgainstRust(t *testing.T) {
	// isize results that are usizes in Rust (unsigned_abs, abs_diff) and
	// usize operands are ints here, so the cases whose value is above
	// isize::MAX are left to TestIsizeUnsignedResultsSaturate.
	types := append(append([]intType{}, fixedIntTypes...), isizeType)
	generate := generateIntMethods(types, false)
	filtered := func(yield func(any)) {
		generate(func(request any) {
			r := request.(numRequest)
			if r.Ty == "isize" && !isizeCaseFits(r) {
				return
			}
			yield(request)
		})
	}
	oracle.Check(t, "std_num", "num_int", filtered, func(raw json.RawMessage) any {
		request := decodeNumRequest(t, raw)
		return oracle.Catch(func() any { return answerIntMethod(request) })
	})
}

// isizeCaseFits reports whether an isize case stays within what an int can
// say about a usize.
func isizeCaseFits(request numRequest) bool {
	maxUsize := new(big.Int).SetUint64(math.MaxInt64)
	switch request.F {
	case "unsigned_abs":
		return new(big.Int).Abs(bigInt(request.A)).Cmp(maxUsize) <= 0
	case "abs_diff":
		difference := new(big.Int).Sub(bigInt(request.A), bigInt(request.B))
		return difference.Abs(difference).Cmp(maxUsize) <= 0
	case "checked_add_unsigned", "checked_sub_unsigned", "saturating_add_unsigned", "saturating_sub_unsigned":
		return bigInt(request.U).Cmp(maxUsize) <= 0
	}
	return true
}

// ---------------------------------------------------------------------------
// u128 and i128

func u128Method(request numRequest) any {
	a, b, c := parseU128(request.A), parseU128(request.B), parseU128(request.C)
	n := uint32(request.N)
	switch request.F {
	case "checked_add":
		return optU128(a.CheckedAdd(b))
	case "checked_sub":
		return optU128(a.CheckedSub(b))
	case "checked_mul":
		return optU128(a.CheckedMul(b))
	case "checked_div", "checked_div_euclid":
		return optU128(a.CheckedDiv(b))
	case "checked_rem", "checked_rem_euclid":
		return optU128(a.CheckedRem(b))
	case "checked_neg":
		return optU128(a.CheckedNeg())
	case "checked_pow":
		return optU128(a.CheckedPow(n))
	case "checked_shl":
		return optU128(a.CheckedShl(n))
	case "checked_shr":
		return optU128(a.CheckedShr(n))
	case "checked_next_power_of_two":
		return optU128(a.CheckedNextPowerOfTwo())
	case "saturating_add":
		return valU128(a.SaturatingAdd(b))
	case "saturating_sub":
		return valU128(a.SaturatingSub(b))
	case "saturating_mul":
		return valU128(a.SaturatingMul(b))
	case "saturating_pow":
		return valU128(a.SaturatingPow(n))
	case "wrapping_add":
		return valU128(a.WrappingAdd(b))
	case "wrapping_sub":
		return valU128(a.WrappingSub(b))
	case "wrapping_mul":
		return valU128(a.WrappingMul(b))
	case "wrapping_neg":
		return valU128(a.WrappingNeg())
	case "wrapping_pow":
		return valU128(a.WrappingPow(n))
	case "wrapping_shl":
		return valU128(a.WrappingShl(n))
	case "wrapping_shr":
		return valU128(a.WrappingShr(n))
	case "overflowing_add":
		sum, overflow := a.OverflowingAdd(b)
		return reply{"r": bigU128(sum).String(), "o": overflow}
	case "overflowing_sub":
		difference, overflow := a.OverflowingSub(b)
		return reply{"r": bigU128(difference).String(), "o": overflow}
	case "overflowing_mul":
		product, overflow := a.OverflowingMul(b)
		return reply{"r": bigU128(product).String(), "o": overflow}
	case "overflowing_pow":
		power, overflow := a.OverflowingPow(n)
		return reply{"r": bigU128(power).String(), "o": overflow}
	case "abs_diff":
		return valU128(a.AbsDiff(b))
	case "div_euclid":
		return valU128(a.DivEuclid(b))
	case "rem_euclid":
		return valU128(a.RemEuclid(b))
	case "div_ceil":
		return valU128(a.DivCeil(b))
	case "is_power_of_two":
		return flag(a.IsPowerOfTwo())
	case "is_multiple_of":
		return flag(a.IsMultipleOf(b))
	case "clamp":
		return valU128(a.Clamp(b, c))
	case "count_ones":
		return val(a.CountOnes())
	case "count_zeros":
		return val(a.CountZeros())
	case "leading_zeros":
		return val(a.LeadingZeros())
	case "trailing_zeros":
		return val(a.TrailingZeros())
	case "ilog2":
		return val(a.Ilog2())
	case "swap_bytes":
		return valU128(a.SwapBytes())
	case "div":
		return valU128(a.Div(b))
	case "rem":
		return valU128(a.Rem(b))
	case "bitand":
		return valU128(a.BitAnd(b))
	case "bitor":
		return valU128(a.BitOr(b))
	case "bitxor":
		return valU128(a.BitXor(b))
	case "not":
		return valU128(a.Not())
	case "cmp":
		return reply{"r": a.Compare(b)}
	case "min":
		return valU128(a.Min(b))
	case "max":
		return valU128(a.Max(b))
	case "pow":
		return modal(request.Mode,
			func() any { return valU128(a.Pow(n)) },
			func() any {
				if power, ok := a.CheckedPow(n).Get(); ok {
					return valU128(power)
				}
				return panicReply("attempt to exponentiate with overflow")
			},
			func() any { return valU128(a.WrappingPow(n)) })
	case "add":
		return modal(request.Mode,
			func() any { return valU128(a.Add(b)) },
			func() any {
				if sum, ok := a.CheckedAdd(b).Get(); ok {
					return valU128(sum)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any { return valU128(a.WrappingAdd(b)) })
	case "sub":
		return modal(request.Mode,
			func() any { return valU128(a.Sub(b)) },
			func() any {
				if difference, ok := a.CheckedSub(b).Get(); ok {
					return valU128(difference)
				}
				return panicReply("attempt to subtract with overflow")
			},
			func() any { return valU128(a.WrappingSub(b)) })
	case "mul":
		return modal(request.Mode,
			func() any { return valU128(a.Mul(b)) },
			func() any {
				if product, ok := a.CheckedMul(b).Get(); ok {
					return valU128(product)
				}
				return panicReply("attempt to multiply with overflow")
			},
			func() any { return valU128(a.WrappingMul(b)) })
	case "shl":
		return modal(request.Mode,
			func() any { return valU128(a.Shl(n)) },
			func() any {
				if shifted, ok := a.CheckedShl(n).Get(); ok {
					return valU128(shifted)
				}
				return panicReply("attempt to shift left with overflow")
			},
			func() any { return valU128(a.WrappingShl(n)) })
	case "shr":
		return modal(request.Mode,
			func() any { return valU128(a.Shr(n)) },
			func() any {
				if shifted, ok := a.CheckedShr(n).Get(); ok {
					return valU128(shifted)
				}
				return panicReply("attempt to shift right with overflow")
			},
			func() any { return valU128(a.WrappingShr(n)) })
	case "next_power_of_two":
		return modal(request.Mode,
			func() any { return valU128(a.NextPowerOfTwo()) },
			func() any {
				if power, ok := a.CheckedNextPowerOfTwo().Get(); ok {
					return valU128(power)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any { return valU128(a.CheckedNextPowerOfTwo().UnwrapOr(rs.U128{})) })
	case "next_multiple_of":
		wrapped := func() any {
			if b == (rs.U128{}) {
				return panicReply("attempt to calculate the remainder with a divisor of zero")
			}
			remainder := a.Rem(b)
			if remainder == (rs.U128{}) {
				return valU128(a)
			}
			return valU128(a.WrappingAdd(b.WrappingSub(remainder)))
		}
		return modal(request.Mode,
			func() any { return valU128(a.NextMultipleOf(b)) },
			func() any {
				if b == (rs.U128{}) {
					return panicReply("attempt to calculate the remainder with a divisor of zero")
				}
				remainder := a.Rem(b)
				if remainder == (rs.U128{}) {
					return valU128(a)
				}
				if sum, ok := a.CheckedAdd(b.WrappingSub(remainder)).Get(); ok {
					return valU128(sum)
				}
				return panicReply("attempt to add with overflow")
			},
			wrapped)
	}
	return unknownRequest(request)
}

func i128Method(request numRequest) any {
	a, b, c := parseI128(request.A), parseI128(request.B), parseI128(request.C)
	n := uint32(request.N)
	switch request.F {
	case "checked_add":
		return optI128(a.CheckedAdd(b))
	case "checked_sub":
		return optI128(a.CheckedSub(b))
	case "checked_mul":
		return optI128(a.CheckedMul(b))
	case "checked_div":
		return optI128(a.CheckedDiv(b))
	case "checked_rem":
		return optI128(a.CheckedRem(b))
	case "checked_neg":
		return optI128(a.CheckedNeg())
	case "checked_abs":
		return optI128(a.CheckedAbs())
	case "checked_pow":
		return optI128(a.CheckedPow(n))
	case "checked_shl":
		return optI128(a.CheckedShl(n))
	case "checked_shr":
		return optI128(a.CheckedShr(n))
	case "saturating_add":
		return valI128(a.SaturatingAdd(b))
	case "saturating_sub":
		return valI128(a.SaturatingSub(b))
	case "saturating_mul":
		return valI128(a.SaturatingMul(b))
	case "saturating_pow":
		return valI128(a.SaturatingPow(n))
	case "saturating_neg":
		return valI128(a.SaturatingNeg())
	case "saturating_abs":
		return valI128(a.SaturatingAbs())
	case "wrapping_add":
		return valI128(a.WrappingAdd(b))
	case "wrapping_sub":
		return valI128(a.WrappingSub(b))
	case "wrapping_mul":
		return valI128(a.WrappingMul(b))
	case "wrapping_neg":
		return valI128(a.WrappingNeg())
	case "wrapping_abs":
		return valI128(a.WrappingAbs())
	case "wrapping_pow":
		return valI128(a.WrappingPow(n))
	case "wrapping_shl":
		return valI128(a.WrappingShl(n))
	case "wrapping_shr":
		return valI128(a.WrappingShr(n))
	case "overflowing_add":
		sum, overflow := a.OverflowingAdd(b)
		return reply{"r": bigI128(sum).String(), "o": overflow}
	case "overflowing_sub":
		difference, overflow := a.OverflowingSub(b)
		return reply{"r": bigI128(difference).String(), "o": overflow}
	case "overflowing_mul":
		product, overflow := a.OverflowingMul(b)
		return reply{"r": bigI128(product).String(), "o": overflow}
	case "overflowing_neg":
		negated, overflow := a.OverflowingNeg()
		return reply{"r": bigI128(negated).String(), "o": overflow}
	case "overflowing_pow":
		power, overflow := a.OverflowingPow(n)
		return reply{"r": bigI128(power).String(), "o": overflow}
	case "abs_diff":
		return valU128(a.AbsDiff(b))
	case "unsigned_abs":
		return valU128(a.UnsignedAbs())
	case "signum":
		return valI128(a.Signum())
	case "div_euclid":
		return valI128(a.DivEuclid(b))
	case "rem_euclid":
		return valI128(a.RemEuclid(b))
	case "clamp":
		return valI128(a.Clamp(b, c))
	case "count_ones":
		return val(a.CountOnes())
	case "count_zeros":
		return val(a.CountZeros())
	case "leading_zeros":
		return val(a.LeadingZeros())
	case "trailing_zeros":
		return val(a.TrailingZeros())
	case "div":
		return valI128(a.Div(b))
	case "rem":
		return valI128(a.Rem(b))
	case "bitand":
		return valI128(a.BitAnd(b))
	case "bitor":
		return valI128(a.BitOr(b))
	case "bitxor":
		return valI128(a.BitXor(b))
	case "not":
		return valI128(a.Not())
	case "cmp":
		return reply{"r": a.Compare(b)}
	case "min":
		return valI128(a.Min(b))
	case "max":
		return valI128(a.Max(b))
	case "pow":
		return modal(request.Mode,
			func() any { return valI128(a.Pow(n)) },
			func() any {
				if power, ok := a.CheckedPow(n).Get(); ok {
					return valI128(power)
				}
				return panicReply("attempt to exponentiate with overflow")
			},
			func() any { return valI128(a.WrappingPow(n)) })
	case "abs":
		return modal(request.Mode,
			func() any { return valI128(a.Abs()) },
			func() any {
				if absolute, ok := a.CheckedAbs().Get(); ok {
					return valI128(absolute)
				}
				return panicReply("attempt to negate with overflow")
			},
			func() any { return valI128(a.WrappingAbs()) })
	case "neg":
		return modal(request.Mode,
			func() any { return valI128(a.Neg()) },
			func() any {
				if negated, ok := a.CheckedNeg().Get(); ok {
					return valI128(negated)
				}
				return panicReply("attempt to negate with overflow")
			},
			func() any { return valI128(a.WrappingNeg()) })
	case "add":
		return modal(request.Mode,
			func() any { return valI128(a.Add(b)) },
			func() any {
				if sum, ok := a.CheckedAdd(b).Get(); ok {
					return valI128(sum)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any { return valI128(a.WrappingAdd(b)) })
	case "sub":
		return modal(request.Mode,
			func() any { return valI128(a.Sub(b)) },
			func() any {
				if difference, ok := a.CheckedSub(b).Get(); ok {
					return valI128(difference)
				}
				return panicReply("attempt to subtract with overflow")
			},
			func() any { return valI128(a.WrappingSub(b)) })
	case "mul":
		return modal(request.Mode,
			func() any { return valI128(a.Mul(b)) },
			func() any {
				if product, ok := a.CheckedMul(b).Get(); ok {
					return valI128(product)
				}
				return panicReply("attempt to multiply with overflow")
			},
			func() any { return valI128(a.WrappingMul(b)) })
	case "shl":
		return modal(request.Mode,
			func() any { return valI128(a.Shl(n)) },
			func() any {
				if shifted, ok := a.CheckedShl(n).Get(); ok {
					return valI128(shifted)
				}
				return panicReply("attempt to shift left with overflow")
			},
			func() any { return valI128(a.WrappingShl(n)) })
	case "shr":
		return modal(request.Mode,
			func() any { return valI128(a.Shr(n)) },
			func() any {
				if shifted, ok := a.CheckedShr(n).Get(); ok {
					return valI128(shifted)
				}
				return panicReply("attempt to shift right with overflow")
			},
			func() any { return valI128(a.WrappingShr(n)) })
	}
	return unknownRequest(request)
}

// wideMethods are the methods of std_num that U128 and I128 have.
var wideSkipped = map[string]bool{
	// Not provided for the 128-bit types.
	"checked_ilog2": true, "checked_ilog10": true, "checked_ilog": true, "isqrt": true, "ilog10": true,
	"ilog": true, "leading_ones": true, "trailing_ones": true, "rotate_left": true, "rotate_right": true,
	"reverse_bits": true, "saturating_div": true, "wrapping_div": true, "wrapping_rem": true,
	"overflowing_div": true, "overflowing_rem": true, "overflowing_shl": true, "overflowing_shr": true,
	"checked_next_multiple_of": true, "checked_add_signed": true, "checked_sub_signed": true,
	"saturating_add_signed": true, "saturating_sub_signed": true, "wrapping_add_signed": true,
	"checked_isqrt": true, "checked_add_unsigned": true, "checked_sub_unsigned": true,
	"saturating_add_unsigned": true, "saturating_sub_unsigned": true, "overflowing_abs": true,
}

var wideSkippedByType = map[string]map[string]bool{
	"u128": {"overflowing_neg": true},
	"i128": {"checked_div_euclid": true, "checked_rem_euclid": true, "ilog2": true, "swap_bytes": true},
}

func TestNumWideAgainstRust(t *testing.T) {
	generate := generateIntMethods([]intType{u128Type, i128Type}, true)
	filtered := func(yield func(any)) {
		generate(func(request any) {
			r := request.(numRequest)
			if wideSkipped[r.F] || wideSkippedByType[r.Ty][r.F] {
				return
			}
			yield(request)
		})
		rng := rand.New(rand.NewPCG(0x77696465, 0x74657874))
		for _, t := range []intType{u128Type, i128Type} {
			for _, value := range t.values(rng, 12) {
				yield(numRequest{Op: "wide_text", Ty: t.name, A: value.String()})
			}
		}
	}
	oracle.Check(t, "std_num", "num_wide", filtered, func(raw json.RawMessage) any {
		request := decodeNumRequest(t, raw)
		return oracle.Catch(func() any {
			if request.Op == "wide_text" {
				return wideText(request)
			}
			return answerIntMethod(request)
		})
	})
}

func wideText(request numRequest) any {
	if request.Ty == "u128" {
		x := parseU128(request.A)
		return reply{
			"display":   x.String(),
			"lower_hex": x.Text(16),
			"upper_hex": strings.ToUpper(x.Text(16)),
			"octal":     x.Text(8),
			"binary":    x.Text(2),
		}
	}
	x := parseI128(request.A)
	bits := rs.CastI128ToU128(x)
	return reply{
		"display":   x.String(),
		"lower_hex": bits.Text(16),
		"upper_hex": strings.ToUpper(bits.Text(16)),
		"octal":     bits.Text(8),
		"binary":    bits.Text(2),
	}
}

// ---------------------------------------------------------------------------
// usize

var (
	usizeBinaryMethods = []string{
		"checked_add", "checked_sub", "checked_mul", "checked_div", "checked_rem", "checked_div_euclid",
		"checked_rem_euclid", "checked_next_multiple_of", "checked_ilog", "saturating_add", "saturating_sub",
		"saturating_mul", "saturating_div", "wrapping_add", "wrapping_sub", "wrapping_mul", "overflowing_add",
		"overflowing_sub", "overflowing_mul", "abs_diff", "ilog", "div_euclid", "rem_euclid", "div_ceil",
		"is_multiple_of",
	}
	usizeUnaryMethods = []string{
		"checked_neg", "checked_next_power_of_two", "checked_ilog2", "checked_ilog10", "wrapping_neg", "isqrt",
		"ilog2", "ilog10", "is_power_of_two",
	}
	usizeMixedMethods = []string{
		"checked_add_signed", "checked_sub_signed", "saturating_add_signed", "saturating_sub_signed",
		"wrapping_add_signed",
	}
	usizeCountMethods = []string{"checked_shl", "checked_shr", "wrapping_shl", "wrapping_shr"}
	usizePowMethods   = []string{"checked_pow", "saturating_pow", "wrapping_pow"}
)

func TestNumUsizeAgainstRust(t *testing.T) {
	// A usize of the port is an int: 63 bits.
	usize := intType{name: "usize", bits: 63}
	generate := func(yield func(any)) {
		rng := rand.New(rand.NewPCG(0x7573697a, 0x65))
		emit := func(request numRequest) {
			request.Op = "usize63"
			yield(request)
		}
		for _, method := range usizeBinaryMethods {
			for _, pair := range usize.pairs(rng, 12, 8) {
				emit(numRequest{F: method, A: pair[0].String(), B: pair[1].String()})
			}
		}
		for _, method := range usizeUnaryMethods {
			for _, value := range usize.values(rng, 6) {
				emit(numRequest{F: method, A: value.String()})
			}
		}
		for _, method := range usizeMixedMethods {
			for _, value := range usize.values(rng, 4) {
				emit(numRequest{F: method, A: value.String(), S: isizeType.edges()[rng.IntN(len(isizeType.edges()))].String()})
				emit(numRequest{F: method, A: value.String(), S: isizeType.random(rng).String()})
			}
		}
		for _, method := range usizeCountMethods {
			for _, count := range usizeType.counts() {
				emit(numRequest{F: method, A: usize.edges()[rng.IntN(len(usize.edges()))].String(), N: count})
				emit(numRequest{F: method, A: usize.random(rng).String(), N: count})
			}
		}
		for _, method := range usizePowMethods {
			for _, c := range usize.powCases(rng) {
				emit(numRequest{F: method, A: c[0].String(), N: c[1].Int64()})
			}
		}
		for _, mode := range modes {
			for _, c := range usize.powCases(rng) {
				emit(numRequest{F: "pow", A: c[0].String(), N: c[1].Int64(), Mode: mode})
			}
			for _, value := range usize.values(rng, 4) {
				emit(numRequest{F: "next_power_of_two", A: value.String(), Mode: mode})
			}
			for _, pair := range usize.pairs(rng, 8, 6) {
				emit(numRequest{F: "next_multiple_of", A: pair[0].String(), B: pair[1].String(), Mode: mode})
			}
		}
	}
	oracle.Check(t, "std_num", "num_usize", generate, func(raw json.RawMessage) any {
		request := decodeNumRequest(t, raw)
		return oracle.Catch(func() any { return usizeMethod(request) })
	})
}

func usizeMethod(request numRequest) any {
	a, b, s := parse[int](request.A), parse[int](request.B), parse[int](request.S)
	n := uint32(request.N)
	switch request.F {
	case "checked_add":
		return opt(rs.UsizeCheckedAdd(a, b))
	case "checked_sub":
		return opt(rs.UsizeCheckedSub(a, b))
	case "checked_mul":
		return opt(rs.UsizeCheckedMul(a, b))
	case "checked_div":
		return opt(rs.UsizeCheckedDiv(a, b))
	case "checked_rem":
		return opt(rs.UsizeCheckedRem(a, b))
	case "checked_div_euclid":
		return opt(rs.UsizeCheckedDivEuclid(a, b))
	case "checked_rem_euclid":
		return opt(rs.UsizeCheckedRemEuclid(a, b))
	case "checked_neg":
		return opt(rs.UsizeCheckedNeg(a))
	case "checked_pow":
		return opt(rs.UsizeCheckedPow(a, n))
	case "checked_add_signed":
		return opt(rs.UsizeCheckedAddSigned(a, s))
	case "checked_sub_signed":
		return opt(rs.UsizeCheckedSubSigned(a, s))
	case "checked_next_power_of_two":
		return opt(rs.UsizeCheckedNextPowerOfTwo(a))
	case "checked_next_multiple_of":
		return opt(rs.UsizeCheckedNextMultipleOf(a, b))
	case "checked_ilog2":
		return opt(rs.UsizeCheckedIlog2(a))
	case "checked_ilog10":
		return opt(rs.UsizeCheckedIlog10(a))
	case "checked_ilog":
		return opt(rs.UsizeCheckedIlog(a, b))
	case "saturating_add":
		return val(rs.UsizeSaturatingAdd(a, b))
	case "saturating_sub":
		return val(rs.UsizeSaturatingSub(a, b))
	case "saturating_mul":
		return val(rs.UsizeSaturatingMul(a, b))
	case "saturating_pow":
		return val(rs.UsizeSaturatingPow(a, n))
	case "saturating_div":
		return val(rs.UsizeSaturatingDiv(a, b))
	case "saturating_add_signed":
		return val(rs.UsizeSaturatingAddSigned(a, s))
	case "saturating_sub_signed":
		return val(rs.UsizeSaturatingSubSigned(a, s))
	case "wrapping_add":
		return val(rs.UsizeWrappingAdd(a, b))
	case "wrapping_sub":
		return val(rs.UsizeWrappingSub(a, b))
	case "wrapping_mul":
		return val(rs.UsizeWrappingMul(a, b))
	case "wrapping_neg":
		return val(rs.UsizeWrappingNeg(a))
	case "wrapping_pow":
		return val(rs.UsizeWrappingPow(a, n))
	case "wrapping_shl":
		return val(rs.UsizeWrappingShl(a, n))
	case "wrapping_shr":
		return val(rs.UsizeWrappingShr(a, n))
	case "wrapping_add_signed":
		return val(rs.UsizeWrappingAddSigned(a, s))
	case "checked_shl":
		return opt(rs.UsizeCheckedShl(a, n))
	case "checked_shr":
		return opt(rs.UsizeCheckedShr(a, n))
	case "overflowing_add":
		return ovf(rs.UsizeOverflowingAdd(a, b))
	case "overflowing_sub":
		return ovf(rs.UsizeOverflowingSub(a, b))
	case "overflowing_mul":
		return ovf(rs.UsizeOverflowingMul(a, b))
	case "abs_diff":
		return val(rs.UsizeAbsDiff(a, b))
	case "isqrt":
		return val(rs.UsizeIsqrt(a))
	case "ilog2":
		return val(rs.UsizeIlog2(a))
	case "ilog10":
		return val(rs.UsizeIlog10(a))
	case "ilog":
		return val(rs.UsizeIlog(a, b))
	case "div_euclid":
		return val(rs.UsizeDivEuclid(a, b))
	case "rem_euclid":
		return val(rs.UsizeRemEuclid(a, b))
	case "div_ceil":
		return val(rs.UsizeDivCeil(a, b))
	case "is_multiple_of":
		return flag(rs.UsizeIsMultipleOf(a, b))
	case "is_power_of_two":
		return flag(rs.UsizeIsPowerOfTwo(a))
	case "pow":
		return modal(request.Mode,
			func() any { return val(rs.UsizePow(a, n)) },
			func() any {
				if power, ok := rs.UsizeCheckedPow(a, n).Get(); ok {
					return val(power)
				}
				return panicReply("attempt to exponentiate with overflow")
			},
			func() any {
				if power, ok := rs.UsizeCheckedPow(a, n).Get(); ok {
					return val(power)
				}
				return val(rs.UsizeWrappingPow(a, n))
			})
	case "next_power_of_two":
		return modal(request.Mode,
			func() any { return val(rs.UsizeNextPowerOfTwo(a)) },
			func() any {
				if power, ok := rs.UsizeCheckedNextPowerOfTwo(a).Get(); ok {
					return val(power)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any {
				if power, ok := rs.UsizeCheckedNextPowerOfTwo(a).Get(); ok {
					return val(power)
				}
				// 2^63, the pattern of the smallest int.
				return val(math.MinInt64)
			})
	case "next_multiple_of":
		return modal(request.Mode,
			func() any { return val(rs.UsizeNextMultipleOf(a, b)) },
			func() any {
				if b == 0 {
					return panicReply("attempt to calculate the remainder with a divisor of zero")
				}
				if multiple, ok := rs.UsizeCheckedNextMultipleOf(a, b).Get(); ok {
					return val(multiple)
				}
				return panicReply("attempt to add with overflow")
			},
			func() any {
				if b == 0 {
					return panicReply("attempt to calculate the remainder with a divisor of zero")
				}
				if a%b == 0 {
					return val(a)
				}
				return val(a + (b - a%b))
			})
	}
	return unknownRequest(request)
}

// ---------------------------------------------------------------------------
// Conversions: try_from, bytes, casts (parsing is strs's)

func TestNumConvertAgainstRust(t *testing.T) {
	generate := func(yield func(any)) {
		rng := rand.New(rand.NewPCG(0x636f6e76, 0x657274))
		// try_from, from every type to every type, on the values around the
		// bounds of both.
		for _, from := range allIntTypes {
			for _, to := range allIntTypes {
				candidates := []*big.Int{big.NewInt(0), big.NewInt(1), big.NewInt(-1), from.min(), from.max(),
					to.min(), to.max(), new(big.Int).Add(to.max(), big.NewInt(1)),
					new(big.Int).Sub(to.min(), big.NewInt(1)), from.random(rng)}
				seen := map[string]bool{}
				for _, candidate := range candidates {
					if !from.holds(candidate) || seen[candidate.String()] {
						continue
					}
					// A usize above isize::MAX has no int to hold it.
					if (from.name == "usize" || to.name == "usize") && candidate.Cmp(isizeType.max()) > 0 {
						continue
					}
					seen[candidate.String()] = true
					yield(numRequest{Op: "try_from", From: from.name, To: to.name, A: candidate.String()})
				}
			}
		}
		// The byte conversions.
		for _, t := range allIntTypes {
			for _, value := range []*big.Int{big.NewInt(0), big.NewInt(1), t.max(), t.min(), t.random(rng), t.random(rng)} {
				if t.name == "usize" && value.Cmp(isizeType.max()) > 0 {
					continue
				}
				for _, order := range []string{"le", "be", "ne"} {
					yield(numRequest{Op: "bytes", Ty: t.name, F: "to_" + order, A: value.String()})
				}
			}
			for index := 0; index < 3; index++ {
				bytes := make([]byte, t.bits/8)
				for position := range bytes {
					bytes[position] = byte(rng.IntN(256))
				}
				if t.name == "usize" {
					// Keep the value an int can hold in every byte order.
					bytes[0] &= 0x7f
					bytes[len(bytes)-1] &= 0x7f
				}
				for _, order := range []string{"le", "be", "ne"} {
					yield(numRequest{Op: "bytes", Ty: t.name, F: "from_" + order, Hex: fmt.Sprintf("%x", bytes)})
				}
			}
		}
		for _, float := range []struct {
			name string
			bits []uint64
		}{{"f32", f32Samples(rng, 4)}, {"f64", f64Samples(rng, 4)}} {
			for _, bits := range float.bits[:12] {
				for _, order := range []string{"le", "be", "ne"} {
					yield(numRequest{Op: "bytes", Ty: float.name, F: "to_" + order, A: strconv.FormatUint(bits, 10)})
					width := 16
					if float.name == "f32" {
						width = 8
					}
					yield(numRequest{Op: "bytes", Ty: float.name, F: "from_" + order, Hex: fmt.Sprintf("%0*x", width, bits)})
				}
			}
		}
		// Float to integer: the bounds of each integer type and the floats
		// on either side of them, NaN, the infinities, and fractions.
		for _, from := range []string{"f32", "f64"} {
			for _, to := range allIntTypes {
				for _, bits := range castSamples(from, to, rng) {
					yield(numRequest{Op: "float_to_int", From: from, To: to.name, Bits: strconv.FormatUint(bits, 10)})
				}
			}
		}
		// Integer to float: values that are not representable and must
		// round.
		for _, from := range allIntTypes {
			for _, to := range []string{"f32", "f64"} {
				values := append(from.edges(), from.random(rng), from.random(rng), from.random(rng))
				for _, shift := range []uint{24, 25, 53, 54, 63, 64, 100, 126} {
					for _, delta := range []int64{-1, 0, 1} {
						// Halfway cases: 2^shift + 2^(shift-precision) + delta.
						for _, precision := range []uint{24, 53} {
							if shift < precision {
								continue
							}
							value := new(big.Int).Lsh(big.NewInt(1), shift)
							value.Add(value, new(big.Int).Lsh(big.NewInt(1), shift-precision))
							value.Add(value, big.NewInt(delta))
							values = append(values, value, new(big.Int).Neg(value))
							value3 := new(big.Int).Lsh(big.NewInt(3), shift-1)
							value3.Add(value3, new(big.Int).Lsh(big.NewInt(1), shift-precision))
							value3.Add(value3, big.NewInt(delta))
							values = append(values, value3)
						}
					}
				}
				seen := map[string]bool{}
				for _, value := range values {
					if !from.holds(value) || seen[value.String()] {
						continue
					}
					if from.name == "usize" && value.Cmp(isizeType.max()) > 0 {
						continue
					}
					seen[value.String()] = true
					yield(numRequest{Op: "int_to_float", From: from.name, To: to, A: value.String()})
				}
			}
		}
		for _, bits := range f32Samples(rng, 20) {
			yield(numRequest{Op: "float_to_float", From: "f32", Bits: strconv.FormatUint(bits, 10)})
		}
		for _, bits := range f64Samples(rng, 40) {
			yield(numRequest{Op: "float_to_float", From: "f64", Bits: strconv.FormatUint(bits, 10)})
		}
		// Doubles on the boundaries of f32: halfway between two f32 values,
		// around f32::MAX and in the subnormal range.
		for _, base := range []float64{1, 16777216, math.MaxFloat32, 1e-40, 1.401298464324817e-45, 0.1} {
			single := float32(base)
			next := math.Nextafter32(single, float32(math.Inf(1)))
			middle := (float64(single) + float64(next)) / 2
			for _, value := range []float64{middle, math.Nextafter(middle, 0), math.Nextafter(middle, math.Inf(1)),
				-middle, float64(single), float64(next)} {
				yield(numRequest{Op: "float_to_float", From: "f64", Bits: strconv.FormatUint(math.Float64bits(value), 10)})
			}
		}
		yield(numRequest{Op: "consts"})
	}
	oracle.Check(t, "std_num", "num_convert", generate, func(raw json.RawMessage) any {
		request := decodeNumRequest(t, raw)
		return oracle.Catch(func() any { return answerConvert(request) })
	})
}

// f64Samples are the bits of doubles of every kind: zeros, subnormals, the
// bounds, infinities, NaNs, integers, halves, and random ones.
func f64Samples(rng *rand.Rand, random int) []uint64 {
	values := []float64{0, math.Copysign(0, -1), 1, -1, 0.5, -0.5, 1.5, -1.5, 2.5, -2.5, 0.1, -0.1, 0.49999999999999994,
		3, 1e-320, 5e-324, 2.2250738585072014e-308, math.MaxFloat64, -math.MaxFloat64, math.Inf(1), math.Inf(-1),
		math.NaN(), 1e16, 9999999999999998, 1e-4, 9.999e-5, 1e15, 123456.789, 4503599627370496.5, 4503599627370497,
		9007199254740993, 255, 256, 65535.5, 1e300, 1e-300, 0.30000000000000004, 100, 1000, 1e21, 1e22, 1e23}
	var samples []uint64
	for _, value := range values {
		samples = append(samples, math.Float64bits(value))
	}
	samples = append(samples, 0xfff8000000000000, 0x7ff0000000000001, 0x8000000000000001)
	for index := 0; index < random; index++ {
		samples = append(samples, rng.Uint64())
		// A random double of moderate size, which is what code computes with.
		samples = append(samples, math.Float64bits((rng.Float64()-0.5)*math.Ldexp(1, rng.IntN(80)-20)))
	}
	return samples
}

func f32Samples(rng *rand.Rand, random int) []uint64 {
	values := []float32{0, float32(math.Copysign(0, -1)), 1, -1, 0.5, -0.5, 1.5, -1.5, 2.5, -2.5, 0.1, -0.1,
		0.49999997, 3, 1e-40, 1e-45, 1.17549435e-38, math.MaxFloat32, -math.MaxFloat32, float32(math.Inf(1)),
		float32(math.Inf(-1)), float32(math.NaN()), 1e16, 1e-4, 9.999e-5, 1e15, 123456.79, 8388608.5, 8388609,
		16777217, 255, 256, 65535.5, 1e30, 1e-30, 100, 1000}
	var samples []uint64
	for _, value := range values {
		samples = append(samples, uint64(math.Float32bits(value)))
	}
	samples = append(samples, 0xffc00000, 0x7f800001, 0x80000001)
	for index := 0; index < random; index++ {
		samples = append(samples, uint64(rng.Uint32()))
		samples = append(samples, uint64(math.Float32bits(float32((rng.Float64()-0.5)*math.Ldexp(1, rng.IntN(60)-15)))))
	}
	return samples
}

// castSamples are the floats a cast to the integer type must get right: the
// type's bounds, the representable floats on either side of them, and the
// usual specials.
func castSamples(from string, to intType, rng *rand.Rand) []uint64 {
	var values []float64
	if to.name == "usize" {
		// The port's usize stops at isize::MAX, so the oracle is asked
		// only about what both hold.
		to = intType{name: "usize", bits: 63}
	}
	for _, bound := range []*big.Int{to.min(), to.max(), new(big.Int).Add(to.max(), big.NewInt(1)), big.NewInt(0)} {
		exact, _ := new(big.Float).SetInt(bound).Float64()
		values = append(values, exact, exact+0.5, exact-0.5, exact+1, exact-1, exact*2,
			math.Nextafter(exact, math.Inf(1)), math.Nextafter(exact, math.Inf(-1)))
	}
	values = append(values, math.NaN(), math.Inf(1), math.Inf(-1), math.Copysign(0, -1), 0.99999, -0.99999, 1.5, -1.5,
		2.5, 255.9, 256, -128.9, -129, 1e10, -1e10, 1e19, -1e19, 1.8446744073709552e19, 9.223372036854776e18,
		3.4028236692093846e38, 1.7014118346046923e38, -1.7014118346046923e38, 1e39, -1e39, 1e300, -1e300)
	for index := 0; index < 4; index++ {
		values = append(values, (rng.Float64()-0.5)*math.Ldexp(1, rng.IntN(int(to.bits)+4)))
	}
	seen := map[uint64]bool{}
	var samples []uint64
	for _, value := range values {
		var bits uint64
		if from == "f32" {
			single := float32(value)
			bits = uint64(math.Float32bits(single))
			value = float64(single)
		} else {
			bits = math.Float64bits(value)
		}
		// Above isize::MAX the port's usize saturates where Rust's does
		// not; that is documented and tested apart.
		if to.name == "usize" && value >= 0x1p63 {
			continue
		}
		if !seen[bits] {
			seen[bits] = true
			samples = append(samples, bits)
		}
	}
	return samples
}

func tryFromError(err error) reply {
	conversion, ok := err.(rs.TryFromIntError)
	if !ok {
		return reply{"go_error": fmt.Sprintf("error is a %T, not a TryFromIntError", err)}
	}
	return reply{"err": conversion.Error(), "debug": rs.DebugString(conversion)}
}

func tryFromReply[T any](value T, err error) reply {
	if err != nil {
		return tryFromError(err)
	}
	return val(value)
}

// tryFrom converts a source value of any integer type to the named type.
func tryFrom[S anyInteger](value S, to string) reply {
	switch to {
	case "u8":
		return tryFromReply(rs.U8TryFrom(value))
	case "u16":
		return tryFromReply(rs.U16TryFrom(value))
	case "u32":
		return tryFromReply(rs.U32TryFrom(value))
	case "u64":
		return tryFromReply(rs.U64TryFrom(value))
	case "usize":
		return tryFromReply(rs.UsizeTryFrom(value))
	case "i8":
		return tryFromReply(rs.I8TryFrom(value))
	case "i16":
		return tryFromReply(rs.I16TryFrom(value))
	case "i32":
		return tryFromReply(rs.I32TryFrom(value))
	case "i64":
		return tryFromReply(rs.I64TryFrom(value))
	case "isize":
		return tryFromReply(rs.IsizeTryFrom(value))
	case "u128":
		converted, err := rs.U128TryFrom(value)
		if err != nil {
			return tryFromError(err)
		}
		return valU128(converted)
	case "i128":
		return valI128(rs.CastToI128(value))
	}
	return reply{"go_error": "unknown target " + to}
}

func tryFromWide[T rs.FixedInt](value any) reply {
	switch v := value.(type) {
	case rs.U128:
		return tryFromReply(rs.TryFromU128[T](v))
	case rs.I128:
		return tryFromReply(rs.TryFromI128[T](v))
	}
	panic("not a wide integer")
}

func answerTryFrom(request numRequest) any {
	switch request.From {
	case "u8":
		return tryFrom(parse[uint8](request.A), request.To)
	case "u16":
		return tryFrom(parse[uint16](request.A), request.To)
	case "u32":
		return tryFrom(parse[uint32](request.A), request.To)
	case "u64":
		return tryFrom(parse[uint64](request.A), request.To)
	case "usize", "isize":
		return tryFrom(parse[int](request.A), request.To)
	case "i8":
		return tryFrom(parse[int8](request.A), request.To)
	case "i16":
		return tryFrom(parse[int16](request.A), request.To)
	case "i32":
		return tryFrom(parse[int32](request.A), request.To)
	case "i64":
		return tryFrom(parse[int64](request.A), request.To)
	}
	var value any
	if request.From == "u128" {
		value = parseU128(request.A)
	} else {
		value = parseI128(request.A)
	}
	switch request.To {
	case "u8":
		return tryFromWide[uint8](value)
	case "u16":
		return tryFromWide[uint16](value)
	case "u32":
		return tryFromWide[uint32](value)
	case "u64":
		return tryFromWide[uint64](value)
	case "i8":
		return tryFromWide[int8](value)
	case "i16":
		return tryFromWide[int16](value)
	case "i32":
		return tryFromWide[int32](value)
	case "i64":
		return tryFromWide[int64](value)
	case "usize":
		if v, ok := value.(rs.U128); ok {
			return tryFromReply(rs.UsizeTryFromU128(v))
		}
		return tryFromReply(rs.UsizeTryFromI128(value.(rs.I128)))
	case "isize":
		if v, ok := value.(rs.U128); ok {
			return tryFromReply(rs.IsizeTryFromU128(v))
		}
		return tryFromReply(rs.IsizeTryFromI128(value.(rs.I128)))
	case "u128":
		if v, ok := value.(rs.U128); ok {
			return valU128(v)
		}
		converted, err := rs.U128TryFromI128(value.(rs.I128))
		if err != nil {
			return tryFromError(err)
		}
		return valU128(converted)
	case "i128":
		if v, ok := value.(rs.I128); ok {
			return valI128(v)
		}
		converted, err := rs.I128TryFromU128(value.(rs.U128))
		if err != nil {
			return tryFromError(err)
		}
		return valI128(converted)
	}
	return unknownRequest(request)
}

func sprint[T any](v T) string { return fmt.Sprint(v) }

func hexBytes(bytes []byte) reply { return reply{"hex": fmt.Sprintf("%x", bytes)} }

func unhex(text string) []byte {
	bytes := make([]byte, len(text)/2)
	for index := range bytes {
		value, err := strconv.ParseUint(text[index*2:index*2+2], 16, 8)
		if err != nil {
			panic(err)
		}
		bytes[index] = byte(value)
	}
	return bytes
}

// bytesOf answers the six byte conversions of one type from its functions.
func bytesOf[T any, A interface {
	~[1]byte | ~[2]byte | ~[4]byte | ~[8]byte | ~[16]byte
}](
	request numRequest, value func(string) T, text func(T) string,
	toLe, toBe, toNe func(T) A, fromLe, fromBe, fromNe func(A) T, slice func(A) []byte, array func([]byte) A,
) any {
	switch request.F {
	case "to_le":
		return hexBytes(slice(toLe(value(request.A))))
	case "to_be":
		return hexBytes(slice(toBe(value(request.A))))
	case "to_ne":
		return hexBytes(slice(toNe(value(request.A))))
	case "from_le":
		return reply{"r": text(fromLe(array(unhex(request.Hex))))}
	case "from_be":
		return reply{"r": text(fromBe(array(unhex(request.Hex))))}
	case "from_ne":
		return reply{"r": text(fromNe(array(unhex(request.Hex))))}
	}
	return unknownRequest(request)
}

func slice1(a [1]byte) []byte   { return a[:] }
func slice2(a [2]byte) []byte   { return a[:] }
func slice4(a [4]byte) []byte   { return a[:] }
func slice8(a [8]byte) []byte   { return a[:] }
func slice16(a [16]byte) []byte { return a[:] }
func array1(b []byte) [1]byte   { return [1]byte(b) }
func array2(b []byte) [2]byte   { return [2]byte(b) }
func array4(b []byte) [4]byte   { return [4]byte(b) }
func array8(b []byte) [8]byte   { return [8]byte(b) }
func array16(b []byte) [16]byte { return [16]byte(b) }

func answerBytes(request numRequest) any {
	switch request.Ty {
	case "u8":
		return bytesOf(request, parse[uint8], sprint, rs.U8ToLeBytes, rs.U8ToBeBytes, rs.U8ToNeBytes,
			rs.U8FromLeBytes, rs.U8FromBeBytes, rs.U8FromNeBytes, slice1, array1)
	case "u16":
		return bytesOf(request, parse[uint16], sprint, rs.U16ToLeBytes, rs.U16ToBeBytes, rs.U16ToNeBytes,
			rs.U16FromLeBytes, rs.U16FromBeBytes, rs.U16FromNeBytes, slice2, array2)
	case "u32":
		return bytesOf(request, parse[uint32], sprint, rs.U32ToLeBytes, rs.U32ToBeBytes, rs.U32ToNeBytes,
			rs.U32FromLeBytes, rs.U32FromBeBytes, rs.U32FromNeBytes, slice4, array4)
	case "u64":
		return bytesOf(request, parse[uint64], sprint, rs.U64ToLeBytes, rs.U64ToBeBytes, rs.U64ToNeBytes,
			rs.U64FromLeBytes, rs.U64FromBeBytes, rs.U64FromNeBytes, slice8, array8)
	case "usize":
		return bytesOf(request, parse[int], sprint, rs.UsizeToLeBytes, rs.UsizeToBeBytes, rs.UsizeToNeBytes,
			rs.UsizeFromLeBytes, rs.UsizeFromBeBytes, rs.UsizeFromNeBytes, slice8, array8)
	case "i8":
		return bytesOf(request, parse[int8], sprint, rs.I8ToLeBytes, rs.I8ToBeBytes, rs.I8ToNeBytes,
			rs.I8FromLeBytes, rs.I8FromBeBytes, rs.I8FromNeBytes, slice1, array1)
	case "i16":
		return bytesOf(request, parse[int16], sprint, rs.I16ToLeBytes, rs.I16ToBeBytes, rs.I16ToNeBytes,
			rs.I16FromLeBytes, rs.I16FromBeBytes, rs.I16FromNeBytes, slice2, array2)
	case "i32":
		return bytesOf(request, parse[int32], sprint, rs.I32ToLeBytes, rs.I32ToBeBytes, rs.I32ToNeBytes,
			rs.I32FromLeBytes, rs.I32FromBeBytes, rs.I32FromNeBytes, slice4, array4)
	case "i64":
		return bytesOf(request, parse[int64], sprint, rs.I64ToLeBytes, rs.I64ToBeBytes, rs.I64ToNeBytes,
			rs.I64FromLeBytes, rs.I64FromBeBytes, rs.I64FromNeBytes, slice8, array8)
	case "isize":
		return bytesOf(request, parse[int], sprint, rs.IsizeToLeBytes, rs.IsizeToBeBytes, rs.IsizeToNeBytes,
			rs.IsizeFromLeBytes, rs.IsizeFromBeBytes, rs.IsizeFromNeBytes, slice8, array8)
	case "u128":
		return bytesOf(request, parseU128, func(x rs.U128) string { return bigU128(x).String() },
			rs.U128.ToLeBytes, rs.U128.ToBeBytes, rs.U128.ToNeBytes,
			rs.U128FromLeBytes, rs.U128FromBeBytes, rs.U128FromNeBytes, slice16, array16)
	case "i128":
		return bytesOf(request, parseI128, func(x rs.I128) string { return bigI128(x).String() },
			rs.I128.ToLeBytes, rs.I128.ToBeBytes, rs.I128.ToNeBytes,
			rs.I128FromLeBytes, rs.I128FromBeBytes, rs.I128FromNeBytes, slice16, array16)
	case "f32":
		return bytesOf(request,
			func(text string) float32 { return math.Float32frombits(parse[uint32](text)) },
			func(x float32) string { return fmt.Sprint(math.Float32bits(x)) },
			rs.F32ToLeBytes, rs.F32ToBeBytes, rs.F32ToNeBytes,
			rs.F32FromLeBytes, rs.F32FromBeBytes, rs.F32FromNeBytes, slice4, array4)
	case "f64":
		return bytesOf(request,
			func(text string) float64 { return math.Float64frombits(parse[uint64](text)) },
			func(x float64) string { return fmt.Sprint(math.Float64bits(x)) },
			rs.F64ToLeBytes, rs.F64ToBeBytes, rs.F64ToNeBytes,
			rs.F64FromLeBytes, rs.F64FromBeBytes, rs.F64FromNeBytes, slice8, array8)
	}
	return unknownRequest(request)
}

func f32Of(bits string) float32 { return math.Float32frombits(parse[uint32](bits)) }

func f64Of(bits string) float64 { return math.Float64frombits(parse[uint64](bits)) }

func f32Bits(value float32) reply {
	if value != value {
		return reply{"bits": "nan"}
	}
	return reply{"bits": fmt.Sprint(math.Float32bits(value))}
}

func f64Bits(value float64) reply {
	if value != value {
		return reply{"bits": "nan"}
	}
	return reply{"bits": fmt.Sprint(math.Float64bits(value))}
}

func answerFloatToInt(request numRequest) any {
	if request.From == "f32" {
		x := f32Of(request.Bits)
		switch request.To {
		case "u8":
			return val(rs.CastF32ToU8(x))
		case "u16":
			return val(rs.CastF32ToU16(x))
		case "u32":
			return val(rs.CastF32ToU32(x))
		case "u64":
			return val(rs.CastF32ToU64(x))
		case "usize":
			return val(rs.CastF32ToUsize(x))
		case "u128":
			return valU128(rs.CastF32ToU128(x))
		case "i8":
			return val(rs.CastF32ToI8(x))
		case "i16":
			return val(rs.CastF32ToI16(x))
		case "i32":
			return val(rs.CastF32ToI32(x))
		case "i64":
			return val(rs.CastF32ToI64(x))
		case "isize":
			return val(rs.CastF32ToIsize(x))
		case "i128":
			return valI128(rs.CastF32ToI128(x))
		}
		return unknownRequest(request)
	}
	x := f64Of(request.Bits)
	switch request.To {
	case "u8":
		return val(rs.CastF64ToU8(x))
	case "u16":
		return val(rs.CastF64ToU16(x))
	case "u32":
		return val(rs.CastF64ToU32(x))
	case "u64":
		return val(rs.CastF64ToU64(x))
	case "usize":
		return val(rs.CastF64ToUsize(x))
	case "u128":
		return valU128(rs.CastF64ToU128(x))
	case "i8":
		return val(rs.CastF64ToI8(x))
	case "i16":
		return val(rs.CastF64ToI16(x))
	case "i32":
		return val(rs.CastF64ToI32(x))
	case "i64":
		return val(rs.CastF64ToI64(x))
	case "isize":
		return val(rs.CastF64ToIsize(x))
	case "i128":
		return valI128(rs.CastF64ToI128(x))
	}
	return unknownRequest(request)
}

// intToFloat pins what num_convert.go says of Go's own conversions: an
// integer converted to a float rounds as Rust's `as` does.
func intToFloat[T anyInteger](request numRequest) any {
	value := parse[T](request.A)
	if request.To == "f32" {
		return f32Bits(float32(value))
	}
	return f64Bits(float64(value))
}

func answerIntToFloat(request numRequest) any {
	switch request.From {
	case "u8":
		return intToFloat[uint8](request)
	case "u16":
		return intToFloat[uint16](request)
	case "u32":
		return intToFloat[uint32](request)
	case "u64":
		return intToFloat[uint64](request)
	case "usize", "isize":
		return intToFloat[int](request)
	case "i8":
		return intToFloat[int8](request)
	case "i16":
		return intToFloat[int16](request)
	case "i32":
		return intToFloat[int32](request)
	case "i64":
		return intToFloat[int64](request)
	case "u128":
		if request.To == "f32" {
			return f32Bits(rs.CastU128ToF32(parseU128(request.A)))
		}
		return f64Bits(rs.CastU128ToF64(parseU128(request.A)))
	case "i128":
		if request.To == "f32" {
			return f32Bits(rs.CastI128ToF32(parseI128(request.A)))
		}
		return f64Bits(rs.CastI128ToF64(parseI128(request.A)))
	}
	return unknownRequest(request)
}

func answerConsts() any {
	bits64 := func(x float64) string { return fmt.Sprint(math.Float64bits(x)) }
	bits32 := func(x float32) string { return fmt.Sprint(math.Float32bits(x)) }
	return reply{
		"f64::NAN":               bits64(rs.F64Nan),
		"f32::NAN":               bits32(rs.F32Nan),
		"f64::EPSILON":           bits64(rs.F64Epsilon),
		"f32::EPSILON":           bits32(rs.F32Epsilon),
		"f64::MIN_POSITIVE":      bits64(rs.F64MinPositive),
		"f32::MIN_POSITIVE":      bits32(rs.F32MinPositive),
		"f64::MAX":               bits64(math.MaxFloat64),
		"f64::MIN":               bits64(-math.MaxFloat64),
		"f32::MAX":               bits32(math.MaxFloat32),
		"f32::MIN":               bits32(-math.MaxFloat32),
		"f64::INFINITY":          bits64(rs.F64Infinity),
		"f64::NEG_INFINITY":      bits64(rs.F64NegInfinity),
		"f32::INFINITY":          bits32(rs.F32Infinity),
		"f32::NEG_INFINITY":      bits32(rs.F32NegInfinity),
		"isize::MAX":             fmt.Sprint(rs.IsizeMax),
		"isize::MIN":             fmt.Sprint(rs.IsizeMin),
		"u128::MAX":              bigU128(rs.U128Max).String(),
		"i128::MAX":              bigI128(rs.I128Max).String(),
		"i128::MIN":              bigI128(rs.I128Min).String(),
		"NonZeroUsize::MIN":      rs.NonZeroUsizeMin.String(),
		"f32::consts::PI":        bits32(math.Pi),
		"f32::consts::TAU":       bits32(2 * math.Pi),
		"f32::consts::FRAC_PI_2": bits32(math.Pi / 2),
		"f64::consts::PI":        bits64(math.Pi),
		"f64::consts::TAU":       bits64(2 * math.Pi),
	}
}

func answerConvert(request numRequest) any {
	switch request.Op {
	case "try_from":
		return answerTryFrom(request)
	case "bytes":
		return answerBytes(request)
	case "float_to_int":
		return answerFloatToInt(request)
	case "int_to_float":
		return answerIntToFloat(request)
	case "float_to_float":
		if request.From == "f32" {
			return f64Bits(float64(f32Of(request.Bits)))
		}
		return f32Bits(float32(f64Of(request.Bits)))
	case "consts":
		return answerConsts()
	}
	return unknownRequest(request)
}

// ---------------------------------------------------------------------------
// Float methods whose results are exact

var (
	floatUnary = []string{"signum", "fract", "is_nan", "is_infinite", "is_finite", "is_sign_negative",
		"is_sign_positive", "recip", "to_degrees", "to_radians", "round", "round_ties_even", "floor", "ceil",
		"trunc", "abs", "sqrt", "debug"}
	floatBinary = []string{"max", "min", "rem", "rem_euclid", "div_euclid", "total_cmp", "copysign"}
)

func TestNumFloatAgainstRust(t *testing.T) {
	generate := func(yield func(any)) {
		rng := rand.New(rand.NewPCG(0x666c6f61, 0x74))
		for _, ty := range []string{"f32", "f64"} {
			samples := func(random int) []uint64 {
				if ty == "f32" {
					return f32Samples(rng, random)
				}
				return f64Samples(rng, random)
			}
			emit := func(request numRequest) {
				request.Op, request.Ty = "float", ty
				yield(request)
			}
			text := func(bits uint64) string { return strconv.FormatUint(bits, 10) }
			for _, method := range floatUnary {
				for _, bits := range samples(8) {
					emit(numRequest{F: method, A: text(bits)})
				}
			}
			// Halfway cases for the rounding methods.
			for _, method := range []string{"round", "round_ties_even", "floor", "ceil", "trunc", "fract"} {
				for _, value := range []float64{0.5, 1.5, 2.5, 3.5, -0.5, -1.5, -2.5, 0.49999999999999994, 0.4999999701976776,
					8388607.5, 8388608.5, 4503599627370495.5, -4503599627370495.5, 1e30, -1e30, 0.9999999, -0.9999999} {
					emit(numRequest{F: method, A: text(floatBitsOf(ty, value))})
				}
			}
			all := samples(6)
			for _, method := range floatBinary {
				for index := 0; index < 60; index++ {
					emit(numRequest{F: method, A: text(all[rng.IntN(len(all))]), B: text(all[rng.IntN(len(all))])})
				}
				// Zeros and NaNs against everything: the cases where Rust
				// and Go's own max, min and remainder part ways.
				for _, special := range []float64{0, math.Copysign(0, -1), math.NaN(), math.Inf(1), math.Inf(-1), 1, -1} {
					for _, other := range []float64{0, math.Copysign(0, -1), math.NaN(), math.Inf(1), math.Inf(-1), 2.5, -2.5} {
						emit(numRequest{F: method, A: text(floatBitsOf(ty, special)), B: text(floatBitsOf(ty, other))})
					}
				}
			}
			for _, method := range []string{"rem", "rem_euclid", "div_euclid"} {
				for index := 0; index < 40; index++ {
					a := (rng.Float64() - 0.5) * math.Ldexp(1, rng.IntN(40))
					b := (rng.Float64() - 0.5) * math.Ldexp(1, rng.IntN(20))
					emit(numRequest{F: method, A: text(floatBitsOf(ty, a)), B: text(floatBitsOf(ty, b))})
				}
				for _, pair := range [][2]float64{{7, 4}, {-7, 4}, {7, -4}, {-7, -4}, {5.5, 2}, {-5.5, 2}, {1e300, 3},
					{-1e-300, 3}, {-0.0000001, 1}, {5, math.Inf(1)}, {-5, math.Inf(1)}, {-5, math.Inf(-1)}, {4, 2}, {-4, 2}} {
					emit(numRequest{F: method, A: text(floatBitsOf(ty, pair[0])), B: text(floatBitsOf(ty, pair[1]))})
				}
			}
			// clamp, in order and out of order, with NaN bounds.
			for index := 0; index < 60; index++ {
				a, b, c := all[rng.IntN(len(all))], all[rng.IntN(len(all))], all[rng.IntN(len(all))]
				emit(numRequest{F: "clamp", A: text(a), B: text(b), C: text(c)})
			}
			for _, bounds := range [][3]float64{{0, 0, 1}, {-0.0, 0, 1}, {math.Copysign(0, -1), 0, 1}, {2, 0, 1},
				{math.NaN(), 0, 1}, {0.5, math.NaN(), 1}, {0.5, 0, math.NaN()}, {0.5, 1, 0}, {0, math.Copysign(0, -1), 0},
				{0.5, 1e16, 1e-5}, {0.5, 123456.7, -1.5e-7}, {1, math.Inf(1), math.Inf(-1)}, {1, 1e15, 1e-4},
				{1, 16777216, 0.1}, {1, 1.5e300, 3}, {1, 100, 1}, {1, 1e21, 9.999e-5}, {1, 12345678.9, 0.000123}} {
				emit(numRequest{F: "clamp", A: text(floatBitsOf(ty, bounds[0])), B: text(floatBitsOf(ty, bounds[1])),
					C: text(floatBitsOf(ty, bounds[2]))})
			}
			// powi.
			for index := 0; index < 50; index++ {
				base := (rng.Float64() - 0.5) * math.Ldexp(1, rng.IntN(12))
				emit(numRequest{F: "powi", A: text(floatBitsOf(ty, base)), N: int64(rng.IntN(41) - 20)})
			}
			for _, base := range []float64{0, math.Copysign(0, -1), 1, -1, 2, 10, 0.1, 1.0000001, math.Inf(1), math.Inf(-1), math.NaN(), 3, -3, 1.7} {
				for _, n := range []int64{0, 1, -1, 2, -2, 3, 5, 7, 31, 64, 100, -100, 1000, math.MaxInt32, math.MinInt32} {
					emit(numRequest{F: "powi", A: text(floatBitsOf(ty, base)), N: n})
				}
			}
			// mul_add, including sums that land halfway between two
			// results.
			for index := 0; index < 80; index++ {
				a, b, c := all[rng.IntN(len(all))], all[rng.IntN(len(all))], all[rng.IntN(len(all))]
				emit(numRequest{F: "mul_add", A: text(a), B: text(b), C: text(c)})
			}
			for index := 0; index < 200; index++ {
				a := (rng.Float64() - 0.5) * math.Ldexp(1, rng.IntN(30))
				b := (rng.Float64() - 0.5) * math.Ldexp(1, rng.IntN(30))
				c := -a * b * (1 + (rng.Float64()-0.5)*math.Ldexp(1, -rng.IntN(60)))
				if index%3 == 0 {
					c = (rng.Float64() - 0.5) * math.Ldexp(1, rng.IntN(60)-30)
				}
				emit(numRequest{F: "mul_add", A: text(floatBitsOf(ty, a)), B: text(floatBitsOf(ty, b)), C: text(floatBitsOf(ty, c))})
			}
			if ty == "f32" {
				// Products whose sum with c is a tie in float32 that
				// rounding through float64 would break the wrong way.
				for index := 0; index < 200; index++ {
					a := float32(1 + rng.Float64())
					b := float32(1 + rng.Float64())
					product := float64(a) * float64(b)
					single := float32(product)
					next := math.Nextafter32(single, float32(math.Inf(1)))
					middle := (float64(single) + float64(next)) / 2
					// c brings the exact sum next to the middle of two f32
					// values, a little above or below it.
					c := float32(middle - product)
					for _, nudge := range []float32{c, math.Nextafter32(c, 1), math.Nextafter32(c, -1), float32(math.Ldexp(1, -40-rng.IntN(20)))} {
						emit(numRequest{F: "mul_add", A: text(floatBitsOf(ty, float64(a))), B: text(floatBitsOf(ty, float64(b))),
							C: text(uint64(math.Float32bits(nudge)))})
					}
				}
			}
		}
	}
	oracle.Check(t, "std_num", "num_float", generate, func(raw json.RawMessage) any {
		request := decodeNumRequest(t, raw)
		return oracle.Catch(func() any {
			if request.Ty == "f32" {
				return f32Method(request)
			}
			return f64Method(request)
		})
	})
}

func floatBitsOf(ty string, value float64) uint64 {
	if ty == "f32" {
		return uint64(math.Float32bits(float32(value)))
	}
	return math.Float64bits(value)
}

func f64Method(request numRequest) any {
	a, b, c := f64Of(request.A), f64Of(request.B), f64Of(request.C)
	n := int32(request.N)
	switch request.F {
	case "max":
		return f64Bits(rs.F64Max(a, b))
	case "min":
		return f64Bits(rs.F64Min(a, b))
	case "clamp":
		return f64Bits(rs.F64Clamp(a, b, c))
	case "signum":
		return f64Bits(rs.F64Signum(a))
	case "fract":
		return f64Bits(rs.F64Fract(a))
	case "rem":
		return f64Bits(math.Mod(a, b))
	case "rem_euclid":
		return f64Bits(rs.F64RemEuclid(a, b))
	case "div_euclid":
		return f64Bits(rs.F64DivEuclid(a, b))
	case "total_cmp":
		return reply{"r": int(rs.F64TotalCmp(a, b))}
	case "powi":
		return f64Bits(rs.F64Powi(a, n))
	case "is_nan":
		return flag(rs.F64IsNan(a))
	case "is_infinite":
		return flag(rs.F64IsInfinite(a))
	case "is_finite":
		return flag(rs.F64IsFinite(a))
	case "is_sign_negative":
		return flag(rs.F64IsSignNegative(a))
	case "is_sign_positive":
		return flag(rs.F64IsSignPositive(a))
	case "copysign":
		return f64Bits(rs.F64Copysign(a, b))
	case "recip":
		return f64Bits(rs.F64Recip(a))
	case "to_degrees":
		return f64Bits(rs.F64ToDegrees(a))
	case "to_radians":
		return f64Bits(rs.F64ToRadians(a))
	case "round":
		return f64Bits(rs.F64Round(a))
	case "round_ties_even":
		return f64Bits(rs.F64RoundTiesEven(a))
	case "floor":
		return f64Bits(rs.F64Floor(a))
	case "ceil":
		return f64Bits(rs.F64Ceil(a))
	case "trunc":
		return f64Bits(rs.F64Trunc(a))
	case "abs":
		return f64Bits(rs.F64Abs(a))
	case "sqrt":
		return f64Bits(rs.F64Sqrt(a))
	case "mul_add":
		return f64Bits(rs.F64MulAdd(a, b, c))
	case "debug":
		// The Debug form of a float is what F64Clamp's panic prints.
		return reply{"r": clampDebug(func() { rs.F64Clamp(0, a, math.NaN()) })}
	}
	return unknownRequest(request)
}

func f32Method(request numRequest) any {
	a, b, c := f32Of(request.A), f32Of(request.B), f32Of(request.C)
	n := int32(request.N)
	switch request.F {
	case "max":
		return f32Bits(rs.F32Max(a, b))
	case "min":
		return f32Bits(rs.F32Min(a, b))
	case "clamp":
		return f32Bits(rs.F32Clamp(a, b, c))
	case "signum":
		return f32Bits(rs.F32Signum(a))
	case "fract":
		return f32Bits(rs.F32Fract(a))
	case "rem":
		return f32Bits(rs.F32Rem(a, b))
	case "rem_euclid":
		return f32Bits(rs.F32RemEuclid(a, b))
	case "div_euclid":
		return f32Bits(rs.F32DivEuclid(a, b))
	case "total_cmp":
		return reply{"r": int(rs.F32TotalCmp(a, b))}
	case "powi":
		return f32Bits(rs.F32Powi(a, n))
	case "is_nan":
		return flag(rs.F32IsNan(a))
	case "is_infinite":
		return flag(rs.F32IsInfinite(a))
	case "is_finite":
		return flag(rs.F32IsFinite(a))
	case "is_sign_negative":
		return flag(rs.F32IsSignNegative(a))
	case "is_sign_positive":
		return flag(rs.F32IsSignPositive(a))
	case "copysign":
		return f32Bits(rs.F32Copysign(a, b))
	case "recip":
		return f32Bits(rs.F32Recip(a))
	case "to_degrees":
		return f32Bits(rs.F32ToDegrees(a))
	case "to_radians":
		return f32Bits(rs.F32ToRadians(a))
	case "round":
		return f32Bits(rs.F32Round(a))
	case "round_ties_even":
		return f32Bits(rs.F32RoundTiesEven(a))
	case "floor":
		return f32Bits(rs.F32Floor(a))
	case "ceil":
		return f32Bits(rs.F32Ceil(a))
	case "trunc":
		return f32Bits(rs.F32Trunc(a))
	case "abs":
		return f32Bits(rs.F32Abs(a))
	case "sqrt":
		return f32Bits(rs.F32Sqrt(a))
	case "mul_add":
		return f32Bits(rs.F32MulAdd(a, b, c))
	case "debug":
		return reply{"r": clampDebug(func() { rs.F32Clamp(0, a, float32(math.NaN())) })}
	}
	return unknownRequest(request)
}

// clampDebug runs a clamp whose upper bound is NaN and returns how its panic
// printed the lower bound: the float's Debug form.
func clampDebug(clamp func()) (text string) {
	defer func() {
		message := rs.PanicInfoOf(recover()).Message
		const prefix, suffix = "min > max, or either was NaN. min = ", ", max = NaN"
		if !strings.HasPrefix(message, prefix) || !strings.HasSuffix(message, suffix) {
			text = "unexpected panic: " + message
			return
		}
		text = strings.TrimSuffix(strings.TrimPrefix(message, prefix), suffix)
	}()
	clamp()
	return "clamp did not panic"
}

// ---------------------------------------------------------------------------
// Float methods that Rust takes from the C library

var (
	libmUnary  = []string{"exp", "exp2", "exp_m1", "ln", "ln_1p", "log2", "log10", "cbrt", "sin", "cos", "tan", "asin", "acos", "atan", "sinh", "cosh", "tanh"}
	libmBinary = []string{"log", "powf", "hypot", "atan2"}
)

// libmGo is the Go function for a C-library method, on the bits of its
// operands.
func libmGo(ty, method string, a, b uint64) uint64 {
	if ty == "f32" {
		x, y := math.Float32frombits(uint32(a)), math.Float32frombits(uint32(b))
		var result float32
		switch method {
		case "exp":
			result = rs.F32Exp(x)
		case "exp2":
			result = rs.F32Exp2(x)
		case "exp_m1":
			result = rs.F32ExpM1(x)
		case "ln":
			result = rs.F32Ln(x)
		case "ln_1p":
			result = rs.F32Ln1p(x)
		case "log":
			result = rs.F32Log(x, y)
		case "log2":
			result = rs.F32Log2(x)
		case "log10":
			result = rs.F32Log10(x)
		case "powf":
			result = rs.F32Powf(x, y)
		case "cbrt":
			result = rs.F32Cbrt(x)
		case "hypot":
			result = rs.F32Hypot(x, y)
		case "sin":
			result = rs.F32Sin(x)
		case "cos":
			result = rs.F32Cos(x)
		case "tan":
			result = rs.F32Tan(x)
		case "asin":
			result = rs.F32Asin(x)
		case "acos":
			result = rs.F32Acos(x)
		case "atan":
			result = rs.F32Atan(x)
		case "atan2":
			result = rs.F32Atan2(x, y)
		case "sinh":
			result = rs.F32Sinh(x)
		case "cosh":
			result = rs.F32Cosh(x)
		case "tanh":
			result = rs.F32Tanh(x)
		default:
			panic("unknown method " + method)
		}
		return uint64(math.Float32bits(result))
	}
	x, y := math.Float64frombits(a), math.Float64frombits(b)
	var result float64
	switch method {
	case "exp":
		result = rs.F64Exp(x)
	case "exp2":
		result = rs.F64Exp2(x)
	case "exp_m1":
		result = rs.F64ExpM1(x)
	case "ln":
		result = rs.F64Ln(x)
	case "ln_1p":
		result = rs.F64Ln1p(x)
	case "log":
		result = rs.F64Log(x, y)
	case "log2":
		result = rs.F64Log2(x)
	case "log10":
		result = rs.F64Log10(x)
	case "powf":
		result = rs.F64Powf(x, y)
	case "cbrt":
		result = rs.F64Cbrt(x)
	case "hypot":
		result = rs.F64Hypot(x, y)
	case "sin":
		result = rs.F64Sin(x)
	case "cos":
		result = rs.F64Cos(x)
	case "tan":
		result = rs.F64Tan(x)
	case "asin":
		result = rs.F64Asin(x)
	case "acos":
		result = rs.F64Acos(x)
	case "atan":
		result = rs.F64Atan(x)
	case "atan2":
		result = rs.F64Atan2(x, y)
	case "sinh":
		result = rs.F64Sinh(x)
	case "cosh":
		result = rs.F64Cosh(x)
	case "tanh":
		result = rs.F64Tanh(x)
	default:
		panic("unknown method " + method)
	}
	return math.Float64bits(result)
}

// floatOrdinal is the position of a float among the floats of its type, so
// that two positions differ by their distance in units in the last place.
func floatOrdinal(bits uint64, signBit uint) int64 {
	magnitude := int64(bits & (1<<signBit - 1))
	if bits>>signBit != 0 {
		return -magnitude
	}
	return magnitude
}

// libmArguments are operands in the range the port's callers use, and the
// values whose results are exact in the C library.
func libmArguments(ty, method string, rng *rand.Rand) [][2]float64 {
	var arguments [][2]float64
	add := func(a, b float64) { arguments = append(arguments, [2]float64{a, b}) }
	uniform := func(low, high float64) float64 { return low + rng.Float64()*(high-low) }
	switch method {
	case "exp", "exp_m1", "sinh", "cosh", "tanh":
		for index := 0; index < 24; index++ {
			add(uniform(-20, 20), 0)
		}
		add(0, 0)
		add(1, 0)
		add(-1, 0)
	case "exp2":
		for index := 0; index < 20; index++ {
			add(uniform(-30, 30), 0)
		}
		for _, exact := range []float64{0, 1, 2, 10, -1, -10, 0.5} {
			add(exact, 0)
		}
	case "ln", "log2", "log10", "ln_1p":
		for index := 0; index < 20; index++ {
			add(uniform(0.001, 1000), 0)
		}
		for index := 0; index < 6; index++ {
			add(math.Ldexp(uniform(1, 2), rng.IntN(120)-60), 0)
		}
		// The results that code truncates: powers of ten and of two.
		for power := -8; power <= 22; power++ {
			if method == "log10" {
				add(math.Pow(10, float64(power)), 0)
			}
			if method == "log2" {
				add(math.Ldexp(1, power*3), 0)
			}
		}
		add(1, 0)
	case "cbrt":
		for index := 0; index < 20; index++ {
			add(uniform(-1000, 1000), 0)
		}
		for _, exact := range []float64{0, 1, 8, 27, -8, 1000, 0.125} {
			add(exact, 0)
		}
	case "sin", "cos", "tan", "atan":
		for index := 0; index < 24; index++ {
			add(uniform(-10, 10), 0)
		}
		for index := 0; index < 6; index++ {
			add(uniform(-1000, 1000), 0)
		}
		add(0, 0)
	case "asin", "acos":
		for index := 0; index < 24; index++ {
			add(uniform(-1, 1), 0)
		}
		add(0, 0)
		add(1, 0)
		add(-1, 0)
	case "log":
		for index := 0; index < 20; index++ {
			add(uniform(0.001, 1000), uniform(1.5, 20))
		}
		add(8, 2)
		add(1000, 10)
		add(81, 3)
	case "powf":
		for index := 0; index < 24; index++ {
			add(uniform(0.001, 50), uniform(-6, 6))
		}
		// The exponents the port uses, and powers that are exact.
		for _, exponent := range []float64{2.4, 0.7, 1.7, 0.5, 2, 3, -1, -2} {
			for index := 0; index < 4; index++ {
				add(uniform(0, 1.5), exponent)
			}
		}
		for power := -6; power <= 15; power++ {
			add(10, float64(power))
		}
		add(2, 10)
		add(2, -10)
		add(0, 0)
		add(1, 1e300)
	case "hypot":
		for index := 0; index < 24; index++ {
			add(uniform(-100, 100), uniform(-100, 100))
		}
		add(3, 4)
		add(0, 0)
		add(5, 12)
		add(1e-6, 0)
	case "atan2":
		for index := 0; index < 24; index++ {
			add(uniform(-100, 100), uniform(-100, 100))
		}
		add(0, 1)
		add(1, 0)
		add(0, -1)
		add(-1, 0)
	}
	return arguments
}

// libmTolerance is how many units in the last place the Go result of a
// C-library method may be from Rust's. Measured against glibc 2.44 on
// x86-64 over 5,000 to 16,000 operands a method: the f32 forms agree bit for
// bit on 98.8% (sin) to 100% of them and are never further than one unit
// (two for log, which divides two logarithms); the f64 forms agree on 67%
// (tan) to 100% and are within one unit for exp, ln, log2, sin, cos, atan,
// atan2, cbrt and tanh, two for the rest, and four for powf.
func libmTolerance(ty, method string) int64 {
	switch {
	case ty == "f32" && method == "log":
		return 2
	case ty == "f32":
		return 1
	case method == "powf":
		return 6
	default:
		return 2
	}
}

func TestNumLibmAgainstRust(t *testing.T) {
	// The request carries the Go result, and Rust replies whether its own
	// is within the tolerance of it. Replaying the corpus checks that Go
	// still computes what was recorded, to one unit in the last place.
	generate := func(yield func(any)) {
		rng := rand.New(rand.NewPCG(0x6c69626d, 0x21))
		for _, ty := range []string{"f32", "f64"} {
			for _, method := range append(append([]string{}, libmUnary...), libmBinary...) {
				for _, argument := range libmArguments(ty, method, rng) {
					a, b := floatBitsOf(ty, argument[0]), floatBitsOf(ty, argument[1])
					yield(numRequest{Op: "libm", Ty: ty, F: method, A: strconv.FormatUint(a, 10), B: strconv.FormatUint(b, 10),
						Go: strconv.FormatUint(libmGo(ty, method, a, b), 10), Tol: libmTolerance(ty, method)})
				}
			}
		}
	}
	oracle.Check(t, "std_num", "num_libm", generate, func(raw json.RawMessage) any {
		request := decodeNumRequest(t, raw)
		recorded := parse[uint64](request.Go)
		now := libmGo(request.Ty, request.F, parse[uint64](request.A), parse[uint64](request.B))
		signBit := uint(63)
		if request.Ty == "f32" {
			signBit = 31
		}
		distance := floatOrdinal(now, signBit) - floatOrdinal(recorded, signBit)
		if distance < -1 || distance > 1 {
			return reply{"close": false, "go_now": fmt.Sprint(now), "go_recorded": fmt.Sprint(recorded)}
		}
		return reply{"close": true}
	})
}
