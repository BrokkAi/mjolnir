// Port of url 2.5.8 src/lib.rs

package url

import (
	"net/netip"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Url is url::Url: a parsed URL record.
//
// It is a plain value: assigning a Url copies it, as `.clone()` does in Rust.
// The zero value is not a URL; every Url comes from UrlParse, Join or one of
// the other constructors.
type Url struct {
	// Syntax in pseudo-BNF:
	//
	//   url = scheme ":" [ hierarchical | non-hierarchical ] [ "?" query ]? [ "#" fragment ]?
	//   non-hierarchical = non-hierarchical-path
	//   non-hierarchical-path = /* Does not start with "/" */
	//   hierarchical = authority? hierarchical-path
	//   authority = "//" userinfo? host [ ":" port ]?
	//   userinfo = username [ ":" password ]? "@"
	//   hierarchical-path = [ "/" path-segment ]+
	serialization string

	// Components
	schemeEnd     uint32 // Before ':'
	usernameEnd   uint32 // Before ':' (if a password is given) or '@' (if not)
	hostStart     uint32
	hostEnd       uint32
	host          hostHostInternal
	port          rs.Option[uint16]
	pathStart     uint32            // Before initial '/', if any
	queryStart    rs.Option[uint32] // Before '?', unlike Position::QueryStart
	fragmentStart rs.Option[uint32] // Before '#', unlike Position::FragmentStart
}

// ParseOptions is url::ParseOptions: full configuration for the URL parser.
type ParseOptions struct {
	baseUrl          rs.Option[*Url]
	encodingOverride FormUrlencodedEncodingOverride
	violationFn      rs.Option[func(SyntaxViolation)]
}

// EncodingOverride is url::EncodingOverride, the re-export of
// form_urlencoded::EncodingOverride.
type EncodingOverride = FormUrlencodedEncodingOverride

// BaseUrl is ParseOptions::base_url: change the base URL.
//
// See the notes of Url.Join for more details about how this base is
// considered when parsing.
func (o ParseOptions) BaseUrl(new rs.Option[*Url]) ParseOptions {
	o.baseUrl = new
	return o
}

// EncodingOverride is ParseOptions::encoding_override: override the
// character encoding of query strings. This is a legacy concept only
// relevant for HTML.
func (o ParseOptions) EncodingOverride(new EncodingOverride) ParseOptions {
	o.encodingOverride = new
	return o
}

// SyntaxViolationCallback is ParseOptions::syntax_violation_callback: call
// the provided function or closure for a non-fatal `SyntaxViolation` when it
// occurs during parsing. Note that since the provided function is `Fn`, the
// caller might need to utilize interior mutability, such as with a
// `RefCell`, to collect the violations.
func (o ParseOptions) SyntaxViolationCallback(new rs.Option[func(SyntaxViolation)]) ParseOptions {
	o.violationFn = new
	return o
}

// Parse is ParseOptions::parse: parse an URL string with the configuration
// so far.
func (o ParseOptions) Parse(input string) (Url, error) {
	parser := &parserParser{
		serialization:         make([]byte, 0, len(input)),
		baseUrl:               o.baseUrl,
		queryEncodingOverride: o.encodingOverride,
		violationFn:           o.violationFn,
		context:               parserContext_UrlParser,
	}
	return parser.parseUrl(input)
}

// UrlParse is Url::parse: parse an absolute URL from a string.
//
// If the function can not parse an absolute URL from the given string, a
// ParseError variant is returned.
func UrlParse(input string) (Url, error) {
	return UrlOptions().Parse(input)
}

// UrlParseWithParams is Url::parse_with_params: parse an absolute URL from a
// string and add params to its query string.
//
// Existing params are not removed.
func UrlParseWithParams(input string, iter []rs.Tuple2[string, string]) (Url, error) {
	url, err := UrlOptions().Parse(input)

	if err == nil {
		url.QueryPairsMut().ExtendPairs(iter).Drop()
	}

	return url, err
}

// https://url.spec.whatwg.org/#potentially-strip-trailing-spaces-from-an-opaque-path
func (u *Url) stripTrailingSpacesFromOpaquePath() {
	if !u.CannotBeABase() {
		return
	}

	if u.fragmentStart.IsSome() {
		return
	}

	if u.queryStart.IsSome() {
		return
	}

	trailingSpaceCount := 0
	for i := len(u.serialization) - 1; i >= 0 && u.serialization[i] == ' '; i-- {
		trailingSpaceCount++
	}

	start := len(u.serialization) - trailingSpaceCount

	u.serialization = u.serialization[:start]
}

// Join is Url::join: parse a string as an URL, with this URL as the base
// URL.
//
// The inverse of this is MakeRelative.
//
// # Notes
//
//   - A trailing slash is significant.
//     Without it, the last path component is considered to be a “file” name
//     to be removed to get at the “directory” that is used as the base.
//   - A scheme relative special URL
//     as input replaces everything in the base URL after the scheme.
//   - An absolute URL (with a scheme) as input replaces the whole base URL
//     (even the scheme).
func (u *Url) Join(input string) (Url, error) {
	return UrlOptions().BaseUrl(rs.Some(u)).Parse(input)
}

// MakeRelative is Url::make_relative: creates a relative URL if possible,
// with this URL as the base URL.
//
// This is the inverse of Join.
func (u *Url) MakeRelative(url *Url) rs.Option[string] {
	if u.CannotBeABase() {
		return rs.None[string]()
	}

	// Scheme, host and port need to be the same
	if u.Scheme() != url.Scheme() || !optionHostEqual(u.Host(), url.Host()) || u.Port() != url.Port() {
		return rs.None[string]()
	}

	// We ignore username/password at this point

	// The path has to be transformed
	relative := ""

	// Extract the filename of both URIs, these need to be handled separately
	extractPathFilename := func(s string) (string, string) {
		lastSlashIdx := strings.LastIndexByte(s, '/')
		if lastSlashIdx < 0 {
			lastSlashIdx = 0
		}
		path, filename := s[:lastSlashIdx], s[lastSlashIdx:]
		if filename == "" {
			return path, ""
		} else {
			return path, filename[1:]
		}
	}

	basePathStr, baseFilename := extractPathFilename(u.Path())
	urlPathStr, urlFilename := extractPathFilename(url.Path())

	basePath := strings.Split(basePathStr, "/")
	urlPath := strings.Split(urlPathStr, "/")

	// Skip over the common prefix
	for len(basePath) > 0 && len(urlPath) > 0 && basePath[0] == urlPath[0] {
		basePath = basePath[1:]
		urlPath = urlPath[1:]
	}

	// Add `..` segments for the remainder of the base path
	for _, basePathSegment := range basePath {
		// Skip empty last segments
		if basePathSegment == "" {
			break
		}

		if relative != "" {
			relative += "/"
		}

		relative += ".."
	}

	// Append the remainder of the other URI
	for _, urlPathSegment := range urlPath {
		if relative != "" {
			relative += "/"
		}

		relative += urlPathSegment
	}

	// Add the filename if they are not the same
	if relative != "" || baseFilename != urlFilename {
		// If the URIs filename is empty this means that it was a directory
		// so we'll have to append a '/'.
		//
		// Otherwise append it directly as the new filename.
		if urlFilename == "" {
			relative += "/"
		} else {
			if relative != "" {
				relative += "/"
			}
			relative += urlFilename
		}
	}

	// Query and fragment are only taken from the other URI
	if query, ok := url.Query().Get(); ok {
		relative += "?"
		relative += query
	}

	if fragment, ok := url.Fragment().Get(); ok {
		relative += "#"
		relative += fragment
	}

	return rs.Some(relative)
}

func optionHostEqual(a rs.Option[Host], b rs.Option[Host]) bool {
	x, okA := a.Get()
	y, okB := b.Get()
	if okA != okB {
		return false
	}
	return !okA || x.Equal(y)
}

// UrlOptions is Url::options: return a default `ParseOptions` that can fully
// configure the URL parser.
func UrlOptions() ParseOptions {
	return ParseOptions{
		baseUrl:          rs.None[*Url](),
		encodingOverride: rs.None[func(string) []byte](),
		violationFn:      rs.None[func(SyntaxViolation)](),
	}
}

// AsStr is Url::as_str: return the serialization of this URL.
//
// This is fast since that serialization is already stored in the `Url`
// struct.
func (u *Url) AsStr() string {
	return u.serialization
}

// IntoString is Url::into_string: return the serialization of this URL.
//
// Deprecated: since url 2.3.0, use StringFromUrl.
func (u *Url) IntoString() string {
	return u.serialization
}

// stringError is the error of a Rust `Result<T, String>`.
type stringError string

func (e stringError) Error() string { return string(e) }

// CheckInvariants is Url::check_invariants: for internal testing, not
// stable.
//
// Methods of the `Url` struct assume a number of invariants. This checks
// each of these invariants and panic if one is not met. This is for testing
// rust-url itself.
//
// The error is the `String` of the Rust `Result<(), String>`.
func (u *Url) CheckInvariants() error {
	fail := func(expr string) error {
		return stringError("!( " + expr + " ) for URL " + rs.DebugString(u.serialization))
	}
	failEq := func(a any, b any, exprA string, exprB string) error {
		return stringError(rs.DebugString(a) + " != " + rs.DebugString(b) + " (" + exprA + " != " + exprB + ") for URL " + rs.DebugString(u.serialization))
	}
	byteEq := func(index uint32, want byte, exprA string, exprB string) error {
		if got := u.byteAt(index); got != want {
			return failEq(got, want, exprA, exprB)
		}
		return nil
	}
	u32Eq := func(a uint32, b uint32, exprA string, exprB string) error {
		if a != b {
			return failEq(a, b, exprA, exprB)
		}
		return nil
	}

	if !(u.schemeEnd >= 1) {
		return fail("self.scheme_end >= 1")
	}
	if !parserAsciiAlpha(rune(u.byteAt(0))) {
		return fail("self.byte_at(0).is_ascii_alphabetic()")
	}
	for _, c := range []byte(u.sliceRange(1, u.schemeEnd)) {
		if !(('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z') || ('0' <= c && c <= '9') || c == '+' || c == '-' || c == '.') {
			return fail("self.slice(1..self.scheme_end).chars().all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '+' | '-' | '.'))")
		}
	}
	if err := byteEq(u.schemeEnd, ':', "self.byte_at(self.scheme_end)", "b':'"); err != nil {
		return err
	}

	if strings.HasPrefix(u.sliceFrom(u.schemeEnd+1), "//") {
		// URL with authority
		if u.usernameEnd != uint32(len(u.serialization)) {
			switch u.byteAt(u.usernameEnd) {
			case ':':
				if !(u.hostStart >= u.usernameEnd+2) {
					return fail("self.host_start >= self.username_end + 2")
				}
				if err := byteEq(u.hostStart-1, '@', "self.byte_at(self.host_start - 1)", "b'@'"); err != nil {
					return err
				}
			case '@':
				if !(u.hostStart == u.usernameEnd+1) {
					return fail("self.host_start == self.username_end + 1")
				}
			default:
				if err := u32Eq(u.usernameEnd, u.schemeEnd+3, "self.username_end", "self.scheme_end + 3"); err != nil {
					return err
				}
			}
		}
		if !(u.hostStart >= u.usernameEnd) {
			return fail("self.host_start >= self.username_end")
		}
		if !(u.hostEnd >= u.hostStart) {
			return fail("self.host_end >= self.host_start")
		}
		hostStr := u.sliceRange(u.hostStart, u.hostEnd)
		switch host := u.host.(type) {
		case *hostHostInternal_None:
			if hostStr != "" {
				return failEq(hostStr, "", "host_str", "\"\"")
			}
		case *hostHostInternal_Ipv4:
			if want := hostIpv4String(host.F0); hostStr != want {
				return failEq(hostStr, want, "host_str", "address.to_string()")
			}
		case *hostHostInternal_Ipv6:
			h := &Host_Ipv6{F0: host.F0}
			if want := h.String(); hostStr != want {
				return failEq(hostStr, want, "host_str", "h.to_string()")
			}
		case *hostHostInternal_Domain:
			if parserSchemeTypeFrom(u.Scheme()).isSpecial() {
				if hostStr == "" {
					return fail("!host_str.is_empty()")
				}
			}
		default:
			panic(rs.Unreachable(u.host))
		}
		if u.pathStart == u.hostEnd {
			if u.port.IsSome() {
				return failEq(u.port, rs.None[uint16](), "self.port", "None")
			}
		} else {
			if err := byteEq(u.hostEnd, ':', "self.byte_at(self.host_end)", "b':'"); err != nil {
				return err
			}
			portStr := u.sliceRange(u.hostEnd+1, u.pathStart)
			parsed, ok := parseU16(portStr)
			if !ok {
				rs.PanicAt(0, "Couldn't parse port?: "+parseIntErrorDebug(portStr))
			}
			if want := rs.Some(parsed); u.port != want {
				return failEq(u.port, want, "self.port", "Some(port_str.parse::<u16>().expect(\"Couldn't parse port?\"))")
			}
		}
		if !(int(u.pathStart) == len(u.serialization) ||
			(u.byteAt(u.pathStart) == '/' || u.byteAt(u.pathStart) == '#' || u.byteAt(u.pathStart) == '?')) {
			return fail("self.path_start as usize == self.serialization.len()\n" +
				"                    || matches!(self.byte_at(self.path_start), b'/' | b'#' | b'?')")
		}
	} else {
		// Anarchist URL (no authority)
		if err := u32Eq(u.usernameEnd, u.schemeEnd+1, "self.username_end", "self.scheme_end + 1"); err != nil {
			return err
		}
		if err := u32Eq(u.hostStart, u.schemeEnd+1, "self.host_start", "self.scheme_end + 1"); err != nil {
			return err
		}
		if err := u32Eq(u.hostEnd, u.schemeEnd+1, "self.host_end", "self.scheme_end + 1"); err != nil {
			return err
		}
		if !rs.Is[*hostHostInternal_None](u.host) {
			return failEq(u.host, &hostHostInternal_None{}, "self.host", "HostInternal::None")
		}
		if u.port.IsSome() {
			return failEq(u.port, rs.None[uint16](), "self.port", "None")
		}
		if strings.HasPrefix(u.Path(), "//") {
			// special case when first path segment is empty
			if err := byteEq(u.schemeEnd+1, '/', "self.byte_at(self.scheme_end + 1)", "b'/'"); err != nil {
				return err
			}
			if err := byteEq(u.schemeEnd+2, '.', "self.byte_at(self.scheme_end + 2)", "b'.'"); err != nil {
				return err
			}
			if err := u32Eq(u.pathStart, u.schemeEnd+3, "self.path_start", "self.scheme_end + 3"); err != nil {
				return err
			}
		} else {
			if err := u32Eq(u.pathStart, u.schemeEnd+1, "self.path_start", "self.scheme_end + 1"); err != nil {
				return err
			}
		}
	}
	if start, ok := u.queryStart.Get(); ok {
		if !(start >= u.pathStart) {
			return fail("start >= self.path_start")
		}
		if err := byteEq(start, '?', "self.byte_at(start)", "b'?'"); err != nil {
			return err
		}
	}
	if start, ok := u.fragmentStart.Get(); ok {
		if !(start >= u.pathStart) {
			return fail("start >= self.path_start")
		}
		if err := byteEq(start, '#', "self.byte_at(start)", "b'#'"); err != nil {
			return err
		}
	}
	if queryStart, ok := u.queryStart.Get(); ok {
		if fragmentStart, ok := u.fragmentStart.Get(); ok {
			if !(fragmentStart > queryStart) {
				return fail("fragment_start > query_start")
			}
		}
	}

	other, err := UrlParse(u.AsStr())
	if err != nil {
		rs.PanicAt(0, "Failed to parse myself?: "+err.(ParseError).rustName())
	}
	if u.serialization != other.serialization {
		return failEq(u.serialization, other.serialization, "&self.serialization", "&other.serialization")
	}
	if err := u32Eq(u.schemeEnd, other.schemeEnd, "self.scheme_end", "other.scheme_end"); err != nil {
		return err
	}
	if err := u32Eq(u.usernameEnd, other.usernameEnd, "self.username_end", "other.username_end"); err != nil {
		return err
	}
	if err := u32Eq(u.hostStart, other.hostStart, "self.host_start", "other.host_start"); err != nil {
		return err
	}
	if err := u32Eq(u.hostEnd, other.hostEnd, "self.host_end", "other.host_end"); err != nil {
		return err
	}
	if !(hostHostInternalEq(u.host, other.host) ||
		// XXX No host round-trips to empty host.
		// See https://github.com/whatwg/url/issues/79
		(u.HostStr().IsNone() && other.HostStr() == rs.Some(""))) {
		return fail("self.host == other.host ||\n" +
			"                // XXX No host round-trips to empty host.\n" +
			"                // See https://github.com/whatwg/url/issues/79\n" +
			"                (self.host_str(), other.host_str()) == (None, Some(\"\"))")
	}
	if u.port != other.port {
		return failEq(u.port, other.port, "self.port", "other.port")
	}
	if err := u32Eq(u.pathStart, other.pathStart, "self.path_start", "other.path_start"); err != nil {
		return err
	}
	if u.queryStart != other.queryStart {
		return failEq(u.queryStart, other.queryStart, "self.query_start", "other.query_start")
	}
	if u.fragmentStart != other.fragmentStart {
		return failEq(u.fragmentStart, other.fragmentStart, "self.fragment_start", "other.fragment_start")
	}
	return nil
}

func parseU16(s string) (uint16, bool) {
	// u16::from_str accepts a leading '+'.
	digits := strings.TrimPrefix(s, "+")
	if digits == "" || !isAllAsciiDigits(digits) {
		return 0, false
	}
	value, err := strconv.ParseUint(digits, 10, 16)
	if err != nil {
		return 0, false
	}
	return uint16(value), true
}

// parseIntErrorDebug is the Debug form of the ParseIntError that
// `s.parse::<u16>()` returns.
func parseIntErrorDebug(s string) string {
	digits := strings.TrimPrefix(s, "+")
	kind := "PosOverflow"
	switch {
	case s == "":
		kind = "Empty"
	case digits == "" || !isAllAsciiDigits(digits):
		kind = "InvalidDigit"
	}
	return "ParseIntError { kind: " + kind + " }"
}

// Origin is Url::origin: return the origin of this URL
// (<https://url.spec.whatwg.org/#origin>)
//
// Note: this returns an opaque origin for `file:` URLs, which causes
// `url.origin() != url.origin()`.
func (u *Url) Origin() Origin {
	return originUrlOrigin(u)
}

// Scheme is Url::scheme: return the scheme of this URL, lower-cased, as an
// ASCII string without the ':' delimiter.
func (u *Url) Scheme() string {
	return u.sliceTo(u.schemeEnd)
}

// IsSpecial is Url::is_special: return whether the URL is special (has a
// special scheme).
func (u *Url) IsSpecial() bool {
	schemeType := parserSchemeTypeFrom(u.Scheme())
	return schemeType.isSpecial()
}

// HasAuthority is Url::has_authority: return whether the URL has an
// 'authority', which can contain a username, password, host, and port
// number.
//
// URLs that do *not* are either path-only like `unix:/run/foo.socket`
// or cannot-be-a-base like `data:text/plain,Stuff`.
//
// See also the `Authority` method.
func (u *Url) HasAuthority() bool {
	debugAssert(u.byteAt(u.schemeEnd) == ':', "self.byte_at(self.scheme_end) == b':'")
	return strings.HasPrefix(u.sliceFrom(u.schemeEnd), "://")
}

// Authority is Url::authority: return the authority of this URL as an ASCII
// string.
//
// Non-ASCII domains are punycode-encoded per IDNA if this is the host
// of a special URL, or percent encoded for non-special URLs.
// IPv6 addresses are given between `[` and `]` brackets.
// Ports are omitted if they match the well known port of a special URL.
//
// Username and password are percent-encoded.
//
// See also the `HasAuthority` method.
func (u *Url) Authority() string {
	schemeSeparatorLen := uint32(len("://"))
	if u.HasAuthority() && u.pathStart > u.schemeEnd+schemeSeparatorLen {
		return u.sliceRange(u.schemeEnd+schemeSeparatorLen, u.pathStart)
	} else {
		return ""
	}
}

// CannotBeABase is Url::cannot_be_a_base: return whether this URL is a
// cannot-be-a-base URL, meaning that parsing a relative URL string with this
// URL as the base will return an error.
//
// This is the case if the scheme and `:` delimiter are not followed by a `/`
// slash, as is typically the case of `data:` and `mailto:` URLs.
func (u *Url) CannotBeABase() bool {
	return !strings.HasPrefix(u.sliceFrom(u.schemeEnd+1), "/")
}

// Username is Url::username: return the username for this URL (typically
// the empty string) as a percent-encoded ASCII string.
func (u *Url) Username() string {
	schemeSeparatorLen := uint32(len("://"))
	if u.HasAuthority() && u.usernameEnd > u.schemeEnd+schemeSeparatorLen {
		return u.sliceRange(u.schemeEnd+schemeSeparatorLen, u.usernameEnd)
	} else {
		return ""
	}
}

// Password is Url::password: return the password for this URL, if any, as a
// percent-encoded ASCII string.
func (u *Url) Password() rs.Option[string] {
	// This ':' is not the one marking a port number since a host can not be empty.
	// (Except for file: URLs, which do not have port numbers.)
	if u.HasAuthority() &&
		u.usernameEnd != uint32(len(u.serialization)) &&
		u.byteAt(u.usernameEnd) == ':' {
		debugAssert(u.byteAt(u.hostStart-1) == '@', "self.byte_at(self.host_start - 1) == b'@'")
		return rs.Some(u.sliceRange(u.usernameEnd+1, u.hostStart-1))
	} else {
		return rs.None[string]()
	}
}

// HasHost is Url::has_host: equivalent to `url.Host().IsSome()`.
func (u *Url) HasHost() bool {
	return !rs.Is[*hostHostInternal_None](u.host)
}

// HostStr is Url::host_str: return the string representation of the host
// (domain or IP address) for this URL, if any.
//
// Non-ASCII domains are punycode-encoded per IDNA if this is the host
// of a special URL, or percent encoded for non-special URLs.
// IPv6 addresses are given between `[` and `]` brackets.
//
// Cannot-be-a-base URLs (typical of `data:` and `mailto:`) and some `file:`
// URLs don’t have a host.
//
// See also the `Host` method.
func (u *Url) HostStr() rs.Option[string] {
	if u.HasHost() {
		return rs.Some(u.sliceRange(u.hostStart, u.hostEnd))
	} else {
		return rs.None[string]()
	}
}

// Host is Url::host: return the parsed representation of the host for this
// URL. Non-ASCII domain labels are punycode-encoded per IDNA if this is the
// host of a special URL, or percent encoded for non-special URLs.
//
// Cannot-be-a-base URLs (typical of `data:` and `mailto:`) and some `file:`
// URLs don’t have a host.
//
// See also the `HostStr` method.
func (u *Url) Host() rs.Option[Host] {
	switch host := u.host.(type) {
	case *hostHostInternal_None:
		return rs.None[Host]()
	case *hostHostInternal_Domain:
		return rs.Some[Host](&Host_Domain{F0: u.sliceRange(u.hostStart, u.hostEnd)})
	case *hostHostInternal_Ipv4:
		return rs.Some[Host](&Host_Ipv4{F0: host.F0})
	case *hostHostInternal_Ipv6:
		return rs.Some[Host](&Host_Ipv6{F0: host.F0})
	default:
		panic(rs.Unreachable(u.host))
	}
}

// Domain is Url::domain: if this URL has a host and it is a domain name
// (not an IP address), return it. Non-ASCII domains are punycode-encoded per
// IDNA if this is the host of a special URL, or percent encoded for
// non-special URLs.
func (u *Url) Domain() rs.Option[string] {
	if rs.Is[*hostHostInternal_Domain](u.host) {
		return rs.Some(u.sliceRange(u.hostStart, u.hostEnd))
	}
	return rs.None[string]()
}

// Port is Url::port: return the port number for this URL, if any.
//
// Note that default port numbers are never reflected by the serialization,
// use the `PortOrKnownDefault()` method if you want a default port number
// returned.
func (u *Url) Port() rs.Option[uint16] {
	return u.port
}

// PortOrKnownDefault is Url::port_or_known_default: return the port number
// for this URL, or the default port number if it is known.
//
// This method only knows the default port number
// of the `http`, `https`, `ws`, `wss` and `ftp` schemes.
//
// For URLs in these schemes, this method always returns `Some(_)`.
// For other schemes, it is the same as `Url.Port()`.
func (u *Url) PortOrKnownDefault() rs.Option[uint16] {
	return u.port.OrElse(func() rs.Option[uint16] { return parserDefaultPort(u.Scheme()) })
}

// Path is Url::path: return the path for this URL, as a percent-encoded
// ASCII string. For cannot-be-a-base URLs, this is an arbitrary string that
// doesn’t start with '/'. For other URLs, this starts with a '/' slash and
// continues with slash-separated path segments.
func (u *Url) Path() string {
	if nextComponentStart, ok := u.queryStart.Get(); ok {
		return u.sliceRange(u.pathStart, nextComponentStart)
	}
	if nextComponentStart, ok := u.fragmentStart.Get(); ok {
		return u.sliceRange(u.pathStart, nextComponentStart)
	}
	return u.sliceFrom(u.pathStart)
}

// PathSegments is Url::path_segments: unless this URL is cannot-be-a-base,
// return the '/' slash-separated path segments, each as a percent-encoded
// ASCII string.
//
// Return `None` for cannot-be-a-base URLs.
//
// When `Some` is returned, the list always contains at least one string
// (which may be empty). In Rust this is an iterator (`str::Split`).
func (u *Url) PathSegments() rs.Option[[]string] {
	path := u.Path()
	if remainder, ok := strings.CutPrefix(path, "/"); ok {
		return rs.Some(strings.Split(remainder, "/"))
	}
	return rs.None[[]string]()
}

// Query is Url::query: return this URL’s query string, if any, as a
// percent-encoded ASCII string.
func (u *Url) Query() rs.Option[string] {
	queryStart, ok := u.queryStart.Get()
	if !ok {
		return rs.None[string]()
	}
	debugAssert(u.byteAt(queryStart) == '?', "self.byte_at(query_start) == b'?'")
	if fragmentStart, ok := u.fragmentStart.Get(); ok {
		return rs.Some(u.sliceRange(queryStart+1, fragmentStart))
	}
	return rs.Some(u.sliceFrom(queryStart + 1))
}

// QueryPairs is Url::query_pairs: parse the URL’s query string, if any, as
// `application/x-www-form-urlencoded` and return an iterator of (key, value)
// pairs.
func (u *Url) QueryPairs() FormUrlencodedParseIter {
	return FormUrlencodedParse([]byte(u.Query().UnwrapOr("")))
}

// Fragment is Url::fragment: return this URL’s fragment identifier, if any.
//
// A fragment is the part of the URL after the `#` symbol.
// The fragment is optional and, if present, contains a fragment identifier
// that identifies a secondary resource, such as a section heading
// of a document.
//
// In HTML, the fragment identifier is usually the id attribute of a an
// element that is scrolled to on load. Browsers typically will not send the
// fragment portion of a URL to the server.
//
// **Note:** the parser did *not* percent-encode this component,
// but the input may have been percent-encoded already.
func (u *Url) Fragment() rs.Option[string] {
	if start, ok := u.fragmentStart.Get(); ok {
		debugAssert(u.byteAt(start) == '#', "self.byte_at(start) == b'#'")
		return rs.Some(u.sliceFrom(start + 1))
	}
	return rs.None[string]()
}

// mutate runs f on a setter parser that owns the serialization, and stores
// the serialization back.
func (u *Url) mutate(f func(parser *parserParser)) {
	parser := parserParserForSetter([]byte(u.serialization))
	u.serialization = ""
	f(parser)
	u.serialization = string(parser.serialization)
}

// SetFragment is Url::set_fragment: change this URL’s fragment identifier.
func (u *Url) SetFragment(fragment rs.Option[string]) {
	// Remove any previous fragment
	if start, ok := u.fragmentStart.Get(); ok {
		debugAssert(u.byteAt(start) == '#', "self.byte_at(start) == b'#'")
		u.serialization = u.serialization[:start]
	}
	// Write the new one
	if input, ok := fragment.Get(); ok {
		u.fragmentStart = rs.Some(rs.Unwrap(parserToU32(len(u.serialization))))
		u.serialization += "#"
		u.mutate(func(parser *parserParser) { parser.parseFragment(parserInputNewNoTrim(input)) })
	} else {
		u.fragmentStart = rs.None[uint32]()
		u.stripTrailingSpacesFromOpaquePath()
	}
}

func (u *Url) takeFragment() rs.Option[string] {
	if start, ok := u.fragmentStart.Take().Get(); ok {
		debugAssert(u.byteAt(start) == '#', "self.byte_at(start) == b'#'")
		fragment := u.sliceFrom(start + 1)
		u.serialization = u.serialization[:start]
		return rs.Some(fragment)
	}
	return rs.None[string]()
}

func (u *Url) restoreAlreadyParsedFragment(fragment rs.Option[string]) {
	if fragment, ok := fragment.Get(); ok {
		rsAssert(u.fragmentStart.IsNone(), "assertion failed: self.fragment_start.is_none()")
		u.fragmentStart = rs.Some(rs.Unwrap(parserToU32(len(u.serialization))))
		u.serialization += "#"
		u.serialization += fragment
	}
}

// SetQuery is Url::set_query: change this URL’s query string. If `query` is
// `None`, this URL's query string will be cleared.
func (u *Url) SetQuery(query rs.Option[string]) {
	fragment := u.takeFragment()

	// Remove any previous query
	if start, ok := u.queryStart.Take().Get(); ok {
		debugAssert(u.byteAt(start) == '?', "self.byte_at(start) == b'?'")
		u.serialization = u.serialization[:start]
	}
	// Write the new query, if any
	if input, ok := query.Get(); ok {
		u.queryStart = rs.Some(rs.Unwrap(parserToU32(len(u.serialization))))
		u.serialization += "?"
		schemeType := parserSchemeTypeFrom(u.Scheme())
		schemeEnd := u.schemeEnd
		u.mutate(func(parser *parserParser) {
			vfn := parser.violationFn
			parser.parseQuery(
				schemeType,
				schemeEnd,
				parserInputNewTrimTabAndNewlines(input, vfn),
			)
		})
	} else {
		u.queryStart = rs.None[uint32]()
		if fragment.IsNone() {
			u.stripTrailingSpacesFromOpaquePath()
		}
	}

	u.restoreAlreadyParsedFragment(fragment)
}

// QueryPairsMut is Url::query_pairs_mut: manipulate this URL’s query string,
// viewed as a sequence of name/value pairs in
// `application/x-www-form-urlencoded` syntax.
//
// The return value has a method-chaining API. Its target takes the URL's
// fragment away while the pairs are written and puts it back when the
// serializer is finished or dropped, so end the chain with Drop (or Finish)
// where the Rust statement ends:
//
//	url.QueryPairsMut().AppendPair("foo", "bar").Drop()
//
// Note: `url.QueryPairsMut().Clear().Drop()` is equivalent to
// `url.SetQuery(rs.Some(""))`, not `url.SetQuery(rs.None[string]())`.
func (u *Url) QueryPairsMut() *FormUrlencodedSerializer[*Url] {
	fragment := u.takeFragment()

	var queryStart int
	if start, ok := u.queryStart.Get(); ok {
		debugAssert(u.byteAt(start) == '?', "self.byte_at(start) == b'?'")
		queryStart = int(start)
	} else {
		queryStart = len(u.serialization)
		u.queryStart = rs.Some(rs.Unwrap(parserToU32(queryStart)))
		u.serialization += "?"
	}

	query := &UrlQuery{
		url:      rs.Some(u),
		fragment: fragment,
	}
	return FormUrlencodedSerializerForSuffix[*Url](query, queryStart+len("?"))
}

func (u *Url) takeAfterPath() string {
	i, ok := u.queryStart.Get()
	if !ok {
		i, ok = u.fragmentStart.Get()
	}
	if ok {
		afterPath := u.sliceFrom(i)
		u.serialization = u.serialization[:i]
		return afterPath
	}
	return ""
}

// SetPath is Url::set_path: change this URL’s path.
func (u *Url) SetPath(path string) {
	afterPath := u.takeAfterPath()
	oldAfterPathPos := rs.Unwrap(parserToU32(len(u.serialization)))
	cannotBeABase := u.CannotBeABase()
	schemeType := parserSchemeTypeFrom(u.Scheme())
	u.serialization = truncate(u.serialization, int(u.pathStart))
	u.mutate(func(parser *parserParser) {
		if cannotBeABase {
			if strings.HasPrefix(path, "/") {
				parser.serialization = append(parser.serialization, "%2F"...)
				path = path[1:]
			}
			parser.parseCannotBeABasePath(parserInputNewNoTrim(path))
		} else {
			hasHost := true // FIXME
			parser.parsePathStart(
				schemeType,
				&hasHost,
				parserInputNewNoTrim(path),
			)
		}
	})
	u.restoreAfterPath(oldAfterPathPos, afterPath)
}

// truncate is String::truncate: a no-op when newLen is past the end.
func truncate(s string, newLen int) string {
	if newLen <= len(s) {
		return s[:newLen]
	}
	return s
}

// PathSegmentsMut is Url::path_segments_mut: return an object with methods
// to manipulate this URL’s path segments.
//
// Return `Err(())` if this URL is cannot-be-a-base.
//
// The object takes the URL's query and fragment away while the path is
// edited and puts them back when it is dropped, so call Drop where the Rust
// value goes out of scope:
//
//	segments, err := url.PathSegmentsMut()
//	if err != nil { ... }
//	segments.Clear().Push("a").Drop()
func (u *Url) PathSegmentsMut() (*PathSegmentsMut, error) {
	if u.CannotBeABase() {
		return nil, errUnit
	} else {
		return pathSegmentsNew(u), nil
	}
}

func (u *Url) restoreAfterPath(oldAfterPathPosition uint32, afterPath string) {
	newAfterPathPosition := rs.Unwrap(parserToU32(len(u.serialization)))
	adjust := func(index *uint32) {
		*index -= oldAfterPathPosition
		*index += newAfterPathPosition
	}
	if index := u.queryStart.AsRef(); index != nil {
		adjust(index)
	}
	if index := u.fragmentStart.AsRef(); index != nil {
		adjust(index)
	}
	u.serialization += afterPath
}

// SetPort is Url::set_port: change this URL’s port number.
//
// Note that default port numbers are not reflected in the serialization.
//
// If this URL is cannot-be-a-base, does not have a host, or has the `file`
// scheme; do nothing and return `Err`.
func (u *Url) SetPort(port rs.Option[uint16]) error {
	// has_host implies !cannot_be_a_base
	if !u.HasHost() || u.hostIsEmptyDomain() || u.Scheme() == "file" {
		return errUnit
	}
	if port.IsSome() && port == parserDefaultPort(u.Scheme()) {
		port = rs.None[uint16]()
	}
	u.setPortInternal(port)
	return nil
}

// hostIsEmptyDomain is `self.host() == Some(Host::Domain(""))`.
func (u *Url) hostIsEmptyDomain() bool {
	host, ok := u.Host().Get()
	if !ok {
		return false
	}
	domain, ok := host.(*Host_Domain)
	return ok && domain.F0 == ""
}

func (u *Url) setPortInternal(port rs.Option[uint16]) {
	oldPort, hadPort := u.port.Get()
	newPort, hasPort := port.Get()
	switch {
	case !hadPort && !hasPort:
	case hadPort && !hasPort:
		u.serialization = stringDrain(u.serialization, int(u.hostEnd), int(u.pathStart))
		offset := u.pathStart - u.hostEnd
		u.pathStart = u.hostEnd
		if index := u.queryStart.AsRef(); index != nil {
			*index -= offset
		}
		if index := u.fragmentStart.AsRef(); index != nil {
			*index -= offset
		}
	case hadPort && hasPort && oldPort == newPort:
	default:
		pathAndAfter := u.sliceFrom(u.pathStart)
		u.serialization = truncate(u.serialization, int(u.hostEnd))
		u.serialization += ":" + strconv.Itoa(int(newPort))
		oldPathStart := u.pathStart
		newPathStart := rs.Unwrap(parserToU32(len(u.serialization)))
		u.pathStart = newPathStart
		adjust := func(index *uint32) {
			*index -= oldPathStart
			*index += newPathStart
		}
		if index := u.queryStart.AsRef(); index != nil {
			adjust(index)
		}
		if index := u.fragmentStart.AsRef(); index != nil {
			adjust(index)
		}
		u.serialization += pathAndAfter
	}
	u.port = port
}

// SetHost is Url::set_host: change this URL’s host.
//
// Removing the host (calling this with `None`) will also remove any
// username, password, and port number.
//
// # Errors
//
// If this URL is cannot-be-a-base or there is an error parsing the given
// `host`, a `ParseError` variant will be returned.
func (u *Url) SetHost(host rs.Option[string]) error {
	if u.CannotBeABase() {
		return ParseError_SetHostOnCannotBeABaseUrl
	}

	schemeType := parserSchemeTypeFrom(u.Scheme())

	if host, ok := host.Get(); ok {
		if host == "" && schemeType.isSpecial() && !schemeType.isFile() {
			return ParseError_EmptyHost
		}
		hostSubstr := host
		// Otherwise, if c is U+003A (:) and the [] flag is unset, then
		if !strings.HasPrefix(host, "[") || !strings.HasSuffix(host, "]") {
			switch colonIndex := strings.IndexByte(host, ':'); {
			case colonIndex == 0:
				// If buffer is the empty string, validation error, return failure.
				return ParseError_InvalidDomainCharacter
			// Let host be the result of host parsing buffer
			case colonIndex > 0:
				hostSubstr = host[:colonIndex]
			}
		}
		if parserSchemeTypeFrom(u.Scheme()).isSpecial() {
			parsed, err := hostParseCow(hostSubstr)
			if err != nil {
				return err
			}
			u.setHostInternal(parsed, rs.None[rs.Option[uint16]]())
		} else {
			parsed, err := hostParseOpaqueCow(hostSubstr)
			if err != nil {
				return err
			}
			u.setHostInternal(parsed, rs.None[rs.Option[uint16]]())
		}
	} else if u.HasHost() {
		if schemeType.isSpecial() && !schemeType.isFile() {
			return ParseError_EmptyHost
		} else if len(u.serialization) == int(u.pathStart) {
			u.serialization += "/"
		}
		debugAssert(u.byteAt(u.schemeEnd) == ':', "self.byte_at(self.scheme_end) == b':'")
		debugAssert(u.byteAt(u.pathStart) == '/', "self.byte_at(self.path_start) == b'/'")

		var newPathStart uint32
		if schemeType.isFile() {
			newPathStart = u.schemeEnd + 3
		} else {
			newPathStart = u.schemeEnd + 1
		}

		u.serialization = stringDrain(u.serialization, int(newPathStart), int(u.pathStart))
		offset := u.pathStart - newPathStart
		u.pathStart = newPathStart
		u.usernameEnd = newPathStart
		u.hostStart = newPathStart
		u.hostEnd = newPathStart
		u.port = rs.None[uint16]()
		if index := u.queryStart.AsRef(); index != nil {
			*index -= offset
		}
		if index := u.fragmentStart.AsRef(); index != nil {
			*index -= offset
		}
	}
	return nil
}

// stringDrain is `string.drain(start..end)` with Rust's panic for a range
// that is out of bounds or backwards.
func stringDrain(s string, start int, end int) string {
	if start > end {
		rs.PanicAt(1, "slice index starts at "+itoa(start)+" but ends at "+itoa(end))
	}
	if end > len(s) {
		rs.PanicAt(1, "range end index "+itoa(end)+" out of range for slice of length "+itoa(len(s)))
	}
	return s[:start] + s[end:]
}

// opt_new_port: None means leave unchanged, Some(None) means remove any port number.
func (u *Url) setHostInternal(host Host, optNewPort rs.Option[rs.Option[uint16]]) {
	var oldSuffixPos uint32
	if optNewPort.IsSome() {
		oldSuffixPos = u.pathStart
	} else {
		oldSuffixPos = u.hostEnd
	}
	suffix := u.sliceFrom(oldSuffixPos)
	u.serialization = truncate(u.serialization, int(u.hostStart))
	if !u.HasAuthority() {
		debugAssert(u.sliceRange(u.schemeEnd, u.hostStart) == ":", "self.slice(self.scheme_end..self.host_start) == \":\"")
		debugAssert(u.usernameEnd == u.hostStart, "self.username_end == self.host_start")
		u.serialization += "/"
		u.serialization += "/"
		u.usernameEnd += 2
		u.hostStart += 2
	}
	u.serialization += host.String()
	u.hostEnd = rs.Unwrap(parserToU32(len(u.serialization)))
	u.host = hostHostInternalFromHost(host)

	if newPort, ok := optNewPort.Get(); ok {
		u.port = newPort
		if port, ok := newPort.Get(); ok {
			u.serialization += ":" + strconv.Itoa(int(port))
		}
	}
	newSuffixPos := rs.Unwrap(parserToU32(len(u.serialization)))
	u.serialization += suffix

	adjust := func(index *uint32) {
		*index -= oldSuffixPos
		*index += newSuffixPos
	}
	adjust(&u.pathStart)
	if index := u.queryStart.AsRef(); index != nil {
		adjust(index)
	}
	if index := u.fragmentStart.AsRef(); index != nil {
		adjust(index)
	}
}

// SetIpHost is Url::set_ip_host: change this URL’s host to the given IP
// address.
//
// If this URL is cannot-be-a-base, do nothing and return `Err`.
//
// Compared to `Url.SetHost`, this skips the host parser.
//
// The address is a std::net::IpAddr in Rust: a 4-byte netip.Addr is
// IpAddr::V4, anything else IpAddr::V6.
func (u *Url) SetIpHost(address netip.Addr) error {
	if u.CannotBeABase() {
		return errUnit
	}

	var host Host
	if address.Is4() {
		host = &Host_Ipv4{F0: address}
	} else {
		host = &Host_Ipv6{F0: address}
	}
	u.setHostInternal(host, rs.None[rs.Option[uint16]]())
	return nil
}

// SetPassword is Url::set_password: change this URL’s password.
//
// If this URL is cannot-be-a-base or does not have a host, do nothing and
// return `Err`.
func (u *Url) SetPassword(password rs.Option[string]) error {
	// has_host implies !cannot_be_a_base
	if !u.HasHost() || u.hostIsEmptyDomain() || u.Scheme() == "file" {
		return errUnit
	}
	passwordStr := password.UnwrapOr("")
	if passwordStr != "" {
		hostAndAfter := u.sliceFrom(u.hostStart)
		u.serialization = truncate(u.serialization, int(u.usernameEnd))
		u.serialization += ":"
		u.serialization += percentEncodingUtf8PercentEncode(passwordStr, parserUserinfo).String()
		u.serialization += "@"

		oldHostStart := u.hostStart
		newHostStart := rs.Unwrap(parserToU32(len(u.serialization)))
		adjust := func(index *uint32) {
			*index -= oldHostStart
			*index += newHostStart
		}
		u.hostStart = newHostStart
		adjust(&u.hostEnd)
		adjust(&u.pathStart)
		if index := u.queryStart.AsRef(); index != nil {
			adjust(index)
		}
		if index := u.fragmentStart.AsRef(); index != nil {
			adjust(index)
		}

		u.serialization += hostAndAfter
	} else if u.byteAt(u.usernameEnd) == ':' {
		// If there is a password to remove
		hasUsernameOrPassword := u.byteAt(u.hostStart-1) == '@'
		debugAssert(hasUsernameOrPassword, "has_username_or_password")
		usernameStart := u.schemeEnd + 3
		emptyUsername := usernameStart == u.usernameEnd
		start := u.usernameEnd // Remove the ':'
		var end uint32
		if emptyUsername {
			end = u.hostStart // Remove the '@' as well
		} else {
			end = u.hostStart - 1 // Keep the '@' to separate the username from the host
		}
		u.serialization = stringDrain(u.serialization, int(start), int(end))
		offset := end - start
		u.hostStart -= offset
		u.hostEnd -= offset
		u.pathStart -= offset
		if index := u.queryStart.AsRef(); index != nil {
			*index -= offset
		}
		if index := u.fragmentStart.AsRef(); index != nil {
			*index -= offset
		}
	}
	return nil
}

// SetUsername is Url::set_username: change this URL’s username.
//
// If this URL is cannot-be-a-base or does not have a host, do nothing and
// return `Err`.
func (u *Url) SetUsername(username string) error {
	// has_host implies !cannot_be_a_base
	if !u.HasHost() || u.hostIsEmptyDomain() || u.Scheme() == "file" {
		return errUnit
	}
	usernameStart := u.schemeEnd + 3
	debugAssert(u.sliceRange(u.schemeEnd, usernameStart) == "://", "self.slice(self.scheme_end..username_start) == \"://\"")
	if u.sliceRange(usernameStart, u.usernameEnd) == username {
		return nil
	}
	afterUsername := u.sliceFrom(u.usernameEnd)
	u.serialization = truncate(u.serialization, int(usernameStart))
	u.serialization += percentEncodingUtf8PercentEncode(username, parserUserinfo).String()

	removedBytes := u.usernameEnd
	u.usernameEnd = rs.Unwrap(parserToU32(len(u.serialization)))
	addedBytes := u.usernameEnd

	newUsernameIsEmpty := u.usernameEnd == usernameStart
	first := byte(0)
	hasFirst := afterUsername != ""
	if hasFirst {
		first = afterUsername[0]
	}
	switch {
	case newUsernameIsEmpty && hasFirst && first == '@':
		removedBytes++
		u.serialization += afterUsername[1:]
	case (!newUsernameIsEmpty && hasFirst && first == '@') || (hasFirst && first == ':') || newUsernameIsEmpty:
		u.serialization += afterUsername
	default:
		addedBytes++
		u.serialization += "@"
		u.serialization += afterUsername
	}

	adjust := func(index *uint32) {
		*index -= removedBytes
		*index += addedBytes
	}
	adjust(&u.hostStart)
	adjust(&u.hostEnd)
	adjust(&u.pathStart)
	if index := u.queryStart.AsRef(); index != nil {
		adjust(index)
	}
	if index := u.fragmentStart.AsRef(); index != nil {
		adjust(index)
	}
	return nil
}

// SetScheme is Url::set_scheme: change this URL’s scheme.
//
// Do nothing and return `Err` under the following circumstances:
//
//   - If the new scheme is not in `[a-zA-Z][a-zA-Z0-9+.-]+`
//   - If this URL is cannot-be-a-base and the new scheme is one of
//     `http`, `https`, `ws`, `wss` or `ftp`
//   - If either the old or new scheme is `http`, `https`, `ws`,
//     `wss` or `ftp` and the other is not one of these
//   - If the new scheme is `file` and this URL includes credentials
//     or has a non-null port
//   - If this URL's scheme is `file` and its host is empty or null
//
// See also the URL specification's section on legal scheme state overrides
// (https://url.spec.whatwg.org/#scheme-state).
func (u *Url) SetScheme(scheme string) error {
	parser := parserParserForSetter(nil)
	remaining, ok := parser.parseScheme(parserInputNewNoTrim(scheme))
	if !ok {
		return errUnit
	}
	newSchemeType := parserSchemeTypeFrom(string(parser.serialization))
	oldSchemeType := parserSchemeTypeFrom(u.Scheme())
	// If url’s scheme is a special scheme and buffer is not a special scheme, then return.
	if (newSchemeType.isSpecial() && !oldSchemeType.isSpecial()) ||
		// If url’s scheme is not a special scheme and buffer is a special scheme, then return.
		(!newSchemeType.isSpecial() && oldSchemeType.isSpecial()) ||
		// If url includes credentials or has a non-null port, and buffer is "file", then return.
		// If url’s scheme is "file" and its host is an empty host or null, then return.
		(newSchemeType.isFile() && u.HasAuthority()) {
		return errUnit
	}

	if !remaining.isEmpty() || (!u.HasHost() && newSchemeType.isSpecial()) {
		return errUnit
	}
	oldSchemeEnd := u.schemeEnd
	newSchemeEnd := rs.Unwrap(parserToU32(len(parser.serialization)))
	adjust := func(index *uint32) {
		*index -= oldSchemeEnd
		*index += newSchemeEnd
	}

	u.schemeEnd = newSchemeEnd
	adjust(&u.usernameEnd)
	adjust(&u.hostStart)
	adjust(&u.hostEnd)
	adjust(&u.pathStart)
	if index := u.queryStart.AsRef(); index != nil {
		adjust(index)
	}
	if index := u.fragmentStart.AsRef(); index != nil {
		adjust(index)
	}

	parser.serialization = append(parser.serialization, u.sliceFrom(oldSchemeEnd)...)
	u.serialization = string(parser.serialization)

	// Update the port so it can be removed
	// If it is the scheme's default
	// we don't mind it silently failing
	// if there was no port in the first place
	previousPort := u.Port()
	_ = u.SetPort(previousPort)

	return nil
}

// UrlFromFilePath is Url::from_file_path: convert a file name as
// `std::path::Path` into an URL in the `file` scheme.
//
// This returns `Err` if the given path is not absolute or, on Windows, if
// the prefix is not a disk prefix (e.g. `C:`) or a UNC prefix (`\\`).
func UrlFromFilePath(path string) (Url, error) {
	serialization := "file://"
	hostStart := uint32(len(serialization))
	hostEnd, host, err := pathToFileUrlSegments(path, &serialization)
	if err != nil {
		return Url{}, err
	}
	return Url{
		serialization: serialization,
		schemeEnd:     uint32(len("file")),
		usernameEnd:   hostStart,
		hostStart:     hostStart,
		hostEnd:       hostEnd,
		host:          host,
		port:          rs.None[uint16](),
		pathStart:     hostEnd,
		queryStart:    rs.None[uint32](),
		fragmentStart: rs.None[uint32](),
	}, nil
}

// UrlFromDirectoryPath is Url::from_directory_path: convert a directory name
// as `std::path::Path` into an URL in the `file` scheme.
//
// This returns `Err` if the given path is not absolute or, on Windows, if
// the prefix is not a disk prefix (e.g. `C:`) or a UNC prefix (`\\`).
//
// Compared to `UrlFromFilePath`, this ensure that URL’s the path has a
// trailing slash so that the entire path is considered when using this URL
// as a base URL.
//
// Note that `std::path` does not consider trailing slashes significant
// and usually does not include them (e.g. in `Path::parent()`).
func UrlFromDirectoryPath(path string) (Url, error) {
	url, err := UrlFromFilePath(path)
	if err != nil {
		return Url{}, err
	}
	if !strings.HasSuffix(url.serialization, "/") {
		url.serialization += "/"
	}
	return url, nil
}

// ToFilePath is Url::to_file_path: assuming the URL is in the `file` scheme
// or similar, convert its path to an absolute `std::path::Path`.
//
// **Note:** This does not actually check the URL’s `scheme`, and may give
// nonsensical results for other schemes. It is the user’s responsibility to
// check the URL’s scheme before calling this.
//
// Returns `Err` if the host is neither empty nor `"localhost"` (except on
// Windows, where `file:` URLs may have a non-local host), or if `Path::new_opt()`
// returns `None`. (That is, if the percent-decoded path contains a NUL byte
// or, for a Windows path, is not UTF-8.)
//
// The path is a Go string holding the bytes of the Rust PathBuf.
func (u *Url) ToFilePath() (string, error) {
	if segments, ok := u.PathSegments().Get(); ok {
		host := rs.None[string]()
		h, hasHost := u.Host().Get()
		domain, isDomain := h.(*Host_Domain)
		switch {
		case !hasHost || (isDomain && domain.F0 == "localhost"):
		case rs.CfgWindows && u.Scheme() == "file":
			host = rs.Some(u.serialization[u.hostStart:u.hostEnd])
		default:
			return "", errUnit
		}

		strLen := len(u.AsStr())
		var estimatedCapacity int
		if rs.CfgWindows {
			// remove scheme: - has possible \\ for hostname
			estimatedCapacity = max(strLen-(len(u.Scheme())+1), 0)
		} else {
			// remove scheme://
			estimatedCapacity = max(strLen-(len(u.Scheme())+3), 0)
		}
		return fileUrlSegmentsToPathbuf(estimatedCapacity, host, segments)
	}
	return "", errUnit
}

// Private helper methods:

// sliceRange, sliceFrom and sliceTo are Url::slice for the three kinds of
// range.
func (u *Url) sliceRange(start uint32, end uint32) string {
	return strSlice(u.serialization, int(start), int(end))
}

func (u *Url) sliceFrom(start uint32) string {
	if int(start) > len(u.serialization) {
		strSliceErrorFail(u.serialization, int(start), len(u.serialization))
	}
	return u.serialization[start:]
}

func (u *Url) sliceTo(end uint32) string {
	if int(end) > len(u.serialization) {
		strSliceErrorFail(u.serialization, 0, int(end))
	}
	return u.serialization[:end]
}

func (u *Url) byteAt(i uint32) byte {
	if int(i) >= len(u.serialization) {
		rs.PanicAt(1, "index out of bounds: the len is "+itoa(len(u.serialization))+" but the index is "+itoa(int(i)))
	}
	return u.serialization[i]
}

// UrlFromStr is `impl str::FromStr for Url`: parse a value from a string.
func UrlFromStr(input string) (Url, error) {
	return UrlParse(input)
}

// UrlTryFromStr is `impl<'a> TryFrom<&'a str> for Url`.
func UrlTryFromStr(s string) (Url, error) {
	return UrlParse(s)
}

// String is the Display text: the serialization.
func (u *Url) String() string {
	return u.serialization
}

// FmtDisplay is `impl fmt::Display for Url`: display the serialization of
// this URL.
func (u *Url) FmtDisplay(formatter *rs.Formatter) error {
	return formatter.Pad(u.serialization)
}

// StringFromUrl is `impl From<Url> for String`: String conversion.
func StringFromUrl(value Url) string {
	return value.serialization
}

// FmtDebug is `impl fmt::Debug for Url`: debug the serialization of this
// URL.
func (u *Url) FmtDebug(formatter *rs.Formatter) error {
	return formatter.
		DebugStruct("Url").
		Field("scheme", u.Scheme()).
		Field("cannot_be_a_base", u.CannotBeABase()).
		Field("username", u.Username()).
		Field("password", u.Password()).
		Field("host", u.Host()).
		Field("port", u.Port()).
		Field("path", u.Path()).
		Field("query", u.Query()).
		Field("fragment", u.Fragment()).
		Finish()
}

// Equal is `impl PartialEq for Url`: URLs compare like their serialization.
func (u *Url) Equal(other *Url) bool {
	return u.serialization == other.serialization
}

// Compare is `impl Ord for Url`: URLs compare like their serialization.
func (u *Url) Compare(other *Url) int {
	return strings.Compare(u.serialization, other.serialization)
}

// HashKey is `impl hash::Hash for Url`: URLs hash like their serialization.
func (u *Url) HashKey() string {
	return u.serialization
}

// AsRef is `impl AsRef<str> for Url`: return the serialization of this URL.
func (u *Url) AsRef() string {
	return u.serialization
}

// SerializeStr is what `impl serde::Serialize for Url` hands to
// `serializer.serialize_str`: a Url serializes as its string.
//
// go/crates/serde did not exist when this package was written, so the two
// halves of the serde impls are plain functions for it to call.
func (u *Url) SerializeStr() string {
	return u.AsStr()
}

// UrlDeserializeExpecting is the `expecting` text of the Deserialize impl's
// visitor, which serde quotes when the value is not a string.
const UrlDeserializeExpecting = "a string representing an URL"

// UrlDeserializeStr is the `visit_str` of `impl serde::Deserialize for Url`:
// the URL that s parses to, or the message that the impl passes to
// `Error::custom`.
func UrlDeserializeStr(s string) (Url, error) {
	url, err := UrlParse(s)
	if err != nil {
		return Url{}, stringError(err.Error() + ": " + rs.DebugString(s))
	}
	return url, nil
}

func pathToFileUrlSegments(
	path string,
	serialization *string,
) (uint32, hostHostInternal, error) {
	if rs.CfgWindows {
		return pathToFileUrlSegmentsWindows(path, serialization)
	}
	return pathToFileUrlSegmentsUnix(path, serialization)
}

// pathToFileUrlSegmentsUnix is path_to_file_url_segments as built for
// `cfg(unix)`.
func pathToFileUrlSegmentsUnix(
	path string,
	serialization *string,
) (uint32, hostHostInternal, error) {
	if !unixPathIsAbsolute(path) {
		return 0, nil, errUnit
	}
	hostEnd := rs.Unwrap(parserToU32(len(*serialization)))
	empty := true
	// skip the root component
	for _, component := range unixPathComponents(path)[1:] {
		empty = false
		*serialization += "/"
		*serialization += percentEncodingPercentEncodeNew(
			[]byte(component),
			parserSpecialPathSegment,
		).String()
	}
	if empty {
		// An URL’s path must not be empty.
		*serialization += "/"
	}
	return hostEnd, &hostHostInternal_None{}, nil
}

// Build this unconditionally to alleviate https://github.com/servo/rust-url/issues/102
func pathToFileUrlSegmentsWindows(
	path string,
	serialization *string,
) (uint32, hostHostInternal, error) {
	prefix, components, absolute := windowsPathComponents(path)
	if !absolute {
		return 0, nil, errUnit
	}

	hostStart := len(*serialization) + 1
	var hostEnd uint32
	var hostInternal hostHostInternal

	switch prefix.kind {
	case windowsPrefixDisk, windowsPrefixVerbatimDisk:
		hostEnd = rs.Unwrap(parserToU32(len(*serialization)))
		hostInternal = &hostHostInternal_None{}
		*serialization += "/"
		*serialization += string(prefix.letter)
		*serialization += ":"
	case windowsPrefixUnc, windowsPrefixVerbatimUnc:
		if runUtf8Validation([]byte(prefix.server)) != nil {
			return 0, nil, errUnit
		}
		host, err := hostParseCow(prefix.server)
		if err != nil {
			return 0, nil, errUnit
		}
		*serialization += host.String()
		hostEnd = rs.Unwrap(parserToU32(len(*serialization)))
		hostInternal = hostHostInternalFromHost(host)
		*serialization += "/"
		if runUtf8Validation([]byte(prefix.share)) != nil {
			return 0, nil, errUnit
		}
		*serialization += percentEncodingPercentEncodeNew([]byte(prefix.share), parserPathSegment).String()
	default:
		return 0, nil, errUnit
	}

	pathOnlyHasPrefix := true
	// components holds the components after the prefix, without the root
	// directory the Rust loop skips.
	for _, component := range components {
		pathOnlyHasPrefix = false
		// FIXME: somehow work with non-unicode?
		if runUtf8Validation([]byte(component)) != nil {
			return 0, nil, errUnit
		}

		*serialization += "/"
		*serialization += percentEncodingPercentEncodeNew([]byte(component), parserPathSegment).String()
	}

	// A windows drive letter must end with a slash.
	if len(*serialization) > hostStart &&
		parserIsWindowsDriveLetter((*serialization)[hostStart:]) &&
		pathOnlyHasPrefix {
		*serialization += "/"
	}

	return hostEnd, hostInternal, nil
}

func fileUrlSegmentsToPathbuf(
	estimatedCapacity int,
	host rs.Option[string],
	segments []string,
) (string, error) {
	if rs.CfgWindows {
		return fileUrlSegmentsToPathbufWindows(estimatedCapacity, host, segments)
	}
	return fileUrlSegmentsToPathbufUnix(estimatedCapacity, host, segments)
}

// fileUrlSegmentsToPathbufUnix is file_url_segments_to_pathbuf as built for
// `cfg(unix)`.
func fileUrlSegmentsToPathbufUnix(
	estimatedCapacity int,
	host rs.Option[string],
	segments []string,
) (string, error) {
	if host.IsSome() {
		return "", errUnit
	}

	bytes := make([]byte, 0, estimatedCapacity)

	for _, segment := range segments {
		bytes = append(bytes, '/')
		decoded, _ := percentEncodingPercentDecodeNew([]byte(segment)).collect()
		bytes = append(bytes, decoded...)
	}

	// A windows drive letter must end with a slash.
	if len(bytes) > 2 &&
		parserAsciiAlpha(rune(bytes[len(bytes)-2])) &&
		(bytes[len(bytes)-1] == ':' || bytes[len(bytes)-1] == '|') {
		bytes = append(bytes, '/')
	}

	path := string(bytes)

	debugAssertMessage(
		unixPathIsAbsolute(path),
		"to_file_path() failed to produce an absolute Path",
	)

	return path, nil
}

// Build this unconditionally to alleviate https://github.com/servo/rust-url/issues/102
func fileUrlSegmentsToPathbufWindows(
	estimatedCapacity int,
	host rs.Option[string],
	segments []string,
) (string, error) {
	_ = estimatedCapacity
	str := ""
	if host, ok := host.Get(); ok {
		str += `\\`
		str += host
	} else {
		if len(segments) == 0 {
			return "", errUnit
		}
		first := segments[0]
		segments = segments[1:]

		switch len(first) {
		case 2:
			if !parserAsciiAlpha(rune(first[0])) || first[1] != ':' {
				return "", errUnit
			}

			str += first

		case 4:
			if !parserAsciiAlpha(rune(first[0])) {
				return "", errUnit
			}
			bytes := first
			if bytes[1] != '%' || bytes[2] != '3' || (bytes[3] != 'a' && bytes[3] != 'A') {
				return "", errUnit
			}

			str += first[0:1]
			str += ":"

		default:
			return "", errUnit
		}
	}

	for _, segment := range segments {
		str += `\`

		// Currently non-unicode windows paths cannot be represented
		decoded, err := percentEncodingPercentDecodeStr(segment).decodeUtf8()
		if err != nil {
			return "", errUnit
		}
		str += decoded
	}
	path := str
	_, _, absolute := windowsPathComponents(path)
	debugAssertMessage(
		absolute,
		"to_file_path() failed to produce an absolute Path",
	)
	return path, nil
}

// UrlQuery is url::UrlQuery: the target of the serializer that
// Url.QueryPairsMut returns. It implements FormUrlencodedTarget.
type UrlQuery struct {
	url      rs.Option[*Url]
	fragment rs.Option[string]
}

// `as_mut_string` string here exposes the internal serialization of an `Url`,
// which should not be exposed to users.
// We achieve that by not giving users direct access to `UrlQuery`:
// * Its fields are private
//   (and so can not be constructed with struct literal syntax outside of this crate),
// * It has no constructor
// * It is only visible (on the type level) to users in the return type of
//   `Url::query_pairs_mut` which is `Serializer<UrlQuery>`
// * `Serializer` keeps its target in a private field
// * Unlike in other `Target` impls, `UrlQuery::finished` does not return `Self`.

// AsMutString is `Target::as_mut_string` for UrlQuery.
func (q *UrlQuery) AsMutString() *string {
	url, ok := q.url.Get()
	if !ok {
		rs.PanicAt(1, "called `Option::unwrap()` on a `None` value")
	}
	return &url.serialization
}

// Finish is `Target::finish` for UrlQuery: put the fragment back and return
// the URL.
func (q *UrlQuery) Finish() *Url {
	url := q.url.Take().Unwrap()
	url.restoreAlreadyParsedFragment(q.fragment.Take())
	return url
}

// Drop is `impl Drop for UrlQuery`.
func (q *UrlQuery) Drop() {
	if url, ok := q.url.Take().Get(); ok {
		url.restoreAlreadyParsedFragment(q.fragment.Take())
	}
}
