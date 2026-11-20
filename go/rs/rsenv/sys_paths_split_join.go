// Port of std 1.99.1 src/sys/paths/unix.rs and src/sys/paths/windows.rs:
// split_paths, join_paths and JoinPathsError.
//
// Both platforms' rules are here, as in go/rs/rspath, so that each can be
// used and tested on any host: SplitPaths and JoinPaths follow the host's.

package rsenv

import (
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// unixPathSeparator is the PATH_SEPARATOR of src/sys/paths/unix.rs.
const unixPathSeparator = ':'

// sysSplitPaths is sys::paths::SplitPaths.
type sysSplitPaths struct {
	windows bool

	// Unix: the parts `split` has not yet given.
	parts []string

	// Windows: the code units of the OsStr still to read, and whether a part
	// must be given even if it is empty.
	data      []uint16
	mustYield bool
}

// sysSplitPathsNew is sys::paths::split_paths under a flavour.
func sysSplitPathsNew(flavor rspath.Flavor, unparsed string) sysSplitPaths {
	if flavor == rspath.Windows || (flavor != rspath.Unix && rspath.Host == rspath.Windows) {
		return sysSplitPaths{windows: true, data: rspath.OsStrEncodeWide(unparsed), mustYield: true}
	}
	// unparsed.as_bytes().split(is_separator): every part, the empty ones
	// included, and one empty part for empty input.
	return sysSplitPaths{parts: strings.Split(unparsed, string(rune(unixPathSeparator)))}
}

func (s *sysSplitPaths) next() rs.Option[string] {
	if s.windows {
		return s.nextWindows()
	}
	if len(s.parts) == 0 {
		return rs.None[string]()
	}
	part := s.parts[0]
	s.parts = s.parts[1:]
	return rs.Some(part)
}

func (s *sysSplitPaths) nextWindows() rs.Option[string] {
	// On Windows, the PATH environment variable is semicolon separated.
	// Double quotes are used as a way of introducing literal semicolons
	// (since c:\some;dir is a valid Windows path). Double quotes are not
	// themselves permitted in path names, so there is no way to escape a
	// double quote. Quoted regions can appear in arbitrary locations, so
	//
	//   c:\foo;c:\som"e;di"r;c:\bar
	//
	// Should parse as [c:\foo, c:\some;dir, c:\bar].
	//
	// (The above is based on testing; there is no clear reference available
	// for the grammar.)

	mustYield := s.mustYield
	s.mustYield = false

	var inProgress []uint16
	inQuote := false
	for len(s.data) > 0 {
		b := s.data[0]
		s.data = s.data[1:]
		if b == '"' {
			inQuote = !inQuote
		} else if b == ';' && !inQuote {
			s.mustYield = true
			break
		} else {
			inProgress = append(inProgress, b)
		}
	}

	if !mustYield && len(inProgress) == 0 {
		return rs.None[string]()
	}
	return rs.Some(rspath.OsStringFromWide(inProgress))
}

// sysJoinPathsError is sys::paths::JoinPathsError, which differs only in its
// text between the platforms.
type sysJoinPathsError struct {
	windows bool
}

// String holds the Display text.
func (e *sysJoinPathsError) String() string {
	if e.windows {
		return "path segment contains `\"`"
	}
	return "path segment contains separator `" + string(rune(unixPathSeparator)) + "`"
}

// FmtDisplay is `impl Display for JoinPathsError`. On Unix Rust writes the
// text with write!, which applies no width; on Windows it formats the text
// as a str, which does.
func (e *sysJoinPathsError) FmtDisplay(f *rs.Formatter) error {
	if e.windows {
		return f.Pad(e.String())
	}
	return f.WriteStr(e.String())
}

// FmtDebug is the derived Debug of the unit struct JoinPathsError.
func (e *sysJoinPathsError) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr("JoinPathsError")
}

// sysJoinPaths is sys::paths::join_paths under a flavour.
func sysJoinPaths(flavor rspath.Flavor, paths []string) (string, *sysJoinPathsError) {
	if flavor == rspath.Windows || (flavor != rspath.Unix && rspath.Host == rspath.Windows) {
		return joinPathsWindows(paths)
	}
	return joinPathsUnix(paths)
}

func joinPathsUnix(paths []string) (string, *sysJoinPathsError) {
	var joined []byte

	for i, path := range paths {
		if i > 0 {
			joined = append(joined, unixPathSeparator)
		}
		if strings.IndexByte(path, unixPathSeparator) >= 0 {
			return "", &sysJoinPathsError{windows: false}
		}
		joined = append(joined, path...)
	}
	return string(joined), nil
}

func joinPathsWindows(paths []string) (string, *sysJoinPathsError) {
	var joined []uint16
	const sep = uint16(';')

	for i, path := range paths {
		if i > 0 {
			joined = append(joined, sep)
		}
		v := rspath.OsStrEncodeWide(path)
		if slices.Contains(v, '"') {
			return "", &sysJoinPathsError{windows: true}
		} else if slices.Contains(v, sep) {
			joined = append(joined, '"')
			joined = append(joined, v...)
			joined = append(joined, '"')
		} else {
			joined = append(joined, v...)
		}
	}

	return rspath.OsStringFromWide(joined), nil
}
