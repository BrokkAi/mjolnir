//go:build unix

// Port of the tests in mj-cli/src/terminal_background.rs, module `unix`.

package cli

import (
	"fmt"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/term"
	"github.com/charmbracelet/x/xpty"
	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// Replies as terminals send them: xterm and VTE with four digits and ST,
// kitty with BEL, rxvt-unicode with `rgba:`, mixed channel widths, a
// terminal that answers only DA1, and keys typed before the replies.
func TestBackgroundRepliesFromRealTerminalsParseUpToTheAttributesReply(t *testing.T) {
	rgb := func(r, g, b uint8) *chat.Rgb { return &chat.Rgb{R: r, G: g, B: b} }
	cases := []struct {
		reply    string
		expected *chat.Rgb
	}{
		{"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\\x1b[?65;1;9c", rgb(253, 246, 227)},
		{"\x1b]11;rgb:1e1e/1e1e/2e2e\x07\x1b[?62;c", rgb(30, 30, 46)},
		{"\x1b]11;rgba:0000/2b2b/3636/ffff\x1b\\\x1b[?1;2c", rgb(0, 43, 54)},
		{"\x1b]11;rgb:ff/80/0\x07\x1b[?6c", rgb(255, 128, 0)},
		{"\x1b[?1;2c", nil},
		{"ls\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?64;1c", rgb(255, 255, 255)},
	}
	for _, test := range cases {
		reply := []byte(test.reply)
		if end, ok := attributesReplyEnd(reply); !ok || end != len(reply) {
			t.Errorf("%q: attributes reply end = %d, %v", test.reply, end, ok)
		}
		got := backgroundReply(reply)
		if (got == nil) != (test.expected == nil) || (got != nil && *got != *test.expected) {
			t.Errorf("%q: background = %v, want %v", test.reply, got, test.expected)
		}
	}
	// A keyboard-flags reply is not DA1, and an unfinished DA1 reply is
	// still being read.
	if end, ok := attributesReplyEnd([]byte("\x1b[?1u")); ok {
		t.Errorf("keyboard flags read as DA1 ending at %d", end)
	}
	if end, ok := attributesReplyEnd([]byte("\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?6")); ok {
		t.Errorf("an unfinished DA1 reply read as ending at %d", end)
	}
}

// rawTerminal is a pseudo-terminal in raw mode, as the dashboard's is when
// it asks.
func rawTerminal(t *testing.T) (master, slave *os.File) {
	t.Helper()
	pty, err := xpty.NewUnixPty(-1, -1)
	if err != nil {
		t.Fatalf("open a pseudo-terminal: %v", err)
	}
	t.Cleanup(func() { _ = pty.Close() })
	master, slave = pty.Master(), pty.Slave()
	if _, err := term.MakeRaw(slave.Fd()); err != nil {
		t.Fatalf("make the pseudo-terminal raw: %v", err)
	}
	return master, slave
}

const terminalBackgroundTestReply = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?1;2c"

// Hard-won: #1609: an answer that had arrived was dropped when the process
// could not read it before the deadline.
func TestAReplyThatHasArrivedIsReadAfterTheDeadline(t *testing.T) {
	master, slave := rawTerminal(t)
	if _, err := master.WriteString(terminalBackgroundTestReply); err != nil {
		t.Fatal(err)
	}
	input := int(slave.Fd())
	// The terminal hands the bytes over on its own schedule.
	delivered := time.Now().Add(5 * time.Second)
	for {
		unread, _ := unix.IoctlGetInt(input, terminalFionread)
		if unread == len(terminalBackgroundTestReply) {
			break
		}
		if !time.Now().Before(delivered) {
			t.Fatalf("%d bytes delivered", unread)
		}
		time.Sleep(time.Millisecond)
	}
	// The process was held up past its wait after it asked.
	replies, err := readTerminalReplies(input, time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if string(replies) != terminalBackgroundTestReply {
		t.Fatalf("replies = %q", replies)
	}
	// With nothing more to read, a passed deadline returns at once.
	replies, err = readTerminalReplies(input, time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if len(replies) != 0 {
		t.Fatalf("replies = %q", replies)
	}
}

// A terminal answers within milliseconds, sometimes before the probe starts
// to wait and sometimes while it waits.
func TestAReplyOnARealTerminalIsReadWheneverItArrives(t *testing.T) {
	master, slave := rawTerminal(t)
	input := int(slave.Fd())
	var missed []string
	for round := range uint64(200) {
		delay := time.Duration(round%11*250) * time.Microsecond
		answered := make(chan error, 1)
		go func() {
			time.Sleep(delay)
			_, err := master.WriteString(terminalBackgroundTestReply)
			answered <- err
		}()
		asked := time.Now()
		replies, err := readTerminalReplies(input, asked.Add(terminalBackgroundAnswerTimeout))
		if err != nil {
			t.Fatal(err)
		}
		if err := <-answered; err != nil {
			t.Fatal(err)
		}
		if background := backgroundReply(replies); background == nil || *background != (chat.Rgb{R: 255, G: 255, B: 255}) {
			missed = append(missed, fmt.Sprintf("round %d, reply %v after asking: read %d bytes in %v",
				round, delay, len(replies), time.Since(asked)))
			// Whatever was missed is still waiting; start the next round
			// clean.
			flushTerminalInput(input)
			if len(missed) == 5 {
				break
			}
		}
	}
	if len(missed) > 0 {
		t.Fatal(strings.Join(missed, "\n"))
	}
}
