// Port of url 2.5.8 src/parser.rs

package url

import (
	"bytes"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// https://url.spec.whatwg.org/#fragment-percent-encode-set
var parserFragment = percentEncodingControls.addAll(" \"<>`")

// https://url.spec.whatwg.org/#path-percent-encode-set
var parserPath = parserFragment.addAll("#?{}")

// https://url.spec.whatwg.org/#userinfo-percent-encode-set
var parserUserinfo = parserPath.addAll("/:;=@[\\]^|")

var parserPathSegment = parserPath.addAll("/%")

// The backslash (\) character is treated as a path separator in special URLs
// so it needs to be additionally escaped in that case.
var parserSpecialPathSegment = parserPathSegment.add('\\')

// https://url.spec.whatwg.org/#query-state
var parserQuery = percentEncodingControls.addAll(" \"#<>")
var parserSpecialQuery = parserQuery.add('\'')

// ParseError is url::ParseError: errors that can occur during parsing.
//
// This may be extended in the future so exhaustive matching is
// forbidden.
type ParseError uint8

const (
	ParseError_EmptyHost ParseError = iota
	ParseError_IdnaError
	ParseError_InvalidPort
	ParseError_InvalidIpv4Address
	ParseError_InvalidIpv6Address
	ParseError_InvalidDomainCharacter
	ParseError_RelativeUrlWithoutBase
	ParseError_RelativeUrlWithCannotBeABaseBase
	ParseError_SetHostOnCannotBeABaseUrl
	ParseError_Overflow
)

var _ = rs.RegisterEnum[ParseError]("ParseError",
	rs.Const(ParseError_EmptyHost, "EmptyHost"),
	rs.Const(ParseError_IdnaError, "IdnaError"),
	rs.Const(ParseError_InvalidPort, "InvalidPort"),
	rs.Const(ParseError_InvalidIpv4Address, "InvalidIpv4Address"),
	rs.Const(ParseError_InvalidIpv6Address, "InvalidIpv6Address"),
	rs.Const(ParseError_InvalidDomainCharacter, "InvalidDomainCharacter"),
	rs.Const(ParseError_RelativeUrlWithoutBase, "RelativeUrlWithoutBase"),
	rs.Const(ParseError_RelativeUrlWithCannotBeABaseBase, "RelativeUrlWithCannotBeABaseBase"),
	rs.Const(ParseError_SetHostOnCannotBeABaseUrl, "SetHostOnCannotBeABaseUrl"),
	rs.Const(ParseError_Overflow, "Overflow"),
)

// String is the Display text of the error.
func (e ParseError) String() string {
	switch e {
	case ParseError_EmptyHost:
		return "empty host"
	case ParseError_IdnaError:
		return "invalid international domain name"
	case ParseError_InvalidPort:
		return "invalid port number"
	case ParseError_InvalidIpv4Address:
		return "invalid IPv4 address"
	case ParseError_InvalidIpv6Address:
		return "invalid IPv6 address"
	case ParseError_InvalidDomainCharacter:
		return "invalid domain character"
	case ParseError_RelativeUrlWithoutBase:
		return "relative URL without a base"
	case ParseError_RelativeUrlWithCannotBeABaseBase:
		return "relative URL with a cannot-be-a-base base"
	case ParseError_SetHostOnCannotBeABaseUrl:
		return "a cannot-be-a-base URL doesn’t have a host to set"
	case ParseError_Overflow:
		return "URLs more than 4 GB are not supported"
	default:
		panic(rs.Unreachable(e))
	}
}

// FmtDisplay is `impl fmt::Display for ParseError`.
func (e ParseError) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(e.String())
}

// Error makes a ParseError a Go error; it is `impl std::error::Error for
// ParseError`.
func (e ParseError) Error() string { return e.String() }

// rustName is the variant's name, which is what derive(Debug) prints.
func (e ParseError) rustName() string {
	switch e {
	case ParseError_EmptyHost:
		return "EmptyHost"
	case ParseError_IdnaError:
		return "IdnaError"
	case ParseError_InvalidPort:
		return "InvalidPort"
	case ParseError_InvalidIpv4Address:
		return "InvalidIpv4Address"
	case ParseError_InvalidIpv6Address:
		return "InvalidIpv6Address"
	case ParseError_InvalidDomainCharacter:
		return "InvalidDomainCharacter"
	case ParseError_RelativeUrlWithoutBase:
		return "RelativeUrlWithoutBase"
	case ParseError_RelativeUrlWithCannotBeABaseBase:
		return "RelativeUrlWithCannotBeABaseBase"
	case ParseError_SetHostOnCannotBeABaseUrl:
		return "SetHostOnCannotBeABaseUrl"
	case ParseError_Overflow:
		return "Overflow"
	default:
		panic(rs.Unreachable(e))
	}
}

// SyntaxViolation is url::SyntaxViolation: non-fatal syntax violations that
// can occur during parsing.
//
// This may be extended in the future so exhaustive matching is
// forbidden.
type SyntaxViolation uint8

const (
	SyntaxViolation_Backslash SyntaxViolation = iota
	SyntaxViolation_C0SpaceIgnored
	SyntaxViolation_EmbeddedCredentials
	SyntaxViolation_ExpectedDoubleSlash
	SyntaxViolation_ExpectedFileDoubleSlash
	SyntaxViolation_FileWithHostAndWindowsDrive
	SyntaxViolation_NonUrlCodePoint
	SyntaxViolation_NullInFragment
	SyntaxViolation_PercentDecode
	SyntaxViolation_TabOrNewlineIgnored
	SyntaxViolation_UnencodedAtSign
)

var _ = rs.RegisterEnum[SyntaxViolation]("SyntaxViolation",
	rs.Const(SyntaxViolation_Backslash, "Backslash"),
	rs.Const(SyntaxViolation_C0SpaceIgnored, "C0SpaceIgnored"),
	rs.Const(SyntaxViolation_EmbeddedCredentials, "EmbeddedCredentials"),
	rs.Const(SyntaxViolation_ExpectedDoubleSlash, "ExpectedDoubleSlash"),
	rs.Const(SyntaxViolation_ExpectedFileDoubleSlash, "ExpectedFileDoubleSlash"),
	rs.Const(SyntaxViolation_FileWithHostAndWindowsDrive, "FileWithHostAndWindowsDrive"),
	rs.Const(SyntaxViolation_NonUrlCodePoint, "NonUrlCodePoint"),
	rs.Const(SyntaxViolation_NullInFragment, "NullInFragment"),
	rs.Const(SyntaxViolation_PercentDecode, "PercentDecode"),
	rs.Const(SyntaxViolation_TabOrNewlineIgnored, "TabOrNewlineIgnored"),
	rs.Const(SyntaxViolation_UnencodedAtSign, "UnencodedAtSign"),
)

// Description is SyntaxViolation::description.
func (v SyntaxViolation) Description() string {
	switch v {
	case SyntaxViolation_Backslash:
		return "backslash"
	case SyntaxViolation_C0SpaceIgnored:
		return "leading or trailing control or space character are ignored in URLs"
	case SyntaxViolation_EmbeddedCredentials:
		return "embedding authentication information (username or password) " +
			"in an URL is not recommended"
	case SyntaxViolation_ExpectedDoubleSlash:
		return "expected //"
	case SyntaxViolation_ExpectedFileDoubleSlash:
		return "expected // after file:"
	case SyntaxViolation_FileWithHostAndWindowsDrive:
		return "file: with host and Windows drive letter"
	case SyntaxViolation_NonUrlCodePoint:
		return "non-URL code point"
	case SyntaxViolation_NullInFragment:
		return "NULL characters are ignored in URL fragment identifiers"
	case SyntaxViolation_PercentDecode:
		return "expected 2 hex digits after %"
	case SyntaxViolation_TabOrNewlineIgnored:
		return "tabs or newlines are ignored in URLs"
	case SyntaxViolation_UnencodedAtSign:
		return "unencoded @ sign in username or password"
	default:
		panic(rs.Unreachable(v))
	}
}

// String is the Display text: the description.
func (v SyntaxViolation) String() string { return v.Description() }

// FmtDisplay is `impl fmt::Display for SyntaxViolation`.
func (v SyntaxViolation) FmtDisplay(f *rs.Formatter) error {
	return f.Pad(v.Description())
}

type parserSchemeType uint8

const (
	parserSchemeType_File parserSchemeType = iota
	parserSchemeType_SpecialNotFile
	parserSchemeType_NotSpecial
)

func (t parserSchemeType) isSpecial() bool {
	return t != parserSchemeType_NotSpecial
}

func (t parserSchemeType) isFile() bool {
	return t == parserSchemeType_File
}

// parserSchemeTypeFrom is `impl<T: AsRef<str>> From<T> for SchemeType`.
func parserSchemeTypeFrom(s string) parserSchemeType {
	switch s {
	case "http", "https", "ws", "wss", "ftp":
		return parserSchemeType_SpecialNotFile
	case "file":
		return parserSchemeType_File
	default:
		return parserSchemeType_NotSpecial
	}
}

func parserDefaultPort(scheme string) rs.Option[uint16] {
	switch scheme {
	case "http", "ws":
		return rs.Some[uint16](80)
	case "https", "wss":
		return rs.Some[uint16](443)
	case "ftp":
		return rs.Some[uint16](21)
	default:
		return rs.None[uint16]()
	}
}

// parserInput is parser::Input: the rest of the text being parsed. Its
// iterator skips ASCII tabs and newlines, which the URL Standard removes
// from the input. It is a value: a copy is the Rust `clone()`.
//
// A Rust &str is valid UTF-8. A Go string that is not is read one byte at a
// time for each invalid byte, and that byte is percent-encoded like any
// other non-ASCII byte.
type parserInput struct {
	// chars is what `chars.as_str()` gives: the text not yet read.
	chars string
}

func parserInputNewNoTrim(input string) parserInput {
	return parserInput{chars: input}
}

func parserInputNewTrimTabAndNewlines(originalInput string, vfn rs.Option[func(SyntaxViolation)]) parserInput {
	input := trimMatchesAscii(originalInput, func(b byte) bool { return b == '\t' || b == '\n' || b == '\r' })
	if vfn, ok := vfn.Get(); ok {
		if len(input) < len(originalInput) {
			vfn(SyntaxViolation_C0SpaceIgnored)
		}
		if strings.ContainsAny(input, "\t\n\r") {
			vfn(SyntaxViolation_TabOrNewlineIgnored)
		}
	}
	return parserInput{chars: input}
}

func parserInputNewTrimC0ControlAndSpace(originalInput string, vfn rs.Option[func(SyntaxViolation)]) parserInput {
	input := trimMatchesAscii(originalInput, func(b byte) bool { return b <= ' ' })
	if vfn, ok := vfn.Get(); ok {
		if len(input) < len(originalInput) {
			vfn(SyntaxViolation_C0SpaceIgnored)
		}
		if strings.ContainsAny(input, "\t\n\r") {
			vfn(SyntaxViolation_TabOrNewlineIgnored)
		}
	}
	return parserInput{chars: input}
}

// trimMatchesAscii is str::trim_matches for a predicate that only ASCII
// bytes satisfy.
func trimMatchesAscii(s string, matches func(byte) bool) string {
	start := 0
	for start < len(s) && matches(s[start]) {
		start++
	}
	end := len(s)
	for end > start && matches(s[end-1]) {
		end--
	}
	return s[start:end]
}

func (input parserInput) isEmpty() bool {
	_, ok := input.next()
	return !ok
}

// startsWithChar, startsWithStr and startsWithFn are Input::starts_with for
// the three kinds of Pattern.
func (input parserInput) startsWithChar(c rune) bool {
	got, ok := input.next()
	return ok && got == c
}

func (input parserInput) startsWithStr(s string) bool {
	_, ok := input.splitPrefixStr(s)
	return ok
}

func (input parserInput) startsWithFn(f func(rune) bool) bool {
	got, ok := input.next()
	return ok && f(got)
}

// splitPrefixChar and splitPrefixStr are Input::split_prefix.
func (input parserInput) splitPrefixChar(c rune) (parserInput, bool) {
	remaining := input
	got, ok := remaining.next()
	if ok && got == c {
		return remaining, true
	}
	return parserInput{}, false
}

func (input parserInput) splitPrefixStr(s string) (parserInput, bool) {
	remaining := input
	for _, c := range s {
		got, ok := remaining.next()
		if !ok || got != c {
			return parserInput{}, false
		}
	}
	return remaining, true
}

func (input parserInput) splitFirst() (rs.Option[rune], parserInput) {
	remaining := input
	c, ok := remaining.next()
	return rs.OptionOf(c, ok), remaining
}

func (input parserInput) countMatching(f func(rune) bool) (uint32, parserInput) {
	count := uint32(0)
	remaining := input
	for {
		probe := remaining
		if c, ok := probe.next(); ok && f(c) {
			remaining = probe
			count++
		} else {
			return count, remaining
		}
	}
}

// nextUtf8 is Input::next_utf8: the next character that is not an ASCII tab
// or newline, with its bytes.
func (input *parserInput) nextUtf8() (rune, string, bool) {
	for {
		c, utf8Bytes, ok := input.nextRaw()
		if !ok {
			return 0, "", false
		}
		if !parserAsciiTabOrNewLine(c) {
			return c, utf8Bytes, true
		}
	}
}

// nextRaw is `self.chars.next()`: the next character whatever it is, with
// its bytes.
func (input *parserInput) nextRaw() (rune, string, bool) {
	if len(input.chars) == 0 {
		return 0, "", false
	}
	c, size := utf8.DecodeRuneInString(input.chars)
	utf8Bytes := input.chars[:size]
	input.chars = input.chars[size:]
	return c, utf8Bytes, true
}

// next is `impl Iterator for Input`: the next character that is not an ASCII
// tab or newline.
func (input *parserInput) next() (rune, bool) {
	c, _, ok := input.nextUtf8()
	return c, ok
}

type parserParser struct {
	serialization         []byte
	baseUrl               rs.Option[*Url]
	queryEncodingOverride FormUrlencodedEncodingOverride
	violationFn           rs.Option[func(SyntaxViolation)]
	context               parserContext
}

type parserContext uint8

const (
	parserContext_UrlParser parserContext = iota
	parserContext_Setter
	parserContext_PathSegmentSetter
)

func (p *parserParser) logViolation(v SyntaxViolation) {
	if f, ok := p.violationFn.Get(); ok {
		f(v)
	}
}

func (p *parserParser) logViolationIf(v SyntaxViolation, test func() bool) {
	if f, ok := p.violationFn.Get(); ok {
		if test() {
			f(v)
		}
	}
}

func parserParserForSetter(serialization []byte) *parserParser {
	return &parserParser{
		serialization:         serialization,
		baseUrl:               rs.None[*Url](),
		queryEncodingOverride: rs.None[func(string) []byte](),
		violationFn:           rs.None[func(SyntaxViolation)](),
		context:               parserContext_Setter,
	}
}

// https://url.spec.whatwg.org/#concept-basic-url-parser
func (p *parserParser) parseUrl(inputStr string) (Url, error) {
	input := parserInputNewTrimC0ControlAndSpace(inputStr, p.violationFn)
	if remaining, ok := p.parseScheme(input); ok {
		return p.parseWithScheme(remaining)
	}

	// No-scheme state
	if baseUrl, ok := p.baseUrl.Get(); ok {
		if input.startsWithChar('#') {
			return p.fragmentOnly(baseUrl, input)
		} else if baseUrl.CannotBeABase() {
			return Url{}, ParseError_RelativeUrlWithCannotBeABaseBase
		} else {
			schemeType := parserSchemeTypeFrom(baseUrl.Scheme())
			if schemeType.isFile() {
				return p.parseFile(input, schemeType, rs.Some(baseUrl))
			} else {
				return p.parseRelative(input, schemeType, baseUrl)
			}
		}
	} else {
		return Url{}, ParseError_RelativeUrlWithoutBase
	}
}

func (p *parserParser) parseScheme(input parserInput) (parserInput, bool) {
	// starts_with will also fail for empty strings so we can skip that comparison for perf
	if !input.startsWithFn(parserAsciiAlpha) {
		return parserInput{}, false
	}
	debugAssert(len(p.serialization) == 0, "self.serialization.is_empty()")
	for {
		c, ok := input.next()
		if !ok {
			break
		}
		switch {
		case ('a' <= c && c <= 'z') || ('0' <= c && c <= '9') || c == '+' || c == '-' || c == '.':
			p.serialization = append(p.serialization, byte(c))
		case 'A' <= c && c <= 'Z':
			p.serialization = append(p.serialization, byte(c)+('a'-'A'))
		case c == ':':
			return input, true
		default:
			p.serialization = p.serialization[:0]
			return parserInput{}, false
		}
	}
	// EOF before ':'
	if p.context == parserContext_Setter {
		return input, true
	} else {
		p.serialization = p.serialization[:0]
		return parserInput{}, false
	}
}

func isSlashOrBackslash(c rune) bool { return c == '/' || c == '\\' }

// leadingSlashes is `input.clone().take_while(|&c| matches!(c, '/' |
// '\\')).collect::<String>()`.
func leadingSlashes(input parserInput) string {
	var out []byte
	for {
		c, ok := input.next()
		if !ok || !isSlashOrBackslash(c) {
			return string(out)
		}
		out = append(out, byte(c))
	}
}

func (p *parserParser) parseWithScheme(input parserInput) (Url, error) {
	schemeEnd, err := parserToU32(len(p.serialization))
	if err != nil {
		return Url{}, err
	}
	schemeType := parserSchemeTypeFrom(string(p.serialization))
	p.serialization = append(p.serialization, ':')
	switch schemeType {
	case parserSchemeType_File:
		p.logViolationIf(SyntaxViolation_ExpectedFileDoubleSlash, func() bool { return !input.startsWithStr("//") })
		baseFileUrl := rs.None[*Url]()
		if base, ok := p.baseUrl.Get(); ok {
			if base.Scheme() == "file" {
				baseFileUrl = rs.Some(base)
			}
		}
		p.serialization = p.serialization[:0]
		return p.parseFile(input, schemeType, baseFileUrl)
	case parserSchemeType_SpecialNotFile:
		// special relative or authority state
		slashesCount, remaining := input.countMatching(isSlashOrBackslash)
		if baseUrl, ok := p.baseUrl.Get(); ok {
			if slashesCount < 2 &&
				baseUrl.Scheme() == string(p.serialization[:schemeEnd]) {
				// "Cannot-be-a-base" URLs only happen with "not special" schemes.
				debugAssert(!baseUrl.CannotBeABase(), "!base_url.cannot_be_a_base()")
				p.serialization = p.serialization[:0]
				return p.parseRelative(input, schemeType, baseUrl)
			}
		}
		// special authority slashes state
		p.logViolationIf(SyntaxViolation_ExpectedDoubleSlash, func() bool {
			return leadingSlashes(input) != "//"
		})
		return p.afterDoubleSlash(remaining, schemeType, schemeEnd)
	case parserSchemeType_NotSpecial:
		return p.parseNonSpecial(input, schemeType, schemeEnd)
	default:
		panic(rs.Unreachable(schemeType))
	}
}

// Scheme other than file, http, https, ws, ws, ftp.
func (p *parserParser) parseNonSpecial(
	input parserInput,
	schemeType parserSchemeType,
	schemeEnd uint32,
) (Url, error) {
	// path or authority state (
	if afterSlashes, ok := input.splitPrefixStr("//"); ok {
		return p.afterDoubleSlash(afterSlashes, schemeType, schemeEnd)
	}
	// Anarchist URL (no authority)
	pathStart, err := parserToU32(len(p.serialization))
	if err != nil {
		return Url{}, err
	}
	usernameEnd := pathStart
	hostStart := pathStart
	hostEnd := pathStart
	var host hostHostInternal = &hostHostInternal_None{}
	port := rs.None[uint16]()
	var remaining parserInput
	if afterSlash, ok := input.splitPrefixChar('/'); ok {
		p.serialization = append(p.serialization, '/')
		hasHost := false
		remaining = p.parsePath(schemeType, &hasHost, int(pathStart), afterSlash)
	} else {
		remaining = p.parseCannotBeABasePath(input)
	}
	return p.withQueryAndFragment(
		schemeType,
		schemeEnd,
		usernameEnd,
		hostStart,
		hostEnd,
		host,
		port,
		pathStart,
		remaining,
	)
}

func (p *parserParser) parseFile(
	input parserInput,
	schemeType parserSchemeType,
	baseFileUrl rs.Option[*Url],
) (Url, error) {
	// file state
	debugAssert(len(p.serialization) == 0, "self.serialization.is_empty()")
	firstChar, inputAfterFirstChar := input.splitFirst()
	if c, ok := firstChar.Get(); ok && isSlashOrBackslash(c) {
		p.logViolationIf(SyntaxViolation_Backslash, func() bool { return c == '\\' })
		// file slash state
		nextChar, inputAfterNextChar := inputAfterFirstChar.splitFirst()
		if nc, ok := nextChar.Get(); ok && isSlashOrBackslash(nc) {
			p.logViolationIf(SyntaxViolation_Backslash, func() bool { return nc == '\\' })
			// file host state
			p.serialization = append(p.serialization, "file://"...)
			schemeEnd := uint32(len("file"))
			hostStart := uint32(len("file://"))
			pathStart, host, remaining, err := p.parseFileHost(inputAfterNextChar)
			if err != nil {
				return Url{}, err
			}
			hostEnd, err := parserToU32(len(p.serialization))
			if err != nil {
				return Url{}, err
			}
			hasHost := !rs.Is[*hostHostInternal_None](host)
			if pathStart {
				remaining = p.parsePathStart(parserSchemeType_File, &hasHost, remaining)
			} else {
				pathStart := len(p.serialization)
				p.serialization = append(p.serialization, '/')
				remaining = p.parsePath(parserSchemeType_File, &hasHost, pathStart, remaining)
			}

			// For file URLs that have a host and whose path starts
			// with the windows drive letter we just remove the host.
			if !hasHost {
				p.serialization = drain(p.serialization, int(hostStart), int(hostEnd))
				hostEnd = hostStart
				host = &hostHostInternal_None{}
			}
			queryStart, fragmentStart, err := p.parseQueryAndFragment(schemeType, schemeEnd, remaining)
			if err != nil {
				return Url{}, err
			}
			return Url{
				serialization: string(p.serialization),
				schemeEnd:     schemeEnd,
				usernameEnd:   hostStart,
				hostStart:     hostStart,
				hostEnd:       hostEnd,
				host:          host,
				port:          rs.None[uint16](),
				pathStart:     hostEnd,
				queryStart:    queryStart,
				fragmentStart: fragmentStart,
			}, nil
		} else {
			p.serialization = append(p.serialization, "file://"...)
			schemeEnd := uint32(len("file"))
			hostStart := len("file://")
			hostEnd := hostStart
			var host hostHostInternal = &hostHostInternal_None{}
			if !parserStartsWithWindowsDriveLetterSegment(inputAfterFirstChar) {
				if baseUrl, ok := baseFileUrl.Get(); ok {
					firstSegment := baseUrl.PathSegments().Unwrap()[0]
					if parserIsNormalizedWindowsDriveLetter(firstSegment) {
						p.serialization = append(p.serialization, '/')
						p.serialization = append(p.serialization, firstSegment...)
					} else if hostStr, ok := baseUrl.HostStr().Get(); ok {
						p.serialization = append(p.serialization, hostStr...)
						hostEnd = len(p.serialization)
						host = baseUrl.host
					}
				}
			}
			// If c is the EOF code point, U+002F (/), U+005C (\), U+003F (?), or U+0023 (#), then decrease pointer by one
			var parsePathInput parserInput
			if c, ok := firstChar.Get(); ok {
				if c == '/' || c == '\\' || c == '?' || c == '#' {
					parsePathInput = input
				} else {
					parsePathInput = inputAfterFirstChar
				}
			} else {
				parsePathInput = inputAfterFirstChar
			}

			hasHost := false
			remaining := p.parsePath(parserSchemeType_File, &hasHost, hostEnd, parsePathInput)

			hostStart32 := uint32(hostStart)

			queryStart, fragmentStart, err := p.parseQueryAndFragment(schemeType, schemeEnd, remaining)
			if err != nil {
				return Url{}, err
			}

			hostEnd32 := uint32(hostEnd)
			return Url{
				serialization: string(p.serialization),
				schemeEnd:     schemeEnd,
				usernameEnd:   hostStart32,
				hostStart:     hostStart32,
				hostEnd:       hostEnd32,
				host:          host,
				port:          rs.None[uint16](),
				pathStart:     hostEnd32,
				queryStart:    queryStart,
				fragmentStart: fragmentStart,
			}, nil
		}
	}
	if baseUrl, ok := baseFileUrl.Get(); ok {
		c, hasFirst := firstChar.Get()
		switch {
		case !hasFirst:
			// Copy everything except the fragment
			var beforeFragment string
			if i, ok := baseUrl.fragmentStart.Get(); ok {
				beforeFragment = strSlice(baseUrl.serialization, 0, int(i))
			} else {
				beforeFragment = baseUrl.serialization
			}
			p.serialization = append(p.serialization, beforeFragment...)
			out := *baseUrl
			out.serialization = string(p.serialization)
			out.fragmentStart = rs.None[uint32]()
			return out, nil
		case c == '?':
			// Copy everything up to the query string
			beforeQuery := baseUrl.beforeQuery()
			p.serialization = append(p.serialization, beforeQuery...)
			queryStart, fragmentStart, err := p.parseQueryAndFragment(schemeType, baseUrl.schemeEnd, input)
			if err != nil {
				return Url{}, err
			}
			out := *baseUrl
			out.serialization = string(p.serialization)
			out.queryStart = queryStart
			out.fragmentStart = fragmentStart
			return out, nil
		case c == '#':
			return p.fragmentOnly(baseUrl, input)
		default:
			if !parserStartsWithWindowsDriveLetterSegment(input) {
				beforeQuery := baseUrl.beforeQuery()
				p.serialization = append(p.serialization, beforeQuery...)
				p.shortenPath(parserSchemeType_File, int(baseUrl.pathStart))
				hasHost := true
				remaining := p.parsePath(
					parserSchemeType_File,
					&hasHost,
					int(baseUrl.pathStart),
					input,
				)
				return p.withQueryAndFragment(
					parserSchemeType_File,
					baseUrl.schemeEnd,
					baseUrl.usernameEnd,
					baseUrl.hostStart,
					baseUrl.hostEnd,
					baseUrl.host,
					baseUrl.port,
					baseUrl.pathStart,
					remaining,
				)
			} else {
				p.serialization = append(p.serialization, "file:///"...)
				schemeEnd := uint32(len("file"))
				pathStart := len("file://")
				hasHost := false
				remaining := p.parsePath(parserSchemeType_File, &hasHost, pathStart, input)
				queryStart, fragmentStart, err := p.parseQueryAndFragment(parserSchemeType_File, schemeEnd, remaining)
				if err != nil {
					return Url{}, err
				}
				pathStart32 := uint32(pathStart)
				return Url{
					serialization: string(p.serialization),
					schemeEnd:     schemeEnd,
					usernameEnd:   pathStart32,
					hostStart:     pathStart32,
					hostEnd:       pathStart32,
					host:          &hostHostInternal_None{},
					port:          rs.None[uint16](),
					pathStart:     pathStart32,
					queryStart:    queryStart,
					fragmentStart: fragmentStart,
				}, nil
			}
		}
	} else {
		p.serialization = append(p.serialization, "file:///"...)
		schemeEnd := uint32(len("file"))
		pathStart := len("file://")
		hasHost := false
		remaining := p.parsePath(parserSchemeType_File, &hasHost, pathStart, input)
		queryStart, fragmentStart, err := p.parseQueryAndFragment(parserSchemeType_File, schemeEnd, remaining)
		if err != nil {
			return Url{}, err
		}
		pathStart32 := uint32(pathStart)
		return Url{
			serialization: string(p.serialization),
			schemeEnd:     schemeEnd,
			usernameEnd:   pathStart32,
			hostStart:     pathStart32,
			hostEnd:       pathStart32,
			host:          &hostHostInternal_None{},
			port:          rs.None[uint16](),
			pathStart:     pathStart32,
			queryStart:    queryStart,
			fragmentStart: fragmentStart,
		}, nil
	}
}

// beforeQuery is the part of the serialization before the query string, or
// before the fragment when there is no query:
//
//	match (base_url.query_start, base_url.fragment_start) {
//	    (None, None) => &*base_url.serialization,
//	    (Some(i), _) | (None, Some(i)) => base_url.slice(..i),
//	}
func (u *Url) beforeQuery() string {
	if i, ok := u.queryStart.Get(); ok {
		return u.sliceTo(i)
	}
	if i, ok := u.fragmentStart.Get(); ok {
		return u.sliceTo(i)
	}
	return u.serialization
}

func (p *parserParser) parseRelative(
	input parserInput,
	schemeType parserSchemeType,
	baseUrl *Url,
) (Url, error) {
	// relative state
	debugAssert(len(p.serialization) == 0, "self.serialization.is_empty()")
	firstChar, inputAfterFirstChar := input.splitFirst()
	c, hasFirst := firstChar.Get()
	switch {
	case !hasFirst:
		// Copy everything except the fragment
		var beforeFragment string
		if i, ok := baseUrl.fragmentStart.Get(); ok {
			beforeFragment = strSlice(baseUrl.serialization, 0, int(i))
		} else {
			beforeFragment = baseUrl.serialization
		}
		p.serialization = append(p.serialization, beforeFragment...)
		out := *baseUrl
		out.serialization = string(p.serialization)
		out.fragmentStart = rs.None[uint32]()
		return out, nil
	case c == '?':
		// Copy everything up to the query string
		beforeQuery := baseUrl.beforeQuery()
		p.serialization = append(p.serialization, beforeQuery...)
		queryStart, fragmentStart, err := p.parseQueryAndFragment(schemeType, baseUrl.schemeEnd, input)
		if err != nil {
			return Url{}, err
		}
		out := *baseUrl
		out.serialization = string(p.serialization)
		out.queryStart = queryStart
		out.fragmentStart = fragmentStart
		return out, nil
	case c == '#':
		return p.fragmentOnly(baseUrl, input)
	case c == '/' || c == '\\':
		slashesCount, remaining := input.countMatching(isSlashOrBackslash)
		if slashesCount >= 2 {
			p.logViolationIf(SyntaxViolation_ExpectedDoubleSlash, func() bool {
				return leadingSlashes(input) != "//"
			})
			schemeEnd := baseUrl.schemeEnd
			debugAssert(baseUrl.byteAt(schemeEnd) == ':', "base_url.byte_at(scheme_end) == b':'")
			p.serialization = append(p.serialization, baseUrl.sliceTo(schemeEnd+1)...)
			if afterPrefix, ok := input.splitPrefixStr("//"); ok {
				return p.afterDoubleSlash(afterPrefix, schemeType, schemeEnd)
			}
			return p.afterDoubleSlash(remaining, schemeType, schemeEnd)
		}
		pathStart := baseUrl.pathStart
		p.serialization = append(p.serialization, baseUrl.sliceTo(pathStart)...)
		p.serialization = append(p.serialization, '/')
		hasHost := true
		remaining = p.parsePath(
			schemeType,
			&hasHost,
			int(pathStart),
			inputAfterFirstChar,
		)
		return p.withQueryAndFragment(
			schemeType,
			baseUrl.schemeEnd,
			baseUrl.usernameEnd,
			baseUrl.hostStart,
			baseUrl.hostEnd,
			baseUrl.host,
			baseUrl.port,
			baseUrl.pathStart,
			remaining,
		)
	default:
		beforeQuery := baseUrl.beforeQuery()
		p.serialization = append(p.serialization, beforeQuery...)
		// FIXME spec says just "remove last entry", not the "pop" algorithm
		p.popPath(schemeType, int(baseUrl.pathStart))
		// A special url always has a path.
		// A path always starts with '/'
		if len(p.serialization) == int(baseUrl.pathStart) &&
			(parserSchemeTypeFrom(baseUrl.Scheme()).isSpecial() || !input.isEmpty()) {
			p.serialization = append(p.serialization, '/')
		}
		hasHost := true
		var remaining parserInput
		if first, rest := input.splitFirst(); first.IsSomeAnd(func(c rune) bool { return c == '/' }) {
			remaining = p.parsePath(
				schemeType,
				&hasHost,
				int(baseUrl.pathStart),
				rest,
			)
		} else {
			remaining = p.parsePath(schemeType, &hasHost, int(baseUrl.pathStart), input)
		}
		return p.withQueryAndFragment(
			schemeType,
			baseUrl.schemeEnd,
			baseUrl.usernameEnd,
			baseUrl.hostStart,
			baseUrl.hostEnd,
			baseUrl.host,
			baseUrl.port,
			baseUrl.pathStart,
			remaining,
		)
	}
}

func (p *parserParser) afterDoubleSlash(
	input parserInput,
	schemeType parserSchemeType,
	schemeEnd uint32,
) (Url, error) {
	p.serialization = append(p.serialization, '/')
	p.serialization = append(p.serialization, '/')
	// authority state
	beforeAuthority := len(p.serialization)
	usernameEnd, remaining, err := p.parseUserinfo(input, schemeType)
	if err != nil {
		return Url{}, err
	}
	hasAuthority := beforeAuthority != len(p.serialization)
	// host state
	hostStart, err := parserToU32(len(p.serialization))
	if err != nil {
		return Url{}, err
	}
	hostEnd, host, port, remaining, err := p.parseHostAndPort(remaining, schemeEnd, schemeType)
	if err != nil {
		return Url{}, err
	}
	if rs.Is[*hostHostInternal_None](host) && hasAuthority {
		return Url{}, ParseError_EmptyHost
	}
	// path state
	pathStart, err := parserToU32(len(p.serialization))
	if err != nil {
		return Url{}, err
	}
	hasHost := true
	remaining = p.parsePathStart(schemeType, &hasHost, remaining)
	return p.withQueryAndFragment(
		schemeType,
		schemeEnd,
		usernameEnd,
		hostStart,
		hostEnd,
		host,
		port,
		pathStart,
		remaining,
	)
}

// Return (username_end, remaining)
func (p *parserParser) parseUserinfo(
	input parserInput,
	schemeType parserSchemeType,
) (uint32, parserInput, error) {
	type lastAtT struct {
		charCount int
		remaining parserInput
	}
	lastAt := rs.None[lastAtT]()
	remaining := input
	charCount := 0
loop:
	for {
		c, ok := remaining.next()
		if !ok {
			break
		}
		switch {
		case c == '@':
			if lastAt.IsSome() {
				p.logViolation(SyntaxViolation_UnencodedAtSign)
			} else {
				p.logViolation(SyntaxViolation_EmbeddedCredentials)
			}
			lastAt = rs.Some(lastAtT{charCount: charCount, remaining: remaining})
		case c == '/' || c == '?' || c == '#':
			break loop
		case c == '\\' && schemeType.isSpecial():
			break loop
		}
		charCount++
	}
	at, ok := lastAt.Get()
	if !ok {
		usernameEnd, err := parserToU32(len(p.serialization))
		return usernameEnd, input, err
	}
	if at.charCount == 0 {
		// Otherwise, if one of the following is true
		// c is the EOF code point, U+002F (/), U+003F (?), or U+0023 (#)
		// url is special and c is U+005C (\)
		// If @ flag is set and buffer is the empty string, validation error, return failure.
		if first, _ := at.remaining.splitFirst(); first.IsSome() {
			c := first.Unwrap()
			if c == '/' || c == '?' || c == '#' || (schemeType.isSpecial() && c == '\\') {
				return 0, parserInput{}, ParseError_EmptyHost
			}
		}
		usernameEnd, err := parserToU32(len(p.serialization))
		return usernameEnd, at.remaining, err
	}
	userinfoCharCount := at.charCount
	remaining = at.remaining

	usernameEnd := rs.None[uint32]()
	hasPassword := false
	hasUsername := false
	for userinfoCharCount > 0 {
		c, utf8C, ok := input.nextUtf8()
		if !ok {
			rs.PanicAt(0, "called `Option::unwrap()` on a `None` value")
		}
		userinfoCharCount--
		if c == ':' && usernameEnd.IsNone() {
			// Start parsing password
			end, err := parserToU32(len(p.serialization))
			if err != nil {
				return 0, parserInput{}, err
			}
			usernameEnd = rs.Some(end)
			// We don't add a colon if the password is empty
			if userinfoCharCount > 0 {
				p.serialization = append(p.serialization, ':')
				hasPassword = true
			}
		} else {
			if !hasPassword {
				hasUsername = true
			}
			p.checkUrlCodePoint(c, input)
			p.serialization = appendUtf8PercentEncode(p.serialization, utf8C, parserUserinfo)
		}
	}
	var usernameEndValue uint32
	if i, ok := usernameEnd.Get(); ok {
		usernameEndValue = i
	} else {
		end, err := parserToU32(len(p.serialization))
		if err != nil {
			return 0, parserInput{}, err
		}
		usernameEndValue = end
	}
	if hasUsername || hasPassword {
		p.serialization = append(p.serialization, '@')
	}
	return usernameEndValue, remaining, nil
}

func (p *parserParser) parseHostAndPort(
	input parserInput,
	schemeEnd uint32,
	schemeType parserSchemeType,
) (uint32, hostHostInternal, rs.Option[uint16], parserInput, error) {
	host, remaining, err := parserParserParseHost(input, schemeType)
	if err != nil {
		return 0, nil, rs.None[uint16](), parserInput{}, err
	}
	p.serialization = append(p.serialization, host.String()...)
	hostEnd, err := parserToU32(len(p.serialization))
	if err != nil {
		return 0, nil, rs.None[uint16](), parserInput{}, err
	}
	if h, ok := host.(*Host_Domain); ok {
		if h.F0 == "" {
			// Port with an empty host
			if remaining.startsWithStr(":") {
				return 0, nil, rs.None[uint16](), parserInput{}, ParseError_EmptyHost
			}
			if schemeType.isSpecial() {
				return 0, nil, rs.None[uint16](), parserInput{}, ParseError_EmptyHost
			}
		}
	}

	port := rs.None[uint16]()
	if afterColon, ok := remaining.splitPrefixChar(':'); ok {
		scheme := func() rs.Option[uint16] { return parserDefaultPort(string(p.serialization[:schemeEnd])) }
		parsedPort, afterPort, err := parserParserParsePort(afterColon, scheme, p.context)
		if err != nil {
			return 0, nil, rs.None[uint16](), parserInput{}, err
		}
		if portValue, ok := parsedPort.Get(); ok {
			p.serialization = append(p.serialization, ':')
			var buffer [5]byte
			portStr := parserFastU16ToStr(&buffer, portValue)
			p.serialization = append(p.serialization, portStr...)
		}
		port, remaining = parsedPort, afterPort
	}
	return hostEnd, hostHostInternalFromHost(host), port, remaining, nil
}

func parserParserParseHost(
	input parserInput,
	schemeType parserSchemeType,
) (Host, parserInput, error) {
	if schemeType.isFile() {
		return parserParserGetFileHost(input)
	}
	// Undo the Input abstraction here to avoid allocating in the common case
	// where the host part of the input does not contain any tab or newline
	inputStr := input.chars
	insideSquareBrackets := false
	hasIgnoredChars := false
	nonIgnoredChars := 0
	byteCount := 0
scan:
	for byteCount < len(inputStr) {
		c, size := utf8.DecodeRuneInString(inputStr[byteCount:])
		switch {
		case c == ':' && !insideSquareBrackets:
			break scan
		case c == '\\' && schemeType.isSpecial():
			break scan
		case c == '/' || c == '?' || c == '#':
			break scan
		case c == '\t' || c == '\n' || c == '\r':
			hasIgnoredChars = true
		case c == '[':
			insideSquareBrackets = true
			nonIgnoredChars++
		case c == ']':
			insideSquareBrackets = false
			nonIgnoredChars++
		default:
			nonIgnoredChars++
		}
		byteCount += size
	}
	var hostStr string
	{
		if hasIgnoredChars {
			var collected []byte
			for range nonIgnoredChars {
				_, utf8C, _ := input.nextUtf8()
				collected = append(collected, utf8C...)
			}
			hostStr = string(collected)
		} else {
			for range nonIgnoredChars {
				input.next()
			}
			hostStr = inputStr[:byteCount]
		}
	}
	if schemeType == parserSchemeType_SpecialNotFile && hostStr == "" {
		return nil, parserInput{}, ParseError_EmptyHost
	}
	if !schemeType.isSpecial() {
		host, err := hostParseOpaqueCow(hostStr)
		if err != nil {
			return nil, parserInput{}, err
		}
		return host, input, nil
	}
	host, err := hostParseCow(hostStr)
	if err != nil {
		return nil, parserInput{}, err
	}
	return host, input, nil
}

func parserParserGetFileHost(input parserInput) (Host, parserInput, error) {
	_, hostStr, remaining, err := parserParserFileHost(input)
	if err != nil {
		return nil, parserInput{}, err
	}
	parsed, err := HostParse(hostStr)
	if err != nil {
		return nil, parserInput{}, err
	}
	var host Host
	switch h := parsed.(type) {
	case *Host_Domain:
		if h.F0 == "localhost" {
			host = &Host_Domain{F0: ""}
		} else {
			host = &Host_Domain{F0: h.F0}
		}
	case *Host_Ipv4:
		host = &Host_Ipv4{F0: h.F0}
	case *Host_Ipv6:
		host = &Host_Ipv6{F0: h.F0}
	default:
		panic(rs.Unreachable(parsed))
	}
	return host, remaining, nil
}

func (p *parserParser) parseFileHost(
	input parserInput,
) (bool, hostHostInternal, parserInput, error) {
	var hasHost bool
	_, hostStr, remaining, err := parserParserFileHost(input)
	if err != nil {
		return false, nil, parserInput{}, err
	}
	var host hostHostInternal
	if hostStr == "" {
		hasHost = false
		host = &hostHostInternal_None{}
	} else {
		parsed, err := hostParseCow(hostStr)
		if err != nil {
			return false, nil, parserInput{}, err
		}
		if d, ok := parsed.(*Host_Domain); ok && d.F0 == "localhost" {
			hasHost = false
			host = &hostHostInternal_None{}
		} else {
			p.serialization = append(p.serialization, parsed.String()...)
			hasHost = true
			host = hostHostInternalFromHost(parsed)
		}
	}
	return hasHost, host, remaining, nil
}

func parserParserFileHost(input parserInput) (bool, string, parserInput, error) {
	// Undo the Input abstraction here to avoid allocating in the common case
	// where the host part of the input does not contain any tab or newline
	inputStr := input.chars
	hasIgnoredChars := false
	nonIgnoredChars := 0
	byteCount := 0
scan:
	for byteCount < len(inputStr) {
		c, size := utf8.DecodeRuneInString(inputStr[byteCount:])
		switch {
		case c == '/' || c == '\\' || c == '?' || c == '#':
			break scan
		case c == '\t' || c == '\n' || c == '\r':
			hasIgnoredChars = true
		default:
			nonIgnoredChars++
		}
		byteCount += size
	}
	var hostStr string
	remaining := input
	{
		if hasIgnoredChars {
			var collected []byte
			for range nonIgnoredChars {
				_, utf8C, _ := remaining.nextUtf8()
				collected = append(collected, utf8C...)
			}
			hostStr = string(collected)
		} else {
			for range nonIgnoredChars {
				remaining.next()
			}
			hostStr = inputStr[:byteCount]
		}
	}
	if parserIsWindowsDriveLetter(hostStr) {
		return false, "", input, nil
	}
	return true, hostStr, remaining, nil
}

func parserParserParsePort(
	input parserInput,
	defaultPort func() rs.Option[uint16],
	context parserContext,
) (rs.Option[uint16], parserInput, error) {
	port := uint32(0)
	hasAnyDigit := false
	for {
		first, remaining := input.splitFirst()
		c, ok := first.Get()
		if !ok {
			break
		}
		if '0' <= c && c <= '9' {
			digit := uint32(c - '0')
			port = port*10 + digit
			if port > 0xFFFF {
				return rs.None[uint16](), parserInput{}, ParseError_InvalidPort
			}
			hasAnyDigit = true
		} else if context == parserContext_UrlParser && !(c == '/' || c == '\\' || c == '?' || c == '#') {
			return rs.None[uint16](), parserInput{}, ParseError_InvalidPort
		} else {
			break
		}
		input = remaining
	}

	if !hasAnyDigit && context == parserContext_Setter && !input.isEmpty() {
		return rs.None[uint16](), parserInput{}, ParseError_InvalidPort
	}

	optPort := rs.Some(uint16(port))
	if !hasAnyDigit || optPort == defaultPort() {
		optPort = rs.None[uint16]()
	}
	return optPort, input, nil
}

func (p *parserParser) parsePathStart(
	schemeType parserSchemeType,
	hasHost *bool,
	input parserInput,
) parserInput {
	pathStart := len(p.serialization)
	maybeC, remaining := input.splitFirst()
	isChar := func(c rune) bool {
		got, ok := maybeC.Get()
		return ok && got == c
	}
	// If url is special, then:
	if schemeType.isSpecial() {
		if isChar('\\') {
			// If c is U+005C (\), validation error.
			p.logViolation(SyntaxViolation_Backslash)
		}
		// A special URL always has a non-empty path.
		if !bytes.HasSuffix(p.serialization, []byte("/")) {
			p.serialization = append(p.serialization, '/')
			// We have already made sure the forward slash is present.
			if isChar('/') || isChar('\\') {
				return p.parsePath(schemeType, hasHost, pathStart, remaining)
			}
		}
		return p.parsePath(schemeType, hasHost, pathStart, input)
	} else if isChar('?') || isChar('#') {
		// Otherwise, if state override is not given and c is U+003F (?),
		// set url’s query to the empty string and state to query state.
		// Otherwise, if state override is not given and c is U+0023 (#),
		// set url’s fragment to the empty string and state to fragment state.
		// The query and path states will be handled by the caller.
		return input
	}

	if maybeC.IsSome() && !isChar('/') {
		p.serialization = append(p.serialization, '/')
	}
	// Otherwise, if c is not the EOF code point:
	return p.parsePath(schemeType, hasHost, pathStart, input)
}

func (p *parserParser) parsePath(
	schemeType parserSchemeType,
	hasHost *bool,
	pathStart int,
	input parserInput,
) parserInput {
	// it's much faster to call utf8_percent_encode in bulk
	pushPending := func(startStr string, remainingLen int) {
		text := startStr[:len(startStr)-remainingLen]
		if text == "" {
			return
		}
		if p.context == parserContext_PathSegmentSetter {
			if schemeType.isSpecial() {
				p.serialization = appendUtf8PercentEncode(p.serialization, text, parserSpecialPathSegment)
			} else {
				p.serialization = appendUtf8PercentEncode(p.serialization, text, parserPathSegment)
			}
		} else {
			p.serialization = appendUtf8PercentEncode(p.serialization, text, parserPath)
		}
	}

	// Relative path state
	for {
		segmentStart := len(p.serialization)
		endsWithSlash := false
		startStr := input.chars
	segment:
		for {
			inputBeforeC := input
			// bypass input.next() and manually handle ascii_tab_or_new_line
			// in order to encode string slices in bulk
			c, _, ok := input.nextRaw()
			if !ok {
				pushPending(startStr, 0)
				break
			}
			switch {
			case c == '\t' || c == '\n' || c == '\r':
				pushPending(startStr, len(inputBeforeC.chars))
				startStr = input.chars
			case c == '/' && p.context != parserContext_PathSegmentSetter:
				pushPending(startStr, len(inputBeforeC.chars))
				p.serialization = append(p.serialization, '/')
				endsWithSlash = true
				break segment
			case c == '\\' && p.context != parserContext_PathSegmentSetter &&
				schemeType.isSpecial():
				pushPending(startStr, len(inputBeforeC.chars))
				p.logViolation(SyntaxViolation_Backslash)
				p.serialization = append(p.serialization, '/')
				endsWithSlash = true
				break segment
			case (c == '?' || c == '#') && p.context == parserContext_UrlParser:
				pushPending(startStr, len(inputBeforeC.chars))
				input = inputBeforeC
				break segment
			default:
				p.checkUrlCodePoint(c, input)
				if schemeType.isFile() &&
					len(p.serialization) > pathStart &&
					parserIsNormalizedWindowsDriveLetter(
						string(p.serialization[pathStart+1:]),
					) {
					pushPending(startStr, len(inputBeforeC.chars))
					startStr = inputBeforeC.chars
					p.serialization = append(p.serialization, '/')
					segmentStart++
				}
			}
		}

		var segmentBeforeSlash string
		if endsWithSlash {
			segmentBeforeSlash = string(bytesSlice(p.serialization, segmentStart, len(p.serialization)-1))
		} else {
			segmentBeforeSlash = string(bytesSlice(p.serialization, segmentStart, len(p.serialization)))
		}
		switch segmentBeforeSlash {
		// If buffer is a double-dot path segment, shorten url’s path,
		case "..", "%2e%2e", "%2e%2E", "%2E%2e", "%2E%2E", "%2e.", "%2E.", ".%2e",
			".%2E":
			debugAssert(segmentStart >= 1 && p.serialization[segmentStart-1] == '/', "self.serialization.as_bytes()[segment_start - 1] == b'/'")
			p.serialization = p.serialization[:segmentStart]
			if bytes.HasSuffix(p.serialization, []byte("/")) &&
				parserParserLastSlashCanBeRemoved(p.serialization, pathStart) {
				p.serialization = p.serialization[:len(p.serialization)-1]
			}
			p.shortenPath(schemeType, pathStart)

			// and then if neither c is U+002F (/), nor url is special and c is U+005C (\), append the empty string to url’s path.
			if endsWithSlash && !bytes.HasSuffix(p.serialization, []byte("/")) {
				p.serialization = append(p.serialization, '/')
			}
		// Otherwise, if buffer is a single-dot path segment and if neither c is U+002F (/),
		// nor url is special and c is U+005C (\), append the empty string to url’s path.
		case ".", "%2e", "%2E":
			p.serialization = p.serialization[:segmentStart]
			if !bytes.HasSuffix(p.serialization, []byte("/")) {
				p.serialization = append(p.serialization, '/')
			}
		default:
			// If url’s scheme is "file", url’s path is empty, and buffer is a Windows drive letter, then
			if schemeType.isFile() &&
				segmentStart == pathStart+1 &&
				parserIsWindowsDriveLetter(segmentBeforeSlash) {
				// Replace the second code point in buffer with U+003A (:).
				if len(segmentBeforeSlash) > 0 {
					c := segmentBeforeSlash[0]
					p.serialization = p.serialization[:segmentStart]
					p.serialization = append(p.serialization, c)
					p.serialization = append(p.serialization, ':')
					if endsWithSlash {
						p.serialization = append(p.serialization, '/')
					}
				}
				// If url’s host is neither the empty string nor null,
				// validation error, set url’s host to the empty string.
				if *hasHost {
					p.logViolation(SyntaxViolation_FileWithHostAndWindowsDrive)
					*hasHost = false // FIXME account for this in callers
				}
			}
		}
		if !endsWithSlash {
			break
		}
	}
	if schemeType.isFile() {
		// while url’s path’s size is greater than 1
		// and url’s path[0] is the empty string,
		// validation error, remove the first item from url’s path.
		//FIXME: log violation
		if pathStart > len(p.serialization) {
			rs.PanicAt(0, "`at` split index (is "+itoa(pathStart)+") should be <= len (is "+itoa(len(p.serialization))+")")
		}
		path := string(p.serialization[pathStart:])
		p.serialization = p.serialization[:pathStart]
		p.serialization = append(p.serialization, '/')
		p.serialization = append(p.serialization, strings.TrimLeft(path, "/")...)
	}

	return input
}

func parserParserLastSlashCanBeRemoved(serialization []byte, pathStart int) bool {
	urlBeforeSegment := serialization[:len(serialization)-1]
	if segmentBeforeStart := bytes.LastIndexByte(urlBeforeSegment, '/'); segmentBeforeStart >= 0 {
		// Do not remove the root slash
		return segmentBeforeStart >= pathStart &&
			// Or a windows drive letter slash
			!parserPathStartsWithWindowsDriveLetter(string(serialization[segmentBeforeStart:]))
	} else {
		return false
	}
}

// https://url.spec.whatwg.org/#shorten-a-urls-path
func (p *parserParser) shortenPath(schemeType parserSchemeType, pathStart int) {
	// If path is empty, then return.
	if len(p.serialization) == pathStart {
		return
	}
	// If url’s scheme is "file", path’s size is 1, and path[0] is a normalized Windows drive letter, then return.
	if schemeType.isFile() &&
		parserIsNormalizedWindowsDriveLetter(string(bytesSlice(p.serialization, pathStart, len(p.serialization)))) {
		return
	}
	// Remove path’s last item.
	p.popPath(schemeType, pathStart)
}

// https://url.spec.whatwg.org/#pop-a-urls-path
func (p *parserParser) popPath(schemeType parserSchemeType, pathStart int) {
	if len(p.serialization) > pathStart {
		slashPosition := bytes.LastIndexByte(p.serialization[pathStart:], '/')
		if slashPosition < 0 {
			rs.PanicAt(0, "called `Option::unwrap()` on a `None` value")
		}
		// + 1 since rfind returns the position before the slash.
		segmentStart := pathStart + slashPosition + 1
		// Don’t pop a Windows drive letter
		if !(schemeType.isFile() &&
			parserIsNormalizedWindowsDriveLetter(string(p.serialization[segmentStart:]))) {
			p.serialization = p.serialization[:segmentStart]
		}
	}
}

func (p *parserParser) parseCannotBeABasePath(input parserInput) parserInput {
	for {
		inputBeforeC := input
		c, utf8C, ok := input.nextUtf8()
		switch {
		case ok && (c == '?' || c == '#') && p.context == parserContext_UrlParser:
			return inputBeforeC
		case ok:
			p.checkUrlCodePoint(c, input)
			p.serialization = appendUtf8PercentEncode(p.serialization, utf8C, percentEncodingControls)
		default:
			return input
		}
	}
}

func (p *parserParser) withQueryAndFragment(
	schemeType parserSchemeType,
	schemeEnd uint32,
	usernameEnd uint32,
	hostStart uint32,
	hostEnd uint32,
	host hostHostInternal,
	port rs.Option[uint16],
	pathStart uint32,
	remaining parserInput,
) (Url, error) {
	// Special case for anarchist URL's with a leading empty path segment
	// This prevents web+demo:/.//not-a-host/ or web+demo:/path/..//not-a-host/,
	// when parsed and then serialized, from ending up as web+demo://not-a-host/
	// (they end up as web+demo:/.//not-a-host/).
	//
	// If url’s host is null, url does not have an opaque path,
	// url’s path’s size is greater than 1, and url’s path[0] is the empty string,
	// then append U+002F (/) followed by U+002E (.) to output.
	schemeEndAsUsize := int(schemeEnd)
	pathStartAsUsize := int(pathStart)
	if pathStartAsUsize == schemeEndAsUsize+1 {
		// Anarchist URL
		if bytes.HasPrefix(bytesSlice(p.serialization, pathStartAsUsize, len(p.serialization)), []byte("//")) {
			// Case 1: The base URL did not have an empty path segment, but the resulting one does
			// Insert the "/." prefix
			p.serialization = insertStr(p.serialization, pathStartAsUsize, "/.")
			pathStart += 2
		}
		rsAssert(!bytes.HasPrefix(bytesSlice(p.serialization, schemeEndAsUsize, len(p.serialization)), []byte("://")),
			"assertion failed: !self.serialization[scheme_end_as_usize..].starts_with(\"://\")")
	} else if pathStartAsUsize == schemeEndAsUsize+3 &&
		string(bytesSlice(p.serialization, schemeEndAsUsize, pathStartAsUsize)) == ":/." {
		// Anarchist URL with leading empty path segment
		// The base URL has a "/." between the host and the path
		if pathStartAsUsize >= len(p.serialization) {
			rs.PanicAt(0, "index out of bounds: the len is "+itoa(len(p.serialization))+" but the index is "+itoa(pathStartAsUsize))
		}
		if left := p.serialization[pathStartAsUsize]; left != '/' {
			rs.PanicAt(0, "assertion `left == right` failed\n  left: "+itoa(int(left))+"\n right: 47")
		}
		if !(pathStartAsUsize+1 < len(p.serialization) && p.serialization[pathStartAsUsize+1] == '/') {
			// Case 2: The base URL had an empty path segment, but the resulting one does not
			// Remove the "/." prefix
			p.serialization = replaceRange(p.serialization, schemeEndAsUsize, pathStartAsUsize, ":")
			pathStart -= 2
		}
		rsAssert(!bytes.HasPrefix(bytesSlice(p.serialization, schemeEndAsUsize, len(p.serialization)), []byte("://")),
			"assertion failed: !self.serialization[scheme_end_as_usize..].starts_with(\"://\")")
	}

	queryStart, fragmentStart, err := p.parseQueryAndFragment(schemeType, schemeEnd, remaining)
	if err != nil {
		return Url{}, err
	}
	return Url{
		serialization: string(p.serialization),
		schemeEnd:     schemeEnd,
		usernameEnd:   usernameEnd,
		hostStart:     hostStart,
		hostEnd:       hostEnd,
		host:          host,
		port:          port,
		pathStart:     pathStart,
		queryStart:    queryStart,
		fragmentStart: fragmentStart,
	}, nil
}

// Return (query_start, fragment_start)
func (p *parserParser) parseQueryAndFragment(
	schemeType parserSchemeType,
	schemeEnd uint32,
	input parserInput,
) (rs.Option[uint32], rs.Option[uint32], error) {
	queryStart := rs.None[uint32]()
	c, ok := input.next()
	switch {
	case ok && c == '#':
	case ok && c == '?':
		start, err := parserToU32(len(p.serialization))
		if err != nil {
			return rs.None[uint32](), rs.None[uint32](), err
		}
		queryStart = rs.Some(start)
		p.serialization = append(p.serialization, '?')
		remaining := p.parseQuery(schemeType, schemeEnd, input)
		if remaining, ok := remaining.Get(); ok {
			input = remaining
		} else {
			return queryStart, rs.None[uint32](), nil
		}
	case !ok:
		return rs.None[uint32](), rs.None[uint32](), nil
	default:
		panic(rs.Panicf("Programming error. parse_query_and_fragment() called without ? or #"))
	}

	fragmentStart, err := parserToU32(len(p.serialization))
	if err != nil {
		return rs.None[uint32](), rs.None[uint32](), err
	}
	p.serialization = append(p.serialization, '#')
	p.parseFragment(input)
	return queryStart, rs.Some(fragmentStart), nil
}

func (p *parserParser) parseQuery(
	schemeType parserSchemeType,
	schemeEnd uint32,
	input parserInput,
) rs.Option[parserInput] {
	isUrlParser := p.context == parserContext_UrlParser
	violationFn := p.violationFn
	// partIterNext is QueryPartIter::next.
	partIterNext := func() (string, bool, bool) {
		start := input.chars
		// bypass self.input.next() in order to get string slices
		// which are faster to operate on
		for {
			c, _, ok := input.nextRaw()
			if !ok {
				break
			}
			switch {
			case c == '\t' || c == '\n' || c == '\r':
				return start[:len(start)-len(input.chars)-1], false, true
			case c == '#' && isUrlParser:
				return start[:len(start)-len(input.chars)-1], true, true
			default:
				if vfn, ok := violationFn.Get(); ok {
					parserCheckUrlCodePoint(vfn, c, input)
				}
			}
		}
		if start == "" {
			return "", false, false
		}
		return start, false, true
	}

	set := parserQuery
	if schemeType.isSpecial() {
		set = parserSpecialQuery
	}
	queryEncodingOverride := p.queryEncodingOverride.Filter(func(func(string) []byte) bool {
		switch string(p.serialization[:schemeEnd]) {
		case "http", "https", "file", "ftp":
			return true
		}
		return false
	})

	for {
		part, isFinished, ok := partIterNext()
		if !ok {
			break
		}
		// slightly faster to be repetitive and not convert text to Cow
		if o, ok := queryEncodingOverride.Get(); ok {
			p.serialization = percentEncodingPercentEncodeNew(o(part), set).appendTo(p.serialization)
		} else {
			p.serialization = appendUtf8PercentEncode(p.serialization, part, set)
		}
		if isFinished {
			return rs.Some(input)
		}
	}

	return rs.None[parserInput]()
}

func (p *parserParser) fragmentOnly(baseUrl *Url, input parserInput) (Url, error) {
	var beforeFragment string
	if i, ok := baseUrl.fragmentStart.Get(); ok {
		beforeFragment = baseUrl.sliceTo(i)
	} else {
		beforeFragment = baseUrl.serialization
	}
	debugAssert(len(p.serialization) == 0, "self.serialization.is_empty()")
	p.serialization = append(p.serialization, beforeFragment...)
	p.serialization = append(p.serialization, '#')
	next, ok := input.next()
	debugAssert(ok && next == '#', "next == Some('#')")
	p.parseFragment(input)
	fragmentStart, err := parserToU32(len(beforeFragment))
	if err != nil {
		return Url{}, err
	}
	out := *baseUrl
	out.serialization = string(p.serialization)
	out.fragmentStart = rs.Some(fragmentStart)
	return out, nil
}

func (p *parserParser) parseFragment(input parserInput) {
	violationFn := p.violationFn
	// partIterNext is FragmentPartIter::next.
	partIterNext := func() (string, bool) {
		start := input.chars
		// bypass self.input.next() in order to get string slices
		// which are faster to operate on
		for {
			c, _, ok := input.nextRaw()
			if !ok {
				break
			}
			switch {
			case c == '\t' || c == '\n' || c == '\r':
				return start[:len(start)-len(input.chars)-1], true
			case c == 0:
				if vfn, ok := violationFn.Get(); ok {
					vfn(SyntaxViolation_NullInFragment)
				}
			default:
				if vfn, ok := violationFn.Get(); ok {
					parserCheckUrlCodePoint(vfn, c, input)
				}
			}
		}
		if start == "" {
			return "", false
		}
		return start, true
	}

	for {
		part, ok := partIterNext()
		if !ok {
			break
		}
		p.serialization = appendUtf8PercentEncode(p.serialization, part, parserFragment)
	}
}

func (p *parserParser) checkUrlCodePoint(c rune, input parserInput) {
	if vfn, ok := p.violationFn.Get(); ok {
		parserCheckUrlCodePoint(vfn, c, input)
	}
}

func isAsciiHexdigit(c rune) bool {
	return ('0' <= c && c <= '9') || ('a' <= c && c <= 'f') || ('A' <= c && c <= 'F')
}

func parserCheckUrlCodePoint(vfn func(SyntaxViolation), c rune, input parserInput) {
	if c == '%' {
		a, okA := input.next()
		b, okB := input.next()
		if !(okA && okB && isAsciiHexdigit(a) && isAsciiHexdigit(b)) {
			vfn(SyntaxViolation_PercentDecode)
		}
	} else if !parserIsUrlCodePoint(c) {
		vfn(SyntaxViolation_NonUrlCodePoint)
	}
}

// Non URL code points:
// U+0000 to U+0020 (space)
// " # % < > [ \ ] ^ ` { | }
// U+007F to U+009F
// surrogates
// U+FDD0 to U+FDEF
// Last two of each plane: U+__FFFE to U+__FFFF for __ in 00 to 10 hex
func parserIsUrlCodePoint(c rune) bool {
	switch {
	case 'a' <= c && c <= 'z',
		'A' <= c && c <= 'Z',
		'0' <= c && c <= '9',
		c == '!', c == '$', c == '&', c == '\'', c == '(', c == ')', c == '*', c == '+', c == ',', c == '-',
		c == '.', c == '/', c == ':', c == ';', c == '=', c == '?', c == '@', c == '_', c == '~',
		0xA0 <= c && c <= 0xD7FF, 0xE000 <= c && c <= 0xFDCF, 0xFDF0 <= c && c <= 0xFFFD,
		0x10000 <= c && c <= 0x1FFFD, 0x20000 <= c && c <= 0x2FFFD,
		0x30000 <= c && c <= 0x3FFFD, 0x40000 <= c && c <= 0x4FFFD,
		0x50000 <= c && c <= 0x5FFFD, 0x60000 <= c && c <= 0x6FFFD,
		0x70000 <= c && c <= 0x7FFFD, 0x80000 <= c && c <= 0x8FFFD,
		0x90000 <= c && c <= 0x9FFFD, 0xA0000 <= c && c <= 0xAFFFD,
		0xB0000 <= c && c <= 0xBFFFD, 0xC0000 <= c && c <= 0xCFFFD,
		0xD0000 <= c && c <= 0xDFFFD, 0xE1000 <= c && c <= 0xEFFFD,
		0xF0000 <= c && c <= 0xFFFFD, 0x100000 <= c && c <= 0x10FFFD:
		return true
	}
	return false
}

// https://infra.spec.whatwg.org/#ascii-tab-or-newline
func parserAsciiTabOrNewLine(ch rune) bool {
	return ch == '\t' || ch == '\n' || ch == '\r'
}

// https://url.spec.whatwg.org/#ascii-alpha
func parserAsciiAlpha(ch rune) bool {
	return ('a' <= ch && ch <= 'z') || ('A' <= ch && ch <= 'Z')
}

func parserToU32(i int) (uint32, error) {
	if uint64(i) <= 0xFFFFFFFF {
		return uint32(i), nil
	} else {
		return 0, ParseError_Overflow
	}
}

func parserIsNormalizedWindowsDriveLetter(segment string) bool {
	return parserIsWindowsDriveLetter(segment) && segment[1] == ':'
}

// Whether the scheme is file:, the path has a single segment, and that segment
// is a Windows drive letter
func parserIsWindowsDriveLetter(segment string) bool {
	return len(segment) == 2 && parserStartsWithWindowsDriveLetter(segment)
}

// Whether path starts with a root slash
// and a windows drive letter eg: "/c:" or "/a:/"
func parserPathStartsWithWindowsDriveLetter(s string) bool {
	if len(s) > 0 {
		c := s[0]
		return (c == '/' || c == '\\' || c == '?' || c == '#') && parserStartsWithWindowsDriveLetter(s[1:])
	} else {
		return false
	}
}

func parserStartsWithWindowsDriveLetter(s string) bool {
	return len(s) >= 2 &&
		parserAsciiAlpha(rune(s[0])) &&
		(s[1] == ':' || s[1] == '|') &&
		(len(s) == 2 || s[2] == '/' || s[2] == '\\' || s[2] == '?' || s[2] == '#')
}

// https://url.spec.whatwg.org/#start-with-a-windows-drive-letter
func parserStartsWithWindowsDriveLetterSegment(input parserInput) bool {
	a, okA := input.next()
	b, okB := input.next()
	c, okC := input.next()
	switch {
	// its first two code points are a Windows drive letter
	// its third code point is U+002F (/), U+005C (\), U+003F (?), or U+0023 (#).
	case okA && okB && okC &&
		parserAsciiAlpha(a) && (b == ':' || b == '|') && (c == '/' || c == '\\' || c == '?' || c == '#'):
		return true
	// its first two code points are a Windows drive letter
	// its length is 2
	case okA && okB && !okC && parserAsciiAlpha(a) && (b == ':' || b == '|'):
		return true
	default:
		return false
	}
}

func parserFastU16ToStr(
	// max 5 digits for u16 (65535)
	buffer *[5]byte,
	value uint16,
) string {
	index := len(buffer)

	for {
		index--
		buffer[index] = '0' + byte(value%10)
		value /= 10
		if value == 0 {
			break
		}
	}

	return string(buffer[index:])
}
