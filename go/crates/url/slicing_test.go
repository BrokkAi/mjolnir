// Port of url 2.5.8 src/slicing.rs (tests)

package url

import "testing"

func TestSlicing_test_count_digits(t *testing.T) {
	itUnitAssertEq(t, slicingCountDigits(0), 1)
	itUnitAssertEq(t, slicingCountDigits(1), 1)
	itUnitAssertEq(t, slicingCountDigits(9), 1)
	itUnitAssertEq(t, slicingCountDigits(10), 2)
	itUnitAssertEq(t, slicingCountDigits(99), 2)
	itUnitAssertEq(t, slicingCountDigits(100), 3)
	itUnitAssertEq(t, slicingCountDigits(9999), 4)
	itUnitAssertEq(t, slicingCountDigits(65535), 5)
}
