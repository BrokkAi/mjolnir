// Port of anyhow 1.0.102 src/fmt.rs (mod tests)

package anyhow

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestFmt_one_digit(t *testing.T) {
	input := "verify\nthis"
	expected := "    2: verify\n       this"
	var output strings.Builder

	(&indented{inner: &output, number: rs.Some(2)}).writeStr(input)

	assertEq(t, expected, output.String())
}

func TestFmt_two_digits(t *testing.T) {
	input := "verify\nthis"
	expected := "   12: verify\n       this"
	var output strings.Builder

	(&indented{inner: &output, number: rs.Some(12)}).writeStr(input)

	assertEq(t, expected, output.String())
}

func TestFmt_no_digits(t *testing.T) {
	input := "verify\nthis"
	expected := "    verify\n    this"
	var output strings.Builder

	(&indented{inner: &output, number: rs.None[int]()}).writeStr(input)

	assertEq(t, expected, output.String())
}
