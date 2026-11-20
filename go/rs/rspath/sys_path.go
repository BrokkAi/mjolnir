// Port of std 1.99.1 src/sys/path/unix.rs, src/sys/path/windows.rs (the parts
// that only look at the text) and src/sys/path/windows_prefix.rs

package rspath

import "github.com/BrokkAi/mjolnir/go/rs"

// isSepByte is sys::path::is_sep_byte: `/` on Unix, `\` and `/` on Windows.
func (f Flavor) isSepByte(b byte) bool {
	if f.resolve() == Windows {
		return b == '\\' || b == '/'
	}
	return b == '/'
}

// isVerbatimSep is sys::path::is_verbatim_sep: the separator inside a
// verbatim (`\\?\`) path, which is `\` alone on Windows.
func (f Flavor) isVerbatimSep(b byte) bool {
	if f.resolve() == Windows {
		return b == '\\'
	}
	return f.isSepByte(b)
}

// parsePrefix is sys::path::parse_prefix.
func (f Flavor) parsePrefix(path string) rs.Option[Prefix] {
	if f.resolve() == Windows {
		return windowsParsePrefix(path)
	}
	return rs.None[Prefix]()
}

// sysIsAbsolute is sys::path::is_absolute.
func (f Flavor) sysIsAbsolute(path string) bool {
	if f.resolve() == Windows {
		return f.HasRoot(path) && f.prefix(path).IsSome()
	}
	return f.HasRoot(path)
}

// prefixParserSlice is windows_prefix::PrefixParserSlice: the first bytes of
// a path with `/` read as `\`, and how far into them the parser is.
type prefixParserSlice struct {
	path   string
	prefix string
	index  int
}

// newPrefixParser is PrefixParser::<8>::new(path).as_slice().
func newPrefixParser(path string) prefixParserSlice {
	const prefixParserLen = 8
	var prefix [prefixParserLen]byte
	n := min(prefixParserLen, len(path))
	for i := 0; i < n; i++ {
		ch := path[i]
		if ch == '/' {
			ch = '\\'
		}
		prefix[i] = ch
	}
	return prefixParserSlice{path: path, prefix: string(prefix[:n]), index: 0}
}

func (p prefixParserSlice) stripPrefix(prefix string) (prefixParserSlice, bool) {
	rest := p.prefix[p.index:]
	if len(rest) >= len(prefix) && rest[:len(prefix)] == prefix {
		return prefixParserSlice{path: p.path, prefix: p.prefix, index: p.index + len(prefix)}, true
	}
	return prefixParserSlice{}, false
}

func (p prefixParserSlice) prefixBytes() string {
	return p.path[:p.index]
}

func (p prefixParserSlice) finish() string {
	return p.path[p.index:]
}

// windowsParsePrefix is windows_prefix::parse_prefix.
func windowsParsePrefix(path string) rs.Option[Prefix] {
	parser := newPrefixParser(path)
	if parser, ok := parser.stripPrefix(`\\`); ok {
		// \\

		// The meaning of verbatim paths can change when they use a different
		// separator.
		if verbatim, ok := parser.stripPrefix(`?\`); ok && !containsByte(verbatim.prefixBytes(), '/') {
			// \\?\
			if unc, ok := verbatim.stripPrefix(`UNC\`); ok {
				// \\?\UNC\server\share

				path := unc.finish()
				server, path := windowsParseNextComponent(path, true)
				share, _ := windowsParseNextComponent(path, true)

				return rs.Some[Prefix](&Prefix_VerbatimUNC{F0: server, F1: share})
			}
			path := verbatim.finish()

			// in verbatim paths only recognize an exact drive prefix
			if drive, ok := windowsParseDriveExact(path); ok {
				// \\?\C:
				return rs.Some[Prefix](&Prefix_VerbatimDisk{F0: drive})
			}
			// \\?\prefix
			prefix, _ := windowsParseNextComponent(path, true)
			return rs.Some[Prefix](&Prefix_Verbatim{F0: prefix})
		} else if device, ok := parser.stripPrefix(`.\`); ok {
			// \\.\COM42
			path := device.finish()
			prefix, _ := windowsParseNextComponent(path, false)
			return rs.Some[Prefix](&Prefix_DeviceNS{F0: prefix})
		}
		path := parser.finish()
		server, path := windowsParseNextComponent(path, false)
		share, _ := windowsParseNextComponent(path, false)

		if len(server) != 0 && len(share) != 0 {
			// \\server\share
			return rs.Some[Prefix](&Prefix_UNC{F0: server, F1: share})
		}
		// no valid prefix beginning with "\\" recognized
		return rs.None[Prefix]()
	}
	// If it has a drive like `C:` then it's a disk.
	// Otherwise there is no prefix.
	if drive, ok := windowsParseDrive(path); ok {
		return rs.Some[Prefix](&Prefix_Disk{F0: drive})
	}
	return rs.None[Prefix]()
}

// windowsParseDrive parses a drive prefix, e.g. "C:" and "C:\whatever".
func windowsParseDrive(path string) (uint8, bool) {
	// In most DOS systems, it is not possible to have more than 26 drive letters.
	// See <https://en.wikipedia.org/wiki/Drive_letter_assignment#Common_assignments>.
	isValidDriveLetter := func(drive byte) bool {
		return (drive >= 'a' && drive <= 'z') || (drive >= 'A' && drive <= 'Z')
	}

	if len(path) >= 2 && path[1] == ':' && isValidDriveLetter(path[0]) {
		drive := path[0]
		if drive >= 'a' && drive <= 'z' {
			drive -= 'a' - 'A'
		}
		return drive, true
	}
	return 0, false
}

// windowsParseDriveExact parses a drive prefix exactly, e.g. "C:".
func windowsParseDriveExact(path string) (uint8, bool) {
	// only parse two bytes: the drive letter and the drive separator
	if len(path) <= 2 || Windows.isSepByte(path[2]) {
		return windowsParseDrive(path)
	}
	return 0, false
}

// windowsParseNextComponent parses the next path component.
//
// Returns the next component and the rest of the path excluding the component and separator.
// Does not recognize `/` as a separator character on Windows if `verbatim` is true.
func windowsParseNextComponent(path string, verbatim bool) (string, string) {
	separator := Windows.isSepByte
	if verbatim {
		separator = Windows.isVerbatimSep
	}

	for separatorStart := 0; separatorStart < len(path); separatorStart++ {
		if separator(path[separatorStart]) {
			separatorEnd := separatorStart + 1

			component := path[:separatorStart]

			// Panic safe
			// The max `separator_end` is `bytes.len()` and `bytes[bytes.len()..]` is a valid index.
			return component, path[separatorEnd:]
		}
	}
	return path, ""
}

func containsByte(s string, b byte) bool {
	for i := 0; i < len(s); i++ {
		if s[i] == b {
			return true
		}
	}
	return false
}
