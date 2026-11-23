// Port of mj-chat/src/chat/pasted_paths.rs.

// Image files named by pasted or dropped text.
//
// A terminal delivers a dropped file, and Explorer's "Copy as path", as text.
// A container or remote session cannot open a path on this machine, so a
// paste made only of image paths becomes image attachments instead. The
// files are read later, on the attachment task.

package chat

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
	panic("unported: mj-chat/src/chat/pasted_paths.rs image_paths")
}

// pathArgument is the path `/attach` names: one quoted, escaped, `file://` or
// Windows path, or else the argument exactly as typed.
func pathArgument(argument string) PastedPath {
	panic("unported: mj-chat/src/chat/pasted_paths.rs path_argument")
}

// imagePath is the image file text names, or nil.
func imagePath(text string) PastedPath {
	panic("unported: mj-chat/src/chat/pasted_paths.rs image_path")
}

// pathFrom is an absolute path named by one token, or nil.
func pathFrom(text string) PastedPath {
	panic("unported: mj-chat/src/chat/pasted_paths.rs path_from")
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
	panic("unported: mj-chat/src/chat/pasted_paths.rs line_tokens")
}
