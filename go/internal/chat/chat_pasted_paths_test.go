// Port of mj-chat/src/chat/pasted_paths.rs (tests).

package chat

import (
	"reflect"
	"runtime"
	"testing"
)

// foundPastedPath is one image path as it was pasted and what it names.
type foundPastedPath struct {
	pasted string
	path   PastedPath
}

func foundPastedPaths(pasted string) ([]foundPastedPath, bool) {
	paths, ok := imagePaths(pasted)
	if !ok {
		return nil, false
	}
	found := make([]foundPastedPath, 0, len(paths))
	for _, path := range paths {
		found = append(found, foundPastedPath{pasted[path.byteRange.Start:path.byteRange.End], path.path})
	}
	return found, true
}

func windowsPastedPath(path string) PastedPath {
	if runtime.GOOS == "windows" {
		return PastedPathLocal{Path: path}
	}
	return PastedPathWindows{Path: path}
}

func assertFoundPastedPaths(t *testing.T, pasted string, want []foundPastedPath) {
	t.Helper()
	found, ok := foundPastedPaths(pasted)
	if !ok {
		t.Fatalf("%q: no image paths, want %#v", pasted, want)
	}
	if !reflect.DeepEqual(found, want) {
		t.Fatalf("%q: found %#v, want %#v", pasted, found, want)
	}
}

// Each form a terminal or file manager pastes for a dropped or copied file,
// and the prose that must stay text.
func TestPastedImagePathsAreRecognizedInEveryFormTerminalsProduce(t *testing.T) {
	// Explorer's "Copy as path", quoted, with an apostrophe and accents.
	explorer := `"C:\Users\me\Pictures\Capture d'écran 2026-10-06 052845.png"`
	assertFoundPastedPaths(t, explorer, []foundPastedPath{{
		explorer,
		windowsPastedPath(`C:\Users\me\Pictures\Capture d'écran 2026-10-06 052845.png`),
	}})
	// Several files copied as paths arrive one per line.
	two := "\"C:\\a b\\one.PNG\"\n\\\\wsl.localhost\\Debian\\tmp\\two.webp\n"
	assertFoundPastedPaths(t, two, []foundPastedPath{
		{"\"C:\\a b\\one.PNG\"", windowsPastedPath(`C:\a b\one.PNG`)},
		{`\\wsl.localhost\Debian\tmp\two.webp`, windowsPastedPath(`\\wsl.localhost\Debian\tmp\two.webp`)},
	})
	// An unquoted Windows path with spaces is one path.
	assertFoundPastedPaths(t, `C:\My Pictures\shot.jpg`, []foundPastedPath{
		{`C:\My Pictures\shot.jpg`, windowsPastedPath(`C:\My Pictures\shot.jpg`)},
	})
	if runtime.GOOS != "windows" {
		// macOS terminals escape spaces and drop several files on one line;
		// GNOME quotes with single quotes and adds a trailing space.
		mac := `/Users/me/Desktop/Screen\ Shot\ 1.png /tmp/b.jpeg `
		assertFoundPastedPaths(t, mac, []foundPastedPath{
			{`/Users/me/Desktop/Screen\ Shot\ 1.png`, PastedPathLocal{Path: "/Users/me/Desktop/Screen Shot 1.png"}},
			{"/tmp/b.jpeg", PastedPathLocal{Path: "/tmp/b.jpeg"}},
		})
		assertFoundPastedPaths(t, "'/home/me/my shot.png' ", []foundPastedPath{
			{"'/home/me/my shot.png'", PastedPathLocal{Path: "/home/me/my shot.png"}},
		})
		assertFoundPastedPaths(t, "file:///home/me/caf%C3%A9%20shot.png", []foundPastedPath{
			{"file:///home/me/caf%C3%A9%20shot.png", PastedPathLocal{Path: "/home/me/café shot.png"}},
		})
	}
	// Anything else stays text.
	for _, prose := range []string{
		"",
		"\n\n",
		`"C:\Users\me\shot.png" we have a graphical bug`,
		"look at /tmp/shot.png",
		"/tmp/notes.txt",
		"relative/shot.png",
		`"C:\unterminated.png`,
	} {
		if found, ok := foundPastedPaths(prose); ok {
			t.Fatalf("%q: found %#v, want none", prose, found)
		}
	}
}
