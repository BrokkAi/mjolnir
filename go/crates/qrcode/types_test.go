// Port of qrcode 0.14.1 src/types.rs (tests)

package qrcode

import "testing"

// typesModeLess is `a < b` through the PartialOrd impl.
func typesModeLess(a TypesMode, b TypesMode) bool {
	ordering, ok := a.PartialCmp(b).Get()
	return ok && ordering < 0
}

// typesModeGreater is `a > b`.
func typesModeGreater(a TypesMode, b TypesMode) bool {
	ordering, ok := a.PartialCmp(b).Get()
	return ok && ordering > 0
}

// typesModeGreaterOrEqual is `a >= b`.
func typesModeGreaterOrEqual(a TypesMode, b TypesMode) bool {
	ordering, ok := a.PartialCmp(b).Get()
	return ok && ordering >= 0
}

func TestTypesModeTests_test_mode_order(t *testing.T) {
	qrAssert(t, typesModeLess(TypesMode_Numeric, TypesMode_Alphanumeric))
	qrAssert(t, typesModeGreater(TypesMode_Byte, TypesMode_Kanji))
	qrAssert(t, !typesModeLess(TypesMode_Numeric, TypesMode_Kanji))
	qrAssert(t, !typesModeGreaterOrEqual(TypesMode_Numeric, TypesMode_Kanji))
}

func TestTypesModeTests_test_max(t *testing.T) {
	qrAssertEq(t, TypesMode_Byte.Max(TypesMode_Kanji), TypesMode_Byte)
	qrAssertEq(t, TypesMode_Numeric.Max(TypesMode_Alphanumeric), TypesMode_Alphanumeric)
	qrAssertEq(t, TypesMode_Alphanumeric.Max(TypesMode_Alphanumeric), TypesMode_Alphanumeric)
	qrAssertEq(t, TypesMode_Numeric.Max(TypesMode_Kanji), TypesMode_Byte)
	qrAssertEq(t, TypesMode_Kanji.Max(TypesMode_Numeric), TypesMode_Byte)
	qrAssertEq(t, TypesMode_Alphanumeric.Max(TypesMode_Numeric), TypesMode_Alphanumeric)
	qrAssertEq(t, TypesMode_Kanji.Max(TypesMode_Kanji), TypesMode_Kanji)
}
