// Port of mj-chat/src/chat/config_picker.rs (test-only items and tests).

package chat

import (
	"fmt"
	"reflect"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (p *configPicker) selectedValue() (string, bool) {
	value, ok := p.selection().(configPickerItemValue)
	if !ok {
		return "", false
	}
	return value.choice.Value, true
}

const configPickerGoldenWidth = 100
const configPickerGoldenHeight = 24

// pickerSelectOption is `SessionConfigOption::select` over ungrouped
// (value, name) pairs.
func pickerSelectOption(id, name, current string, values [][2]string) schema.SessionConfigOption {
	options := make([]schema.SessionConfigSelectOption, 0, len(values))
	for _, value := range values {
		options = append(options, schema.SessionConfigSelectOption{
			Value: schema.SessionConfigValueId(value[0]),
			Name:  value[1],
		})
	}
	return schema.SessionConfigOption{
		Kind: schema.SessionConfigOptionKindSelect,
		ID:   schema.SessionConfigId(id),
		Name: name,
		Select: &schema.SessionConfigSelect{
			CurrentValue: schema.SessionConfigValueId(current),
			Options:      options,
		},
	}
}

func withCategory(option schema.SessionConfigOption, category schema.SessionConfigOptionCategory) schema.SessionConfigOption {
	option.Category = &category
	return option
}

func sameValues(values []string) [][2]string {
	pairs := make([][2]string, 0, len(values))
	for _, value := range values {
		pairs = append(pairs, [2]string{value, value})
	}
	return pairs
}

func pickerModelOption(current string, values [][2]string) schema.SessionConfigOption {
	return withCategory(pickerSelectOption("model", "Model", current, values), schema.SessionConfigOptionCategoryModel)
}

func pickerEffortOption(current string, values []string) schema.SessionConfigOption {
	return withCategory(pickerSelectOption("effort", "Effort", current, sameValues(values)),
		schema.SessionConfigOptionCategoryThoughtLevel)
}

func pickerApprovalOption(current string, values []string) schema.SessionConfigOption {
	return pickerSelectOption("approval_mode", "Approval mode", current, sameValues(values))
}

func pickerModeOption(current string, values []string) schema.SessionConfigOption {
	return pickerSelectOption("mode", "Permission mode", current, sameValues(values))
}

func chatWithModels() *ChatState {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.setConfigOptions([]schema.SessionConfigOption{
		pickerModelOption("gpt-5.6-luna", [][2]string{
			{"auto", "Auto"},
			{"gpt-5.6-luna", "Luna"},
			{"gpt-5.6-terra", "Terra"},
		}),
		pickerEffortOption("high", []string{"low", "medium", "high", "max"}),
	})
	return chat
}

// actionEquals compares chat actions as Rust's derived PartialEq does,
// including variants that hold slices.
func actionEquals(action, want ChatAction) bool {
	return reflect.DeepEqual(action, want)
}

// configPickerActionDebug is Rust's Debug for the actions these goldens
// record.
func configPickerActionDebug(action ChatAction) string {
	switch action := action.(type) {
	case ChatActionNone:
		return "None"
	case ChatActionSetConfig:
		return fmt.Sprintf("SetConfig { key: %s, value: %s }",
			core.RustDebugString(action.Key), core.RustDebugString(action.Value))
	}
	return fmt.Sprintf("%#v", action)
}

func selectedPickerValue(t *testing.T, chat *ChatState) (string, bool) {
	t.Helper()
	if chat.configPicker == nil {
		t.Fatal("no picker is open")
	}
	return chat.configPicker.selectedValue()
}

func TestASessionRefreshWhileOpenDoesNotMoveTheCursor(t *testing.T) {
	chat := chatWithModels()
	if !chat.openConfigPicker("model") {
		t.Fatal("picker did not open")
	}
	chat.HandleKey(key(tea.KeyDown))
	before, beforeOK := selectedPickerValue(t, chat)
	// The harness re-advertises a reordered list mid-selection.
	chat.setConfigOptions([]schema.SessionConfigOption{pickerModelOption("auto", [][2]string{
		{"gpt-5.6-terra", "Terra"}, {"auto", "Auto"},
	})})
	after, afterOK := selectedPickerValue(t, chat)
	if after != before || afterOK != beforeOK {
		t.Fatalf("selection moved from %q to %q", before, after)
	}
}

func TestConfigCommandReachesAnArbitraryAdvertisedSetting(t *testing.T) {
	chat := chatWithModels()
	chat.setConfigOptions([]schema.SessionConfigOption{
		pickerModelOption("auto", [][2]string{{"auto", "Auto"}}),
		pickerEffortOption("high", []string{"low", "high"}),
		pickerApprovalOption("prompt", []string{"prompt", "allow"}),
		pickerModeOption("default", []string{"default", "plan"}),
	})
	chat.SetPolicyConfigKeys([]string{"mode"})

	chat.setInput("/config")
	if action := chat.submitInput(); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if !chat.configPickerActive() || !chat.configPicker.dialog {
		t.Fatal("the settings dialog did not open")
	}
	if !strings.Contains(strings.Join(drawnTranscript(chat, 100, 24), "\n"), "Approval mode") {
		t.Fatal("Approval mode is not listed")
	}
	if strings.Contains(strings.Join(drawnTranscript(chat, 100, 24), "\n"), "Permission mode") {
		t.Fatal("the policy-owned setting is listed")
	}
	pickerTypeText(chat, "approval")
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if !chat.configPicker.dialog {
		t.Fatal("the value dialog is not a dialog")
	}
	if !strings.Contains(strings.Join(drawnTranscript(chat, 100, 24), "\n"), "allow") {
		t.Fatal("allow is not listed")
	}
	pickerTypeText(chat, "allow")
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionSetConfig{Key: "approval_mode", Value: "allow"}) {
		t.Fatalf("action = %#v", action)
	}

	chat.setInput("/config approval_mode")
	if action := chat.submitInput(); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if !chat.configPicker.dialog {
		t.Fatal("the value dialog is not a dialog")
	}
	chat.HandleKey(key(tea.KeyEscape))

	chat.setInput("/config approval_mode prompt")
	if action := chat.submitInput(); !actionEquals(action, ChatActionSetConfig{Key: "approval_mode", Value: "prompt"}) {
		t.Fatalf("action = %#v", action)
	}

	chat.setInput("/config mode plan")
	if action := chat.submitInput(); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if notice, ok := chat.Notice(); !ok || !strings.Contains(notice, "permission policy sets this") || !strings.Contains(notice, "/plan") {
		t.Fatalf("notice = %q", notice)
	}

	// Hard-won: 2d6176f: `/config mode` reached a policy selector named otherwise
	interaction := pickerModeOption("default", []string{"default", "plan"})
	interaction.ID = "interaction_mode"
	interaction = withCategory(interaction, schema.SessionConfigOptionCategoryMode)
	chat.setConfigOptions([]schema.SessionConfigOption{interaction})
	chat.SetPolicyConfigKeys([]string{"interaction_mode"})
	chat.setInput("/config mode plan")
	if action := chat.submitInput(); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if notice, ok := chat.Notice(); !ok || !strings.Contains(notice, "permission policy sets this") {
		t.Fatalf("notice = %q", notice)
	}
}

func TestConfigCommandRefusesValuesOutsideTheLiveCatalogue(t *testing.T) {
	chat := chatWithModels()
	chat.setConfigOptions([]schema.SessionConfigOption{pickerApprovalOption("prompt", []string{"prompt", "allow"})})
	chat.setInput("/config approval_mode bypass")

	if action := chat.submitInput(); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}
	if notice, ok := chat.Notice(); !ok || !strings.Contains(notice, "choices: prompt, allow") {
		t.Fatalf("notice = %q", notice)
	}
}

func TestClickingARowAppliesThatRowOnceOnRelease(t *testing.T) {
	chat := chatWithModels()
	chat.openConfigPicker("model")
	rows := drawnTranscript(chat, 100, 24)
	row := slices.IndexFunc(rows, func(row string) bool { return strings.Contains(row, "Terra") })
	if row < 0 {
		t.Fatal("Terra is not drawn")
	}
	column := chat.configPicker.area.Min.X + 2
	press := tea.MouseClickMsg{X: column, Y: row, Button: tea.MouseLeft}
	if action := chat.HandleMouse(press); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("press action = %#v", action)
	}
	// A redraw between press and release must preserve the gesture and
	// cursor.
	drawnTranscript(chat, 100, 24)
	release := tea.MouseReleaseMsg{X: column, Y: row, Button: tea.MouseLeft}
	if action := chat.HandleMouse(release); !actionEquals(action, ChatActionSetConfig{Key: "model", Value: "gpt-5.6-terra"}) {
		t.Fatalf("release action = %#v", action)
	}
	if chat.configPickerActive() {
		t.Fatal("the picker stayed open")
	}
	if action := chat.HandleMouse(release); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("second release action = %#v", action)
	}
}

func TestFilteringAfterNavigationHandlesEmptyResultsAndBackspace(t *testing.T) {
	chat := chatWithModels()
	chat.openConfigPicker("model")
	chat.HandleKey(key(tea.KeyDown))
	for _, character := range "autoz" {
		chat.HandleKey(key(character))
	}
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if !strings.Contains(strings.Join(drawnTranscript(chat, 100, 24), "\n"), "No matches") {
		t.Fatal("No matches is not drawn")
	}
	chat.HandleKey(key(tea.KeyBackspace))
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionSetConfig{Key: "model", Value: "auto"}) {
		t.Fatalf("action = %#v", action)
	}
}

func TestStaleChoicesAndShutdownDoNotSubmit(t *testing.T) {
	chat := chatWithModels()
	chat.openConfigPicker("model")
	chat.setConfigOptions([]schema.SessionConfigOption{pickerModelOption("auto", [][2]string{{"auto", "Auto"}})})
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if notice, _ := chat.Notice(); !strings.Contains(notice, "no longer advertises") {
		t.Fatalf("notice = %q", notice)
	}
	chat.openConfigPicker("model")
	chat.phase = core.WorkerPhaseClosing
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if notice, _ := chat.Notice(); !strings.Contains(notice, "closing") {
		t.Fatalf("notice = %q", notice)
	}
}

func TestLongListsScrollAndStayInsideNarrowPanes(t *testing.T) {
	chat := chatWithModels()
	choices := make([][2]string, 0, 24)
	for index := range 24 {
		name := fmt.Sprintf("model-%02d", index)
		choices = append(choices, [2]string{name, name})
	}
	chat.setConfigOptions([]schema.SessionConfigOption{pickerModelOption("model-00", choices)})
	chat.openConfigPicker("model")
	for range 23 {
		chat.HandleKey(key(tea.KeyDown))
	}
	for _, width := range []int{100, 30, 12} {
		rows := drawnTranscript(chat, width, 24)
		picker := chat.configPicker
		if picker.area.Max.X > width || picker.area.Max.Y > 24 || picker.area.Dy() > 10 {
			t.Fatalf("picker area %v at width %d", picker.area, width)
		}
		if !strings.Contains(strings.Join(rows, "\n"), "model-23") {
			t.Fatalf("model-23 is not drawn at width %d", width)
		}
		if value, ok := picker.selectedValue(); !ok || value != "model-23" {
			t.Fatalf("selected %q at width %d", value, width)
		}
	}
}

func TestOutsideClickOnSubagentsOnlyDismissesTheDropdown(t *testing.T) {
	chat := chatWithModels()
	chat.SetSubagentCount(1)
	drawnTranscript(chat, 100, 24)
	chat.openConfigPicker("model")
	drawnTranscript(chat, 100, 24)
	area := chat.subagentControlArea
	if area == nil {
		t.Fatal("no sub-agent control was drawn")
	}
	click := tea.MouseClickMsg{X: area.Min.X, Y: area.Min.Y, Button: tea.MouseLeft}
	if !chat.ComponentHandlesMouse(click) {
		t.Fatal("the component did not claim the click")
	}
	if action := chat.HandleMouse(click); !actionEquals(action, ChatActionNone{}) {
		t.Fatalf("action = %#v", action)
	}
	if chat.configPickerActive() {
		t.Fatal("the dropdown stayed open")
	}
}

func TestDropdownOwnsEnterEvenIfANavigationControlHadFocus(t *testing.T) {
	chat := chatWithModels()
	chat.SetSubagentCount(1)
	chat.focusSubagentControl()
	chat.openConfigPicker("model")
	if action := chat.HandleKey(key(tea.KeyEnter)); !actionEquals(action, ChatActionSetConfig{Key: "model", Value: "gpt-5.6-luna"}) {
		t.Fatalf("action = %#v", action)
	}
}

func TestUnavailableAndClosingConfigurationHasNoDropdownHitbox(t *testing.T) {
	chat := chatWithModels()
	chat.modelValues = nil
	drawnTranscript(chat, 100, 24)
	hasChip := func(key string) bool {
		return slices.ContainsFunc(chat.configChipAreas, func(chip configChipArea) bool { return chip.key == key })
	}
	if hasChip("model") {
		t.Fatal("a model chip was drawn without model values")
	}
	if !hasChip("effort") {
		t.Fatal("no effort chip was drawn")
	}
	chat.phase = core.WorkerPhaseClosed
	drawnTranscript(chat, 100, 24)
	if len(chat.configChipAreas) != 0 {
		t.Fatalf("chips drawn while closed: %v", chat.configChipAreas)
	}
}

func pickerTypeText(chat *ChatState, text string) {
	for _, character := range text {
		chat.HandleKey(key(character))
	}
}

func renderedChat(chat *ChatState) string {
	buffer := uv.NewBuffer(configPickerGoldenWidth, configPickerGoldenHeight)
	renderFullFrame(NewFrame(buffer), chat, false)
	return strings.Join(BufferLines(buffer), "\n")
}

func appendPickerState(output *strings.Builder, label, surface string, action ChatAction) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, configPickerGoldenWidth, configPickerGoldenHeight)
	output.WriteString(surface)
	output.WriteByte('\n')
	if action != nil {
		fmt.Fprintf(output, "action: %s\n", configPickerActionDebug(action))
	}
}

func TestGoldenConfigSelectionRender(t *testing.T) {
	var output strings.Builder

	worker := snapshot()
	chat := NewChatState(&worker, nil)
	primary := "Primary description is not a row"
	fallback := "Fallback description is not a row"
	option := withCategory(pickerSelectOption("model", "Model", "m-fallback", [][2]string{
		{"m-primary", "Model name"},
		{"m-fallback", "  "},
	}), schema.SessionConfigOptionCategoryModel)
	options := option.Select.Options.([]schema.SessionConfigSelectOption)
	options[0].Description = &primary
	options[1].Description = &fallback
	chat.setConfigOptions([]schema.SessionConfigOption{option})
	pickerTypeText(chat, "/model m")
	appendPickerState(&output, "autocomplete labels and blank-name fallback", renderedChat(chat), nil)

	chat = chatWithModels()
	pickerTypeText(chat, "/model lun")
	appendPickerState(&output, "advertised value autocomplete", renderedChat(chat), nil)
	chat.HandleKey(key(tea.KeyEnter))
	appendPickerState(&output, "accepted advertised value", renderedChat(chat), nil)

	chat = chatWithModels()
	renderedChat(chat)
	chip := slices.IndexFunc(chat.configChipAreas, func(chip configChipArea) bool { return chip.key == "model" })
	if chip < 0 {
		t.Fatal("rendered model title chip")
	}
	modelChip := chat.configChipAreas[chip].area
	click := tea.MouseClickMsg{X: modelChip.Min.X + modelChip.Dx()/2, Y: modelChip.Min.Y, Button: tea.MouseLeft}
	action := chat.HandleMouse(click)
	appendPickerState(&output, "prompt model chip opens selector", renderedChat(chat), action)

	clicked := chatWithModels()
	pickerTypeText(clicked, "/model")
	clicked.HandleKey(key(tea.KeyEnter))
	renderedChat(clicked)
	clicked.HandleMouse(tea.MouseClickMsg{X: 99, Y: 23, Button: tea.MouseLeft})
	clicked.HandleMouse(tea.MouseReleaseMsg{X: 99, Y: 23, Button: tea.MouseLeft})
	appendPickerState(&output, "outside click dismisses selector", renderedChat(clicked), nil)

	escaped := chatWithModels()
	pickerTypeText(escaped, "/model")
	escaped.HandleKey(key(tea.KeyEnter))
	escaped.HandleKey(key(tea.KeyEscape))
	appendPickerState(&output, "escape dismisses selector", renderedChat(escaped), nil)

	bareModel := chatWithModels()
	pickerTypeText(bareModel, "/model")
	bareModel.HandleKey(key(tea.KeyEnter))
	appendPickerState(&output, "bare model command selects current value", renderedChat(bareModel), nil)

	completedModel := chatWithModels()
	pickerTypeText(completedModel, "/mod")
	completedModel.HandleKey(key(tea.KeyEnter))
	completedModel.HandleKey(key(tea.KeyEnter))
	appendPickerState(&output, "completed bare command opens selector", renderedChat(completedModel), nil)

	filtered := chatWithModels()
	pickerTypeText(filtered, "/model")
	filtered.HandleKey(key(tea.KeyEnter))
	pickerTypeText(filtered, "terra")
	appendPickerState(&output, "filtered selector before applying", renderedChat(filtered), nil)
	action = filtered.HandleKey(key(tea.KeyEnter))
	appendPickerState(&output, "filtered selector applies selected value", renderedChat(filtered), action)

	effort := chatWithModels()
	pickerTypeText(effort, "/effort")
	effort.HandleKey(key(tea.KeyEnter))
	effort.HandleKey(key(tea.KeyDown))
	effort.HandleKey(key(tea.KeyDown))
	selectedEffort, ok := selectedPickerValue(t, effort)
	if !ok {
		t.Fatal("selected effort")
	}
	effortSurface := fmt.Sprintf("%s\nselection: %s", renderedChat(effort), selectedEffort)
	appendPickerState(&output, "effort selection stops at final value", effortSurface, nil)
	effort.HandleKey(key(tea.KeyEscape))
	appendPickerState(&output, "effort escape closes without changing current value", renderedChat(effort), nil)

	worker = snapshot()
	unavailable := NewChatState(&worker, nil)
	pickerTypeText(unavailable, "/model")
	unavailable.HandleKey(key(tea.KeyEnter))
	appendPickerState(&output, "bare model command without advertised values", renderedChat(unavailable), nil)

	anchored := chatWithModels()
	pickerTypeText(anchored, "/model")
	anchored.HandleKey(key(tea.KeyEnter))
	appendPickerState(&output, "selector anchored at prompt chip", renderedChat(anchored), nil)

	for _, theme := range core.UiThemeAll {
		var surface string
		WithTheme(theme, func() {
			WithSymbols(core.SymbolSetAscii, func() {
				chat := chatWithModels()
				chat.SetSubagentCount(1)
				pickerTypeText(chat, "/model")
				chat.HandleKey(key(tea.KeyEnter))
				surface = renderedChat(chat)
			})
		})
		appendPickerState(&output, fmt.Sprintf("ASCII selector in %s theme", theme.Label()), surface, nil)
	}

	core.AssertPlatformGolden(t, "testdata", "config-selection-render", output.String())
}
