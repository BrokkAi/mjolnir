// Port of base64 0.22.1 src/alphabet.rs

package base64

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestAlphabet_detects_duplicate_start(t *testing.T) {
	assertEq[error](t,
		&AlphabetParseAlphabetError_DuplicatedByte{F0: 'A'},
		rs.UnwrapErr(AlphabetAlphabetNew("AACDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/")),
	)
}

func TestAlphabet_detects_duplicate_end(t *testing.T) {
	assertEq[error](t,
		&AlphabetParseAlphabetError_DuplicatedByte{F0: '/'},
		rs.UnwrapErr(AlphabetAlphabetNew("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789//")),
	)
}

func TestAlphabet_detects_duplicate_middle(t *testing.T) {
	assertEq[error](t,
		&AlphabetParseAlphabetError_DuplicatedByte{F0: 'Z'},
		rs.UnwrapErr(AlphabetAlphabetNew("ABCDEFGHIJKLMNOPQRSTUVWXYZZbcdefghijklmnopqrstuvwxyz0123456789+/")),
	)
}

func TestAlphabet_detects_length(t *testing.T) {
	assertEq[error](t,
		&AlphabetParseAlphabetError_InvalidLength{},
		rs.UnwrapErr(AlphabetAlphabetNew(
			"xxxxxxxxxABCDEFGHIJKLMNOPQRSTUVWXYZZbcdefghijklmnopqrstuvwxyz0123456789+/",
		)),
	)
}

func TestAlphabet_detects_padding(t *testing.T) {
	assertEq[error](t,
		&AlphabetParseAlphabetError_ReservedByte{F0: '='},
		rs.UnwrapErr(AlphabetAlphabetNew("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+=")),
	)
}

func TestAlphabet_detects_unprintable(t *testing.T) {
	// form feed
	assertEq[error](t,
		&AlphabetParseAlphabetError_UnprintableByte{F0: 0xc},
		rs.UnwrapErr(AlphabetAlphabetNew("\x0cBCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/")),
	)
}

func TestAlphabet_same_as_unchecked(t *testing.T) {
	assertEq(t,
		AlphabetStandard,
		rs.Unwrap(AlphabetAlphabetTryFromStr("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/")),
	)
}

func TestAlphabet_str_same_as_input(t *testing.T) {
	alphabet := "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
	a := rs.Unwrap(AlphabetAlphabetTryFromStr(alphabet))
	assertEq(t, alphabet, a.AsStr())
}
