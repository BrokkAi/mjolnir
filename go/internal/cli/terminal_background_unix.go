//go:build unix

// Port of mj-cli/src/terminal_background.rs, module `unix`.

package cli

import (
	"bytes"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// terminalBackgroundAnswerTimeout is as long as crossterm waits for its own
// keyboard query.
const terminalBackgroundAnswerTimeout = 2 * time.Second

// terminalBackgroundQuery asks for the background (OSC 11) and then for the
// primary device attributes (DA1).
const terminalBackgroundQuery = "\x1b]11;?\x1b\\\x1b[c"

// askTerminalBackground writes the query to standard output and reads the
// replies from the descriptor the input reader uses: standard input when it
// is the terminal, otherwise the controlling terminal. The bytes are not
// logged: keys typed while mj starts arrive here too.
func askTerminalBackground() (*chat.Rgb, error) {
	panic("unported: mj-cli/src/terminal_background.rs unix::ask")
}

// readTerminalReplies reads one byte at a time from input, so nothing after
// the DA1 reply is taken from the input reader. A background that arrived
// is used even when DA1 never does.
//
// The deadline bounds the wait for bytes that have not arrived. Bytes
// already here are read whatever the time, so a process that was held up
// after it asked, on a machine that is paging for instance, still finds the
// answer the terminal gave in time.
func readTerminalReplies(input int, deadline time.Time) ([]byte, error) {
	panic("unported: mj-cli/src/terminal_background.rs unix::read_replies")
}

// attributesReplyEnd is where a DA1 reply (`ESC [ ? <digits and ;> c`) ends
// in replies; false when there is none.
func attributesReplyEnd(replies []byte) (int, bool) {
	search := 0
	for {
		offset := findSequence(replies[search:], []byte("\x1b[?"))
		if offset < 0 {
			return 0, false
		}
		start := search + offset + 3
		parameters := 0
		for _, b := range replies[start:] {
			if (b < '0' || b > '9') && b != ';' {
				break
			}
			parameters++
		}
		if end := start + parameters; end < len(replies) && replies[end] == 'c' {
			return end + 1, true
		}
		search = start
	}
}

// backgroundReply is the color in an OSC 11 reply in replies:
// `ESC ] 11 ; rgb:R/G/B` or `rgba:R/G/B/A`, each channel one to four hex
// digits, ended by BEL or `ESC \`.
func backgroundReply(replies []byte) *chat.Rgb {
	start := findSequence(replies, []byte("\x1b]11;"))
	if start < 0 {
		return nil
	}
	body := replies[start+5:]
	end := bytes.IndexAny(body, "\x07\x1b")
	if end < 0 || !utf8.Valid(body[:end]) {
		return nil
	}
	spec := string(body[:end])
	var channels string
	if rest, ok := strings.CutPrefix(spec, "rgb:"); ok {
		channels = rest
	} else if rest, ok := strings.CutPrefix(spec, "rgba:"); ok {
		channels = rest
	} else {
		return nil
	}
	parts := strings.Split(channels, "/")
	if len(parts) < 3 {
		return nil
	}
	var rgb [3]uint8
	for i := range rgb {
		value, ok := colorChannel(parts[i])
		if !ok {
			return nil
		}
		rgb[i] = value
	}
	return &chat.Rgb{R: rgb[0], G: rgb[1], B: rgb[2]}
}

// colorChannel is a one-to-four-digit hex channel scaled to eight bits
// (Rust's `channel`). Like u32::from_str_radix, it takes one leading '+'.
func colorChannel(digits string) (uint8, bool) {
	if len(digits) < 1 || len(digits) > 4 {
		return 0, false
	}
	hex := strings.TrimPrefix(digits, "+")
	if hex == "" || strings.HasPrefix(hex, "+") || strings.HasPrefix(hex, "-") {
		return 0, false
	}
	value, err := strconv.ParseUint(hex, 16, 32)
	if err != nil {
		return 0, false
	}
	maximum := uint64(1)<<(4*len(digits)) - 1
	scaled := (value*255 + maximum/2) / maximum
	if scaled > 255 {
		return 0, false
	}
	return uint8(scaled), true
}

// findSequence is where needle first starts in haystack, or -1 (Rust's
// `find`).
func findSequence(haystack, needle []byte) int {
	return bytes.Index(haystack, needle)
}
