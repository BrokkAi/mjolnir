// Port of vt100 0.16.2 tests/quickcheck.rs

package vt100_test

import (
	"math/rand/v2"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The tests run a property on generated inputs, as quickcheck 1.0.3 does.
// quickcheckGen draws the inputs the way quickcheck's Gen and Arbitrary
// impls do, from a seeded generator rather than from the system's entropy,
// so that a run is reproducible: a failure names the input and the seed.
// quickcheck shrinks a failing input before it reports it; this reports the
// input as it was drawn.

// quickcheckGen is `quickcheck::Gen`: a source of randomness and the size
// that bounds generated collections (QuickCheck::new() makes it 100).
type quickcheckGen struct {
	rng  *rand.Rand
	size int
}

func quickcheckGenNew(size int, seed uint64) *quickcheckGen {
	return &quickcheckGen{rng: rand.New(rand.NewPCG(seed, 0x7174636865636b)), size: size}
}

// genRangeUsize is quickcheck's `g.gen_range(0..s)`: uniform.
func (self *quickcheckGen) genRangeUsize(end int) int {
	return self.rng.IntN(end)
}

// arbitraryU8 is `u8::arbitrary(g)`: one time in ten one of the problem
// values 0, 1 and u8::MAX, otherwise any value.
func (self *quickcheckGen) arbitraryU8() uint8 {
	if self.rng.IntN(10) == 0 {
		return []uint8{0, 1, 0xFF}[self.rng.IntN(3)]
	}
	return uint8(self.rng.Uint32())
}

// arbitraryU32 is `u32::arbitrary(g)`.
func (self *quickcheckGen) arbitraryU32() uint32 {
	if self.rng.IntN(10) == 0 {
		return []uint32{0, 1, 0xFFFF_FFFF}[self.rng.IntN(3)]
	}
	return self.rng.Uint32()
}

// arbitraryUsize is `usize::arbitrary(g)` on a 64-bit target.
func (self *quickcheckGen) arbitraryUsize() uint64 {
	if self.rng.IntN(10) == 0 {
		return []uint64{0, 1, 0xFFFF_FFFF_FFFF_FFFF}[self.rng.IntN(3)]
	}
	return self.rng.Uint64()
}

// arbitraryVecU8 is `Vec::<u8>::arbitrary(g)`.
func (self *quickcheckGen) arbitraryVecU8() []uint8 {
	size := self.genRangeUsize(self.size)
	out := make([]uint8, 0, size)
	for i := 0; i < size; i++ {
		out = append(out, self.arbitraryU8())
	}
	return out
}

// quickcheckTerminalInput is `TerminalInput`.
type quickcheckTerminalInput []uint8

// The test's `gen_range(g, start..end)`: `T::arbitrary(g) % (end - start) +
// start`, for each type it is used with.
func quickcheckGenRangeUsize(g *quickcheckGen, start, end uint64) uint64 {
	return g.arbitraryUsize()%(end-start) + start
}

func quickcheckGenRangeU32(g *quickcheckGen, start, end uint32) uint32 {
	return g.arbitraryU32()%(end-start) + start
}

func quickcheckGenRangeU8(g *quickcheckGen, start, end uint8) uint8 {
	return g.arbitraryU8()%(end-start) + start
}

// quickcheckTerminalInputArbitrary is `TerminalInput::arbitrary`.
func quickcheckTerminalInputArbitrary(g *quickcheckGen) quickcheckTerminalInput {
	size := func() uint64 {
		s := uint64(g.size)
		return quickcheckGenRangeUsize(g, 0, s)
	}()
	out := quickcheckTerminalInput{}
	for i := uint64(0); i < size; i++ {
		out = append(out, quickcheckChooseTerminalInputFragment(g)...)
	}
	return out
}

type quickcheckFragment uint8

const (
	quickcheckFragment_Text quickcheckFragment = iota
	quickcheckFragment_Control
	quickcheckFragment_Escape
	quickcheckFragment_Csi
	quickcheckFragment_Osc
	quickcheckFragment_Dcs
)

func quickcheckFragmentArbitrary(g *quickcheckGen) quickcheckFragment {
	switch n := g.arbitraryU8(); {
	case n <= 231:
		return quickcheckFragment_Text
	case n <= 239:
		return quickcheckFragment_Control
	case n <= 247:
		return quickcheckFragment_Escape
	default:
		return quickcheckFragment_Csi
	}
}

func quickcheckChooseTerminalInputFragment(g *quickcheckGen) []uint8 {
	switch quickcheckFragmentArbitrary(g) {
	case quickcheckFragment_Text:
		u := quickcheckGenRangeU32(g, 32, 1<<20-2048)
		// surrogates aren't valid codepoints on their own
		if u >= 0xD800 {
			u += 2048
		}
		if !utf8.ValidRune(rune(u)) {
			panic(rs.Panicf("failed to create char from {}: converted integer out of range for `char`", u))
		}
		return utf8.AppendRune(nil, rune(u))
	case quickcheckFragment_Control:
		return []uint8{quickcheckGenRangeU8(g, 7, 14)}
	case quickcheckFragment_Escape:
		v := []uint8{0x1b}
		c := quickcheckGenRangeU8(g, '0', '~')
		v = append(v, c)
		return v
	case quickcheckFragment_Csi:
		v := []uint8{0x1b, '['}
		// TODO: params
		c := quickcheckGenRangeU8(g, '@', '~')
		v = append(v, c)
		return v
	case quickcheckFragment_Osc:
		// TODO
		panic(rs.Unimplemented())
	case quickcheckFragment_Dcs:
		// TODO
		panic(rs.Unimplemented())
	default:
		panic(rs.Unreachable())
	}
	// TODO: sometimes add garbage in random places
}

func quickcheckContentsFormattedReproducesStateRandom(t testing.TB, input []uint8) bool {
	t.Helper()
	return helpersContentsFormattedReproducesState(t, input)
}

func quickcheckContentsFormattedReproducesStateStructured(
	t testing.TB,
	input quickcheckTerminalInput,
) bool {
	t.Helper()
	return helpersContentsFormattedReproducesState(t, input)
}

// quickcheckRun is `QuickCheck::new().tests(tests).quickcheck(property)`:
// the property must hold for every input drawn.
func quickcheckRun[A ~[]uint8](
	t *testing.T,
	tests int,
	seed uint64,
	arbitrary func(g *quickcheckGen) A,
	property func(t testing.TB, input A) bool,
) {
	t.Helper()
	g := quickcheckGenNew(100, seed)
	for i := 0; i < tests; i++ {
		input := arbitrary(g)
		if !property(t, input) {
			t.Fatalf("[quickcheck] TEST FAILED (seed %#x, test %d). Arguments: (%s)",
				seed, i, rs.Format("{:?}", helpersBytes(input)))
		}
	}
}

func TestItQuickcheck_qc_structured_long(t *testing.T) {
	ignored(t)
	quickcheckRun(t, 1_000_000, 0x5354524c, quickcheckTerminalInputArbitrary,
		quickcheckContentsFormattedReproducesStateStructured)
}

func TestItQuickcheck_qc_structured_short(t *testing.T) {
	quickcheckRun(t, 1_000, 0x53545253, quickcheckTerminalInputArbitrary,
		quickcheckContentsFormattedReproducesStateStructured)
}

func TestItQuickcheck_qc_random_long(t *testing.T) {
	ignored(t)
	quickcheckRun(t, 10_000_000, 0x524e444c, (*quickcheckGen).arbitraryVecU8,
		quickcheckContentsFormattedReproducesStateRandom)
}

func TestItQuickcheck_qc_random_short(t *testing.T) {
	quickcheckRun(t, 10_000, 0x524e4453, (*quickcheckGen).arbitraryVecU8,
		quickcheckContentsFormattedReproducesStateRandom)
}
