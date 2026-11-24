// Port of mj-sessionwiki/src/sessionwiki/transcript_grep.rs.
//
// Serves Mjolnir preview passages and history tools; this is not part of
// upstream SessionWiki.

package sessionwiki

import (
	"slices"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

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
	var needles []string
	for _, term := range queryTerms(pattern) {
		needles = append(needles, grepLowercase(term))
	}
	if len(needles) == 0 || len(session.Messages) == 0 {
		return grepResult{}
	}
	texts := make([]string, len(session.Messages))
	for index := range session.Messages {
		texts[index] = brokksessionwiki.Redact(brokksessionwiki.NFC(strings.TrimSpace(session.Messages[index].Text)))
	}
	found := make([][][2]int, len(texts))
	for index := range texts {
		if slices.Contains(opts.anchorRoles, session.Messages[index].Role) {
			found[index] = matchesIn(texts[index], needles)
		}
	}

	last := len(texts) - 1
	remaining := -1
	if opts.maxMatches != nil {
		remaining = *opts.maxMatches
	}
	// groups are inclusive (start, end) message ranges.
	var groups [][2]int
	for index := range texts {
		if len(found[index]) == 0 {
			continue
		}
		if remaining == 0 {
			break
		}
		if remaining > 0 {
			remaining--
		}
		start := max(index-opts.contextMessages, 0)
		end := min(index+opts.contextMessages, last)
		if len(groups) > 0 && start <= groups[len(groups)-1][1]+1 {
			previous := &groups[len(groups)-1]
			previous[1] = max(previous[1], end)
		} else {
			groups = append(groups, [2]int{start, end})
		}
	}
	if len(groups) == 0 {
		return grepResult{}
	}

	var hits []grepHit
	previousEnd := -1
	for _, group := range groups {
		start, end := group[0], group[1]
		omitted := start
		if previousEnd >= 0 {
			omitted = start - previousEnd - 1
		}
		for index := start; index <= end; index++ {
			text, matches, truncated := excerpt(texts[index], found[index], opts.chars)
			hit := grepHit{
				i:         index,
				role:      session.Messages[index].Role,
				ts:        session.Messages[index].Ts,
				text:      text,
				matches:   matches,
				truncated: truncated,
			}
			if index == start {
				hit.omittedBefore = omitted
			}
			hits = append(hits, hit)
		}
		previousEnd = end
	}
	return grepResult{hits: hits, omittedAfter: last - previousEnd}
}

// matchesIn is the ranges of every term, only when all terms occur in this
// message.
func matchesIn(text string, needles []string) [][2]int {
	var lowered strings.Builder
	lowered.Grow(len(text))
	// origin maps each byte of lowered back to the start of the character
	// of text it came from.
	origin := make([]int, 0, len(text)+1)
	for index, character := range text {
		before := lowered.Len()
		for _, lower := range grepCharLowercase(character) {
			lowered.WriteRune(lower)
		}
		for range lowered.Len() - before {
			origin = append(origin, index)
		}
	}
	origin = append(origin, len(text))
	folded := lowered.String()

	var hits [][2]int
	for _, needle := range needles {
		from := 0
		before := len(hits)
		for {
			offset := strings.Index(folded[from:], needle)
			if offset < 0 {
				break
			}
			start := from + offset
			from = start + len(needle)
			begin := origin[start]
			end := origin[from]
			if end <= begin {
				_, size := utf8.DecodeRuneInString(text[begin:])
				end = begin + size
			}
			hits = append(hits, [2]int{begin, end})
		}
		if len(hits) == before {
			return nil
		}
	}
	slices.SortFunc(hits, func(a, b [2]int) int {
		if a[0] != b[0] {
			return a[0] - b[0]
		}
		return a[1] - b[1]
	})
	var merged [][2]int
	for _, hit := range hits {
		if len(merged) > 0 && hit[0] <= merged[len(merged)-1][1] {
			previous := &merged[len(merged)-1]
			previous[1] = max(previous[1], hit[1])
		} else {
			merged = append(merged, hit)
		}
	}
	return merged
}

// excerpt caps one message around its first match and rebases the match
// ranges.
func excerpt(text string, hits [][2]int, chars int) (string, [][2]int, bool) {
	total := utf8.RuneCountInString(text)
	if chars == 0 || total <= chars {
		return text, slices.Clone(hits), false
	}
	first := 0
	if len(hits) > 0 {
		first = utf8.RuneCountInString(text[:hits[0][0]])
	}
	windowStart := min(max(first-chars/4, 0), total-chars)
	begin := byteOfChar(text, windowStart)
	end := byteOfChar(text, windowStart+chars)
	var kept [][2]int
	for _, hit := range hits {
		start := max(hit[0], begin)
		stop := min(hit[1], end)
		if start < stop {
			kept = append(kept, [2]int{start - begin, stop - begin})
		}
	}
	return text[begin:end], kept, true
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

// grepCharLowercase is Rust's char::to_lowercase: the full lowercase
// mapping, which differs from the simple one unicode.ToLower reads only for
// 'İ'.
func grepCharLowercase(character rune) []rune {
	if character == 'İ' {
		return []rune{'i', '̇'}
	}
	return []rune{unicode.ToLower(character)}
}

// grepLowercase is Rust's str::to_lowercase: every character's full
// lowercase mapping, with a capital sigma that ends a word lowered to its
// final form.
func grepLowercase(text string) string {
	var lowered strings.Builder
	lowered.Grow(len(text))
	for index, character := range text {
		if character == 'Σ' {
			before := []rune(text[:index])
			slices.Reverse(before)
			after := []rune(text[index+len("Σ"):])
			if grepCaseIgnorableThenCased(before) && !grepCaseIgnorableThenCased(after) {
				lowered.WriteRune('ς')
			} else {
				lowered.WriteRune('σ')
			}
			continue
		}
		for _, lower := range grepCharLowercase(character) {
			lowered.WriteRune(lower)
		}
	}
	return lowered.String()
}

// grepCaseIgnorableThenCased is Rust's `case_ignorable_then_cased`: whether
// the first character that is not case-ignorable is cased.
func grepCaseIgnorableThenCased(characters []rune) bool {
	for _, character := range characters {
		if grepCaseIgnorable(character) {
			continue
		}
		return unicode.IsUpper(character) || unicode.IsLower(character) || unicode.IsTitle(character) ||
			unicode.Is(unicode.Other_Lowercase, character) || unicode.Is(unicode.Other_Uppercase, character)
	}
	return false
}

// grepCaseIgnorable is Unicode's Case_Ignorable property: the nonspacing,
// enclosing, format, modifier letter and modifier symbol categories, and the
// characters whose Word_Break is MidLetter, MidNumLet or Single_Quote.
func grepCaseIgnorable(character rune) bool {
	switch character {
	case '\'', '.', ':', '·', '·', '՟', '״', '‘', '’', '․',
		'‧', '︓', '﹒', '﹕', '＇', '．', '：':
		return true
	}
	return unicode.In(character, unicode.Mn, unicode.Me, unicode.Cf, unicode.Lm, unicode.Sk)
}
