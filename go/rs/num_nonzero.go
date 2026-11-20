// Port of core 1.99.1 src/num/nonzero.rs (NonZeroUsize)

package rs

import "strconv"

// NonZeroUsize is core::num::NonZeroUsize (NonZero<usize>): a usize that is
// never zero. Rust makes the zero value unrepresentable; here it is a
// porting error to hold one, and NonZeroUsizeNew is the only way to make a
// value from a number that is not a constant.
//
// `Option<NonZeroUsize>` is rs.Option[rs.NonZeroUsize].
type NonZeroUsize int

const (
	// NonZeroUsizeMin is `NonZeroUsize::MIN`: 1.
	NonZeroUsizeMin NonZeroUsize = 1
	// NonZeroUsizeMax is `NonZeroUsize::MAX`.
	NonZeroUsizeMax = NonZeroUsize(UsizeMax)
)

// NonZeroUsizeNew is `NonZeroUsize::new(n)`: None for zero.
func NonZeroUsizeNew(n int) Option[NonZeroUsize] {
	if n == 0 {
		return Option[NonZeroUsize]{}
	}
	return Some(NonZeroUsize(n))
}

// NonZeroUsizeTryFrom is `NonZeroUsize::try_from(n)`.
func NonZeroUsizeTryFrom(n int) (NonZeroUsize, error) {
	if n == 0 {
		return 0, TryFromIntError{kind: IntErrorKind_Zero}
	}
	return NonZeroUsize(n), nil
}

// Get is `n.get()`, and `usize::from(n)`.
func (n NonZeroUsize) Get() int { return int(n) }

// String is the number in decimal, its Display and Debug form.
func (n NonZeroUsize) String() string { return strconv.Itoa(int(n)) }

// FmtDisplay is `impl Display for NonZeroUsize`: the number.
func (n NonZeroUsize) FmtDisplay(f *Formatter) error { return DisplayFmt(int(n), f) }

// FmtDebug is `impl Debug for NonZeroUsize`: the number.
func (n NonZeroUsize) FmtDebug(f *Formatter) error { return DebugFmt(int(n), f) }

// CheckedAdd is `n.checked_add(other)`, with other a usize.
func (n NonZeroUsize) CheckedAdd(other int) Option[NonZeroUsize] {
	return OptionMap(UsizeCheckedAdd(int(n), other), func(sum int) NonZeroUsize { return NonZeroUsize(sum) })
}

// SaturatingAdd is `n.saturating_add(other)`, with other a usize.
func (n NonZeroUsize) SaturatingAdd(other int) NonZeroUsize {
	return NonZeroUsize(UsizeSaturatingAdd(int(n), other))
}

// CheckedMul is `n.checked_mul(other)`.
func (n NonZeroUsize) CheckedMul(other NonZeroUsize) Option[NonZeroUsize] {
	return OptionMap(UsizeCheckedMul(int(n), int(other)), func(product int) NonZeroUsize { return NonZeroUsize(product) })
}

// SaturatingMul is `n.saturating_mul(other)`.
func (n NonZeroUsize) SaturatingMul(other NonZeroUsize) NonZeroUsize {
	return NonZeroUsize(UsizeSaturatingMul(int(n), int(other)))
}
