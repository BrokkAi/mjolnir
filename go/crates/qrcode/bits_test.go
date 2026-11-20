// Port of qrcode 0.14.1 src/bits.rs (tests)

package qrcode

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// qrAssertErr is `assert_eq!(result, Ok(()))` (want nil) and
// `assert_eq!(result, Err(e))`.
func qrAssertErr(t *testing.T, got error, want error) {
	t.Helper()
	if got != want {
		t.Fatalf("assertion `left == right` failed\n  left: %s\n right: %s", qrResultDebug(got), qrResultDebug(want))
	}
}

func qrResultDebug(err error) string {
	if err == nil {
		return "Ok(())"
	}
	return "Err(" + rs.DebugString(err) + ")"
}

// qrAssertVersion is `assert_eq!` on two versions.
func qrAssertVersion(t *testing.T, left Version, right Version) {
	t.Helper()
	if !left.Equal(right) {
		t.Fatalf("assertion `left == right` failed\n  left: %s\n right: %s", rs.DebugString(left), rs.DebugString(right))
	}
}

func TestBits_test_push_number(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})

	bits.pushNumber(3, 0b010)                 // 0:0 .. 0:3
	bits.pushNumber(3, 0b110)                 // 0:3 .. 0:6
	bits.pushNumber(3, 0b101)                 // 0:6 .. 1:1
	bits.pushNumber(7, 0b001_1010)            // 1:1 .. 2:0
	bits.pushNumber(4, 0b1100)                // 2:0 .. 2:4
	bits.pushNumber(12, 0b1011_0110_1101)     // 2:4 .. 4:0
	bits.pushNumber(10, 0b01_1001_0001)       // 4:0 .. 5:2
	bits.pushNumber(15, 0b111_0010_1110_0011) // 5:2 .. 7:1

	bytes := bits.IntoBytes()

	qrAssertSliceEq(t,
		bytes,
		[]byte{
			0b010_110_10, // 90
			0b1_001_1010, // 154
			0b1100_1011,  // 203
			0b0110_1101,  // 109
			0b01_1001_00, // 100
			0b01_111_001, // 121
			0b0_1110_001, // 113
			0b1_0000000,  // 128
		},
	)
}

func TestBitsEciTests_test_9(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushEciDesignator(9), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b0111_0000, 0b1001_0000})
}

func TestBitsEciTests_test_899(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushEciDesignator(899), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b0111_10_00, 0b00111000, 0b0011_0000})
}

func TestBitsEciTests_test_999999(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushEciDesignator(999999), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b0111_110_0, 0b11110100, 0b00100011, 0b1111_0000})
}

func TestBitsEciTests_test_invalid_designator(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushEciDesignator(1000000), TypesQrError_InvalidEciDesignator)
}

func TestBitsEciTests_test_unsupported_character_set(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 4})
	qrAssertErr(t, bits.PushEciDesignator(9), TypesQrError_UnsupportedCharacterSet)
}

func TestBitsNumericTests_test_iso_18004_2006_example_1(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("01234567")), nil)
	qrAssertSliceEq(t,
		bits.IntoBytes(),
		[]byte{0b0001_0000, 0b001000_00, 0b00001100, 0b01010110, 0b01_100001, 0b1_0000000},
	)
}

func TestBitsNumericTests_test_iso_18004_2000_example_2(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("0123456789012345")), nil)
	qrAssertSliceEq(t,
		bits.IntoBytes(),
		[]byte{
			0b0001_0000,
			0b010000_00,
			0b00001100,
			0b01010110,
			0b01_101010,
			0b0110_1110,
			0b000101_00,
			0b11101010,
			0b0101_0000,
		},
	)
}

func TestBitsNumericTests_test_iso_18004_2006_example_2(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 3})
	qrAssertErr(t, bits.PushNumericData([]byte("0123456789012345")), nil)
	qrAssertSliceEq(t,
		bits.IntoBytes(),
		[]byte{
			0b00_10000_0,
			0b00000110,
			0b0_0101011,
			0b001_10101,
			0b00110_111,
			0b0000101_0,
			0b01110101,
			0b00101_000,
		},
	)
}

func TestBitsNumericTests_test_data_too_long_error(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("12345678")), TypesQrError_DataTooLong)
}

func TestBitsAlphanumericTests_test_iso_18004_2006_example(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushAlphanumericData([]byte("AC-42")), nil)
	qrAssertSliceEq(t,
		bits.IntoBytes(),
		[]byte{0b0010_0000, 0b00101_001, 0b11001110, 0b11100111, 0b001_00001, 0b0_0000000},
	)
}

func TestBitsAlphanumericTests_test_micro_qr_unsupported(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushAlphanumericData([]byte("A")), TypesQrError_UnsupportedCharacterSet)
}

func TestBitsAlphanumericTests_test_data_too_long(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 2})
	qrAssertErr(t, bits.PushAlphanumericData([]byte("ABCDEFGH")), TypesQrError_DataTooLong)
}

func TestBitsByteTests_test(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushByteData([]byte("\x12\x34\x56\x78\x9a\xbc\xde\xf0")), nil)
	qrAssertSliceEq(t,
		bits.IntoBytes(),
		[]byte{
			0b0100_0000,
			0b1000_0001,
			0b0010_0011,
			0b0100_0101,
			0b0110_0111,
			0b1000_1001,
			0b1010_1011,
			0b1100_1101,
			0b1110_1111,
			0b0000_0000,
		},
	)
}

func TestBitsByteTests_test_micro_qr_unsupported(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 2})
	qrAssertErr(t, bits.PushByteData([]byte("?")), TypesQrError_UnsupportedCharacterSet)
}

func TestBitsByteTests_test_data_too_long(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 3})
	qrAssertErr(t, bits.PushByteData([]byte("0123456701234567")), TypesQrError_DataTooLong)
}

func TestBitsKanjiTests_test_iso_18004_example(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushKanjiData([]byte("\x93\x5f\xe4\xaa")), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b1000_0000, 0b0010_0110, 0b11001111, 0b1_1101010, 0b101010_00})
}

func TestBitsKanjiTests_test_micro_qr_unsupported(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 2})
	qrAssertErr(t, bits.PushKanjiData([]byte("?")), TypesQrError_UnsupportedCharacterSet)
}

func TestBitsKanjiTests_test_data_too_long(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 3})
	qrAssertErr(t, bits.PushKanjiData([]byte("\x93_\x93_\x93_\x93_\x93_\x93_\x93_\x93_")), TypesQrError_DataTooLong)
}

func TestBitsFinishTests_test_hello_world(t *testing.T) {
	bits := BitsBitsNew(&Version_Normal{F0: 1})
	qrAssertErr(t, bits.PushAlphanumericData([]byte("HELLO WORLD")), nil)
	qrAssertErr(t, bits.PushTerminator(EcLevel_Q), nil)
	qrAssertSliceEq(t,
		bits.IntoBytes(),
		[]byte{
			0b00100000, 0b01011011, 0b00001011, 0b01111000, 0b11010001, 0b01110010, 0b11011100, 0b01001101,
			0b01000011, 0b01000000, 0b11101100, 0b00010001, 0b11101100,
		},
	)
}

func TestBitsFinishTests_test_too_long(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("9999999")), nil)
	qrAssertErr(t, bits.PushTerminator(EcLevel_L), TypesQrError_DataTooLong)
}

func TestBitsFinishTests_test_no_terminator(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("99999")), nil)
	qrAssertErr(t, bits.PushTerminator(EcLevel_L), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b101_11111, 0b00111_110, 0b0011_0000})
}

func TestBitsFinishTests_test_no_padding(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("9999")), nil)
	qrAssertErr(t, bits.PushTerminator(EcLevel_L), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b100_11111, 0b00111_100, 0b1_000_0000})
}

func TestBitsFinishTests_test_micro_version_1_half_byte_padding(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("999")), nil)
	qrAssertErr(t, bits.PushTerminator(EcLevel_L), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b011_11111, 0b00111_000, 0b0000_0000})
}

func TestBitsFinishTests_test_micro_version_1_full_byte_padding(t *testing.T) {
	bits := BitsBitsNew(&Version_Micro{F0: 1})
	qrAssertErr(t, bits.PushNumericData([]byte("")), nil)
	qrAssertErr(t, bits.PushTerminator(EcLevel_L), nil)
	qrAssertSliceEq(t, bits.IntoBytes(), []byte{0b000_000_00, 0b11101100, 0})
}

// bitsEncodeTestsEncode is the `encode` helper of encode_tests.
func bitsEncodeTestsEncode(data []byte, version Version, ecLevel EcLevel) ([]byte, error) {
	bits := BitsBitsNew(version)
	if err := bits.PushOptimalData(data); err != nil {
		return nil, err
	}
	if err := bits.PushTerminator(ecLevel); err != nil {
		return nil, err
	}
	return bits.IntoBytes(), nil
}

func TestBitsEncodeTests_test_alphanumeric(t *testing.T) {
	res, err := bitsEncodeTestsEncode([]byte("HELLO WORLD"), &Version_Normal{F0: 1}, EcLevel_Q)
	qrAssertErr(t, err, nil)
	qrAssertSliceEq(t,
		res,
		[]byte{
			0b00100000, 0b01011011, 0b00001011, 0b01111000, 0b11010001, 0b01110010, 0b11011100, 0b01001101,
			0b01000011, 0b01000000, 0b11101100, 0b00010001, 0b11101100,
		},
	)
}

func TestBitsEncodeTests_test_auto_mode_switch(t *testing.T) {
	res, err := bitsEncodeTestsEncode([]byte("123A"), &Version_Micro{F0: 2}, EcLevel_L)
	qrAssertErr(t, err, nil)
	qrAssertSliceEq(t, res, []byte{0b0_0011_000, 0b1111011_1, 0b001_00101, 0b0_00000_00, 0b11101100})
}

func TestBitsEncodeTests_test_too_long(t *testing.T) {
	_, err := bitsEncodeTestsEncode([]byte(">>>>>>>>"), &Version_Normal{F0: 1}, EcLevel_H)
	qrAssertErr(t, err, TypesQrError_DataTooLong)
}

func TestBitsEncodeAutoTests_test_find_min_version(t *testing.T) {
	qrAssertVersion(t, bitsFindMinVersion(60, EcLevel_L), &Version_Normal{F0: 1})
	qrAssertVersion(t, bitsFindMinVersion(200, EcLevel_L), &Version_Normal{F0: 2})
	qrAssertVersion(t, bitsFindMinVersion(200, EcLevel_H), &Version_Normal{F0: 3})
	qrAssertVersion(t, bitsFindMinVersion(20000, EcLevel_L), &Version_Normal{F0: 37})
	qrAssertVersion(t, bitsFindMinVersion(640, EcLevel_L), &Version_Normal{F0: 4})
	qrAssertVersion(t, bitsFindMinVersion(641, EcLevel_L), &Version_Normal{F0: 5})
	qrAssertVersion(t, bitsFindMinVersion(999999, EcLevel_H), &Version_Normal{F0: 40})
}

func TestBitsEncodeAutoTests_test_alpha_q(t *testing.T) {
	bits, err := BitsEncodeAuto([]byte("HELLO WORLD"), EcLevel_Q)
	qrAssertErr(t, err, nil)
	qrAssertVersion(t, bits.Version(), &Version_Normal{F0: 1})
}

func TestBitsEncodeAutoTests_test_alpha_h(t *testing.T) {
	bits, err := BitsEncodeAuto([]byte("HELLO WORLD"), EcLevel_H)
	qrAssertErr(t, err, nil)
	qrAssertVersion(t, bits.Version(), &Version_Normal{F0: 2})
}

func TestBitsEncodeAutoTests_test_mixed(t *testing.T) {
	bits, err := BitsEncodeAuto([]byte("This is a mixed data test. 1234567890"), EcLevel_H)
	qrAssertErr(t, err, nil)
	qrAssertVersion(t, bits.Version(), &Version_Normal{F0: 4})
}
