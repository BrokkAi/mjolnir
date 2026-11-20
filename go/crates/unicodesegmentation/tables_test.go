package unicodesegmentation

// src/tables.rs has no tests. This file checks a claim tables.go makes.

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// TestStdAndCrateTablesAgree checks that the two ways `util::is_alphabetic`
// and `util::is_numeric` can answer give the same answers. They take the
// standard library's tables when its Unicode version is the crate's, as it is
// for the toolchain Mjolnir is built with, and the crate's own tables
// (`derived_property::Alphabetic`, `general_category::N`) otherwise. Agreement
// means the choice cannot change which pieces `unicode_words` keeps.
func TestStdAndCrateTablesAgree(t *testing.T) {
	if tablesUnicodeVersionU8 != strs.CharUnicodeVersion {
		t.Fatalf("char::UNICODE_VERSION is %v and the crate's is %v: is_alphabetic and is_numeric read the crate's tables",
			strs.CharUnicodeVersion, tablesUnicodeVersionU8)
	}
	differences := 0
	for c := rune(0x80); c <= 0x10FFFF; c++ {
		if c >= 0xD800 && c <= 0xDFFF {
			continue
		}
		if std, crate := strs.IsAlphabetic(c), tablesDerivedPropertyAlphabetic(c); std != crate {
			differences++
			if differences <= 20 {
				t.Errorf("U+%04X: char::is_alphabetic is %v, derived_property::Alphabetic is %v", c, std, crate)
			}
		}
		if std, crate := strs.IsNumeric(c), tablesGeneralCategoryN(c); std != crate {
			differences++
			if differences <= 20 {
				t.Errorf("U+%04X: char::is_numeric is %v, general_category::N is %v", c, std, crate)
			}
		}
	}
	if differences > 20 {
		t.Errorf("%d more differences", differences-20)
	}
}
