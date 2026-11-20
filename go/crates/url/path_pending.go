// Port of std 1.99.1 src/path.rs (Components) and
// src/sys/path/windows_prefix.rs, as far as Url::from_file_path needs them.
//
// These belong to go/rs/rspath, which did not exist when this package was
// written. They are private copies; replace them with the rspath functions
// when that package lands.

package url

import "strings"

// unixPathIsAbsolute is Path::is_absolute on Unix.
func unixPathIsAbsolute(path string) bool {
	return strings.HasPrefix(path, "/")
}

// unixPathComponents is Path::components on Unix, each component as its
// `as_os_str()`: "/" for the root directory, ".." for a parent directory,
// and "." only for a relative path that starts with it. Empty components and
// other "." components are skipped, as Rust normalizes them away.
func unixPathComponents(path string) []string {
	var components []string
	rest := path
	hasRoot := strings.HasPrefix(path, "/")
	if hasRoot {
		components = append(components, "/")
		rest = rest[1:]
	} else if rest == "." || strings.HasPrefix(rest, "./") {
		components = append(components, ".")
		rest = rest[1:]
	}
	for _, component := range strings.Split(rest, "/") {
		switch component {
		case "", ".":
		default:
			components = append(components, component)
		}
	}
	return components
}

type windowsPrefixKind uint8

const (
	windowsPrefixNone windowsPrefixKind = iota
	windowsPrefixVerbatim
	windowsPrefixVerbatimUnc
	windowsPrefixVerbatimDisk
	windowsPrefixDeviceNs
	windowsPrefixUnc
	windowsPrefixDisk
)

// windowsPrefix is std::path::Prefix.
type windowsPrefix struct {
	kind windowsPrefixKind
	// letter is the drive of Disk and VerbatimDisk, upper-cased.
	letter byte
	// server and share are the two parts of UNC and VerbatimUNC; server also
	// holds the single part of Verbatim and DeviceNS.
	server string
	share  string
}

// length is Prefix::len.
func (p windowsPrefix) length() int {
	switch p.kind {
	case windowsPrefixVerbatim:
		return 4 + len(p.server)
	case windowsPrefixVerbatimUnc:
		if len(p.share) > 0 {
			return 8 + len(p.server) + 1 + len(p.share)
		}
		return 8 + len(p.server)
	case windowsPrefixVerbatimDisk:
		return 6
	case windowsPrefixUnc:
		if len(p.share) > 0 {
			return 2 + len(p.server) + 1 + len(p.share)
		}
		return 2 + len(p.server)
	case windowsPrefixDeviceNs:
		return 4 + len(p.server)
	case windowsPrefixDisk:
		return 2
	}
	return 0
}

func (p windowsPrefix) isVerbatim() bool {
	return p.kind == windowsPrefixVerbatim || p.kind == windowsPrefixVerbatimDisk || p.kind == windowsPrefixVerbatimUnc
}

func windowsIsSepByte(b byte) bool { return b == '\\' || b == '/' }

// windowsParseNextComponent is parse_next_component: the next component and
// the rest of the path excluding the component and separator. `/` is not a
// separator when verbatim is true.
func windowsParseNextComponent(path string, verbatim bool) (string, string) {
	for i := 0; i < len(path); i++ {
		if path[i] == '\\' || (!verbatim && path[i] == '/') {
			return path[:i], path[i+1:]
		}
	}
	return path, ""
}

// Parses a drive prefix, e.g. "C:" and "C:\whatever"
func windowsParseDrive(path string) (byte, bool) {
	if len(path) >= 2 && path[1] == ':' && parserAsciiAlpha(rune(path[0])) {
		drive := path[0]
		if 'a' <= drive && drive <= 'z' {
			drive -= 'a' - 'A'
		}
		return drive, true
	}
	return 0, false
}

// Parses a drive prefix exactly, e.g. "C:"
func windowsParseDriveExact(path string) (byte, bool) {
	// only parse two bytes: the drive letter and the drive separator
	if len(path) <= 2 || windowsIsSepByte(path[2]) {
		return windowsParseDrive(path)
	}
	return 0, false
}

// windowsParsePrefix is std's parse_prefix for Windows.
func windowsParsePrefix(path string) windowsPrefix {
	// The first bytes with `/` read as `\`.
	var prefix [8]byte
	prefixLen := min(8, len(path))
	for i := 0; i < prefixLen; i++ {
		if path[i] == '/' {
			prefix[i] = '\\'
		} else {
			prefix[i] = path[i]
		}
	}
	normalized := string(prefix[:prefixLen])
	index := 0
	strip := func(want string) bool {
		if strings.HasPrefix(normalized[index:], want) {
			index += len(want)
			return true
		}
		return false
	}
	if strip(`\\`) {
		// \\

		// The meaning of verbatim paths can change when they use a different
		// separator.
		before := index
		if strip(`?\`) && !strings.Contains(path[:index], "/") {
			// \\?\
			if strip(`UNC\`) {
				// \\?\UNC\server\share

				server, rest := windowsParseNextComponent(path[index:], true)
				share, _ := windowsParseNextComponent(rest, true)

				return windowsPrefix{kind: windowsPrefixVerbatimUnc, server: server, share: share}
			}
			rest := path[index:]

			// in verbatim paths only recognize an exact drive prefix
			if drive, ok := windowsParseDriveExact(rest); ok {
				// \\?\C:
				return windowsPrefix{kind: windowsPrefixVerbatimDisk, letter: drive}
			}
			// \\?\prefix
			name, _ := windowsParseNextComponent(rest, true)
			return windowsPrefix{kind: windowsPrefixVerbatim, server: name}
		}
		index = before
		if strip(`.\`) {
			// \\.\COM42
			name, _ := windowsParseNextComponent(path[index:], false)
			return windowsPrefix{kind: windowsPrefixDeviceNs, server: name}
		}
		server, rest := windowsParseNextComponent(path[index:], false)
		share, _ := windowsParseNextComponent(rest, false)

		if server != "" && share != "" {
			// \\server\share
			return windowsPrefix{kind: windowsPrefixUnc, server: server, share: share}
		}
		// no valid prefix beginning with "\\" recognized
		return windowsPrefix{kind: windowsPrefixNone}
	}
	// If it has a drive like `C:` then it's a disk.
	// Otherwise there is no prefix.
	if drive, ok := windowsParseDrive(path); ok {
		return windowsPrefix{kind: windowsPrefixDisk, letter: drive}
	}
	return windowsPrefix{kind: windowsPrefixNone}
}

// windowsPathComponents is Path::components on Windows for an absolute path:
// the prefix, and the components after it as their `as_os_str()`, without the
// root directory. The last result is Path::is_absolute; the components are
// nil when it is false.
func windowsPathComponents(path string) (windowsPrefix, []string, bool) {
	prefix := windowsParsePrefix(path)
	hasPrefix := prefix.kind != windowsPrefixNone
	rest := path
	if hasPrefix {
		rest = path[min(prefix.length(), len(path)):]
	}
	hasPhysicalRoot := len(rest) > 0 && windowsIsSepByte(rest[0])
	hasRoot := hasPhysicalRoot || (hasPrefix && prefix.kind != windowsPrefixDisk)
	if !(hasRoot && hasPrefix) {
		return prefix, nil, false
	}
	if hasPhysicalRoot {
		rest = rest[1:]
	}
	verbatim := prefix.isVerbatim()
	var components []string
	for len(rest) > 0 {
		var component string
		component, rest = windowsParseNextComponent(rest, verbatim)
		switch component {
		case "":
		case ".":
			// . components are normalized away, except in a verbatim path
			if verbatim {
				components = append(components, component)
			}
		default:
			components = append(components, component)
		}
	}
	return prefix, components, true
}
