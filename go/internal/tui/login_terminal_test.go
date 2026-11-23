// Port of the tests in mj-tui/src/login_terminal.rs.

package tui

import (
	"fmt"
	"strings"
	"testing"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loginTerminalRender draws pane over a dashboard backdrop into a fresh
// width by height terminal and returns the rows.
func loginTerminalRender(width, height int, pane *LoginPane) string {
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	area := frame.Area()
	backdrop := make([]chat.Line, 0, area.Dy())
	for row := 0; row < area.Dy(); row++ {
		backdrop = append(backdrop, chat.RawLine(fmt.Sprintf(
			"dashboard row %02d %s", row, strings.Repeat("─", max(area.Dx()-18, 0)),
		)))
	}
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(backdrop...)).WithStyle(chat.Base()), area)
	pane.Render(frame, area)
	return strings.Join(bufferLines(buffer), "\n")
}

func TestLoginTerminalRenderingPreservesFragmentedAnsiAndReportsCompletion(t *testing.T) {
	cols, rows := LoginPaneTerminalSize(76, 12)
	pane := NewLoginPane("fixture", cols, rows)
	rendered := []string{loginTerminalRender(76, 12, pane)}
	pane.Status("Signed in. Enter returns to Settings.")
	for _, chunk := range [][]byte{
		[]byte("Open https://example.test/login\r\n\x1b[3"),
		[]byte("2mCode: \x1b[0m\xe2\x9c"),
		[]byte("\x93\r\nSaved credentials.\x1b[6n"),
	} {
		pane.Output(chunk)
	}
	rendered = append(rendered, loginTerminalRender(76, 12, pane))
	pane.Status("The daemon connection ended during login or harness preparation; retry Login. Enter returns to Settings.")
	rendered = append(rendered, loginTerminalRender(76, 12, pane))
	core.AssertGolden(t, "testdata", "login-terminal", strings.Join(rendered, "\n\n"))
}
