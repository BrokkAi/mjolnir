// Port of qrcode 0.14.1 src/ec.rs (tests)

package qrcode

import "testing"

func TestEcEcTests_test_poly_mod_1(t *testing.T) {
	res := EcCreateErrorCorrectionCode([]byte(" [\x0bx\xd1r\xdcMC@\xec\x11\xec\x11\xec\x11"), 10)
	qrAssertEq(t, string(res), "\xc4#'w\xeb\xd7\xe7\xe2]\x17")
}

func TestEcEcTests_test_poly_mod_2(t *testing.T) {
	res := EcCreateErrorCorrectionCode([]byte(" [\x0bx\xd1r\xdcMC@\xec\x11\xec"), 13)
	qrAssertEq(t, string(res), "\xa8H\x16R\xd96\x9c\x00.\x0f\xb4z\x10")
}

func TestEcEcTests_test_poly_mod_3(t *testing.T) {
	res := EcCreateErrorCorrectionCode([]byte("CUF\x86W&U\xc2w2\x06\x12\x06g&"), 18)
	qrAssertEq(t, string(res), "\xd5\xc7\x0b-s\xf7\xf1\xdf\xe5\xf8\x9au\x9aoV\xa1o'")
}

func TestEc_test_interleave(t *testing.T) {
	res := ecInterleave([][]byte{[]byte("1234"), []byte("5678"), []byte("abcdef"), []byte("ghijkl")})
	qrAssertEq(t, string(res), "15ag26bh37ci48djekfl")
}

func TestEcConstructCodewordsTest_test_add_ec_simple(t *testing.T) {
	msg := []byte(" [\x0bx\xd1r\xdcMC@\xec\x11\xec\x11\xec\x11")
	blocksVec, ecVec, err := EcConstructCodewords(msg, &Version_Normal{F0: 1}, EcLevel_M)
	qrAssertErr(t, err, nil)
	qrAssertEq(t, string(blocksVec), string(msg))
	qrAssertEq(t, string(ecVec), "\xc4#'w\xeb\xd7\xe7\xe2]\x17")
}

func TestEcConstructCodewordsTest_test_add_ec_complex(t *testing.T) {
	msg := []byte("CUF\x86W&U\xc2w2\x06\x12\x06g&\xf6\xf6B\x07v\x86\xf2\x07&V\x16\xc6\xc7\x92\x06" +
		"\xb6\xe6\xf7w2\x07v\x86W&R\x06\x86\x972\x07F\xf7vV\xc2\x06\x972\x10\xec\x11\xec" +
		"\x11\xec\x11\xec")
	expectedBlocks := "C\xf6\xb6FU\xf6\xe6\xf7FB\xf7v\x86\x07wVWv2\xc2&\x86\x07\x06U\xf2v" +
		"\x97\xc2\x07\x862w&W\x102V&\xec\x06\x16R\x11\x12\xc6\x06\xec\x06" +
		"\xc7\x86\x11g\x92\x97\xec&\x062\x11\x07\xec"
	expectedEc := "\xd5W\x94\xeb\xc7\xcct\x9f\x0b`\xb1\x05-<\xd4\xads\xcaL\x18\xf7\xb6\x85" +
		"\x93\xf1|K;\xdf\x9d\xf2!\xe5\xc8\xeej\xf8\x86L(\x9a\x1b\xc3\xffu\x81" +
		"\xe6\xac\x9a\xd1\xbdRo\x11\n\x02V\xa3l\x83\xa1\xa3\xf0 ox\xc0\xb2'\x85" +
		"\x8d\xec"

	blocksVec, ecVec, err := EcConstructCodewords(msg, &Version_Normal{F0: 5}, EcLevel_Q)
	qrAssertErr(t, err, nil)
	qrAssertEq(t, string(blocksVec), expectedBlocks)
	qrAssertEq(t, string(ecVec), expectedEc)
}

// ecMaxAllowedErrorsCase is `assert_eq!(Ok(want), max_allowed_errors(..))`.
func ecMaxAllowedErrorsCase(t *testing.T, want int, version Version, ecLevel EcLevel) {
	t.Helper()
	got, err := EcMaxAllowedErrors(version, ecLevel)
	qrAssertErr(t, err, nil)
	qrAssertEq(t, want, got)
}

func TestEcMaxAllowedErrorsTest_test_low_versions(t *testing.T) {
	ecMaxAllowedErrorsCase(t, 0, &Version_Micro{F0: 1}, EcLevel_L)

	ecMaxAllowedErrorsCase(t, 1, &Version_Micro{F0: 2}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 2, &Version_Micro{F0: 2}, EcLevel_M)

	ecMaxAllowedErrorsCase(t, 2, &Version_Micro{F0: 3}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 4, &Version_Micro{F0: 3}, EcLevel_M)

	ecMaxAllowedErrorsCase(t, 3, &Version_Micro{F0: 4}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 5, &Version_Micro{F0: 4}, EcLevel_M)
	ecMaxAllowedErrorsCase(t, 7, &Version_Micro{F0: 4}, EcLevel_Q)

	ecMaxAllowedErrorsCase(t, 2, &Version_Normal{F0: 1}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 4, &Version_Normal{F0: 1}, EcLevel_M)
	ecMaxAllowedErrorsCase(t, 6, &Version_Normal{F0: 1}, EcLevel_Q)
	ecMaxAllowedErrorsCase(t, 8, &Version_Normal{F0: 1}, EcLevel_H)

	ecMaxAllowedErrorsCase(t, 4, &Version_Normal{F0: 2}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 8, &Version_Normal{F0: 2}, EcLevel_M)
	ecMaxAllowedErrorsCase(t, 11, &Version_Normal{F0: 2}, EcLevel_Q)
	ecMaxAllowedErrorsCase(t, 14, &Version_Normal{F0: 2}, EcLevel_H)

	ecMaxAllowedErrorsCase(t, 7, &Version_Normal{F0: 3}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 13, &Version_Normal{F0: 3}, EcLevel_M)
	ecMaxAllowedErrorsCase(t, 18, &Version_Normal{F0: 3}, EcLevel_Q)
	ecMaxAllowedErrorsCase(t, 22, &Version_Normal{F0: 3}, EcLevel_H)

	ecMaxAllowedErrorsCase(t, 10, &Version_Normal{F0: 4}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 18, &Version_Normal{F0: 4}, EcLevel_M)
	ecMaxAllowedErrorsCase(t, 26, &Version_Normal{F0: 4}, EcLevel_Q)
	ecMaxAllowedErrorsCase(t, 32, &Version_Normal{F0: 4}, EcLevel_H)
}

func TestEcMaxAllowedErrorsTest_test_high_versions(t *testing.T) {
	ecMaxAllowedErrorsCase(t, 375, &Version_Normal{F0: 40}, EcLevel_L)
	ecMaxAllowedErrorsCase(t, 686, &Version_Normal{F0: 40}, EcLevel_M)
	ecMaxAllowedErrorsCase(t, 1020, &Version_Normal{F0: 40}, EcLevel_Q)
	ecMaxAllowedErrorsCase(t, 1215, &Version_Normal{F0: 40}, EcLevel_H)
}
