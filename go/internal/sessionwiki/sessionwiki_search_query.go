// Port of mj-sessionwiki/src/sessionwiki/search_query.rs.
//
// Query syntax shared by live transcript search and its previews.

package sessionwiki

import "strings"

// queryTerms is Rust's `terms`: SessionWiki's query syntax. Whitespace
// separates required terms, double quotes group a phrase, and doubled quotes
// escape a literal quote. An unfinished phrase consumes the remainder, so
// search works while typing. SessionWiki's parser is private; index
// integration tests protect agreement.
func queryTerms(query string) []string {
	panic("unported: mj-sessionwiki/src/sessionwiki/search_query.rs terms")
}

// pushTerm ends the current term, keeping it only when it is not blank.
func pushTerm(terms *[]string, current *strings.Builder) {
	if strings.TrimSpace(current.String()) != "" {
		*terms = append(*terms, current.String())
	}
	current.Reset()
}
