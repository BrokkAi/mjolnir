// Port of mj-sessionwiki/src/sessionwiki/transcript_grep.rs.
//
// Serves Mjolnir preview passages and history tools; this is not part of
// upstream SessionWiki.

package sessionwiki

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
)

// grepOpts is how much of a session a transcript search returns.
type grepOpts struct {
	// contextMessages is the messages of context kept on each side of a
	// matching message.
	contextMessages int
	// chars is the characters kept per message, windowed around its first
	// match with a quarter of the budget as lead-in. Zero keeps the whole
	// message.
	chars int
	// maxMatches stops after this many matching messages; nil returns all of
	// them.
	maxMatches *int
	// anchorRoles is the roles whose messages may anchor a hit. Other roles
	// may still appear as context around a hit.
	anchorRoles []brokksessionwiki.Role
}

// defaultGrepOpts is Rust's GrepOpts::default.
func defaultGrepOpts() grepOpts {
	return grepOpts{
		anchorRoles: []brokksessionwiki.Role{
			brokksessionwiki.RoleUser, brokksessionwiki.RoleAssistant, brokksessionwiki.RoleTool,
		},
	}
}

// grepHit is one message returned by a transcript search.
type grepHit struct {
	i    int
	role brokksessionwiki.Role
	ts   *time.Time
	// text is the redacted, NFC-normalized, trimmed, and optionally windowed
	// message text.
	text string
	// matches are byte ranges of matches in text; empty for context
	// messages.
	matches   [][2]int
	truncated bool
	// omittedBefore is the number of messages skipped before this passage
	// group.
	omittedBefore int
}

type grepResult struct {
	hits         []grepHit
	omittedAfter int
}

// grepSession is the matching messages of session and their surrounding
// context.
func grepSession(session *brokksessionwiki.Session, pattern string, opts *grepOpts) grepResult {
	panic("unported: mj-sessionwiki/src/sessionwiki/transcript_grep.rs grep_session")
}

// matchesIn is the ranges of every term, only when all terms occur in this
// message.
func matchesIn(text string, needles []string) [][2]int {
	panic("unported: mj-sessionwiki/src/sessionwiki/transcript_grep.rs matches_in")
}

// excerpt caps one message around its first match and rebases the match
// ranges.
func excerpt(text string, hits [][2]int, chars int) (string, [][2]int, bool) {
	panic("unported: mj-sessionwiki/src/sessionwiki/transcript_grep.rs excerpt")
}

// byteOfChar is the byte offset of character charIndex in text, or its
// length past the end.
func byteOfChar(text string, charIndex int) int {
	index := 0
	for offset := range text {
		if index == charIndex {
			return offset
		}
		index++
	}
	return len(text)
}
