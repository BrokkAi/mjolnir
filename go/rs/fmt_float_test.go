// Port of coretests 1.99.1 tests/fmt/float.rs

package rs

import (
	"math"
	"strings"
	"testing"
)

func TestFmtFloat_test_format_f64(t *testing.T) {
	fmtExpectEq(t, "1", Format("{:.0}", 1.0))
	fmtExpectEq(t, "9", Format("{:.0}", 9.4))
	fmtExpectEq(t, "10", Format("{:.0}", 9.9))
	fmtExpectEq(t, "9.8", Format("{:.1}", 9.849))
	fmtExpectEq(t, "9.9", Format("{:.1}", 9.851))
	fmtExpectEq(t, "0", Format("{:.0}", 0.5))
	fmtExpectEq(t, "1.23456789e6", Format("{:e}", 1234567.89))
	fmtExpectEq(t, "1.23456789e3", Format("{:e}", 1234.56789))
	fmtExpectEq(t, "1.23456789E6", Format("{:E}", 1234567.89))
	fmtExpectEq(t, "1.23456789E3", Format("{:E}", 1234.56789))
	fmtExpectEq(t, "0.0", Format("{:?}", 0.0))
	fmtExpectEq(t, "1.01", Format("{:?}", 1.01))

	highCutoff := 1e16
	fmtExpectEq(t, "1e16", Format("{:?}", highCutoff))
	fmtExpectEq(t, "-1e16", Format("{:?}", -highCutoff))
	epsilon := math.Nextafter(1, 2) - 1 // f64::EPSILON
	if fmtIsExponential(Format("{:?}", highCutoff*(1.0-2.0*epsilon))) {
		t.Error("just below 1e16 prints with an exponent")
	}
	fmtExpectEq(t, "-3.0", Format("{:?}", -3.0))
	fmtExpectEq(t, "0.0001", Format("{:?}", 0.0001))
	fmtExpectEq(t, "9e-5", Format("{:?}", 0.00009))
	fmtExpectEq(t, "1234567.9", Format("{:.1?}", 1234567.89))
	fmtExpectEq(t, "1234.6", Format("{:.1?}", 1234.56789))
}

func TestFmtFloat_test_format_f64_rounds_ties_to_even(t *testing.T) {
	fmtExpectEq(t, "0", Format("{:.0}", 0.5))
	fmtExpectEq(t, "2", Format("{:.0}", 1.5))
	fmtExpectEq(t, "2", Format("{:.0}", 2.5))
	fmtExpectEq(t, "4", Format("{:.0}", 3.5))
	fmtExpectEq(t, "4", Format("{:.0}", 4.5))
	fmtExpectEq(t, "6", Format("{:.0}", 5.5))
	fmtExpectEq(t, "128", Format("{:.0}", 127.5))
	fmtExpectEq(t, "128", Format("{:.0}", 128.5))
	fmtExpectEq(t, "0.2", Format("{:.1}", 0.25))
	fmtExpectEq(t, "0.8", Format("{:.1}", 0.75))
	fmtExpectEq(t, "0.12", Format("{:.2}", 0.125))
	fmtExpectEq(t, "0.88", Format("{:.2}", 0.875))
	fmtExpectEq(t, "0.062", Format("{:.3}", 0.062))
	fmtExpectEq(t, "-0", Format("{:.0}", -0.5))
	fmtExpectEq(t, "-2", Format("{:.0}", -1.5))
	fmtExpectEq(t, "-2", Format("{:.0}", -2.5))
	fmtExpectEq(t, "-4", Format("{:.0}", -3.5))
	fmtExpectEq(t, "-4", Format("{:.0}", -4.5))
	fmtExpectEq(t, "-6", Format("{:.0}", -5.5))
	fmtExpectEq(t, "-128", Format("{:.0}", -127.5))
	fmtExpectEq(t, "-128", Format("{:.0}", -128.5))
	fmtExpectEq(t, "-0.2", Format("{:.1}", -0.25))
	fmtExpectEq(t, "-0.8", Format("{:.1}", -0.75))
	fmtExpectEq(t, "-0.12", Format("{:.2}", -0.125))
	fmtExpectEq(t, "-0.88", Format("{:.2}", -0.875))
	fmtExpectEq(t, "-0.062", Format("{:.3}", -0.062))

	fmtExpectEq(t, "2e0", Format("{:.0e}", 1.5))
	fmtExpectEq(t, "2e0", Format("{:.0e}", 2.5))
	fmtExpectEq(t, "4e0", Format("{:.0e}", 3.5))
	fmtExpectEq(t, "4e0", Format("{:.0e}", 4.5))
	fmtExpectEq(t, "6e0", Format("{:.0e}", 5.5))
	fmtExpectEq(t, "1.28e2", Format("{:.2e}", 127.5))
	fmtExpectEq(t, "1.28e2", Format("{:.2e}", 128.5))
	fmtExpectEq(t, "-2e0", Format("{:.0e}", -1.5))
	fmtExpectEq(t, "-2e0", Format("{:.0e}", -2.5))
	fmtExpectEq(t, "-4e0", Format("{:.0e}", -3.5))
	fmtExpectEq(t, "-4e0", Format("{:.0e}", -4.5))
	fmtExpectEq(t, "-6e0", Format("{:.0e}", -5.5))
	fmtExpectEq(t, "-1.28e2", Format("{:.2e}", -127.5))
	fmtExpectEq(t, "-1.28e2", Format("{:.2e}", -128.5))

	fmtExpectEq(t, "2E0", Format("{:.0E}", 1.5))
	fmtExpectEq(t, "2E0", Format("{:.0E}", 2.5))
	fmtExpectEq(t, "4E0", Format("{:.0E}", 3.5))
	fmtExpectEq(t, "4E0", Format("{:.0E}", 4.5))
	fmtExpectEq(t, "6E0", Format("{:.0E}", 5.5))
	fmtExpectEq(t, "1.28E2", Format("{:.2E}", 127.5))
	fmtExpectEq(t, "1.28E2", Format("{:.2E}", 128.5))
	fmtExpectEq(t, "-2E0", Format("{:.0E}", -1.5))
	fmtExpectEq(t, "-2E0", Format("{:.0E}", -2.5))
	fmtExpectEq(t, "-4E0", Format("{:.0E}", -3.5))
	fmtExpectEq(t, "-4E0", Format("{:.0E}", -4.5))
	fmtExpectEq(t, "-6E0", Format("{:.0E}", -5.5))
	fmtExpectEq(t, "-1.28E2", Format("{:.2E}", -127.5))
	fmtExpectEq(t, "-1.28E2", Format("{:.2E}", -128.5))
}

func TestFmtFloat_test_format_f32(t *testing.T) {
	fmtExpectEq(t, "1", Format("{:.0}", float32(1.0)))
	fmtExpectEq(t, "9", Format("{:.0}", float32(9.4)))
	fmtExpectEq(t, "10", Format("{:.0}", float32(9.9)))
	fmtExpectEq(t, "9.8", Format("{:.1}", float32(9.849)))
	fmtExpectEq(t, "9.9", Format("{:.1}", float32(9.851)))
	fmtExpectEq(t, "0", Format("{:.0}", float32(0.5)))
	fmtExpectEq(t, "1.2345679e6", Format("{:e}", float32(1234567.89)))
	fmtExpectEq(t, "1.2345679e3", Format("{:e}", float32(1234.56789)))
	fmtExpectEq(t, "1.2345679E6", Format("{:E}", float32(1234567.89)))
	fmtExpectEq(t, "1.2345679E3", Format("{:E}", float32(1234.56789)))
	fmtExpectEq(t, "0.0", Format("{:?}", float32(0.0)))
	fmtExpectEq(t, "1.01", Format("{:?}", float32(1.01)))

	highCutoff := float32(1e16)
	fmtExpectEq(t, "1e16", Format("{:?}", highCutoff))
	fmtExpectEq(t, "-1e16", Format("{:?}", -highCutoff))
	epsilon := math.Nextafter32(1, 2) - 1 // f32::EPSILON
	if fmtIsExponential(Format("{:?}", highCutoff*(1.0-2.0*epsilon))) {
		t.Error("just below 1e16 prints with an exponent")
	}
	fmtExpectEq(t, "-3.0", Format("{:?}", float32(-3)))
	fmtExpectEq(t, "0.0001", Format("{:?}", float32(0.0001)))
	fmtExpectEq(t, "9e-5", Format("{:?}", float32(0.00009)))
	fmtExpectEq(t, "1234567.9", Format("{:.1?}", float32(1234567.89)))
	fmtExpectEq(t, "1234.6", Format("{:.1?}", float32(1234.56789)))
}

func TestFmtFloat_test_format_f32_rounds_ties_to_even(t *testing.T) {
	f := func(v float32) float32 { return v }
	fmtExpectEq(t, "0", Format("{:.0}", f(0.5)))
	fmtExpectEq(t, "2", Format("{:.0}", f(1.5)))
	fmtExpectEq(t, "2", Format("{:.0}", f(2.5)))
	fmtExpectEq(t, "4", Format("{:.0}", f(3.5)))
	fmtExpectEq(t, "4", Format("{:.0}", f(4.5)))
	fmtExpectEq(t, "6", Format("{:.0}", f(5.5)))
	fmtExpectEq(t, "128", Format("{:.0}", f(127.5)))
	fmtExpectEq(t, "128", Format("{:.0}", f(128.5)))
	fmtExpectEq(t, "0.2", Format("{:.1}", f(0.25)))
	fmtExpectEq(t, "0.8", Format("{:.1}", f(0.75)))
	fmtExpectEq(t, "0.12", Format("{:.2}", f(0.125)))
	fmtExpectEq(t, "0.88", Format("{:.2}", f(0.875)))
	fmtExpectEq(t, "0.062", Format("{:.3}", f(0.062)))
	fmtExpectEq(t, "-0", Format("{:.0}", f(-0.5)))
	fmtExpectEq(t, "-2", Format("{:.0}", f(-1.5)))
	fmtExpectEq(t, "-2", Format("{:.0}", f(-2.5)))
	fmtExpectEq(t, "-4", Format("{:.0}", f(-3.5)))
	fmtExpectEq(t, "-4", Format("{:.0}", f(-4.5)))
	fmtExpectEq(t, "-6", Format("{:.0}", f(-5.5)))
	fmtExpectEq(t, "-128", Format("{:.0}", f(-127.5)))
	fmtExpectEq(t, "-128", Format("{:.0}", f(-128.5)))
	fmtExpectEq(t, "-0.2", Format("{:.1}", f(-0.25)))
	fmtExpectEq(t, "-0.8", Format("{:.1}", f(-0.75)))
	fmtExpectEq(t, "-0.12", Format("{:.2}", f(-0.125)))
	fmtExpectEq(t, "-0.88", Format("{:.2}", f(-0.875)))
	fmtExpectEq(t, "-0.062", Format("{:.3}", f(-0.062)))

	fmtExpectEq(t, "2e0", Format("{:.0e}", f(1.5)))
	fmtExpectEq(t, "2e0", Format("{:.0e}", f(2.5)))
	fmtExpectEq(t, "4e0", Format("{:.0e}", f(3.5)))
	fmtExpectEq(t, "4e0", Format("{:.0e}", f(4.5)))
	fmtExpectEq(t, "6e0", Format("{:.0e}", f(5.5)))
	fmtExpectEq(t, "1.28e2", Format("{:.2e}", f(127.5)))
	fmtExpectEq(t, "1.28e2", Format("{:.2e}", f(128.5)))
	fmtExpectEq(t, "-2e0", Format("{:.0e}", f(-1.5)))
	fmtExpectEq(t, "-2e0", Format("{:.0e}", f(-2.5)))
	fmtExpectEq(t, "-4e0", Format("{:.0e}", f(-3.5)))
	fmtExpectEq(t, "-4e0", Format("{:.0e}", f(-4.5)))
	fmtExpectEq(t, "-6e0", Format("{:.0e}", f(-5.5)))
	fmtExpectEq(t, "-1.28e2", Format("{:.2e}", f(-127.5)))
	fmtExpectEq(t, "-1.28e2", Format("{:.2e}", f(-128.5)))

	fmtExpectEq(t, "2E0", Format("{:.0E}", f(1.5)))
	fmtExpectEq(t, "2E0", Format("{:.0E}", f(2.5)))
	fmtExpectEq(t, "4E0", Format("{:.0E}", f(3.5)))
	fmtExpectEq(t, "4E0", Format("{:.0E}", f(4.5)))
	fmtExpectEq(t, "6E0", Format("{:.0E}", f(5.5)))
	fmtExpectEq(t, "1.28E2", Format("{:.2E}", f(127.5)))
	fmtExpectEq(t, "1.28E2", Format("{:.2E}", f(128.5)))
	fmtExpectEq(t, "-2E0", Format("{:.0E}", f(-1.5)))
	fmtExpectEq(t, "-2E0", Format("{:.0E}", f(-2.5)))
	fmtExpectEq(t, "-4E0", Format("{:.0E}", f(-3.5)))
	fmtExpectEq(t, "-4E0", Format("{:.0E}", f(-4.5)))
	fmtExpectEq(t, "-6E0", Format("{:.0E}", f(-5.5)))
	fmtExpectEq(t, "-1.28E2", Format("{:.2E}", f(-127.5)))
	fmtExpectEq(t, "-1.28E2", Format("{:.2E}", f(-128.5)))
}

// fmtExactExpWriter is the ExactExpWriter of
// test_format_f64_max_precision_exponential: a fmt::Write that checks what
// it receives byte by byte without keeping it.
type fmtExactExpWriter struct {
	t               *testing.T
	prefix          string
	zeroesRemaining uint32
	suffix          string
	prefixPos       int
	suffixPos       int
	totalLen        uint32
}

func fmtExactExpWriterNew(t *testing.T, prefix, suffix string) *fmtExactExpWriter {
	return &fmtExactExpWriter{t: t, prefix: prefix, zeroesRemaining: math.MaxUint16, suffix: suffix}
}

func (w *fmtExactExpWriter) finish() {
	w.t.Helper()
	if w.prefixPos != len(w.prefix) || w.zeroesRemaining != 0 || w.suffixPos != len(w.suffix) || w.totalLen != math.MaxUint16+4 {
		w.t.Errorf("prefix %d of %d, %d zeroes missing, suffix %d of %d, %d bytes",
			w.prefixPos, len(w.prefix), w.zeroesRemaining, w.suffixPos, len(w.suffix), w.totalLen)
	}
}

func (w *fmtExactExpWriter) writeStr(s string) error {
	for i := 0; i < len(s); i++ {
		b := s[i]
		w.totalLen++

		switch {
		case w.prefixPos < len(w.prefix):
			if b != w.prefix[w.prefixPos] {
				w.t.Fatalf("byte %d is %q, want %q", w.totalLen, b, w.prefix[w.prefixPos])
			}
			w.prefixPos++
		case w.zeroesRemaining > 0:
			if b != '0' {
				w.t.Fatalf("byte %d is %q, want '0'", w.totalLen, b)
			}
			w.zeroesRemaining--
		default:
			if w.suffixPos >= len(w.suffix) || b != w.suffix[w.suffixPos] {
				w.t.Fatalf("byte %d is %q past the end of the suffix %q", w.totalLen, b, w.suffix)
			}
			w.suffixPos++
		}
	}

	return nil
}

func (w *fmtExactExpWriter) writeChar(c rune) error { return w.writeStr(string(c)) }

func TestFmtFloat_test_format_f64_max_precision_exponential(t *testing.T) {
	assertExactExp := func(format string, value float64, prefix, suffix string) {
		t.Helper()
		writer := fmtExactExpWriterNew(t, prefix, suffix)
		if err := fmtWriteTemplate(writer, fmtParse(format), []any{value}); err != nil {
			t.Fatal(err)
		}
		writer.finish()
	}

	assertExactExp("{:.65535e}", 0.0, "0.", "e0")
	assertExactExp("{:.65535e}", 1.0, "1.", "e0")
	assertExactExp("{:.65535E}", 0.0, "0.", "E0")
	assertExactExp("{:.65535E}", 1.0, "1.", "E0")
	assertExactExp("{:65535.65535e}", 1.0, "1.", "e0")
}

func fmtIsExponential(s string) bool {
	return strings.Contains(s, "e") || strings.Contains(s, "E")
}
