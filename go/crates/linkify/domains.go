// Port of linkify 0.11.0 src/domains.rs

// Domain name related scanning, used by both email and URL scanners.
//
// This is called domains for familiarity but it's about the authority part of URLs as defined in
// https://datatracker.ietf.org/doc/html/rfc3986#section-3.2
//
//	authority   = [ userinfo "@" ] host [ ":" port ]
//
//
//	userinfo    = *( unreserved / pct-encoded / sub-delims / ":" )
//
//	host        = IP-literal / IPv4address / reg-name
//
//	IP-literal = "[" ( IPv6address / IPvFuture  ) "]"
//
//	IPv4address = dec-octet "." dec-octet "." dec-octet "." dec-octet
//
//	reg-name    = *( unreserved / pct-encoded / sub-delims )
//
//
//	unreserved  = ALPHA / DIGIT / "-" / "." / "_" / "~"
//
//	sub-delims  = "!" / "$" / "&" / "'" / "(" / ")" / "*" / "+" / "," / ";" / "="
//
//	pct-encoded = "%" HEXDIG HEXDIG

package linkify

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func domainsFindAuthorityEnd(
	s string,
	userinfoAllowed bool,
	requireHost bool,
	portAllowed bool,
	iriParsingEnabled bool,
) (rs.Option[int], rs.Option[int]) {
	end := rs.Some(0)

	maybeLastDot := rs.None[int]()
	lastDot := rs.None[int]()
	numberDots := 0
	dotAllowed := false
	hyphenAllowed := false
	allNumeric := true
	maybeHost := true
	hostEnded := false

	// for (i, c) in s.char_indices()
loop:
	for i := 0; i < len(s); {
		c, size := utf8.DecodeRuneInString(s[i:])
		var canBeLast bool
		switch {
		// ALPHA
		case ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z') || c >= 0x80:
			if !iriParsingEnabled && c > 0x80 {
				break loop
			}
			// Can start or end a domain label, but not numeric
			dotAllowed = true
			hyphenAllowed = true
			lastDot = maybeLastDot
			allNumeric = false

			if hostEnded {
				maybeHost = false
			}

			canBeLast = !requireHost || !hostEnded

		// DIGIT
		case '0' <= c && c <= '9':
			// Same as above, except numeric
			dotAllowed = true
			hyphenAllowed = true
			if lastDot != maybeLastDot {
				lastDot = maybeLastDot
				numberDots += 1
			}

			if hostEnded {
				maybeHost = false
			}

			canBeLast = !requireHost || !hostEnded
		// unreserved
		case c == '-':
			// Hyphen can't be at start of a label, e.g. `-b` in `a.-b.com`
			if !hyphenAllowed {
				maybeHost = false
			}
			// Hyphen can't be at end of a label, e.g. `b-` in `a.b-.com`
			dotAllowed = false
			allNumeric = false

			canBeLast = !requireHost
		case c == '.':
			if !dotAllowed {
				// Label can't be empty, e.g. `.example.com` or `a..com`
				hostEnded = true
			}
			dotAllowed = false
			hyphenAllowed = false
			maybeLastDot = rs.Some(i)

			canBeLast = false
		case c == '_' || c == '~':
			// Hostnames can't contain these and we don't want to treat them as delimiters.
			maybeHost = false

			canBeLast = false
		// sub-delims
		case c == '!' || c == '$' || c == '&' || c == '\'' || c == '(' || c == ')' || c == '*' || c == '+' || c == ',' || c == ';' || c == '=':
			// Can't be in hostnames, but we treat them as delimiters
			hostEnded = true

			if !userinfoAllowed && requireHost {
				// We don't have to look further
				break loop
			}

			canBeLast = false
		case c == ':':
			// Could be in userinfo, or we're getting a port now.
			if !userinfoAllowed && !portAllowed {
				break loop
			}

			// Don't advance the last dot when we get to port numbers
			maybeLastDot = lastDot

			canBeLast = false
		case c == '@':
			if !userinfoAllowed {
				// We already had userinfo, can't have another `@` in a valid authority.
				return rs.None[int](), rs.None[int]()
			}

			// Sike! Everything before this has been userinfo, so let's reset our
			// opinions about all the host bits.
			userinfoAllowed = false

			maybeLastDot = rs.None[int]()
			lastDot = rs.None[int]()
			dotAllowed = false
			hyphenAllowed = false
			allNumeric = true
			maybeHost = true
			hostEnded = false

			canBeLast = false
		case c == '/':
			if !requireHost {
				// For schemes where we allow anything, we want to stop at delimiter characters
				// except if we get a slash closing the URL, which happened here.
				end = rs.Some(i)
			}
			break loop
		default:
			// Anything else, this might be the end of the authority (can be empty).
			// Now let the rest of the code handle checking whether the end of the URL is
			// valid.
			break loop
		}

		if canBeLast {
			end = rs.Some(i + size)
		}
		i += size
	}

	if requireHost {
		if maybeHost {
			if allNumeric {
				// For IPv4 addresses, require 4 numbers
				if numberDots != 3 {
					return rs.None[int](), rs.None[int]()
				}
			} else {
				// If we have something that is not just numeric (not an IP address),
				// check that the TLD looks reasonable. This is to avoid linking things like
				// `abc@v1.1`.
				if lastDot, ok := lastDot.Get(); ok {
					if !domainsValidTld(s[lastDot+1:]) {
						return rs.None[int](), rs.None[int]()
					}
				}
			}

			return end, lastDot
		} else {
			return rs.None[int](), rs.None[int]()
		}
	} else {
		return end, lastDot
	}
}

func domainsValidTld(tld string) bool {
	// tld.chars().take_while(|c| c.is_ascii_alphabetic()).take(2).count() >= 2
	count := 0
	for i := 0; i < len(tld) && count < 2; i++ {
		c := tld[i]
		if !(('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z')) {
			break
		}
		count++
	}
	return count >= 2
}
