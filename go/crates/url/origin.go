// Port of url 2.5.8 src/origin.rs

package url

import (
	"strconv"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func originUrlOrigin(url *Url) Origin {
	scheme := url.Scheme()
	switch scheme {
	case "blob":
		result, err := UrlParse(url.Path())
		if err == nil {
			return originUrlOrigin(&result)
		}
		return OriginNewOpaque()
	case "ftp", "http", "https", "ws", "wss":
		return &Origin_Tuple{
			F0: scheme,
			F1: url.Host().Unwrap().ToOwned(),
			F2: url.PortOrKnownDefault().Unwrap(),
		}
	// TODO: Figure out what to do if the scheme is a file
	case "file":
		return OriginNewOpaque()
	default:
		return OriginNewOpaque()
	}
}

// Origin is url::Origin: the origin of an URL. A value is a pointer to one
// of the variant structs: *Origin_Opaque or *Origin_Tuple.
//
// Two URLs with the same origin are considered
// to originate from the same entity and can therefore trust
// each other.
//
// The origin is determined based on the scheme as follows:
//
//   - If the scheme is "blob" the origin is the origin of the
//     URL contained in the path component. If parsing fails,
//     it is an opaque origin.
//   - If the scheme is "ftp", "http", "https", "ws", or "wss",
//     then the origin is a tuple of the scheme, host, and port.
//   - If the scheme is anything else, the origin is opaque, meaning
//     the URL does not have the same origin as any other URL.
//
// For more information see <https://url.spec.whatwg.org/#origin>
type Origin interface {
	isOrigin()
	// IsTuple is Origin::is_tuple: return whether this origin is a (scheme,
	// host, port) tuple (as opposed to an opaque origin).
	IsTuple() bool
	// AsciiSerialization is Origin::ascii_serialization:
	// <https://html.spec.whatwg.org/multipage/#ascii-serialisation-of-an-origin>
	AsciiSerialization() string
	// UnicodeSerialization is Origin::unicode_serialization:
	// <https://html.spec.whatwg.org/multipage/#unicode-serialisation-of-an-origin>
	UnicodeSerialization() string
	// Equal is the derived PartialEq.
	Equal(other Origin) bool
}

// Origin_Opaque is Origin::Opaque: a globally unique identifier.
type Origin_Opaque struct{ F0 OpaqueOrigin }

// Origin_Tuple is Origin::Tuple: consists of the URL's scheme, host and
// port.
type Origin_Tuple struct {
	F0 string
	F1 Host
	F2 uint16
}

var _ = rs.RegisterEnum[Origin]("Origin",
	rs.Variant[Origin_Opaque]("Opaque", rs.ShapeTuple),
	rs.Variant[Origin_Tuple]("Tuple", rs.ShapeTuple),
)

func (*Origin_Opaque) isOrigin() {}
func (*Origin_Tuple) isOrigin()  {}

var originCounter atomic.Uint64

// OriginNewOpaque is Origin::new_opaque: creates a new opaque origin that is
// only equal to itself.
func OriginNewOpaque() Origin {
	return &Origin_Opaque{F0: OpaqueOrigin(originCounter.Add(1) - 1)}
}

func (*Origin_Opaque) IsTuple() bool { return false }
func (*Origin_Tuple) IsTuple() bool  { return true }

func (*Origin_Opaque) AsciiSerialization() string { return "null" }

func (o *Origin_Tuple) AsciiSerialization() string {
	scheme, host, port := o.F0, o.F1, o.F2
	if parserDefaultPort(scheme) == rs.Some(port) {
		return scheme + "://" + host.String()
	} else {
		return scheme + "://" + host.String() + ":" + strconv.Itoa(int(port))
	}
}

func (*Origin_Opaque) UnicodeSerialization() string { return "null" }

func (o *Origin_Tuple) UnicodeSerialization() string {
	scheme, host, port := o.F0, o.F1, o.F2
	if domain, ok := host.(*Host_Domain); ok {
		unicode, _ := idnaDomainToUnicode(domain.F0)
		host = &Host_Domain{F0: unicode}
	}
	if parserDefaultPort(scheme) == rs.Some(port) {
		return scheme + "://" + host.String()
	} else {
		return scheme + "://" + host.String() + ":" + strconv.Itoa(int(port))
	}
}

func (o *Origin_Opaque) Equal(other Origin) bool {
	x, ok := other.(*Origin_Opaque)
	return ok && o.F0 == x.F0
}

func (o *Origin_Tuple) Equal(other Origin) bool {
	x, ok := other.(*Origin_Tuple)
	return ok && o.F0 == x.F0 && o.F1.Equal(x.F1) && o.F2 == x.F2
}

// OpaqueOrigin is url::OpaqueOrigin: opaque identifier for URLs that have
// file or other schemes.
type OpaqueOrigin int

var _ = rs.RegisterNewtype[OpaqueOrigin]("OpaqueOrigin")
