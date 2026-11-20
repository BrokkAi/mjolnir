// Port of url 2.5.8 src/slicing.rs

package url

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Rust slices a Url with ranges of Position: `&url[Position::BeforePath..]`.
// Go has no overloading, so the four Index impls are four methods.

// IndexRangeFull is `impl Index<RangeFull> for Url`: `&url[..]`.
func (u *Url) IndexRangeFull() string {
	return u.serialization
}

// IndexRangeFrom is `impl Index<RangeFrom<Position>> for Url`:
// `&url[start..]`.
func (u *Url) IndexRangeFrom(start Position) string {
	return strSlice(u.serialization, u.index(start), len(u.serialization))
}

// IndexRangeTo is `impl Index<RangeTo<Position>> for Url`: `&url[..end]`.
func (u *Url) IndexRangeTo(end Position) string {
	return strSlice(u.serialization, 0, u.index(end))
}

// IndexRange is `impl Index<Range<Position>> for Url`: `&url[start..end]`.
func (u *Url) IndexRange(start Position, end Position) string {
	return strSlice(u.serialization, u.index(start), u.index(end))
}

// Counts how many base-10 digits are required to represent n in the given base
func slicingCountDigits(n uint16) int {
	switch {
	case n <= 9:
		return 1
	case n <= 99:
		return 2
	case n <= 999:
		return 3
	case n <= 9999:
		return 4
	default:
		return 5
	}
}

// Position is url::Position: indicates a position within a URL based on its
// components.
//
// A range of positions can be used for slicing `Url`:
//
//	url.IndexRangeFrom(url.Position_BeforePath) == "/b?c#d"
//
// In a pseudo-grammar (where `[`…`]?` makes a sub-sequence optional),
// URL components and delimiters that separate them are:
//
//	url =
//	    scheme ":"
//	    [ "//" [ username [ ":" password ]? "@" ]? host [ ":" port ]? ]?
//	    path [ "?" query ]? [ "#" fragment ]?
//
// When a given component is not present, its "before" and "after" position
// are the same (so that a range of these two positions is the empty string)
// and that position is at the start of the next component.
//
// The end of a component and the start of the next are either the same or
// separate by a delimiter. (Not that the initial `/` of a path is considered
// part of the path here, not a delimiter.) For example, `&url[..BeforeFragment]`
// would include a `#` delimiter (if present in `url`), so `&url[..AfterQuery]`
// might be a better choice to remove the fragment for example.
//
// `&url[BeforePath..]` is the path, query and fragment, including the
// delimiters `?` and `#` if present: the "relative" URL in HTTP terms.
type Position uint8

const (
	Position_BeforeScheme Position = iota
	Position_AfterScheme
	Position_BeforeUsername
	Position_AfterUsername
	Position_BeforePassword
	Position_AfterPassword
	Position_BeforeHost
	Position_AfterHost
	Position_BeforePort
	Position_AfterPort
	Position_BeforePath
	Position_AfterPath
	Position_BeforeQuery
	Position_AfterQuery
	Position_BeforeFragment
	Position_AfterFragment
)

var _ = rs.RegisterEnum[Position]("Position",
	rs.Const(Position_BeforeScheme, "BeforeScheme"),
	rs.Const(Position_AfterScheme, "AfterScheme"),
	rs.Const(Position_BeforeUsername, "BeforeUsername"),
	rs.Const(Position_AfterUsername, "AfterUsername"),
	rs.Const(Position_BeforePassword, "BeforePassword"),
	rs.Const(Position_AfterPassword, "AfterPassword"),
	rs.Const(Position_BeforeHost, "BeforeHost"),
	rs.Const(Position_AfterHost, "AfterHost"),
	rs.Const(Position_BeforePort, "BeforePort"),
	rs.Const(Position_AfterPort, "AfterPort"),
	rs.Const(Position_BeforePath, "BeforePath"),
	rs.Const(Position_AfterPath, "AfterPath"),
	rs.Const(Position_BeforeQuery, "BeforeQuery"),
	rs.Const(Position_AfterQuery, "AfterQuery"),
	rs.Const(Position_BeforeFragment, "BeforeFragment"),
	rs.Const(Position_AfterFragment, "AfterFragment"),
)

func (u *Url) index(position Position) int {
	switch position {
	case Position_BeforeScheme:
		return 0

	case Position_AfterScheme:
		return int(u.schemeEnd)

	case Position_BeforeUsername:
		if u.HasAuthority() {
			return int(u.schemeEnd) + len("://")
		} else {
			debugAssert(u.byteAt(u.schemeEnd) == ':', "self.byte_at(self.scheme_end) == b':'")
			debugAssert(u.schemeEnd+uint32(len(":")) == u.usernameEnd, "self.scheme_end + \":\".len() as u32 == self.username_end")
			return int(u.schemeEnd) + len(":")
		}

	case Position_AfterUsername:
		return int(u.usernameEnd)

	case Position_BeforePassword:
		if u.HasAuthority() && u.byteAt(u.usernameEnd) == ':' {
			return int(u.usernameEnd) + len(":")
		} else {
			debugAssert(u.usernameEnd == u.hostStart, "self.username_end == self.host_start")
			return int(u.usernameEnd)
		}

	case Position_AfterPassword:
		if u.HasAuthority() && u.byteAt(u.usernameEnd) == ':' {
			debugAssert(u.byteAt(u.hostStart-uint32(len("@"))) == '@', "self.byte_at(self.host_start - \"@\".len() as u32) == b'@'")
			return int(u.hostStart) - len("@")
		} else {
			debugAssert(u.usernameEnd == u.hostStart, "self.username_end == self.host_start")
			return int(u.hostStart)
		}

	case Position_BeforeHost:
		return int(u.hostStart)

	case Position_AfterHost:
		return int(u.hostEnd)

	case Position_BeforePort:
		if u.port.IsSome() {
			debugAssert(u.byteAt(u.hostEnd) == ':', "self.byte_at(self.host_end) == b':'")
			return int(u.hostEnd) + len(":")
		} else {
			return int(u.hostEnd)
		}

	case Position_AfterPort:
		if port, ok := u.port.Get(); ok {
			debugAssert(u.byteAt(u.hostEnd) == ':', "self.byte_at(self.host_end) == b':'")
			return int(u.hostEnd) + len(":") + slicingCountDigits(port)
		} else {
			return int(u.hostEnd)
		}

	case Position_BeforePath:
		return int(u.pathStart)

	case Position_AfterPath:
		if q, ok := u.queryStart.Get(); ok {
			return int(q)
		}
		if f, ok := u.fragmentStart.Get(); ok {
			return int(f)
		}
		return len(u.serialization)

	case Position_BeforeQuery:
		if q, ok := u.queryStart.Get(); ok {
			debugAssert(u.byteAt(q) == '?', "self.byte_at(q) == b'?'")
			return int(q) + len("?")
		}
		if f, ok := u.fragmentStart.Get(); ok {
			return int(f)
		}
		return len(u.serialization)

	case Position_AfterQuery:
		if f, ok := u.fragmentStart.Get(); ok {
			return int(f)
		}
		return len(u.serialization)

	case Position_BeforeFragment:
		if f, ok := u.fragmentStart.Get(); ok {
			debugAssert(u.byteAt(f) == '#', "self.byte_at(f) == b'#'")
			return int(f) + len("#")
		}
		return len(u.serialization)

	case Position_AfterFragment:
		return len(u.serialization)

	default:
		panic(rs.Unreachable(position))
	}
}
