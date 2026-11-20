package bytesize

import (
	"math"
	"math/rand/v2"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Helpers shared by the ported tests: assertions, the skip for tests that
// need rs.Format, and generators that draw values the way the crate's
// quickcheck properties do.

// assertEq is `assert_eq!`.
func assertEq[T any](t *testing.T, left T, right T) {
	t.Helper()
	if !rs.Eq(left, right) {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// assert is `assert!(cond)`.
func assert(t *testing.T, cond bool) {
	t.Helper()
	if !cond {
		t.Fatal("assertion failed")
	}
}

// requireFormatter skips a test that formats through rs.Format, which
// panics with "not yet ported" until go/rs has its formatter
// (go/rs/formatter_pending.go).
func requireFormatter(t *testing.T) {
	t.Helper()
	if !formatterPorted() {
		t.Skip("waiting for rs.Format and rs.Formatter (go/rs/formatter_pending.go)")
	}
}

func formatterPorted() (ported bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			if strings.HasPrefix(rs.PanicInfoOf(recovered).Message, "not yet ported: core::fmt") {
				ported = false
				return
			}
			panic(recovered)
		}
	}()
	_ = rs.Format("{:#?} {:>4.1}", rs.Some(1), "x")
	return true
}

// quickcheckCases is how many cases quickcheck tries for each property.
const quickcheckCases = 100

func quickcheckRandom(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x6279746573697a65))
}

// quickcheckU64 is quickcheck 1.0's `u64::arbitrary`: one time in ten one of
// the problem values 0, 1 and u64::MAX, otherwise any u64.
func quickcheckU64(random *rand.Rand) uint64 {
	if random.IntN(10) == 0 {
		problems := []uint64{0, 1, math.MaxUint64}
		return problems[random.IntN(len(problems))]
	}
	return random.Uint64()
}

// quickcheckString is quickcheck 1.0's `String::arbitrary` at the default
// size of 100: up to 99 chars drawn the way `char::arbitrary` draws them.
func quickcheckString(random *rand.Rand) string {
	var out strings.Builder
	for range random.IntN(100) {
		out.WriteRune(quickcheckChar(random))
	}
	return out.String()
}

func quickcheckChar(random *rand.Rand) rune {
	switch mode := random.IntN(100); {
	case mode <= 49:
		// ASCII + some control characters
		return rune(random.IntN(0xB0))
	case mode <= 59:
		// Unicode BMP characters
		for {
			if c := rune(random.IntN(0x10000)); c < 0xD800 || c > 0xDFFF {
				return c
			}
		}
	case mode <= 84:
		// Characters often used in programming languages
		chars := []rune(" \u0020 \t\n~`!@#$%^&*()_-=+[]{}:;'\"\\|,<>./?0123456789")
		return chars[random.IntN(len(chars))]
	case mode <= 89:
		// Tricky Unicode, part 1
		chars := []rune{
			0x0149, 0xfff0, 0xfff1, 0xfff2, 0xfff3, 0xfff4, 0xfff5, 0xfff6, 0xfff7, 0xfff8, 0xfff9, 0xfffA, 0xfffB,
			0xfffC, 0xfffD, 0xfffE, 0xfffF, 0x0600, 0x0601, 0x0602, 0x0603, 0x0604, 0x0605, 0x061C, 0x06DD, 0x070F,
			0x180E, 0x110BD, 0x1D173, 0xe0001, 0xe0020, 0xe000, 0xe001, 0xef8ff, 0xf0000, 0xffffd, 0xffffe, 0xfffff,
			0x100000, 0x10FFFD, 0x10FFFE, 0x10FFFF, 0x3000, 0x1680,
		}
		return chars[random.IntN(len(chars))]
	case mode <= 94:
		// Tricky unicode, part 2
		return rune(0x2000 + random.IntN(0x70))
	default:
		// Completely arbitrary characters
		for {
			if c := rune(random.IntN(0x110000)); c < 0xD800 || c > 0xDFFF {
				return c
			}
		}
	}
}
