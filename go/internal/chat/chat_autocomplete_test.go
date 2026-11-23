// Port of mj-chat/src/chat/autocomplete.rs (test-only items and tests).

package chat

import (
	"encoding/json"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (c *ChatState) listsCommand(name string) bool {
	for _, command := range c.commandChoices {
		if command.name == name {
			return true
		}
	}
	return false
}

func TestTabAcceptsAnOpenCompletionBeforeItCyclesFocus(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.HandleKey(key('/'))
	chat.HandleKey(key('h'))
	if chat.autocomplete == nil {
		t.Fatal("the popup is not open")
	}

	if action := chat.HandleKey(key(tea.KeyTab)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.autocomplete != nil {
		t.Fatal("the popup was not accepted")
	}
	if chat.input != "/help " {
		t.Fatalf("input = %q", chat.input)
	}

	// With nothing to complete, the same key is the handle on the next pane.
	if action := chat.HandleKey(key(tea.KeyTab)); !actionEquals(action, ChatActionCycleFocus{Reverse: false}) {
		t.Fatalf("action = %#v", action)
	}
	if action := chat.HandleKey(tea.KeyPressMsg{Code: tea.KeyTab, Mod: tea.ModShift}); !actionEquals(action, ChatActionCycleFocus{Reverse: true}) {
		t.Fatalf("action = %#v", action)
	}
}

func TestCommandUpdatesReplaceStaleAdapterCapabilities(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	for _, availableCommands := range []string{
		`[{"name": "plan", "description": "toggle plan mode"}, {"name": "goal", "description": "set a persistent goal"}]`,
		`[{"name": "plan", "description": "toggle plan mode"}]`,
	} {
		chat.applySessionUpdate(1, json.RawMessage(`{"sessionUpdate": "available_commands_update", "availableCommands": `+availableCommands+`}`))
	}

	if slices.ContainsFunc(chat.commandChoices, func(command commandChoice) bool { return command.name == "plan" }) {
		t.Fatal("a stale plan command is listed")
	}
	if slices.ContainsFunc(chat.commandChoices, func(command commandChoice) bool { return command.name == "goal" }) {
		t.Fatal("a stale goal command is listed")
	}
}

func TestExactConfigValueIsReadyToSubmitDuringSessionRefreshes(t *testing.T) {
	options := []schema.SessionConfigOption{
		withCategory(pickerSelectOption("effort", "Effort", "high", [][2]string{
			{"high", "Thinking High"},
			{"max", "Thinking Max"},
		}), schema.SessionConfigOptionCategoryThoughtLevel),
	}
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.setConfigOptions(options)
	chat.setInput("/effort ma")

	if chat.autocomplete == nil {
		t.Fatal("no completion offered")
	}
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.input != "/effort max" {
		t.Fatalf("input = %q", chat.input)
	}
	if chat.autocomplete != nil {
		t.Fatal("the completion stayed open")
	}

	// A running session can publish another snapshot between key presses.
	chat.setConfigOptions(options)
	if chat.autocomplete != nil {
		t.Fatal("a refresh reopened the completion")
	}
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionSetConfig{Key: "effort", Value: "max"}) {
		t.Fatalf("action = %#v", action)
	}
}

func TestConfigCommandCompletesArbitraryOptionIDsAndValues(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.setConfigOptions([]schema.SessionConfigOption{pickerSelectOption("approval_mode", "Approval mode", "prompt", [][2]string{
		{"prompt", "Ask first"},
		{"allow", "Allow without asking"},
	})})
	chat.setInput("/config app")
	if chat.autocomplete == nil {
		t.Fatal("no completion offered")
	}
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.input != "/config approval_mode " {
		t.Fatalf("input = %q", chat.input)
	}

	chat.setInput("/config approval_mode allow")
	if chat.autocomplete != nil {
		t.Fatal("an exact value offered a completion")
	}
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionSetConfig{Key: "approval_mode", Value: "allow"}) {
		t.Fatalf("action = %#v", action)
	}
}

func goalChat(actions []string) *ChatState {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	advertise(chat, 1, []string{"goal"})
	encodedActions, err := json.Marshal(actions)
	if err != nil {
		panic(err)
	}
	chat.applySessionUpdate(2, json.RawMessage(`{
		"sessionUpdate":"session_info_update", "_meta": {
			"mjGoalCapability":{"version":1,"controlMethod":"_session/goal","actions":`+string(encodedActions)+`},
			"goal":{"objective":"finish","status":"active"}
		}
	}`))
	return chat
}

func TestGoalControlKeepsAttachedDraftsAndBypassesPendingPlanTransition(t *testing.T) {
	chat := goalChat([]string{"clear"})
	chat.setPromptImagesSupported(true)
	chat.setInput("/goal clear ")
	if !chat.reserveAttachment(1) {
		t.Fatal("the attachment was not reserved")
	}
	draft := chat.input
	if action := chat.submitInput(); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.input != draft || len(chat.inputImages) != 1 {
		t.Fatalf("input = %q with %d images", chat.input, len(chat.inputImages))
	}
	if notice, _ := chat.feedback.Current(); !strings.Contains(notice, "delete its marker") {
		t.Fatalf("notice = %q", notice)
	}
	chat = goalChat([]string{"clear"})
	chat.planCommandPending = true
	chat.input = "/goal clear"
	if action := chat.submitInput(); !actionEquals(action, ChatActionGoalControl{Action: core.GoalControlActionClear}) {
		t.Fatalf("action = %#v", action)
	}
}
