// Port of mj-tui/src/docs_screenshots.rs.

// Deterministic documentation captures rendered by the real terminal UI.
//
// The test is skipped unless asked for because it writes committed
// documentation assets. Run it explicitly whenever the terminal surface
// changes. Clear NO_COLOR so Settings describes the same theme as the
// captured terminal palette:
//
//	NO_COLOR= MJ_GENERATE_DOCS_SCREENSHOTS=1 go test ./internal/tui -run TestGenerateDocumentationScreenshots -v
//
// The constants and helpers carry a docsScreenshot prefix where their Rust
// names (COLUMNS, capture, ...) are too general for the flat package.

package tui

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strings"
	"testing"
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

// TestGenerateDocumentationScreenshots is Rust's ignored
// generate_documentation_screenshots: it writes the committed documentation
// screenshots, so it only runs when MJ_GENERATE_DOCS_SCREENSHOTS is set.
func TestGenerateDocumentationScreenshots(t *testing.T) {
	if os.Getenv("MJ_GENERATE_DOCS_SCREENSHOTS") == "" {
		t.Skip("writes the committed documentation screenshots")
	}
	output := documentationScreenshotDirectory()
	if err := os.MkdirAll(output, 0o755); err != nil {
		t.Fatalf("create documentation screenshot directory: %v", err)
	}

	dashboard := documentationDashboard()
	docsScreenshotsCapture(
		filepath.Join(output, "dashboard.svg"),
		"Mjolnir terminal workspace",
		"The Mjolnir terminal workspace with session navigation, a conversation containing user, agent, and tool messages, an unsent prompt, target capacity, quota, and contextual keyboard controls.",
		dashboard,
	)

	wizard := documentationDashboard()
	openNewSessionWizard(wizard)
	docsScreenshotsCapture(
		filepath.Join(output, "new-session.svg"),
		"Mjolnir new-session wizard",
		"The Mjolnir new-session wizard over the terminal workspace, at the profile-selection step.",
		wizard,
	)

	palette := documentationDashboard()
	openPalette(palette)
	docsScreenshotsCapture(
		filepath.Join(output, "command-palette.svg"),
		"Mjolnir command palette",
		"The Mjolnir command palette over the terminal workspace, with session, pane, and global commands grouped together.",
		palette,
	)

	setup := documentationDashboard()
	chord(setup, CommandIdOpenConfig)
	docsScreenshotsCapture(
		filepath.Join(output, "setup.svg"),
		"Mjolnir Settings",
		"The compact Settings modal with Interface, Notifications, Advanced, Agent Profiles, Projects, Machines, Runtimes, Code Review, and Web Access settings.",
		setup,
	)

	t.Logf("wrote documentation screenshots to %s", output)
}

// documentationScreenshotDirectory is docs/src/assets/screenshots at the
// repository root; tests run in go/internal/tui.
func documentationScreenshotDirectory() string {
	return filepath.Join("..", "..", "..", "docs", "src", "assets", "screenshots")
}

func documentationDashboard() *DashboardState {
	configured := config()
	github := "BrokkAi/mjolnir"
	configured.Bundles = map[string]core.ProjectBundle{
		"mjolnir": {
			PrimaryRepo: "mjolnir",
			Repositories: []core.ProjectRepository{{
				ID:          "mjolnir",
				Github:      &github,
				Destination: "mjolnir",
			}},
		},
	}
	configured.Profiles["kimi-1"] = core.NewHarnessProfile(core.HarnessKindKimi, "/profiles/kimi")

	now := time.Now().UTC().Format(time.RFC3339Nano)
	var sessions core.SnapshotMap[string, core.SessionRecord]
	for index, spec := range []struct {
		id, title, profile string
		harness            core.HarnessKind
		target             string
	}{
		{docsScreenshotConversationID, docsScreenshotConversationTitle, "codex-1", core.HarnessKindCodex, "podman"},
		{"recovery-audit", "Audit checkpoint recovery", "claude-1", core.HarnessKindClaude, "podman"},
		{"target-guides", "Expand the target guides", "kimi-1", core.HarnessKindKimi, "podman"},
	} {
		session := runningSession()
		session.ID = spec.id
		session.Title = spec.title
		session.ACPSessionTitle = nil
		title := spec.title
		session.SessionTitleOverride = &title
		session.HarnessKind = spec.harness
		session.LastProfile = spec.profile
		session.BundleID = "mjolnir"
		session.TargetTemplateID = spec.target
		native := fmt.Sprintf("native-%d", index)
		session.NativeSessionID = &native
		session.CreatedAt = now
		session.UpdatedAt = now
		session.WorkspaceID = "default"
		session.State = core.SessionStateRunning
		if index == 2 {
			session.WorkspaceID = "maintenance"
			session.State = core.SessionStateStopped
		}
		sessions.Insert(session.ID, session)
	}

	refreshed := docsScreenshotsNowEpochSeconds()
	api := "API"
	quotas := map[string]client.ProfileQuota{
		"claude-1": profileQuota("claude-1", core.HarnessKindClaude, 73, 84, refreshed),
		"codex-1":  profileQuota("codex-1", core.HarnessKindCodex, 61, 92, refreshed),
		"codex-2":  profileQuota("codex-2", core.HarnessKindCodex, 38, 77, refreshed),
		"kimi-1": {
			ProfileID:               "kimi-1",
			Harness:                 core.HarnessKindKimi,
			Windows:                 []client.QuotaWindow{},
			Extra:                   &api,
			RefreshedAtEpochSeconds: refreshed,
		},
	}
	dashboard := NewDashboardState(configured, core.StateState{
		Version:  core.StateVersion,
		Sessions: sessions,
	}, quotas)
	dashboard.SetWorkspaceName("Mjolnir")
	dashboard.SetWorkspaceNames(map[string]string{
		"default":     "Mjolnir",
		"maintenance": "Maintenance",
	})
	dashboard.SelectActiveSession(docsScreenshotConversationID)
	current := docsScreenshotConversationID
	dashboard.SetCurrentSession(&current)
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeStandard)
	dashboard.focus = FocusPrompt

	conversation := documentationConversation()
	dashboard.ApplyMaterializedSession(&conversation)
	applyDocumentationTranscript(
		dashboard,
		"recovery-audit",
		[]*core.TranscriptItem{agentMessage(1, "Checkpoint verification and fresh-target resume paths are covered.")},
		true,
		refreshed,
	)
	applyDocumentationTranscript(
		dashboard,
		"target-guides",
		[]*core.TranscriptItem{agentMessage(1, "Podman, Docker, SSH, Apple container, and EC2 guides are linked.")},
		false,
		refreshed,
	)

	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{{
		ID:        "local",
		Host:      "local",
		TargetIDs: []string{"podman"},
		Kind:      core.DeploymentCapacityKindHost,
		Local:     true,
	}})
	cpu := uint8(27)
	disk := uint64(1_000 * 1024 * 1024 * 1024)
	dashboard.applyDeploymentCapacity(
		"local",
		&core.DeploymentCapacityUsage{
			CPUPercent:       &cpu,
			MemoryUsedBytes:  19 * 1024 * 1024 * 1024,
			MemoryTotalBytes: 64 * 1024 * 1024 * 1024,
			LogicalCores:     16,
			DiskTotalBytes:   &disk,
		},
		nil,
		refreshed,
	)
	return dashboard
}

func documentationConversation() core.MaterializedSession {
	userContent, err := json.Marshal(map[string]any{
		"type": "text",
		"text": "Refresh the terminal styling while keeping the current panels.",
	})
	if err != nil {
		panic(err)
	}
	toolCall, err := json.Marshal(map[string]any{
		"toolCallId": "inspect-terminal",
		"title":      "Inspect terminal layout and shared styles",
		"kind":       "read",
		"status":     "completed",
		"content":    []any{},
	})
	if err != nil {
		panic(err)
	}
	conversation := materializedSessionFor(docsScreenshotConversationID, []*core.TranscriptItem{
		transcriptItem(1, core.TranscriptBodyUser{Content: []json.RawMessage{userContent}}),
		agentMessage(2, "I'll give the navigation, conversation, and prompt a consistent visual treatment."),
		transcriptItem(3, core.TranscriptBodyTool{
			Call:            core.OpaqueToolCallFromValue(toolCall),
			TerminalOutputs: nil,
			TerminalRefs:    nil,
			Presentation:    nil,
		}),
		agentMessage(4, "**The workspace is now easier to scan.**\n\n"+
			"- Quiet graphite panels with mint focus accents\n"+
			"- Clear author labels and readable conversation spacing\n"+
			"- Compact session details and a distinct prompt\n\n"+
			"The shared styles live in `mj-chat/src/theme.rs`; session behavior is unchanged."),
	})
	conversation.Execution = core.MaterializedExecutionStateIdle{}
	lastActivity := int64(docsScreenshotsNowEpochSeconds())*1_000 - 3_000
	conversation.LastActivityAtMs = &lastActivity
	return conversation
}

func applyDocumentationTranscript(dashboard *DashboardState, sessionID string, transcript []*core.TranscriptItem, running bool, nowEpochSeconds uint64) {
	nowMs := int64(nowEpochSeconds) * 1_000
	if nowEpochSeconds > math.MaxInt64 {
		nowMs = math.MaxInt64 / 1_000 * 1_000
	}
	materialized := materializedSessionFor(sessionID, transcript)
	if running {
		materialized.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: nowMs - 74_000}
	} else {
		materialized.Execution = core.MaterializedExecutionStateIdle{}
	}
	lastActivity := nowMs - 3_000
	materialized.LastActivityAtMs = &lastActivity
	dashboard.ApplyMaterializedSession(&materialized)
}

func profileQuota(profileID string, harness core.HarnessKind, weeklyRemaining, fiveHourRemaining uint8, refreshed uint64) client.ProfileQuota {
	weeklyReset, fiveHourReset := "4d", "2h 14m"
	return client.ProfileQuota{
		ProfileID: profileID,
		Harness:   harness,
		Windows: []client.QuotaWindow{
			{Label: "Weekly", RemainingPercent: &weeklyRemaining, Resets: &weeklyReset},
			{Label: "5h", RemainingPercent: &fiveHourRemaining, Resets: &fiveHourReset},
		},
		RefreshedAtEpochSeconds: refreshed,
	}
}

// docsScreenshotsNowEpochSeconds is the module's now_epoch_seconds.
func docsScreenshotsNowEpochSeconds() uint64 {
	return uint64(max(time.Now().Unix(), 0))
}

// docsScreenshotsCapture is the module's capture: it draws dashboard with an
// attached documentation conversation and writes the SVG to path.
func docsScreenshotsCapture(path, title, description string, dashboard *DashboardState) {
	// The real combined renderer needs an attached chat as well as
	// session-list state. This in-memory backend cannot contact a daemon or
	// live session.
	fixture := client.NewReplacementSessionTestFixture(docsScreenshotConversationID, 4)
	active := chat.ActiveChatOpen(
		fixture.Stopped,
		"mjolnir",
		nil,
		fixture.Control,
		chat.SessionHeaderIdentity{},
		"",
		chat.NewNotices(),
	)
	conversation := documentationConversation()
	active.ChatState = chat.ChatStateFromMaterialized(&conversation, nil, nil)
	active.SetHeaderSummary("podman", "codex-1", docsScreenshotConversationTitle)
	active.SetHarnessKind(core.HarnessKindCodex)
	active.SetDraft("Check the narrow-terminal layout and the light theme next.")
	chats := map[string]*chat.ActiveChat{docsScreenshotConversationID: active}
	buffer := uv.NewBuffer(docsScreenshotColumns, docsScreenshotRows)
	renderCombinedForTest(chat.NewFrame(buffer), dashboard, chats, map[PaneId]string{}, false)
	if _, plain := dashboard.mode.(ModeDashboard); plain {
		visible := strings.Join(bufferLines(buffer), "\n")
		for _, content := range []string{
			"Refresh the terminal styling",
			"The workspace is now easier to scan.",
			"Check the narrow-terminal layout",
		} {
			if !strings.Contains(visible, content) {
				panic(fmt.Sprintf("capture must display %s:\n%s", content, visible))
			}
		}
	}
	svg := bufferSVG(buffer, title, description)
	if err := os.WriteFile(path, []byte(svg), 0o644); err != nil {
		panic(fmt.Sprintf("write %s: %v", path, err))
	}
}

func bufferSVG(buffer *uv.Buffer, title, description string) string {
	area := buffer.Bounds()
	width := area.Dx()*docsScreenshotCellWidth + docsScreenshotPadding*2
	height := area.Dy()*docsScreenshotCellHeight + docsScreenshotPadding*2
	var svg strings.Builder
	fmt.Fprintf(&svg, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"%d\" height=\"%d\" viewBox=\"0 0 %d %d\" role=\"img\" aria-labelledby=\"title description\">\n", width, height, width, height)
	fmt.Fprintf(&svg, "  <title id=\"title\">%s</title>\n", xmlEscape(title))
	fmt.Fprintf(&svg, "  <desc id=\"description\">%s</desc>\n", xmlEscape(description))
	fmt.Fprintf(&svg, "  <rect width=\"100%%\" height=\"100%%\" rx=\"8\" fill=\"%s\"/>\n", docsScreenshotTerminalBackground)
	svg.WriteString("  <g font-family=\"Source Code Pro, JetBrains Mono, Menlo, Consolas, monospace\" font-size=\"15\" font-variant-ligatures=\"none\">\n")

	cellStyle := func(x, y int) (chat.Style, string) {
		cell := buffer.CellAt(x, y)
		if cell == nil {
			return chat.Style{Fg: chat.ColorReset, Bg: chat.ColorReset}, " "
		}
		symbol := cell.Content
		if symbol == "" {
			symbol = " "
		}
		return chat.CellStyle(cell), symbol
	}
	for y := area.Min.Y; y < area.Max.Y; y++ {
		backgroundAt := func(x int) chat.Color {
			style, _ := cellStyle(x, y)
			if style.AddModifier.Contains(chat.ModifierReversed) {
				return style.Fg
			}
			return style.Bg
		}
		start := area.Min.X
		for start < area.Max.X {
			background := backgroundAt(start)
			end := start + 1
			for end < area.Max.X && backgroundAt(end) == background {
				end++
			}
			if background != chat.ColorReset {
				fmt.Fprintf(&svg, "    <rect x=\"%d\" y=\"%d\" width=\"%d\" height=\"%d\" fill=\"%s\"/>\n",
					docsScreenshotPadding+(start-area.Min.X)*docsScreenshotCellWidth,
					docsScreenshotPadding+(y-area.Min.Y)*docsScreenshotCellHeight,
					(end-start)*docsScreenshotCellWidth,
					docsScreenshotCellHeight,
					colorHex(background, docsScreenshotTerminalBackground),
				)
			}
			start = end
		}
		for x := area.Min.X; x < area.Max.X; x++ {
			style, symbol := cellStyle(x, y)
			reversed := style.AddModifier.Contains(chat.ModifierReversed)
			foreground := style.Fg
			if reversed {
				foreground = style.Bg
			}
			drawX := docsScreenshotPadding + (x-area.Min.X)*docsScreenshotCellWidth
			drawY := docsScreenshotPadding + (y-area.Min.Y)*docsScreenshotCellHeight
			if strings.TrimSpace(symbol) == "" {
				continue
			}
			var attributes strings.Builder
			if style.AddModifier.Contains(chat.ModifierBold) {
				attributes.WriteString(" font-weight=\"700\"")
			}
			if style.AddModifier.Contains(chat.ModifierDim) {
				attributes.WriteString(" opacity=\"0.62\"")
			}
			if style.AddModifier.Contains(chat.ModifierItalic) {
				attributes.WriteString(" font-style=\"italic\"")
			}
			if style.AddModifier.Contains(chat.ModifierUnderlined) {
				attributes.WriteString(" text-decoration=\"underline\"")
			}
			fmt.Fprintf(&svg, "    <text x=\"%d\" y=\"%d\" fill=\"%s\"%s>%s</text>\n",
				drawX, drawY+14, colorHex(foreground, "#efede8"), attributes.String(), xmlEscape(symbol))
		}
	}
	svg.WriteString("  </g>\n</svg>\n")
	return svg.String()
}

// colorHex reproduces the terminal's rendered colors exactly, including the
// standard indexed color cube; this encoder does not choose live UI colors.
func colorHex(color chat.Color, fallback string) string {
	if red, green, blue, ok := color.RGB(); ok {
		return fmt.Sprintf("#%02x%02x%02x", red, green, blue)
	}
	if index, ok := color.Index(); ok {
		switch {
		case index <= 15:
			named := []chat.Color{
				chat.ColorBlack, chat.ColorRed, chat.ColorGreen, chat.ColorYellow,
				chat.ColorBlue, chat.ColorMagenta, chat.ColorCyan, chat.ColorGray,
				chat.ColorDarkGray, chat.ColorLightRed, chat.ColorLightGreen, chat.ColorLightYellow,
				chat.ColorLightBlue, chat.ColorLightMagenta, chat.ColorLightCyan, chat.ColorWhite,
			}
			return colorHex(named[index], fallback)
		case index <= 231:
			cube := index - 16
			level := func(value uint8) uint8 {
				if value == 0 {
					return 0
				}
				return 55 + value*40
			}
			return colorHex(chat.ColorRGB(level(cube/36), level((cube/6)%6), level(cube%6)), fallback)
		default:
			gray := 8 + (index-232)*10
			return colorHex(chat.ColorRGB(gray, gray, gray), fallback)
		}
	}
	switch color {
	case chat.ColorBlack:
		return "#09070e"
	case chat.ColorRed:
		return "#ff6b6b"
	case chat.ColorGreen:
		return "#64d98b"
	case chat.ColorYellow:
		return "#f2c94c"
	case chat.ColorBlue:
		return "#69a7ff"
	case chat.ColorMagenta:
		return "#c792ff"
	case chat.ColorCyan:
		return "#70d7e8"
	case chat.ColorGray:
		return "#b8adc9"
	case chat.ColorDarkGray:
		return "#71677f"
	case chat.ColorLightRed:
		return "#ff9292"
	case chat.ColorLightGreen:
		return "#8ee8aa"
	case chat.ColorLightYellow:
		return "#ffe184"
	case chat.ColorLightBlue:
		return "#9bc4ff"
	case chat.ColorLightMagenta:
		return "#d9b5ff"
	case chat.ColorLightCyan:
		return "#a5ecf5"
	case chat.ColorWhite:
		return "#f4f0fa"
	}
	// Reset, and a color the cell does not set.
	return fallback
}

var xmlEscaper = strings.NewReplacer("&", "&amp;", "<", "&lt;", ">", "&gt;", "\"", "&quot;", "'", "&apos;")

func xmlEscape(value string) string {
	return xmlEscaper.Replace(value)
}
