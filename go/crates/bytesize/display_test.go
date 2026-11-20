// Port of bytesize 2.7.0 src/display.rs

package bytesize

import (
	"errors"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The documentation example of Display.
func TestDisplay_doc_display(t *testing.T) {
	assertEq(t,
		"1.0 MiB",
		ByteSizeMib(1).Display().Iec().String(),
	)

	assertEq(t,
		"42.0k",
		ByteSizeKb(42).Display().SiShort().String(),
	)
}

// The quickcheck properties draw their inputs as quickcheck 1.0 does
// (support_test.go), from a fixed seed.

func TestDisplay_ideal_unit_selection_std_no_std_iec(t *testing.T) {
	random := quickcheckRandom(11)
	for range quickcheckCases {
		bytes := ByteSize(quickcheckU64(random))
		if uint64(bytes) < 1025 {
			continue
		}

		size := float64(bytes)

		assertEq(t, displayIdealUnitStd(size, KIB, libLnKib), displayIdealUnitNoStd(size, KIB))
	}
}

func TestDisplay_ideal_unit_selection_std_no_std_si(t *testing.T) {
	random := quickcheckRandom(12)
	for range quickcheckCases {
		bytes := ByteSize(quickcheckU64(random))
		if uint64(bytes) < 1025 {
			continue
		}

		size := float64(bytes)

		assertEq(t, displayIdealUnitStd(size, KB, libLnKb), displayIdealUnitNoStd(size, KB))
	}
}

func TestDisplay_ideal_unit_selection_std_no_std_iec_bits(t *testing.T) {
	random := quickcheckRandom(13)
	for range quickcheckCases {
		bytes := ByteSize(quickcheckU64(random))
		if uint64(bytes) < 128 {
			continue
		}

		size := float64(bytes) * float64(displayBitsPerByte)

		assertEq(t,
			displayIdealUnitStd(size, KIB, libLnKib),
			displayIdealUnitNoStd(size, KIB),
		)
	}
}

func TestDisplay_ideal_unit_selection_std_no_std_si_bits(t *testing.T) {
	random := quickcheckRandom(14)
	for range quickcheckCases {
		bytes := ByteSize(quickcheckU64(random))
		if uint64(bytes) < 125 {
			continue
		}

		size := float64(bytes) * float64(displayBitsPerByte)

		assertEq(t, displayIdealUnitStd(size, KB, libLnKb), displayIdealUnitNoStd(size, KB))
	}
}

// Regression test for #142 / the `std` vs `no_std` display divergence: `f64::ln()` isn't
// precise enough to trust right at a power-of-`unit` boundary, so `ideal_unit_std` used to
// disagree with the exact, loop-based `ideal_unit_no_std` for sizes just below 1024^5 bytes
// (and the equivalent 1000^5 boundary for SI units) — previously "1.0 PiB" under the `std`
// feature vs "1024.0 TiB" without it, for the exact same byte count.
func TestDisplay_ideal_unit_std_matches_no_std_near_pebi_boundary(t *testing.T) {
	for _, bytes := range []uint64{
		1_125_899_906_842_621, // 1024^5 - 3
		1_125_899_906_842_622, // 1024^5 - 2
		1_125_899_906_842_623, // 1024^5 - 1
		1_125_899_906_842_624, // 1024^5 exactly
	} {
		size := float64(bytes)
		if displayIdealUnitStd(size, KIB, libLnKib) != displayIdealUnitNoStd(size, KIB) {
			t.Fatalf("mismatch at %d bytes (IEC)", bytes)
		}
	}

	for _, bytes := range []uint64{
		999_999_999_999_996,   // 1000^5 - 4
		999_999_999_999_997,   // 1000^5 - 3
		999_999_999_999_998,   // 1000^5 - 2
		999_999_999_999_999,   // 1000^5 - 1
		1_000_000_000_000_000, // 1000^5 exactly
	} {
		size := float64(bytes)
		if displayIdealUnitStd(size, KB, libLnKb) != displayIdealUnitNoStd(size, KB) {
			t.Fatalf("mismatch at %d bytes (SI)", bytes)
		}
	}
}

func TestDisplay_display_matches_just_below_pebi_boundary(t *testing.T) {
	assertEq(t,
		"1024.0 TiB",
		(&Display{
			byteSize: ByteSize(1_125_899_906_842_623),
			format:   displayFormat_Iec,
		}).String(),
	)
	assertEq(t,
		"1.0 PiB",
		(&Display{
			byteSize: ByteSize(1_125_899_906_842_624),
			format:   displayFormat_Iec,
		}).String(),
	)
}

func assertToString(t *testing.T, expected string, byteSize ByteSize, format displayFormat) {
	t.Helper()
	assertEq(t, expected, (&Display{byteSize: byteSize, format: format}).String())
}

func TestDisplay_to_string_iec(t *testing.T) {
	display := &Display{
		byteSize: ByteSizeGib(1),
		format:   displayFormat_Iec,
	}
	assertEq(t, "1.0 GiB", display.String())

	display = &Display{
		byteSize: ByteSizeGb(1),
		format:   displayFormat_Iec,
	}
	assertEq(t, "953.7 MiB", display.String())
}

func TestDisplay_to_string_si(t *testing.T) {
	display := &Display{
		byteSize: ByteSizeGib(1),
		format:   displayFormat_Si,
	}
	assertEq(t, "1.1 GB", display.String())

	display = &Display{
		byteSize: ByteSizeGb(1),
		format:   displayFormat_Si,
	}
	assertEq(t, "1.0 GB", display.String())
}

func TestDisplay_to_string_short(t *testing.T) {
	display := &Display{
		byteSize: ByteSizeGib(1),
		format:   displayFormat_IecShort,
	}
	assertEq(t, "1.0G", display.String())

	display = &Display{
		byteSize: ByteSizeGb(1),
		format:   displayFormat_IecShort,
	}
	assertEq(t, "953.7M", display.String())
}

func TestDisplay_test_to_string_as(t *testing.T) {
	assertToString(t, "215 B", ByteSizeB(215), displayFormat_Iec)
	assertToString(t, "215 B", ByteSizeB(215), displayFormat_Si)

	assertToString(t, "1.0 KiB", ByteSizeKib(1), displayFormat_Iec)
	assertToString(t, "1.0 kB", ByteSizeKib(1), displayFormat_Si)

	assertToString(t, "293.9 KiB", ByteSizeKb(301), displayFormat_Iec)
	assertToString(t, "301.0 kB", ByteSizeKb(301), displayFormat_Si)

	assertToString(t, "1.0 MiB", ByteSizeMib(1), displayFormat_Iec)
	assertToString(t, "1.0 MB", ByteSizeMib(1), displayFormat_Si)

	assertToString(t, "1.9 GiB", ByteSizeMib(1907), displayFormat_Iec)
	assertToString(t, "2.0 GB", ByteSizeMib(1908), displayFormat_Si)

	assertToString(t, "399.6 MiB", ByteSizeMb(419), displayFormat_Iec)
	assertToString(t, "419.0 MB", ByteSizeMb(419), displayFormat_Si)

	assertToString(t, "482.4 GiB", ByteSizeGb(518), displayFormat_Iec)
	assertToString(t, "518.0 GB", ByteSizeGb(518), displayFormat_Si)

	assertToString(t, "741.2 TiB", ByteSizeTb(815), displayFormat_Iec)
	assertToString(t, "815.0 TB", ByteSizeTb(815), displayFormat_Si)

	assertToString(t, "540.9 PiB", ByteSizePb(609), displayFormat_Iec)
	assertToString(t, "609.0 PB", ByteSizePb(609), displayFormat_Si)
}

func TestDisplay_to_string_bits(t *testing.T) {
	assertToString(t, "1016 b", ByteSize(127), displayFormat_IecBits)
	assertToString(t, "1.0 Kib", ByteSize(128), displayFormat_IecBits)
	assertToString(t, "992 b", ByteSize(124), displayFormat_SiBits)
	assertToString(t, "1.0 kb", ByteSize(125), displayFormat_SiBits)

	assertToString(t, "7.8 Kib", ByteSizeKb(1), displayFormat_IecBits)
	assertToString(t, "8.0 kb", ByteSizeKb(1), displayFormat_SiBits)
	assertToString(t, "128.0 Eib", ByteSize(^uint64(0)), displayFormat_IecBits)
	assertToString(t, "147.6 Eb", ByteSize(^uint64(0)), displayFormat_SiBits)
}

func TestDisplay_display_bits_public_api(t *testing.T) {
	assertEq(t, "1.0 Kib", ByteSize(128).Display().IecBits().String())
	assertEq(t, "1.0 kb", ByteSize(125).Display().SiBits().String())
}

// failingWriter is the test's FailingWriter: every write fails.
type failingWriter struct{}

var errFmt = errors.New("an error occurred when formatting an argument")

func (failingWriter) Write([]byte) (int, error) { return 0, errFmt }

func TestDisplay_display_propagates_write_errors(t *testing.T) {
	requireFormatter(t)
	writer := failingWriter{}

	assertEq(t,
		errFmt,
		rs.Write(
			writer,
			"{}",
			&Display{
				byteSize: ByteSize(127),
				format:   displayFormat_IecBits,
			},
		),
	)
	assertEq(t,
		errFmt,
		rs.Write(
			writer,
			"{}",
			&Display{
				byteSize: ByteSize(1),
				format:   displayFormat_Iec,
			},
		),
	)
}

func TestDisplay_precision(t *testing.T) {
	requireFormatter(t)
	size := ByteSizeMib(1908)
	assertEq(t, "1.9 GiB", rs.Format("{size}", rs.Arg("size", size)))
	assertEq(t, "2 GiB", rs.Format("{size:.0}", rs.Arg("size", size)))
	assertEq(t, "1.86328 GiB", rs.Format("{size:.5}", rs.Arg("size", size)))
}
