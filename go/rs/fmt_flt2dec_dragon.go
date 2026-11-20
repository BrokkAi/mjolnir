// Port of core 1.99.1 src/num/imp/flt2dec/strategy/dragon.rs

package rs

import "math/big"

// Almost direct (but slightly optimized) Rust translation of Figure 3 of "Printing
// Floating-Point Numbers Quickly and Accurately"[^1].
//
// [^1]: Burger, R. G. and Dybvig, R. K. 1996. Printing floating-point numbers
//   quickly and accurately. SIGPLAN Not. 31, 5 (May. 1996), 108-116.
//
// Rust computes with its fixed-size Big32x40; here the same integers are
// math/big values. The arithmetic is exact in both, so the digits agree; only
// the steps Rust takes to keep its intermediate products small (multiplying
// by powers of five and shifting) are written as plain multiplications.

var (
	fmtBigTen   = big.NewInt(10)
	fmtBigFive  = big.NewInt(5)
	fmtBigPow10 [512]*big.Int
)

func init() {
	fmtBigPow10[0] = big.NewInt(1)
	for i := 1; i < len(fmtBigPow10); i++ {
		fmtBigPow10[i] = new(big.Int).Mul(fmtBigPow10[i-1], fmtBigTen)
	}
}

// fmtDragonMulPow10 is dragon::mul_pow10: x *= 10^n.
func fmtDragonMulPow10(x *big.Int, n int) *big.Int {
	return x.Mul(x, fmtBigPow10[n])
}

// fmtDragonDiv2Pow10 is dragon::div_2pow10: x /= 2 * 10^n, truncating.
func fmtDragonDiv2Pow10(x *big.Int, n int) *big.Int {
	var divisor *big.Int
	if n < len(fmtBigPow10) {
		divisor = new(big.Int).Lsh(fmtBigPow10[n], 1)
	} else {
		divisor = new(big.Int).Exp(fmtBigTen, big.NewInt(int64(n)), nil)
		divisor.Lsh(divisor, 1)
	}
	return x.Quo(x, divisor)
}

// fmtDragonDivRemUpto16 is dragon::div_rem_upto_16: only usable when
// `x < 16 * scale`; `scaleN` should be `scale.mul_small(N)`
func fmtDragonDivRemUpto16(x, scale, scale2, scale4, scale8 *big.Int) byte {
	var d byte
	if x.Cmp(scale8) >= 0 {
		x.Sub(x, scale8)
		d += 8
	}
	if x.Cmp(scale4) >= 0 {
		x.Sub(x, scale4)
		d += 4
	}
	if x.Cmp(scale2) >= 0 {
		x.Sub(x, scale2)
		d += 2
	}
	if x.Cmp(scale) >= 0 {
		x.Sub(x, scale)
		d += 1
	}
	return d
}

// fmtDragonFormatShortest is dragon::format_shortest: the shortest mode
// implementation for Dragon.
func fmtDragonFormatShortest(d *fmtDecoded, buf []byte) ([]byte, int16) {
	// the number `v` to format is known to be:
	// - equal to `mant * 2^exp`;
	// - preceded by `(mant - 2 * minus) * 2^exp` in the original type; and
	// - followed by `(mant + 2 * plus) * 2^exp` in the original type.
	//
	// obviously, `minus` and `plus` cannot be zero. (for infinities, we use out-of-range values.)
	// also we assume that at least one digit is generated, i.e., `mant` cannot be zero too.
	//
	// this also means that any number between `low = (mant - minus) * 2^exp` and
	// `high = (mant + plus) * 2^exp` will map to this exact floating point number,
	// with bounds included when the original mantissa was even (i.e., `!mant_was_odd`).

	// `a.cmp(&b) < rounding` is `if d.inclusive {a <= b} else {a < b}`
	rounding := 0 // Ordering::Equal
	if d.inclusive {
		rounding = 1 // Ordering::Greater
	}

	// estimate `k_0` from original inputs satisfying `10^(k_0-1) < high <= 10^(k_0+1)`.
	// the tight bound `k` satisfying `10^(k-1) < high <= 10^k` is calculated later.
	k := fmtEstimateScalingFactor(d.mant+d.plus, d.exp)

	// convert `{mant, plus, minus} * 2^exp` into the fractional form so that:
	// - `v = mant / scale`
	// - `low = (mant - minus) / scale`
	// - `high = (mant + plus) / scale`
	mant := new(big.Int).SetUint64(d.mant)
	minus := new(big.Int).SetUint64(d.minus)
	plus := new(big.Int).SetUint64(d.plus)
	scale := big.NewInt(1)
	if d.exp < 0 {
		scale.Lsh(scale, uint(-d.exp))
	} else {
		mant.Lsh(mant, uint(d.exp))
		minus.Lsh(minus, uint(d.exp))
		plus.Lsh(plus, uint(d.exp))
	}

	// divide `mant` by `10^k`. now `scale / 10 < mant + plus <= scale * 10`.
	if k >= 0 {
		fmtDragonMulPow10(scale, int(k))
	} else {
		fmtDragonMulPow10(mant, int(-k))
		fmtDragonMulPow10(minus, int(-k))
		fmtDragonMulPow10(plus, int(-k))
	}

	// fixup when `mant + plus > scale` (or `>=`).
	// we are not actually modifying `scale`, since we can skip the initial multiplication instead.
	// now `scale < mant + plus <= scale * 10` and we are ready to generate digits.
	//
	// note that `d[0]` *can* be zero, when `scale - plus < mant < scale`.
	// in this case rounding-up condition (`up` below) will be triggered immediately.
	sum := new(big.Int)
	if scale.Cmp(sum.Add(mant, plus)) < rounding {
		// equivalent to scaling `scale` by 10
		k++
	} else {
		mant.Mul(mant, fmtBigTen)
		minus.Mul(minus, fmtBigTen)
		plus.Mul(plus, fmtBigTen)
	}

	// cache `(2, 4, 8) * scale` for digit generation.
	scale2 := new(big.Int).Lsh(scale, 1)
	scale4 := new(big.Int).Lsh(scale, 2)
	scale8 := new(big.Int).Lsh(scale, 3)

	var down, up bool
	i := 0
	for {
		// invariants, where `d[0..n-1]` are digits generated so far:
		// - `v = mant / scale * 10^(k-n-1) + d[0..n-1] * 10^(k-n)`
		// - `v - low = minus / scale * 10^(k-n-1)`
		// - `high - v = plus / scale * 10^(k-n-1)`
		// - `(mant + plus) / scale <= 10` (thus `mant / scale < 10`)
		// where `d[i..j]` is a shorthand for `d[i] * 10^(j-i) + ... + d[j-1] * 10 + d[j]`.

		// generate one digit: `d[n] = floor(mant / scale) < 10`.
		digit := fmtDragonDivRemUpto16(mant, scale, scale2, scale4, scale8)
		buf[i] = '0' + digit
		i++

		// this is a simplified description of the modified Dragon algorithm.
		// many intermediate derivations and completeness arguments are omitted for convenience.
		//
		// start with modified invariants, as we've updated `n`:
		// - `v = mant / scale * 10^(k-n) + d[0..n-1] * 10^(k-n)`
		// - `v - low = minus / scale * 10^(k-n)`
		// - `high - v = plus / scale * 10^(k-n)`
		//
		// assume that `d[0..n-1]` is the shortest representation between `low` and `high`,
		// i.e., `d[0..n-1]` satisfies both of the following but `d[0..n-2]` doesn't:
		// - `low < d[0..n-1] * 10^(k-n) < high` (bijectivity: digits round to `v`); and
		// - `abs(v / 10^(k-n) - d[0..n-1]) <= 1/2` (the last digit is correct).
		//
		// the second condition simplifies to `2 * mant <= scale`.
		// solving invariants in terms of `mant`, `low` and `high` yields
		// a simpler version of the first condition: `-plus < mant < minus`.
		// since `-plus < 0 <= mant`, we have the correct shortest representation
		// when `mant < minus` and `2 * mant <= scale`.
		// (the former becomes `mant <= minus` when the original mantissa is even.)
		//
		// when the second doesn't hold (`2 * mant > scale`), we need to increase the last digit.
		// this is enough for restoring that condition: we already know that
		// the digit generation guarantees `0 <= v / 10^(k-n) - d[0..n-1] < 1`.
		// in this case, the first condition becomes `-plus < mant - scale < minus`.
		// since `mant < scale` after the generation, we have `scale < mant + plus`.
		// (again, this becomes `scale <= mant + plus` when the original mantissa is even.)
		//
		// in short:
		// - stop and round `down` (keep digits as is) when `mant < minus` (or `<=`).
		// - stop and round `up` (increase the last digit) when `scale < mant + plus` (or `<=`).
		// - keep generating otherwise.
		down = mant.Cmp(minus) < rounding
		up = scale.Cmp(sum.Add(mant, plus)) < rounding
		if down || up {
			break
		} // we have the shortest representation, proceed to the rounding

		// restore the invariants.
		// this makes the algorithm always terminating: `minus` and `plus` always increases,
		// but `mant` is clipped modulo `scale` and `scale` is fixed.
		mant.Mul(mant, fmtBigTen)
		minus.Mul(minus, fmtBigTen)
		plus.Mul(plus, fmtBigTen)
	}

	// rounding up happens when
	// i) only the rounding-up condition was triggered, or
	// ii) both conditions were triggered and tie breaking prefers rounding up.
	if up && (!down || mant.Lsh(mant, 1).Cmp(scale) >= 0) {
		// if rounding up changes the length, the exponent should also change.
		// it seems that this condition is very hard to satisfy (possibly impossible),
		// but we are just being safe and consistent here.
		if c, grew := fmtRoundUp(buf[:i]); grew {
			buf[i] = c
			i++
			k++
		}
	}

	return buf[:i], k
}

// fmtDragonFormatExact is dragon::format_exact: the exact and fixed mode
// implementation for Dragon.
func fmtDragonFormatExact(d *fmtDecoded, buf []byte, limit int16) ([]byte, int16) {
	// estimate `k_0` from original inputs satisfying `10^(k_0-1) < v <= 10^(k_0+1)`.
	k := fmtEstimateScalingFactor(d.mant, d.exp)

	// `v = mant / scale`.
	mant := new(big.Int).SetUint64(d.mant)
	scale := big.NewInt(1)
	if d.exp < 0 {
		scale.Lsh(scale, uint(-d.exp))
	} else {
		mant.Lsh(mant, uint(d.exp))
	}

	// divide `mant` by `10^k`. now `scale / 10 < mant <= scale * 10`.
	if k >= 0 {
		fmtDragonMulPow10(scale, int(k))
	} else {
		fmtDragonMulPow10(mant, int(-k))
	}

	// fixup when `mant + plus >= scale`, where `plus / scale = 10^-buf.len() / 2`.
	// in order to keep the fixed-size bignum, we actually use `mant + floor(plus) >= scale`.
	// we are not actually modifying `scale`, since we can skip the initial multiplication instead.
	// again with the shortest algorithm, `d[0]` can be zero but will be eventually rounded up.
	plus := fmtDragonDiv2Pow10(new(big.Int).Set(scale), len(buf))
	if plus.Add(plus, mant).Cmp(scale) >= 0 {
		// equivalent to scaling `scale` by 10
		k++
	} else {
		mant.Mul(mant, fmtBigTen)
	}

	// if we are working with the last-digit limitation, we need to shorten the buffer
	// before the actual rendering in order to avoid double rounding.
	// note that we have to enlarge the buffer again when rounding up happens!
	var length int
	if k < limit {
		// oops, we cannot even produce *one* digit.
		// this is possible when, say, we've got something like 9.5 and it's being rounded to 10.
		// we return an empty buffer, with an exception of the later rounding-up case
		// which occurs when `k == limit` and has to produce exactly one digit.
		length = 0
	} else if int(int32(k)-int32(limit)) < len(buf) {
		length = int(k - limit)
	} else {
		length = len(buf)
	}

	if length > 0 {
		// cache `(2, 4, 8) * scale` for digit generation.
		// (this can be expensive, so do not calculate them when the buffer is empty.)
		scale2 := new(big.Int).Lsh(scale, 1)
		scale4 := new(big.Int).Lsh(scale, 2)
		scale8 := new(big.Int).Lsh(scale, 3)

		for i := 0; i < length; i++ {
			if mant.Sign() == 0 {
				// following digits are all zeroes, we stop here
				// do *not* try to perform rounding! rather, fill remaining digits.
				for j := i; j < length; j++ {
					buf[j] = '0'
				}
				return buf[:length], k
			}

			digit := fmtDragonDivRemUpto16(mant, scale, scale2, scale4, scale8)
			buf[i] = '0' + digit
			mant.Mul(mant, fmtBigTen)
		}
	}

	// rounding up if we stop in the middle of digits
	// if the following digits are exactly 5000..., check the prior digit and try to
	// round to even (i.e., avoid rounding up when the prior digit is even).
	order := mant.Cmp(scale.Mul(scale, fmtBigFive))
	if order > 0 || (order == 0 && length > 0 && buf[length-1]&1 == 1) {
		// if rounding up changes the length, the exponent should also change.
		// but we've been requested a fixed number of digits, so do not alter the buffer...
		if c, grew := fmtRoundUp(buf[:length]); grew {
			// ...unless we've been requested the fixed precision instead.
			// we also need to check that, if the original buffer was empty,
			// the additional digit can only be added when `k == limit` (edge case).
			k++
			if k > limit && length < len(buf) {
				buf[length] = c
				length++
			}
		}
	}

	return buf[:length], k
}
