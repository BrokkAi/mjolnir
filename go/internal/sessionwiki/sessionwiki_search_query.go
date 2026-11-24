// Port of mj-sessionwiki/src/sessionwiki/search_query.rs.
//
// Query syntax shared by live transcript search and its previews.

package sessionwiki

import (
	"strings"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
)

// queryTerms is Rust's `terms`: SessionWiki's query syntax. Whitespace
// separates required terms, double quotes group a phrase, and doubled quotes
// escape a literal quote. An unfinished phrase consumes the remainder, so
// search works while typing. SessionWiki's parser is private; index
// integration tests protect agreement.
func queryTerms(query string) []string {
	characters := []rune(brokksessionwiki.NFC(query))
	var terms []string
	var current strings.Builder
	quoted := false
	for index := 0; index < len(characters); index++ {
		character := characters[index]
		switch {
		case character == '"':
			if quoted && index+1 < len(characters) && characters[index+1] == '"' {
				index++
				current.WriteRune('"')
			} else {
				pushTerm(&terms, &current)
				quoted = !quoted
			}
		// Rust's char::is_whitespace is the Unicode White_Space property,
		// which unicode.IsSpace also reads.
		case !quoted && unicode.IsSpace(character):
			pushTerm(&terms, &current)
		default:
			current.WriteRune(character)
		}
	}
	pushTerm(&terms, &current)
	return terms
}

// pushTerm ends the current term, keeping it only when it is not blank.
func pushTerm(terms *[]string, current *strings.Builder) {
	if strings.TrimSpace(current.String()) != "" {
		*terms = append(*terms, current.String())
	}
	current.Reset()
}
