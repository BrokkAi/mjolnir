// Port of linkify 0.11.0 src/url.rs

package linkify

import (
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Minimum valid URL length
//
// The shortest valid URL (without a scheme) might be g.cn (Google China),
// which consists of four characters.
// We set this as a lower threshold for parsing URLs from plaintext
// to avoid false positives and as a slight performance optimization.
// This threshold might be adjusted in the future.
const urlMinUrlLength = 4

var urlQuotes = []rune{'\'', '"'}

// urlIsQuote is `QUOTES.contains(&c)`.
func urlIsQuote(c rune) bool {
	for _, quote := range urlQuotes {
		if quote == c {
			return true
		}
	}
	return false
}

// Scan for URLs starting from the trigger character ":" (requires "://").
//
// Based on RFC 3986.
type urlUrlScanner struct {
	iriParsingEnabled bool
}

// Scan for plain domains (without scheme) such as `test.com` or `test.com/hi-there`.
type urlDomainScanner struct {
	iriParsingEnabled bool
}

// Scan for an URL at the given separator index in the string.
//
// Returns `None` if none was found.
func (self *urlUrlScanner) scan(s string, separator int) rs.Option[scannerRange] {
	// There must be something before separator for scheme
	if separator == 0 {
		return rs.None[scannerRange]()
	}

	if !strings.HasPrefix(s[separator:], "://") {
		// We only support schemes with authority, not things like `myscheme:mything`.
		return rs.None[scannerRange]()
	}

	afterSeparator := separator + len("://")

	// Need at least one character after '//'
	if afterSeparator >= len(s) {
		return rs.None[scannerRange]()
	}

	if start, quote := urlFindSchemeStart(s[0:separator]); start.IsSome() {
		start := start.Unwrap()
		scheme := s[start:separator]
		s := s[afterSeparator:]

		requireHost := urlSchemeRequiresHost(scheme)

		if afterAuthority, _ := domainsFindAuthorityEnd(s, true, requireHost, true, self.iriParsingEnabled); afterAuthority.IsSome() {
			afterAuthority := afterAuthority.Unwrap()
			if end, ok := urlFindUrlEnd(s[afterAuthority:], quote, self.iriParsingEnabled).Get(); ok {
				if afterAuthority == 0 && end == 0 {
					return rs.None[scannerRange]()
				}

				range_ := scannerRange{
					start: start,
					end:   afterSeparator + afterAuthority + end,
				}
				return rs.Some(range_)
			}
		}
	}

	return rs.None[scannerRange]()
}

func (self *urlDomainScanner) scan(s string, separator int) rs.Option[scannerRange] {
	// There must be something before separator for domain, and a minimum number of characters
	if separator == 0 || len(s) < urlMinUrlLength {
		return rs.None[scannerRange]()
	}

	if start, quote := urlFindDomainStart(s[0:separator], self.iriParsingEnabled); start.IsSome() {
		start := start.Unwrap()
		s := s[start:]

		if domainEnd, lastDot := domainsFindAuthorityEnd(s, false, true, true, self.iriParsingEnabled); domainEnd.IsSome() && lastDot.IsSome() {
			domainEnd := domainEnd.Unwrap()
			if end, ok := urlFindUrlEnd(s[domainEnd:], quote, self.iriParsingEnabled).Get(); ok {
				range_ := scannerRange{
					start: start,
					end:   start + domainEnd + end,
				}
				return rs.Some(range_)
			}
		}
	}

	return rs.None[scannerRange]()
}

// Find start of scheme, e.g. from `https://`, start at `s` and end at `h`.
func urlFindSchemeStart(s string) (rs.Option[int], rs.Option[rune]) {
	first := rs.None[int]()
	special := rs.None[int]()
	quote := rs.None[rune]()
	// for (i, c) in s.char_indices().rev()
loop:
	for end := len(s); end > 0; {
		c, size := utf8.DecodeLastRuneInString(s[:end])
		i := end - size
		end = i
		switch {
		case ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z'):
			first = rs.Some(i)
		case '0' <= c && c <= '9':
			special = rs.Some(i)
		case c == '+' || c == '-' || c == '.':
		case c == '@':
			return rs.None[int](), rs.None[rune]()
		case urlIsQuote(c):
			// Check if there's a quote before the scheme,
			// and stop once we encounter one of those quotes.
			// https://github.com/robinst/linkify/issues/20
			quote = rs.Some(c)
			break loop
		default:
			break loop
		}
	}

	// We don't want to extract "abc://foo" out of "1abc://foo".
	// ".abc://foo" and others are ok though, as they feel more like separators.
	if first, ok := first.Get(); ok {
		if special, ok := special.Get(); ok {
			// Comparing the byte indices with `- 1` is ok as scheme must be ASCII
			if first > 0 && first-1 == special {
				return rs.None[int](), quote
			}
		}
	}
	return first, quote
}

// Whether a scheme requires that authority looks like a host name (domain or IP address) or not
// (can contain reg-name with arbitrary allowed characters).
//
// We could make this configurable, but let's keep it simple until someone asks (hi!).
func urlSchemeRequiresHost(scheme string) bool {
	switch scheme {
	case "https", "http", "ftp", "ssh":
		return true
	default:
		return false
	}
}

// Find the start of a plain domain URL (no scheme), e.g. from `blog.`, start at `g` and end at `b`.
// The rules are:
//   - Domain is labels separated by `.`. Because we're starting at the first `.`, we only need to
//     handle one label.
//   - Label can not start or end with `-`
//   - Label can contain letters, digits, `-` or Unicode if iri_allowed flag is true
func urlFindDomainStart(s string, iriParsingEnabled bool) (rs.Option[int], rs.Option[rune]) {
	first := rs.None[int]()
	quote := rs.None[rune]()

	// for (i, c) in s.char_indices().rev()
loop:
	for end := len(s); end > 0; {
		c, size := utf8.DecodeLastRuneInString(s[:end])
		i := end - size
		end = i
		switch {
		case ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z') || ('0' <= c && c <= '9'):
			first = rs.Some(i)
		case c >= 0x80 && iriParsingEnabled:
			first = rs.Some(i)
		// If we had something valid like `https://www.` we'd have found it with the ":"
		// scanner already. We don't want to allow `.../www.example.com` just by itself.
		// We *could* allow `//www.example.com` (scheme-relative URLs) in the future.
		case c == '/':
			return rs.None[int](), rs.None[rune]()
		// Similar to above, if this was an email we'd have found it already.
		case c == '@':
			return rs.None[int](), rs.None[rune]()
		// If this was a valid domain, we'd have extracted it already from the previous "."
		case c == '.':
			return rs.None[int](), rs.None[rune]()
		case c == '-':
			if first.IsNone() {
				// Domain label can't end with `-`
				return rs.None[int](), rs.None[rune]()
			} else {
				first = rs.Some(i)
			}
		case urlIsQuote(c):
			// Check if there's a quote before, and stop once we encounter one of those quotes,
			// e.g. with `"www.example.com"`
			quote = rs.Some(c)
			break loop
		default:
			break loop
		}
	}

	if first, ok := first.Get(); ok {
		if strings.HasPrefix(s[first:], "-") {
			// Domain label can't start with `-`
			return rs.None[int](), rs.None[rune]()
		}
	}

	return first, quote
}

// Find the end of a URL. At this point we already scanned past a valid authority. So e.g. in
// `https://example.com/foo` we're starting at `/` and want to end at `o`.
func urlFindUrlEnd(s string, quote rs.Option[rune], iriParsingEnabled bool) rs.Option[int] {
	round := 0
	square := 0
	curly := 0
	singleQuote := false

	previousIsUrlChar := true
	end := rs.Some(0)

	if !strings.HasPrefix(s[0:], "/") && !strings.HasPrefix(s[0:], "?") {
		return rs.Some(0)
	}

	// for (i, c) in s.char_indices()
loop:
	for i := 0; i < len(s); {
		c, size := utf8.DecodeRuneInString(s[i:])
		var canBeLast bool
		switch {
		case (0x00 <= c && c <= 0x1F) || c == ' ' || c == '|' || c == '"' || c == '<' || c == '>' || c == '`' || (0x7F <= c && c <= 0x9F):
			// These can never be part of an URL, so stop now. See RFC 3986 and RFC 3987.
			// Some characters are not in the above list, even they are not in "unreserved"
			// or "reserved":
			//   '\\', '^', '{', '}'
			// The reason for this is that other link detectors also allow them. Also see
			// below, we require the braces to be balanced.
			break loop
		case c == '?' || c == '!' || c == '.' || c == ',' || c == ':' || c == ';' || c == '*':
			// These may be part of an URL but not at the end. It's not that the spec
			// doesn't allow them, but they are frequently used in plain text as delimiters
			// where they're not meant to be part of the URL.
			canBeLast = false
		case c == '/':
			// A slash can be the end of a URL if the previous character is a valid URL
			// character. This means that delimiters like `!` or `.` before a `/` are
			// included in the URL, e.g. `/!/`, but non-URL characters (like non-ASCII
			// when IRI parsing is disabled) before a `/` are not.
			// See https://github.com/robinst/linkify/issues/90
			canBeLast = previousIsUrlChar
		case c == '(':
			round += 1
			canBeLast = false
		case c == ')':
			round -= 1
			if round < 0 {
				// More closing than opening brackets, stop now
				break loop
			}
			canBeLast = true
		case c == '[':
			square += 1
			canBeLast = false
		case c == ']':
			square -= 1
			if square < 0 {
				// More closing than opening brackets, stop now
				break loop
			}
			canBeLast = true
		case c == '{':
			curly += 1
			canBeLast = false
		case c == '}':
			curly -= 1
			if curly < 0 {
				// More closing than opening brackets, stop now
				break loop
			}
			canBeLast = true
		case rs.Some(c) == quote:
			// Found matching quote from beginning of URL, stop now
			break loop
		case c == '\'':
			singleQuote = !singleQuote
			// A single quote can only be the end of an URL if there's an even number
			canBeLast = !singleQuote
		case c >= 0x80 && !iriParsingEnabled:
			canBeLast = false

		default:
			canBeLast = true
		}
		if canBeLast {
			end = rs.Some(i + size)
		}
		// Track whether the current character is a valid URL character (even if it can't
		// be the last character). Delimiters like `!` are valid URL characters, but
		// non-ASCII characters are not valid URL characters when IRI parsing is disabled.
		// This matters for `/` above: a slash after a delimiter like `!` should extend
		// the URL (e.g. `/!/`), but a slash after a non-URL character should not.
		previousIsUrlChar = c < 0x80 || iriParsingEnabled
		i += size
	}

	return end
}
