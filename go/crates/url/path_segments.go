// Port of url 2.5.8 src/path_segments.rs

package url

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// PathSegmentsMut is url::PathSegmentsMut: exposes methods to manipulate the
// path of an URL that is not cannot-be-base.
//
// The path always starts with a `/` slash, and is made of slash-separated
// segments. There is always at least one segment (which may be the empty
// string).
//
// It takes the URL's query and fragment away while the path is edited, and
// Drop puts them back: call Drop where the Rust value goes out of scope.
type PathSegmentsMut struct {
	url                  *Url
	afterFirstSlash      int
	afterPath            string
	oldAfterPathPosition uint32
	dropped              bool
}

// Not re-exported outside the crate
func pathSegmentsNew(url *Url) *PathSegmentsMut {
	afterPath := url.takeAfterPath()
	oldAfterPathPosition := rs.Unwrap(parserToU32(len(url.serialization)))
	// Special urls always have a non empty path
	if parserSchemeTypeFrom(url.Scheme()).isSpecial() {
		debugAssert(url.byteAt(url.pathStart) == '/', "url.byte_at(url.path_start) == b'/'")
	} else {
		debugAssert(
			len(url.serialization) == int(url.pathStart) ||
				url.byteAt(url.pathStart) == '/',
			"url.serialization.len() == url.path_start as usize ||\n    url.byte_at(url.path_start) == b'/'",
		)
	}
	return &PathSegmentsMut{
		afterFirstSlash:      int(url.pathStart) + len("/"),
		url:                  url,
		oldAfterPathPosition: oldAfterPathPosition,
		afterPath:            afterPath,
		dropped:              false,
	}
}

// Drop is `impl Drop for PathSegmentsMut`: put the query and the fragment
// back after the path. A second call does nothing.
func (p *PathSegmentsMut) Drop() {
	if p.dropped {
		return
	}
	p.dropped = true
	p.url.
		restoreAfterPath(p.oldAfterPathPosition, p.afterPath)
}

// Clear is PathSegmentsMut::clear: remove all segments in the path, leaving
// the minimal `url.Path() == "/"`.
//
// Returns the receiver to allow method chaining.
func (p *PathSegmentsMut) Clear() *PathSegmentsMut {
	p.url.serialization = truncate(p.url.serialization, p.afterFirstSlash)
	return p
}

// PopIfEmpty is PathSegmentsMut::pop_if_empty: remove the last segment of
// this URL’s path if it is empty, except if these was only one segment to
// begin with.
//
// In other words, remove one path trailing slash, if any,
// unless it is also the initial slash (so that `url.Path() == "/"`).
//
// Returns the receiver to allow method chaining.
func (p *PathSegmentsMut) PopIfEmpty() *PathSegmentsMut {
	if p.afterFirstSlash >= len(p.url.serialization) {
		return p
	}
	if strings.HasSuffix(p.url.serialization[p.afterFirstSlash:], "/") {
		p.url.serialization = p.url.serialization[:len(p.url.serialization)-1]
	}
	return p
}

// Pop is PathSegmentsMut::pop: remove the last segment of this URL’s path.
//
// If the path only has one segment, make it empty such that `url.Path() ==
// "/"`.
//
// Returns the receiver to allow method chaining.
func (p *PathSegmentsMut) Pop() *PathSegmentsMut {
	if p.afterFirstSlash >= len(p.url.serialization) {
		return p
	}
	lastSlash := strings.LastIndexByte(p.url.serialization[p.afterFirstSlash:], '/')
	if lastSlash < 0 {
		lastSlash = 0
	}
	p.url.
		serialization = truncate(p.url.serialization, p.afterFirstSlash+lastSlash)
	return p
}

// Push is PathSegmentsMut::push: append the given segment at the end of this
// URL’s path.
//
// See the documentation for `Extend`.
//
// Returns the receiver to allow method chaining.
func (p *PathSegmentsMut) Push(segment string) *PathSegmentsMut {
	return p.Extend([]string{segment})
}

// Extend is PathSegmentsMut::extend: append each segment from the given
// iterator at the end of this URL’s path.
//
// Each segment is percent-encoded like in `Url::parse` or `Url::join`,
// except that `%` and `/` characters are also encoded (to `%25` and `%2F`).
// This is unlike `Url::parse` where `%` is left as-is in case some of the
// input is already percent-encoded, and `/` denotes a path segment
// separator.)
//
// Note that, in addition to slashes between new segments,
// this always adds a slash between the existing path and the new segments
// *except* if the existing path is `"/"`.
// If the previous last segment was empty (if the path had a trailing slash)
// the path after `.Extend()` will contain two consecutive slashes.
// If that is undesired, call `.PopIfEmpty()` first.
//
// To obtain a behavior similar to `Url::join`, call `.Pop()`
// unconditionally first.
//
// Returns the receiver to allow method chaining.
func (p *PathSegmentsMut) Extend(segments []string) *PathSegmentsMut {
	schemeType := parserSchemeTypeFrom(p.url.Scheme())
	pathStart := int(p.url.pathStart)
	p.url.mutate(func(parser *parserParser) {
		parser.context = parserContext_PathSegmentSetter
		for _, segment := range segments {
			if segment == "." || segment == ".." {
				continue
			}
			if len(parser.serialization) > pathStart+1 ||
				// Non special url's path might still be empty
				len(parser.serialization) == pathStart {
				parser.serialization = append(parser.serialization, '/')
			}
			hasHost := true // FIXME account for this?
			parser.parsePath(
				schemeType,
				&hasHost,
				pathStart,
				parserInputNewNoTrim(segment),
			)
		}
	})
	return p
}
