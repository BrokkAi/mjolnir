// Port of mj-cli/src/terminal_background.rs.
//
// Asks the terminal for its background color, for the Auto theme.
//
// The question is OSC 11 (`ESC ] 11 ; ? ESC \`), followed by Primary Device
// Attributes (DA1, `ESC [ c`). Every VT-compatible terminal answers DA1 and
// answers queries in order, so the DA1 reply marks the end of anything the
// terminal will say about its background: a terminal that ignores OSC 11
// costs one round trip, not a timeout.
//
// The replies must be read here, before the terminal's input reader starts.
// A reader that parses an unknown `ESC ]` sequence as Alt and the
// characters after it would type a reply into the composer.

package cli

import (
	"log/slog"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// queryTerminalBackground is the terminal's background, or nil when it does
// not report one. Call in raw mode, before anything else reads the
// terminal's input. Bytes that arrive ahead of the replies, such as keys
// typed while mj starts, are consumed with them.
func queryTerminalBackground() *chat.Rgb {
	background, err := askTerminalBackground()
	if err != nil {
		slog.Warn("could not ask the terminal for its background color", "error", err)
		return nil
	}
	return background
}
