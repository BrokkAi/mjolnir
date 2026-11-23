// Port of mj-chat/src/chat/test_support.rs.

// Shared fixtures for the chat module's tests.

package chat

import (
	"encoding/json"
	"strings"
	"unicode"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func snapshot() core.WorkerSnapshot {
	var value core.WorkerSnapshot
	err := json.Unmarshal([]byte(`{
		"session_id": "1234567890",
		"phase": "idle",
		"latest_seq": 0,
		"last_checkpoint_seq": null,
		"active_prompt": null,
		"config": {},
		"handled_requests": {}
	}`), &value)
	if err != nil {
		panic(err)
	}
	return value
}

// key is a press of code with no modifiers held. code is a character or one
// of Bubble Tea's key codes (tea.KeyEnter, tea.KeyUp, ...); characters carry
// their text as a terminal reports them.
func key(code rune) tea.KeyPressMsg {
	press := tea.KeyPressMsg{Code: code}
	if code < tea.KeyExtended && unicode.IsPrint(code) {
		press.Text = string(code)
	}
	return press
}

func ctrl(character rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl}
}

// alt is an Alt chord. Alt is one modifier everywhere, so it never goes
// through ctrl.
func alt(character rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: character, Mod: tea.ModAlt}
}

// mouseIn places a mouse event of kind (a tea.MouseWheelMsg, MouseClickMsg,
// ... carrying its button) over area, which is what decides where it
// scrolls.
func mouseIn(kind tea.MouseMsg, area uv.Rectangle) tea.MouseMsg {
	mouse := kind.Mouse()
	mouse.X = area.Min.X + 1
	mouse.Y = area.Min.Y + 1
	mouse.Mod = 0
	switch kind.(type) {
	case tea.MouseClickMsg:
		return tea.MouseClickMsg(mouse)
	case tea.MouseReleaseMsg:
		return tea.MouseReleaseMsg(mouse)
	case tea.MouseWheelMsg:
		return tea.MouseWheelMsg(mouse)
	default:
		return tea.MouseMotionMsg(mouse)
	}
}

func queued(id, text string) QueuedPrompt {
	return QueuedPrompt{
		id:                     id,
		text:                   text,
		kind:                   core.QueuedCommandKindPrompt{},
		images:                 []PromptImage{},
		attachmentsUnsupported: false,
	}
}

func transcriptText(chat *ChatState, width int) []string {
	return lineText(transcriptLines(chat, width))
}

func lineText(lines []Line) []string {
	texts := make([]string, 0, len(lines))
	for _, line := range lines {
		var text strings.Builder
		for _, span := range line.Spans {
			text.WriteString(span.Content)
		}
		texts = append(texts, text.String())
	}
	return texts
}

// drawnTranscript draws the chat and returns the transcript rows currently
// on screen.
func drawnTranscript(chat *ChatState, width, height int) []string {
	return drawnTranscriptSelecting(chat, width, height, false)
}

// drawnTranscriptSelecting is like drawnTranscript, but says whether the
// selection engine still owns a transcript selection, which is what freezes
// the transcript's row space.
func drawnTranscriptSelecting(chat *ChatState, width, height int, transcriptSelected bool) []string {
	buffer := uv.NewBuffer(width, height)
	renderFullFrame(NewFrame(buffer), chat, transcriptSelected)
	area := buffer.Bounds()
	rows := make([]string, 0, area.Dy())
	for y := area.Min.Y; y < area.Max.Y; y++ {
		var row strings.Builder
		for x := area.Min.X; x < area.Max.X; x++ {
			// ratatui resets the cells a wide grapheme covers to a space;
			// ultraviolet leaves them empty.
			if cell := buffer.CellAt(x, y); cell != nil && cell.Content != "" {
				row.WriteString(cell.Content)
			} else {
				row.WriteString(" ")
			}
		}
		rows = append(rows, strings.TrimRightFunc(row.String(), unicode.IsSpace))
	}
	return rows
}

func agentTranscriptItem(stableID string, ordinal uint64) *core.TranscriptItem {
	return agentMessageItem(stableID, ordinal, "hello")
}

func agentMessageItem(stableID string, ordinal uint64, text string) *core.TranscriptItem {
	chunk, err := json.Marshal(map[string]any{"content": map[string]any{"type": "text", "text": text}})
	if err != nil {
		panic(err)
	}
	return &core.TranscriptItem{
		StableID:                  stableID,
		Position:                  ordinal,
		LatestContentEventOrdinal: &ordinal,
		Body:                      core.TranscriptBodyAgent{Chunks: []json.RawMessage{chunk}, Streaming: false},
	}
}

func grokChat() *ChatState {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.SetSessionModes(&schema.SessionModeState{
		CurrentModeID: "default",
		AvailableModes: []schema.SessionMode{
			{ID: "default", Name: "Default"},
			{ID: "plan", Name: "Plan"},
		},
	})
	return chat
}

func modeConfigOption(current string, values []string) schema.SessionConfigOption {
	option := selectConfigOption("interaction_mode", current, values)
	category := schema.SessionConfigOptionCategoryMode
	option.Category = &category
	return option
}

func fastModeOption(current string) schema.SessionConfigOption {
	option := selectConfigOption("fast-mode", current, []string{"off", "on"})
	option.Name = "Fast mode"
	return option
}

func selectConfigOption(id, current string, values []string) schema.SessionConfigOption {
	options := make([]schema.SessionConfigSelectOption, 0, len(values))
	for _, value := range values {
		options = append(options, schema.SessionConfigSelectOption{
			Value: schema.SessionConfigValueId(value),
			Name:  value,
		})
	}
	return schema.SessionConfigOption{
		Kind: schema.SessionConfigOptionKindSelect,
		ID:   schema.SessionConfigId(id),
		Name: "Mode",
		Select: &schema.SessionConfigSelect{
			CurrentValue: schema.SessionConfigValueId(current),
			Options:      options,
		},
	}
}

func advertise(chat *ChatState, seq uint64, names []string) {
	commands := make([]map[string]string, 0, len(names))
	for _, name := range names {
		commands = append(commands, map[string]string{"name": name, "description": "d"})
	}
	update, err := json.Marshal(map[string]any{
		"sessionUpdate":     "available_commands_update",
		"availableCommands": commands,
	})
	if err != nil {
		panic(err)
	}
	chat.applySessionUpdate(seq, update)
}
