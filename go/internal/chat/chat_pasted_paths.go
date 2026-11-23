// Port of mj-chat/src/chat/pasted_paths.rs.

// Image files named by pasted or dropped text.
//
// A terminal delivers a dropped file, and Explorer's "Copy as path", as text.
// A container or remote session cannot open a path on this machine, so a
// paste made only of image paths becomes image attachments instead. The
// files are read later, on the attachment task.

package chat

import (
	"path/filepath"
	"runtime"
	"strings"
	"unicode"
	"unicode/utf8"
)

var imageExtensions = [4]string{"png", "jpg", "jpeg", "webp"}

// PastedPath is a path as it was pasted, before it is resolved on this
// machine.
type PastedPath interface {
	isPastedPath()
	// resolve returns the file to read. Under WSL a Windows path is
	// translated by running `wslpath -u`, so call this on the attachment
	// task, never on the event loop.
	resolve() (string, error)
}

// PastedPathLocal is a path this platform opens as written.
type PastedPathLocal struct {
	Path string
}

// PastedPathWindows is a Windows drive or UNC path on a Unix client, which
// only WSL can open.
type PastedPathWindows struct {
	Path string
}

func (PastedPathLocal) isPastedPath()   {}
func (PastedPathWindows) isPastedPath() {}

func (p PastedPathLocal) resolve() (string, error) { return p.Path, nil }

func (p PastedPathWindows) resolve() (string, error) { return windowsPathOnThisHost(p.Path) }

// PastedImagePath is one image path in pasted text. byteRange (Rust's
// `range`, a Go keyword) covers it as pasted, quotes and escapes included,
// so a failed read can put back exactly what was pasted.
type PastedImagePath struct {
	byteRange ByteRange
	path      PastedPath
}

// imagePaths returns the image paths pasted consists of, or false when it
// holds anything else. Every non-blank line must be one or more absolute
// PNG, JPEG or WebP paths: quoted, backslash-escaped as macOS terminals drop
// them, `file://` URLs, or Windows drive and UNC paths.
func imagePaths(pasted string) ([]PastedImagePath, bool) {
	var found []PastedImagePath
	lineOffset := 0
	for _, line := range strings.Split(pasted, "\n") {
		offset := lineOffset
		lineOffset += len(line) + 1
		paths, ok := lineImagePaths(line)
		if !ok {
			// An unquoted path with spaces, as a Windows path is often
			// pasted, only parses as the whole line.
			trimmed := strings.TrimSpace(line)
			start := len(line) - len(strings.TrimLeftFunc(line, unicode.IsSpace))
			path := imagePath(trimmed)
			if path == nil {
				return nil, false
			}
			paths = []PastedImagePath{{byteRange: ByteRange{Start: start, End: start + len(trimmed)}, path: path}}
		}
		for _, path := range paths {
			found = append(found, PastedImagePath{
				byteRange: ByteRange{Start: path.byteRange.Start + offset, End: path.byteRange.End + offset},
				path:      path.path,
			})
		}
	}
	if len(found) == 0 {
		return nil, false
	}
	return found, true
}

// lineImagePaths is every token of line as an image path, or false when the
// line does not tokenize or one of its tokens is not an image path.
func lineImagePaths(line string) ([]PastedImagePath, bool) {
	tokens, ok := lineTokens(line)
	if !ok {
		return nil, false
	}
	paths := make([]PastedImagePath, 0, len(tokens))
	for _, token := range tokens {
		path := imagePath(token.text)
		if path == nil {
			return nil, false
		}
		paths = append(paths, PastedImagePath{byteRange: token.byteRange, path: path})
	}
	return paths, true
}

// pathArgument is the path `/attach` names: one quoted, escaped, `file://` or
// Windows path, or else the argument exactly as typed.
func pathArgument(argument string) PastedPath {
	argument = strings.TrimSpace(argument)
	if tokens, ok := lineTokens(argument); ok && len(tokens) == 1 {
		if path := pathFrom(tokens[0].text); path != nil {
			return path
		}
		return PastedPathLocal{Path: tokens[0].text}
	}
	if path := pathFrom(argument); path != nil {
		return path
	}
	return PastedPathLocal{Path: argument}
}

// imagePath is the image file text names, or nil.
func imagePath(text string) PastedPath {
	path := pathFrom(text)
	var name string
	switch path := path.(type) {
	case PastedPathLocal:
		name = path.Path
	case PastedPathWindows:
		name = path.Path
	default:
		return nil
	}
	extension, ok := pastedPathExtension(name)
	if !ok {
		return nil
	}
	for _, image := range imageExtensions {
		if pastedEqualFold(extension, image) {
			return path
		}
	}
	return nil
}

// pastedPathExtension is Rust's Path::extension on this platform: the text after
// the last dot of the file name, where a leading dot does not count.
// Separators are this platform's, so a Windows path on Unix is one name.
func pastedPathExtension(path string) (string, bool) {
	isSeparator := func(b byte) bool { return b == '/' || (runtime.GOOS == "windows" && b == '\\') }
	end := len(path)
	for {
		for end > 0 && isSeparator(path[end-1]) {
			end--
		}
		// A `.` after the first component is no component of its own.
		if end > 1 && path[end-1] == '.' && isSeparator(path[end-2]) {
			end--
			continue
		}
		break
	}
	start := end
	for start > 0 && !isSeparator(path[start-1]) {
		start--
	}
	name := path[start:end]
	if name == "" || name == "." || name == ".." {
		return "", false
	}
	dot := strings.LastIndexByte(name, '.')
	if dot <= 0 {
		return "", false
	}
	return name[dot+1:], true
}

// pastedEqualFold is Rust's eq_ignore_ascii_case: Unicode folding would
// accept look-alike letters.
func pastedEqualFold(left, right string) bool {
	if len(left) != len(right) {
		return false
	}
	for index := 0; index < len(left); index++ {
		a, b := left[index], right[index]
		if 'A' <= a && a <= 'Z' {
			a += 'a' - 'A'
		}
		if 'A' <= b && b <= 'Z' {
			b += 'a' - 'A'
		}
		if a != b {
			return false
		}
	}
	return true
}

// pathFrom is an absolute path named by one token, or nil.
func pathFrom(text string) PastedPath {
	if len(text) >= 7 && pastedEqualFold(text[:7], "file://") {
		path, ok := pastedFileURLPath(text)
		if !ok {
			return nil
		}
		return PastedPathLocal{Path: path}
	}
	if isWindowsPath(text) {
		if runtime.GOOS == "windows" {
			return PastedPathLocal{Path: text}
		}
		return PastedPathWindows{Path: text}
	}
	if filepath.IsAbs(text) {
		return PastedPathLocal{Path: text}
	}
	return nil
}

// pastedFileURLPath is the local file a `file://` URL names, as the url crate's
// Url::parse and Url::to_file_path find it: the query and fragment are
// dropped, `.` and `..` segments are resolved, and each segment is
// percent-decoded (an invalid escape stays literal). A host other than
// `localhost` names another machine, which only Windows opens, as a UNC
// path.
func pastedFileURLPath(text string) (string, bool) {
	rest := text[len("file://"):]
	if end := strings.IndexAny(rest, "?#"); end >= 0 {
		rest = rest[:end]
	}
	// A file URL is a special URL, where a backslash separates segments too.
	rest = strings.ReplaceAll(rest, `\`, "/")
	host, path := rest, ""
	if slash := strings.IndexByte(rest, '/'); slash >= 0 {
		host, path = rest[:slash], rest[slash:]
	}
	if pastedIsURLDriveLetter(host) {
		// `file://C:/…` names a drive, not a host.
		host, path = "", "/"+rest
	}
	if pastedEqualFold(host, "localhost") {
		host = ""
	}
	if strings.ContainsRune(host, ':') {
		// A file URL has no port.
		return "", false
	}
	var segments []string
	if path != "" {
		raw := strings.Split(path[1:], "/")
		for index, segment := range raw {
			last := index == len(raw)-1
			switch {
			case pastedIsURLSingleDot(segment):
				if last {
					segments = append(segments, "")
				}
			case pastedIsURLDoubleDot(segment):
				if len(segments) > 0 {
					segments = segments[:len(segments)-1]
				}
				if last {
					segments = append(segments, "")
				}
			default:
				segments = append(segments, segment)
			}
		}
	}
	if len(segments) == 0 {
		segments = []string{""}
	}
	if runtime.GOOS == "windows" {
		return pastedWindowsFileURLPath(host, segments)
	}
	if host != "" {
		return "", false
	}
	var bytes []byte
	for _, segment := range segments {
		bytes = append(bytes, '/')
		bytes = append(bytes, pastedPercentDecode(segment)...)
	}
	// A windows drive letter must end with a slash.
	if n := len(bytes); n > 2 && pastedIsAlpha(bytes[n-2]) && (bytes[n-1] == ':' || bytes[n-1] == '|') {
		bytes = append(bytes, '/')
	}
	return string(bytes), true
}

// pastedWindowsFileURLPath is to_file_path on Windows: a drive path, or a UNC path
// when the URL has a host.
func pastedWindowsFileURLPath(host string, segments []string) (string, bool) {
	var path strings.Builder
	if host != "" {
		path.WriteString(`\\` + host)
	} else {
		first := segments[0]
		segments = segments[1:]
		switch {
		case len(first) == 2 && pastedIsAlpha(first[0]) && first[1] == ':':
			path.WriteString(first)
		case len(first) == 4 && pastedIsAlpha(first[0]) && first[1] == '%' && first[2] == '3' && (first[3] == 'a' || first[3] == 'A'):
			path.WriteString(first[:1] + ":")
		default:
			return "", false
		}
	}
	for _, segment := range segments {
		decoded := pastedPercentDecode(segment)
		if !utf8.Valid(decoded) {
			return "", false
		}
		path.WriteByte('\\')
		path.Write(decoded)
	}
	return path.String(), true
}

func pastedIsURLDriveLetter(text string) bool {
	return len(text) == 2 && pastedIsAlpha(text[0]) && (text[1] == ':' || text[1] == '|')
}

func pastedIsURLSingleDot(segment string) bool {
	return segment == "." || pastedEqualFold(segment, "%2e")
}

func pastedIsURLDoubleDot(segment string) bool {
	switch len(segment) {
	case 2:
		return segment == ".."
	case 4:
		return pastedEqualFold(segment, ".%2e") || pastedEqualFold(segment, "%2e.")
	case 6:
		return pastedEqualFold(segment, "%2e%2e")
	}
	return false
}

func pastedIsAlpha(b byte) bool {
	return 'a' <= b && b <= 'z' || 'A' <= b && b <= 'Z'
}

// pastedPercentDecode decodes `%XY` escapes, leaving any other `%` as it is.
func pastedPercentDecode(segment string) []byte {
	decoded := make([]byte, 0, len(segment))
	for index := 0; index < len(segment); index++ {
		if segment[index] == '%' && index+2 < len(segment) && pastedIsHexDigit(segment[index+1]) && pastedIsHexDigit(segment[index+2]) {
			decoded = append(decoded, pastedHexValue(segment[index+1])<<4|pastedHexValue(segment[index+2]))
			index += 2
			continue
		}
		decoded = append(decoded, segment[index])
	}
	return decoded
}

func pastedIsHexDigit(b byte) bool {
	return '0' <= b && b <= '9' || 'a' <= b && b <= 'f' || 'A' <= b && b <= 'F'
}

func pastedHexValue(b byte) byte {
	switch {
	case '0' <= b && b <= '9':
		return b - '0'
	case 'a' <= b && b <= 'f':
		return b - 'a' + 10
	default:
		return b - 'A' + 10
	}
}

// isWindowsPath reports a drive path such as `C:\…` or `C:/…`, or a UNC path
// such as `\\server\…`.
func isWindowsPath(text string) bool {
	if len(text) >= 2 && text[0] == '\\' && text[1] == '\\' {
		return true
	}
	if len(text) < 3 {
		return false
	}
	first := text[0]
	return (first >= 'a' && first <= 'z' || first >= 'A' && first <= 'Z') &&
		text[1] == ':' && (text[2] == '\\' || text[2] == '/')
}

// pastedToken is one whitespace-separated token of a pasted line and the
// byte range it covered.
type pastedToken struct {
	byteRange ByteRange
	text      string
}

// lineTokens splits one line into whitespace-separated tokens with their
// byte ranges. A token is quoted with `"` or `'`, or bare with backslash
// escapes. A Windows path keeps its backslashes, and so does every token on
// Windows. False when a quote is left open or is followed by more text.
func lineTokens(line string) ([]pastedToken, bool) {
	var tokens []pastedToken
	index := 0
	for index < len(line) {
		first, size := utf8.DecodeRuneInString(line[index:])
		if unicode.IsSpace(first) {
			index += size
			continue
		}
		start := index
		end := start
		var text strings.Builder
		if first == '"' || first == '\'' {
			index += size
			closed := false
			for index < len(line) {
				character, size := utf8.DecodeRuneInString(line[index:])
				if character == first {
					index += size
					end = index
					closed = true
					break
				}
				text.WriteString(line[index : index+size])
				index += size
			}
			if !closed {
				return nil, false
			}
			if index < len(line) {
				if next, _ := utf8.DecodeRuneInString(line[index:]); !unicode.IsSpace(next) {
					return nil, false
				}
			}
		} else {
			escapes := runtime.GOOS != "windows" && !isWindowsPath(line[start:])
			for index < len(line) {
				character, size := utf8.DecodeRuneInString(line[index:])
				if unicode.IsSpace(character) {
					break
				}
				index += size
				end = index
				if escapes && character == '\\' && index < len(line) {
					_, escapedSize := utf8.DecodeRuneInString(line[index:])
					text.WriteString(line[index : index+escapedSize])
					index += escapedSize
					end = index
					continue
				}
				text.WriteString(line[end-size : end])
			}
		}
		tokens = append(tokens, pastedToken{byteRange: ByteRange{Start: start, End: end}, text: text.String()})
	}
	return tokens, true
}
