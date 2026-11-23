// Port of mj-tui/src/docs_screenshots.rs.

// Deterministic documentation captures rendered by the real terminal UI.
//
// The test is skipped unless asked for because it writes committed
// documentation assets. Run it explicitly whenever the terminal surface
// changes. Clear NO_COLOR so Settings describes the same theme as the
// captured terminal palette.
//
// The constants and helpers carry a docsScreenshot prefix where their Rust
// names (COLUMNS, capture, ...) are too general for the flat package.

package tui

import (
	"path/filepath"
	"strings"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	docsScreenshotColumns            = 140
	docsScreenshotRows               = 42
	docsScreenshotCellWidth          = 9
	docsScreenshotCellHeight         = 18
	docsScreenshotPadding            = 14
	docsScreenshotTerminalBackground = "#0f1214"
	docsScreenshotConversationID     = "terminal-polish"
	docsScreenshotConversationTitle  = "Polish the terminal workspace"
)

// documentationScreenshotDirectory is docs/src/assets/screenshots at the
// repository root; tests run in go/internal/tui.
func documentationScreenshotDirectory() string {
	return filepath.Join("..", "..", "..", "docs", "src", "assets", "screenshots")
}

func documentationDashboard() *DashboardState {
	panic("unported: mj-tui/src/docs_screenshots.rs documentation_dashboard")
}

func documentationConversation() core.MaterializedSession {
	panic("unported: mj-tui/src/docs_screenshots.rs documentation_conversation")
}

func applyDocumentationTranscript(dashboard *DashboardState, sessionID string, transcript []*core.TranscriptItem, running bool, nowEpochSeconds uint64) {
	panic("unported: mj-tui/src/docs_screenshots.rs apply_documentation_transcript")
}

func profileQuota(profileID string, harness core.HarnessKind, weeklyRemaining, fiveHourRemaining uint8, refreshed uint64) client.ProfileQuota {
	panic("unported: mj-tui/src/docs_screenshots.rs profile_quota")
}

// docsScreenshotsNowEpochSeconds is the module's now_epoch_seconds.
func docsScreenshotsNowEpochSeconds() uint64 {
	return uint64(max(time.Now().Unix(), 0))
}

// docsScreenshotsCapture is the module's capture: it draws dashboard with an
// attached documentation conversation and writes the SVG to path.
func docsScreenshotsCapture(path, title, description string, dashboard *DashboardState) {
	panic("unported: mj-tui/src/docs_screenshots.rs capture")
}

func bufferSVG(buffer *uv.Buffer, title, description string) string {
	panic("unported: mj-tui/src/docs_screenshots.rs buffer_svg")
}

// colorHex reproduces the terminal's rendered colors exactly, including the
// standard indexed color cube; this encoder does not choose live UI colors.
func colorHex(color chat.Color, fallback string) string {
	panic("unported: mj-tui/src/docs_screenshots.rs color_hex")
}

var xmlEscaper = strings.NewReplacer("&", "&amp;", "<", "&lt;", ">", "&gt;", "\"", "&quot;", "'", "&apos;")

func xmlEscape(value string) string {
	return xmlEscaper.Replace(value)
}
