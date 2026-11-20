// Port of mj-checkpoint/src/native.rs.
//
// Native harness storage shared by import and checkpoint restoration.

package checkpoint

import (
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MuseSessionsRoot is where Muse keeps the sessions of configHome. External
// Muse sessions use XDG data storage; private mj profiles use their isolated
// data directory. Configuration credentials are never a session root.
func MuseSessionsRoot(configHome string) (string, error) {
	data, ok := core.HarnessKindMuse.NestedDataDir(configHome)
	if !ok {
		return "", errors.New("Muse data directory is unavailable")
	}
	return filepath.Join(data, "sessions"), nil
}

// nativeDebugString renders text as Rust's `{:?}` does for a str, for the
// error messages of the native modules.
func nativeDebugString(text string) string {
	var b strings.Builder
	b.WriteByte('"')
	for i, r := range text {
		switch {
		case r == '\t':
			b.WriteString(`\t`)
		case r == '\r':
			b.WriteString(`\r`)
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\\':
			b.WriteString(`\\`)
		case r == '"':
			b.WriteString(`\"`)
		case r == 0:
			b.WriteString(`\0`)
		case r == utf8.RuneError && !strings.HasPrefix(text[i:], "�"):
			// Invalid UTF-8 cannot occur in a Rust str; keep the byte visible.
			fmt.Fprintf(&b, `\u{%x}`, text[i])
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			// Rust escapes non-printable and grapheme-extending characters.
			fmt.Fprintf(&b, `\u{%x}`, r)
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
	return b.String()
}
