// Port of unicase 2.9.0 src/lib.rs, src/ascii.rs and src/unicode/mod.rs

package pulldowncmark

import (
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// The crate keys its reference and footnote definitions by
// `UniCase<CowStr>`: a string that compares and hashes without regard to
// case, with ASCII rules when the string is ASCII and Unicode case folding
// otherwise.
//
// Two UniCase values are equal exactly when their folded character sequences
// are equal, whichever of the two encodings each has: `Ascii == Ascii` is
// `eq_ignore_ascii_case`, every other pairing compares the characters
// `chars().flat_map(lookup)` yields, and for an ASCII string those are its
// characters in lower case. The Hash impls write the same bytes for equal
// sequences. A Go map keyed by the folded string is therefore the same
// collection as Rust's `HashMap<UniCase<_>, _>`, and that is how this package
// holds it.

type unicaseFoldEntry struct {
	from rune
	to   string
}

// unicaseLookup is unicode::map::lookup followed by the Fold iterator: the
// characters c folds to, in the order the crate yields them, and whether the
// fold differs from c itself.
func unicaseLookup(c rune) (string, bool) {
	left := 0
	right := len(unicaseFoldTable)
	for left < right {
		mid := left + (right-left)/2
		switch from := unicaseFoldTable[mid].from; {
		case from < c:
			left = mid + 1
		case from > c:
			right = mid
		default:
			return unicaseFoldTable[mid].to, true
		}
	}
	return "", false
}

// unicaseToFoldedCase is `UniCase::new(s).to_folded_case()`: the key under
// which s is stored and looked up.
func unicaseToFoldedCase(s string) string {
	if strs.IsAscii(s) {
		// Encoding::Ascii: `s.to_ascii_lowercase()`.
		for i := 0; i < len(s); i++ {
			if 'A' <= s[i] && s[i] <= 'Z' {
				return strings.ToLower(s)
			}
		}
		return s
	}
	// Encoding::Unicode: `s.chars().flat_map(lookup).collect()`.
	var folded strings.Builder
	folded.Grow(len(s))
	for i := 0; i < len(s); {
		c, size := utf8.DecodeRuneInString(s[i:])
		if to, ok := unicaseLookup(c); ok {
			folded.WriteString(to)
		} else {
			folded.WriteString(s[i : i+size])
		}
		i += size
	}
	return folded.String()
}
