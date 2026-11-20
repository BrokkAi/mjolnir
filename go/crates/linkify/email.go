// Port of linkify 0.11.0 src/email.rs

package linkify

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Scan for email address starting from the trigger character "@".
//
// Based on RFC 6531, but also accepts invalid IDNs. Doesn't try to handle IP addresses in domain part or
// quoting in local part.
type emailEmailScanner struct {
	domainMustHaveDot bool
}

func (self *emailEmailScanner) scan(s string, at int) rs.Option[scannerRange] {
	if start, ok := self.findStart(s[0:at]).Get(); ok {
		after := at + 1
		if end, ok := self.findEnd(s[after:]).Get(); ok {
			range_ := scannerRange{
				start: start,
				end:   after + end,
			}
			return rs.Some(range_)
		}
	}
	return rs.None[scannerRange]()
}

// See "Local-part" in RFC 5321, plus extensions in RFC 6531
func (self *emailEmailScanner) findStart(s string) rs.Option[int] {
	first := rs.None[int]()
	atomBoundary := true
	// for (i, c) in s.char_indices().rev()
	for end := len(s); end > 0; {
		c, size := utf8.DecodeLastRuneInString(s[:end])
		i := end - size
		end = i
		if emailEmailScannerLocalAtomAllowed(c) {
			first = rs.Some(i)
			atomBoundary = false
		} else if c == '.' {
			if atomBoundary {
				break
			}
			atomBoundary = true
		} else if c == '@' {
			// In `@me@a.com`, we don't want to extract `me@a.com`.
			return rs.None[int]()
		} else {
			break
		}
	}
	return first
}

// See "Domain" in RFC 5321, plus extension of "sub-domain" in RFC 6531
func (self *emailEmailScanner) findEnd(s string) rs.Option[int] {
	if end, lastDot := domainsFindAuthorityEnd(s, false, true, false, true); end.IsSome() {
		if !self.domainMustHaveDot || lastDot.IsSome() {
			return end
		} else {
			return rs.None[int]()
		}
	} else {
		return rs.None[int]()
	}
}

// See "Atom" in RFC 5321, "atext" in RFC 5322
func emailEmailScannerLocalAtomAllowed(c rune) bool {
	switch {
	case ('a' <= c && c <= 'z') ||
		('A' <= c && c <= 'Z') ||
		('0' <= c && c <= '9') ||
		c == '!' ||
		c == '#' ||
		c == '$' ||
		c == '%' ||
		c == '&' ||
		c == '\'' ||
		c == '*' ||
		c == '+' ||
		c == '-' ||
		c == '/' ||
		c == '=' ||
		c == '?' ||
		c == '^' ||
		c == '_' ||
		c == '`' ||
		c == '{' ||
		c == '|' ||
		c == '}' ||
		c == '~':
		return true
	default:
		return c >= 0x80
	}
}
