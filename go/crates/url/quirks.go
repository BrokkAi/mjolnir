// Port of url 2.5.8 src/quirks.rs

package url

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Getters and setters for URL components implemented per <https://url.spec.whatwg.org/#api>
//
// Unless you need to be interoperable with web browsers,
// you probably want to use `Url` method instead.
//
// Rust code reaches these as `url::quirks::<name>`, so they carry the prefix
// Quirks. The module is `#[doc(hidden)]` in the crate.

// QuirksDomainToAscii is quirks::domain_to_ascii:
// <https://url.spec.whatwg.org/#dom-url-domaintoascii>
func QuirksDomainToAscii(domain string) string {
	host, err := HostParse(domain)
	if err == nil {
		if d, ok := host.(*Host_Domain); ok {
			return d.F0
		}
	}
	return ""
}

// QuirksDomainToUnicode is quirks::domain_to_unicode:
// <https://url.spec.whatwg.org/#dom-url-domaintounicode>
func QuirksDomainToUnicode(domain string) string {
	host, err := HostParse(domain)
	if err == nil {
		if d, ok := host.(*Host_Domain); ok {
			unicode, _ := idnaDomainToUnicode(d.F0)
			return unicode
		}
	}
	return ""
}

// QuirksHref is the getter for <https://url.spec.whatwg.org/#dom-url-href>
func QuirksHref(url *Url) string {
	return url.AsStr()
}

// QuirksSetHref is the setter for <https://url.spec.whatwg.org/#dom-url-href>
func QuirksSetHref(url *Url, value string) error {
	parsed, err := UrlParse(value)
	if err != nil {
		return err
	}
	*url = parsed
	return nil
}

// QuirksOrigin is the getter for <https://url.spec.whatwg.org/#dom-url-origin>
func QuirksOrigin(url *Url) string {
	return url.Origin().AsciiSerialization()
}

// QuirksProtocol is the getter for <https://url.spec.whatwg.org/#dom-url-protocol>
func QuirksProtocol(url *Url) string {
	return strSlice(url.AsStr(), 0, len(url.Scheme())+len(":"))
}

// QuirksSetProtocol is the setter for <https://url.spec.whatwg.org/#dom-url-protocol>
func QuirksSetProtocol(url *Url, newProtocol string) error {
	// The scheme state in the spec ignores everything after the first `:`,
	// but `set_scheme` errors if there is more.
	if position := strings.IndexByte(newProtocol, ':'); position >= 0 {
		newProtocol = newProtocol[:position]
	}
	return url.SetScheme(newProtocol)
}

// QuirksUsername is the getter for <https://url.spec.whatwg.org/#dom-url-username>
func QuirksUsername(url *Url) string {
	return url.Username()
}

// QuirksSetUsername is the setter for <https://url.spec.whatwg.org/#dom-url-username>
func QuirksSetUsername(url *Url, newUsername string) error {
	return url.SetUsername(newUsername)
}

// QuirksPassword is the getter for <https://url.spec.whatwg.org/#dom-url-password>
func QuirksPassword(url *Url) string {
	return url.Password().UnwrapOr("")
}

// QuirksSetPassword is the setter for <https://url.spec.whatwg.org/#dom-url-password>
func QuirksSetPassword(url *Url, newPassword string) error {
	if newPassword == "" {
		return url.SetPassword(rs.None[string]())
	} else {
		return url.SetPassword(rs.Some(newPassword))
	}
}

// QuirksHost is the getter for <https://url.spec.whatwg.org/#dom-url-host>
func QuirksHost(url *Url) string {
	return url.IndexRange(Position_BeforeHost, Position_AfterPort)
}

// QuirksSetHost is the setter for <https://url.spec.whatwg.org/#dom-url-host>
func QuirksSetHost(url *Url, newHost string) error {
	// If context object’s url’s cannot-be-a-base-URL flag is set, then return.
	if url.CannotBeABase() {
		return errUnit
	}
	// Host parsing rules are strict,
	// We don't want to trim the input
	input := parserInputNewNoTrim(newHost)
	var host Host
	var optPort rs.Option[rs.Option[uint16]]
	{
		scheme := url.Scheme()
		schemeType := parserSchemeTypeFrom(scheme)
		if schemeType == parserSchemeType_File && newHost == "" {
			url.setHostInternal(&Host_Domain{F0: ""}, rs.None[rs.Option[uint16]]())
			return nil
		}

		if h, remaining, err := parserParserParseHost(input, schemeType); err == nil {
			host = h
			if remaining, ok := remaining.splitPrefixChar(':'); ok {
				if remaining.isEmpty() {
					optPort = rs.None[rs.Option[uint16]]()
				} else {
					port, _, err := parserParserParsePort(remaining, func() rs.Option[uint16] { return parserDefaultPort(scheme) }, parserContext_Setter)
					if err == nil {
						optPort = rs.Some(port)
					} else {
						optPort = rs.None[rs.Option[uint16]]()
					}
				}
			} else {
				optPort = rs.None[rs.Option[uint16]]()
			}
		} else {
			return errUnit
		}
	}
	// Make sure we won't set an empty host to a url with a username or a port
	if d, ok := host.(*Host_Domain); ok && d.F0 == "" &&
		(QuirksUsername(url) != "" || optPort.IsSomeAnd(func(port rs.Option[uint16]) bool { return port.IsSome() }) || url.Port().IsSome()) {
		return errUnit
	}
	url.setHostInternal(host, optPort)
	return nil
}

// QuirksHostname is the getter for <https://url.spec.whatwg.org/#dom-url-hostname>
func QuirksHostname(url *Url) string {
	return url.HostStr().UnwrapOr("")
}

// QuirksSetHostname is the setter for <https://url.spec.whatwg.org/#dom-url-hostname>
func QuirksSetHostname(url *Url, newHostname string) error {
	if url.CannotBeABase() {
		return errUnit
	}
	// Host parsing rules are strict we don't want to trim the input
	input := parserInputNewNoTrim(newHostname)
	schemeType := parserSchemeTypeFrom(url.Scheme())
	if schemeType == parserSchemeType_File && newHostname == "" {
		url.setHostInternal(&Host_Domain{F0: ""}, rs.None[rs.Option[uint16]]())
		return nil
	}

	if host, remaining, err := parserParserParseHost(input, schemeType); err == nil {
		if remaining.startsWithChar(':') {
			return errUnit
		}
		if h, ok := host.(*Host_Domain); ok {
			if h.F0 == "" {
				// Empty host on special not file url
				if parserSchemeTypeFrom(url.Scheme()) == parserSchemeType_SpecialNotFile ||
					// Port with an empty host
					QuirksPort(url) != "" ||
					// Empty host that includes credentials
					url.Username() != "" ||
					url.Password().UnwrapOr("") != "" {
					return errUnit
				}
			}
		}
		url.setHostInternal(host, rs.None[rs.Option[uint16]]())
		return nil
	} else {
		return errUnit
	}
}

// QuirksPort is the getter for <https://url.spec.whatwg.org/#dom-url-port>
func QuirksPort(url *Url) string {
	return url.IndexRange(Position_BeforePort, Position_AfterPort)
}

// QuirksSetPort is the setter for <https://url.spec.whatwg.org/#dom-url-port>
func QuirksSetPort(url *Url, newPort string) error {
	var newPortValue rs.Option[uint16]
	var err error
	{
		// has_host implies !cannot_be_a_base
		scheme := url.Scheme()
		if !url.HasHost() || url.hostIsEmptyDomain() || scheme == "file" {
			return errUnit
		}
		newPortValue, _, err = parserParserParsePort(
			parserInputNewNoTrim(newPort),
			func() rs.Option[uint16] { return parserDefaultPort(scheme) },
			parserContext_Setter,
		)
	}
	if err == nil {
		url.setPortInternal(newPortValue)
		return nil
	} else {
		return errUnit
	}
}

// QuirksPathname is the getter for <https://url.spec.whatwg.org/#dom-url-pathname>
func QuirksPathname(url *Url) string {
	return url.Path()
}

// QuirksSetPathname is the setter for <https://url.spec.whatwg.org/#dom-url-pathname>
func QuirksSetPathname(url *Url, newPathname string) {
	if url.CannotBeABase() {
		return
	}
	if strings.HasPrefix(newPathname, "/") ||
		(parserSchemeTypeFrom(url.Scheme()).isSpecial() &&
			// \ is a segment delimiter for 'special' URLs"
			strings.HasPrefix(newPathname, "\\")) {
		url.SetPath(newPathname)
	} else if parserSchemeTypeFrom(url.Scheme()).isSpecial() ||
		newPathname != "" ||
		!url.HasHost() {
		pathToSet := "/"
		pathToSet += newPathname
		url.SetPath(pathToSet)
	} else {
		url.SetPath(newPathname)
	}
}

// QuirksSearch is the getter for <https://url.spec.whatwg.org/#dom-url-search>
func QuirksSearch(url *Url) string {
	return quirksTrim(url.IndexRange(Position_AfterPath, Position_AfterQuery))
}

// QuirksSetSearch is the setter for <https://url.spec.whatwg.org/#dom-url-search>
func QuirksSetSearch(url *Url, newSearch string) {
	switch {
	case newSearch == "":
		url.SetQuery(rs.None[string]())
	case strings.HasPrefix(newSearch, "?"):
		url.SetQuery(rs.Some(newSearch[1:]))
	default:
		url.SetQuery(rs.Some(newSearch))
	}
}

// QuirksHash is the getter for <https://url.spec.whatwg.org/#dom-url-hash>
func QuirksHash(url *Url) string {
	return quirksTrim(url.IndexRangeFrom(Position_AfterQuery))
}

// QuirksSetHash is the setter for <https://url.spec.whatwg.org/#dom-url-hash>
func QuirksSetHash(url *Url, newHash string) {
	switch {
	// If the given value is the empty string,
	// then set context object’s url’s fragment to null and return.
	case newHash == "":
		url.SetFragment(rs.None[string]())
	// Let input be the given value with a single leading U+0023 (#) removed, if any.
	case strings.HasPrefix(newHash, "#"):
		url.SetFragment(rs.Some(newHash[1:]))
	default:
		url.SetFragment(rs.Some(newHash))
	}
}

func quirksTrim(s string) string {
	if len(s) == 1 {
		return ""
	} else {
		return s
	}
}
