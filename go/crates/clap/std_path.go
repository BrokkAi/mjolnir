// Port of std 1.99.1 src/path.rs (Path::file_name, Path::file_stem and the
// component parsing they rest on) and src/sys/path/windows_prefix.rs
//
// go/rs/rspath has the whole of std::path for product code. The two calls
// clap makes on argv[0] are repeated here, unexported, because this package
// must not depend on a package that is still being written.

package clap

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// pathFileName is `Path::new(path).file_name()` on the host platform: the
// final component, when it is a normal one.
func pathFileName(path string) (string, bool) {
	if rs.CfgWindows {
		return pathFileNameWindows(path)
	}
	return pathFileNameUnix(path)
}

// pathFileStem is `Path::new(path).file_stem()` on the host platform.
func pathFileStem(path string) (string, bool) {
	name, ok := pathFileName(path)
	if !ok {
		return "", false
	}
	// rsplit_file_at_dot
	if name == ".." {
		return name, true
	}
	index := strings.LastIndexByte(name, '.')
	if index < 0 {
		// `before` is None: the stem is `after`, the whole name.
		return name, true
	}
	before := name[:index]
	if before == "" {
		return name, true
	}
	return before, true
}

// pathFileNameUnix is Path::file_name with Unix separators.
func pathFileNameUnix(path string) (string, bool) {
	start := 0
	if strings.HasPrefix(path, "/") {
		// The physical root.
		start = 1
	} else if path == "." || strings.HasPrefix(path, "./") {
		// include_cur_dir: a leading `.` is a component of its own.
		start = 1
	}
	return pathLastNormalWith(path[start:], false, func(b byte) bool { return b == '/' })
}

// pathLastNormalWith walks the body of a path from the back, as
// Components::next_back does: empty components and `.` are skipped, `..`
// ends the search without a file name.
func pathLastNormalWith(body string, verbatim bool, isSep func(byte) bool) (string, bool) {
	for len(body) > 0 {
		index := -1
		for i := len(body) - 1; i >= 0; i-- {
			if isSep(body[i]) {
				index = i
				break
			}
		}
		comp := body[index+1:]
		if index < 0 {
			body = ""
		} else {
			body = body[:index]
		}
		switch comp {
		case "":
			continue
		case ".":
			if verbatim {
				// Component::CurDir
				return "", false
			}
			continue
		case "..":
			return "", false
		default:
			return comp, true
		}
	}
	return "", false
}

// pathWindowsPrefix is sys::path::windows_prefix::parse_prefix, reduced to
// what the component walk needs: the prefix's length, whether it is a
// verbatim one, and whether it is a bare drive.
type pathWindowsPrefix struct {
	len      int
	verbatim bool
	drive    bool
}

func pathIsSepByteWindows(b byte) bool { return b == '\\' || b == '/' }

func pathParseDrive(path string) bool {
	if len(path) < 2 || path[1] != ':' {
		return false
	}
	drive := path[0]
	return (drive >= 'a' && drive <= 'z') || (drive >= 'A' && drive <= 'Z')
}

func pathParseNextComponent(path string, verbatim bool) (string, string) {
	for i := 0; i < len(path); i++ {
		if path[i] == '\\' || (!verbatim && path[i] == '/') {
			return path[:i], path[i+1:]
		}
	}
	return path, ""
}

func pathParsePrefixWindows(path string) (pathWindowsPrefix, bool) {
	// PrefixParser::<8>: the first eight bytes with `/` read as `\`.
	var prefix [8]byte
	n := min(len(path), 8)
	for i := 0; i < n; i++ {
		if path[i] == '/' {
			prefix[i] = '\\'
		} else {
			prefix[i] = path[i]
		}
	}
	head := string(prefix[:n])
	osStrLen := func(x, y string) int {
		if len(y) > 0 {
			return len(x) + 1 + len(y)
		}
		return len(x)
	}

	if !strings.HasPrefix(head, `\\`) {
		// If it has a drive like `C:` then it's a disk.
		// Otherwise there is no prefix.
		if pathParseDrive(path) {
			return pathWindowsPrefix{len: 2, drive: true}, true
		}
		return pathWindowsPrefix{}, false
	}
	// \\

	// The meaning of verbatim paths can change when they use a different
	// separator.
	if strings.HasPrefix(head[2:], `?\`) && !strings.ContainsRune(path[:4], '/') {
		// \\?\
		if strings.HasPrefix(head[4:], `UNC\`) {
			// \\?\UNC\server\share
			server, rest := pathParseNextComponent(path[8:], true)
			share, _ := pathParseNextComponent(rest, true)
			return pathWindowsPrefix{len: 8 + osStrLen(server, share), verbatim: true}, true
		}
		rest := path[4:]
		// in verbatim paths only recognize an exact drive prefix
		if (len(rest) <= 2 || pathIsSepByteWindows(rest[2])) && pathParseDrive(rest) {
			// \\?\C:
			return pathWindowsPrefix{len: 6, verbatim: true}, true
		}
		// \\?\prefix
		component, _ := pathParseNextComponent(rest, true)
		return pathWindowsPrefix{len: 4 + len(component), verbatim: true}, true
	}
	if strings.HasPrefix(head[2:], `.\`) {
		// \\.\COM42
		component, _ := pathParseNextComponent(path[4:], false)
		return pathWindowsPrefix{len: 4 + len(component)}, true
	}
	server, rest := pathParseNextComponent(path[2:], false)
	share, _ := pathParseNextComponent(rest, false)
	if server != "" && share != "" {
		// \\server\share
		return pathWindowsPrefix{len: 2 + osStrLen(server, share)}, true
	}
	// no valid prefix beginning with "\\" recognized
	return pathWindowsPrefix{}, false
}

// pathFileNameWindows is Path::file_name with Windows prefixes and
// separators.
func pathFileNameWindows(path string) (string, bool) {
	prefix, hasPrefix := pathParsePrefixWindows(path)
	isSep := func(b byte) bool {
		if prefix.verbatim {
			return b == '\\'
		}
		return pathIsSepByteWindows(b)
	}
	rest := path[min(prefix.len, len(path)):]
	hasPhysicalRoot := len(rest) > 0 && pathIsSepByteWindows(rest[0])
	hasRoot := hasPhysicalRoot || (hasPrefix && !prefix.drive)
	includeCurDir := false
	if !hasRoot {
		switch {
		case rest == ".":
			includeCurDir = true
		case len(rest) >= 2 && rest[0] == '.' && isSep(rest[1]):
			includeCurDir = true
		}
	}
	start := 0
	if hasPhysicalRoot {
		start++
	}
	if includeCurDir {
		start++
	}
	return pathLastNormalWith(rest[start:], prefix.verbatim, isSep)
}
