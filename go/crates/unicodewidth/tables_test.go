// Port of unicode-width 0.2.2 src/tables.rs

// The test module of src/tables.rs. Its table NORMALIZATION_TEST (20034
// entries taken from Unicode's NormalizationTest.txt) is read from
// testdata/normalization_test.txt, which gen.go writes from the crate's file.

package unicodewidth

import (
	"bufio"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"
)

func tablesStrWidthTest(s string, init tablesWidthInfo) int {
	// A Rust `s.chars().rfold(..)`; `sum.checked_add(..).unwrap()` cannot
	// overflow for these strings.
	sum := 0
	nextInfo := init
	for end := len(s); end > 0; {
		c, size := utf8.DecodeLastRuneInString(s[:end])
		end -= size
		add, info := tablesWidthInStr(c, nextInfo)
		sum += int(add)
		nextInfo = info
	}
	return sum
}

// cfg(feature = "cjk")
func tablesStrWidthTestCjk(s string, init tablesWidthInfo) int {
	sum := 0
	nextInfo := init
	for end := len(s); end > 0; {
		c, size := utf8.DecodeLastRuneInString(s[:end])
		end -= size
		add, info := tablesWidthInStrCjk(c, nextInfo)
		sum += int(add)
		nextInfo = info
	}
	return sum
}

func TestTables_test_normalization(t *testing.T) {
	for _, entry := range tablesNormalizationTest(t) {
		orig, nfc, nfd, nfkc, nfkd := entry[0], entry[1], entry[2], entry[3], entry[4]
		for _, init := range tablesNormalizationTestWidths {
			if tablesStrWidthTest(orig, init) != tablesStrWidthTest(nfc, init) {
				t.Fatalf("width of X = %q differs from toNFC(X) = %q with mode WidthInfo(%X)", orig, nfc, uint16(init))
			}
			if tablesStrWidthTest(orig, init) != tablesStrWidthTest(nfd, init) {
				t.Fatalf("width of X = %q differs from toNFD(X) = %q with mode WidthInfo(%X)", orig, nfd, uint16(init))
			}
			if tablesStrWidthTest(nfkc, init) != tablesStrWidthTest(nfkd, init) {
				t.Fatalf("width of toNFKC(X) = %q differs from toNFKD(X) = %q with mode WidthInfo(%X)", nfkc, nfkd, uint16(init))
			}
		}

		// cfg(feature = "cjk")
		for _, init := range tablesNormalizationTestWidthsCjk {
			if tablesStrWidthTestCjk(orig, init) != tablesStrWidthTestCjk(nfc, init) {
				t.Fatalf("CJK width of X = %q differs from toNFC(X) = %q with mode WidthInfo(%X)", orig, nfc, uint16(init))
			}
			if tablesStrWidthTestCjk(orig, init) != tablesStrWidthTestCjk(nfd, init) {
				t.Fatalf("CJK width of X = %q differs from toNFD(X) = %q with mode WidthInfo(%X)", orig, nfd, uint16(init))
			}
			if tablesStrWidthTestCjk(nfkc, init) != tablesStrWidthTestCjk(nfkd, init) {
				t.Fatalf("CJK width of toNFKC(X) = %q differs from toNFKD(X) = %q with mode WidthInfo(%d)", nfkc, nfkd, uint16(init))
			}
		}
	}
}

var tablesNormalizationTestWidths = [41]tablesWidthInfo{
	tablesWidthInfoDefault,
	tablesWidthInfoLineFeed,
	tablesWidthInfoEmojiModifier,
	tablesWidthInfoRegionalIndicator,
	tablesWidthInfoSeveralRegionalIndicator,
	tablesWidthInfoEmojiPresentation,
	tablesWidthInfoZwjEmojiPresentation,
	tablesWidthInfoVs16ZwjEmojiPresentation,
	tablesWidthInfoKeycapZwjEmojiPresentation,
	tablesWidthInfoVs16KeycapZwjEmojiPresentation,
	tablesWidthInfoRegionalIndicatorZwjPresentation,
	tablesWidthInfoEvenRegionalIndicatorZwjPresentation,
	tablesWidthInfoOddRegionalIndicatorZwjPresentation,
	tablesWidthInfoTagEndZwjEmojiPresentation,
	tablesWidthInfoTagD1EndZwjEmojiPresentation,
	tablesWidthInfoTagD2EndZwjEmojiPresentation,
	tablesWidthInfoTagD3EndZwjEmojiPresentation,
	tablesWidthInfoTagA1EndZwjEmojiPresentation,
	tablesWidthInfoTagA2EndZwjEmojiPresentation,
	tablesWidthInfoTagA3EndZwjEmojiPresentation,
	tablesWidthInfoTagA4EndZwjEmojiPresentation,
	tablesWidthInfoTagA5EndZwjEmojiPresentation,
	tablesWidthInfoTagA6EndZwjEmojiPresentation,
	tablesWidthInfoKiratRaiVowelSignE,
	tablesWidthInfoKiratRaiVowelSignAi,
	tablesWidthInfoVariationSelector12Or3,
	tablesWidthInfoVariationSelector15,
	tablesWidthInfoVariationSelector16,
	tablesWidthInfoJoiningGroupAlef,
	tablesWidthInfoHebrewLetterLamed,
	tablesWidthInfoZwjHebrewLetterLamed,
	tablesWidthInfoBugineseLetterYa,
	tablesWidthInfoZwjBugineseLetterYa,
	tablesWidthInfoBugineseVowelSignIZwjLetterYa,
	tablesWidthInfoTifinaghConsonant,
	tablesWidthInfoZwjTifinaghConsonant,
	tablesWidthInfoTifinaghJoinerConsonant,
	tablesWidthInfoLisuToneLetterMyaNaJeu,
	tablesWidthInfoOldTurkicLetterOrkhonI,
	tablesWidthInfoZwjOldTurkicLetterOrkhonI,
	tablesWidthInfoKhmerCoengEligibleLetter,
}

// cfg(feature = "cjk")
var tablesNormalizationTestWidthsCjk = [42]tablesWidthInfo{
	tablesWidthInfoDefault,
	tablesWidthInfoLineFeed,
	tablesWidthInfoEmojiModifier,
	tablesWidthInfoRegionalIndicator,
	tablesWidthInfoSeveralRegionalIndicator,
	tablesWidthInfoEmojiPresentation,
	tablesWidthInfoZwjEmojiPresentation,
	tablesWidthInfoVs16ZwjEmojiPresentation,
	tablesWidthInfoKeycapZwjEmojiPresentation,
	tablesWidthInfoVs16KeycapZwjEmojiPresentation,
	tablesWidthInfoRegionalIndicatorZwjPresentation,
	tablesWidthInfoEvenRegionalIndicatorZwjPresentation,
	tablesWidthInfoOddRegionalIndicatorZwjPresentation,
	tablesWidthInfoTagEndZwjEmojiPresentation,
	tablesWidthInfoTagD1EndZwjEmojiPresentation,
	tablesWidthInfoTagD2EndZwjEmojiPresentation,
	tablesWidthInfoTagD3EndZwjEmojiPresentation,
	tablesWidthInfoTagA1EndZwjEmojiPresentation,
	tablesWidthInfoTagA2EndZwjEmojiPresentation,
	tablesWidthInfoTagA3EndZwjEmojiPresentation,
	tablesWidthInfoTagA4EndZwjEmojiPresentation,
	tablesWidthInfoTagA5EndZwjEmojiPresentation,
	tablesWidthInfoTagA6EndZwjEmojiPresentation,
	tablesWidthInfoKiratRaiVowelSignE,
	tablesWidthInfoKiratRaiVowelSignAi,
	tablesWidthInfoVariationSelector12Or3,
	tablesWidthInfoVariationSelector16,
	tablesWidthInfoJoiningGroupAlef,
	tablesWidthInfoCombiningLongSolidusOverlay,
	tablesWidthInfoSolidusOverlayAlef,
	tablesWidthInfoHebrewLetterLamed,
	tablesWidthInfoZwjHebrewLetterLamed,
	tablesWidthInfoBugineseLetterYa,
	tablesWidthInfoZwjBugineseLetterYa,
	tablesWidthInfoBugineseVowelSignIZwjLetterYa,
	tablesWidthInfoTifinaghConsonant,
	tablesWidthInfoZwjTifinaghConsonant,
	tablesWidthInfoTifinaghJoinerConsonant,
	tablesWidthInfoLisuToneLetterMyaNaJeu,
	tablesWidthInfoOldTurkicLetterOrkhonI,
	tablesWidthInfoZwjOldTurkicLetterOrkhonI,
	tablesWidthInfoKhmerCoengEligibleLetter,
}

// tablesNormalizationTest is the crate's `NORMALIZATION_TEST: [(&str, &str,
// &str, &str, &str); 20034]`, read from testdata/normalization_test.txt.
func tablesNormalizationTest(t *testing.T) [][5]string {
	t.Helper()
	file, err := os.Open(filepath.Join("testdata", "normalization_test.txt"))
	if err != nil {
		t.Fatalf("run `go generate` first to write `normalization_test.txt`: %v", err)
	}
	defer file.Close()
	entries := make([][5]string, 0, 20034)
	lines := bufio.NewScanner(file)
	for lines.Scan() {
		line := lines.Text()
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		columns := strings.Split(strings.TrimSuffix(line, ";"), ";")
		if len(columns) != 5 {
			t.Fatalf("normalization_test.txt: %d columns in %q", len(columns), line)
		}
		var entry [5]string
		for index, column := range columns {
			var text strings.Builder
			for _, cp := range strings.Fields(column) {
				value, err := strconv.ParseUint(cp, 16, 32)
				if err != nil {
					t.Fatalf("normalization_test.txt: %v in %q", err, line)
				}
				text.WriteRune(rune(value))
			}
			entry[index] = text.String()
		}
		entries = append(entries, entry)
	}
	if err := lines.Err(); err != nil {
		t.Fatal(err)
	}
	if len(entries) != 20034 {
		t.Fatalf("normalization_test.txt holds %d entries, expected 20034", len(entries))
	}
	return entries
}
