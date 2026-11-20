// Port of qrcode 0.14.1 src/lib.rs (tests)

package qrcode

import "testing"

// The image_tests, svg_tests and pic_tests modules need the `image`, `svg`
// and `pic` features, which Mjolnir's build turns off.

func TestLib_test_annex_i_qr(t *testing.T) {
	// This uses the ISO Annex I as test vector.
	code, err := QrCodeWithVersion([]byte("01234567"), &Version_Normal{F0: 1}, EcLevel_M)
	qrAssertErr(t, err, nil)
	qrAssertEq(t, code.ToDebugStr('#', '.'), "#######..#.##.#######\n"+
		"#.....#..####.#.....#\n"+
		"#.###.#.#.....#.###.#\n"+
		"#.###.#.##....#.###.#\n"+
		"#.###.#.#.###.#.###.#\n"+
		"#.....#.#...#.#.....#\n"+
		"#######.#.#.#.#######\n"+
		"........#..##........\n"+
		"#.#####..#..#.#####..\n"+
		"...#.#.##.#.#..#.##..\n"+
		"..#...##.#.#.#..#####\n"+
		"....#....#.....####..\n"+
		"...######..#.#..#....\n"+
		"........#.#####..##..\n"+
		"#######..##.#.##.....\n"+
		"#.....#.#.#####...#.#\n"+
		"#.###.#.#...#..#.##..\n"+
		"#.###.#.##..#..#.....\n"+
		"#.###.#.#.##.#..#.#..\n"+
		"#.....#........##.##.\n"+
		"#######.####.#..#.#..")
}

func TestLib_test_annex_i_micro_qr(t *testing.T) {
	code, err := QrCodeWithVersion([]byte("01234567"), &Version_Micro{F0: 2}, EcLevel_L)
	qrAssertErr(t, err, nil)
	qrAssertEq(t, code.ToDebugStr('#', '.'), "#######.#.#.#\n"+
		"#.....#.###.#\n"+
		"#.###.#..##.#\n"+
		"#.###.#..####\n"+
		"#.###.#.###..\n"+
		"#.....#.#...#\n"+
		"#######..####\n"+
		".........##..\n"+
		"##.#....#...#\n"+
		".##.#.#.#.#.#\n"+
		"###..#######.\n"+
		"...#.#....##.\n"+
		"###.#..##.###")
}
