// Port of mj-chat/src/chat/config_picker.rs.

// Compact, anchored session-setting and value dropdowns with type-to-filter.

package chat

import (
	"slices"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type configControl int

const (
	configControlFilter configControl = iota
	configControlValues
)

// configPicker snapshots its choices so a refresh cannot reorder the list
// under the cursor. Form owns the cursor and pointer gesture; live values are
// checked before applying the choice.
type configPicker struct {
	title    string
	key      *string
	current  *string
	dialog   bool
	items    []configPickerItem
	filter   TextInput
	form     Form[configControl]
	filtered []int
	area     uv.Rectangle
}

type configPickerItem interface {
	isConfigPickerItem()
}

type configPickerItemOption struct {
	key     string
	label   string
	current *string
}

type configPickerItemValue struct {
	key    string
	choice core.SessionConfigChoice
}

func (configPickerItemOption) isConfigPickerItem() {}
func (configPickerItemValue) isConfigPickerItem()  {}

// configPickerNewOptions is the session-settings menu over options, or nil
// when no option offers choices.
func configPickerNewOptions(options []schema.SessionConfigOption, dialog bool) *configPicker {
	panic("unported: mj-chat/src/chat/config_picker.rs configPickerNewOptions")
}

func configPickerNewValues(key string, choices []core.SessionConfigChoice, current *string, dialog bool) configPicker {
	items := make([]configPickerItem, 0, len(choices))
	for _, choice := range choices {
		items = append(items, configPickerItemValue{key: key, choice: choice})
	}
	ownedKey := key
	return newConfigPicker(key, &ownedKey, current, dialog, items)
}

func newConfigPicker(title string, key *string, current *string, dialog bool, items []configPickerItem) configPicker {
	panic("unported: mj-chat/src/chat/config_picker.rs newConfigPicker")
}

func (p *configPicker) selected() int {
	panic("unported: mj-chat/src/chat/config_picker.rs selected")
}

// selection is the item under the cursor, or nil when the filter matches
// nothing.
func (p *configPicker) selection() configPickerItem {
	selected := p.selected()
	if selected < 0 || selected >= len(p.filtered) {
		return nil
	}
	index := p.filtered[selected]
	if index < 0 || index >= len(p.items) {
		return nil
	}
	return p.items[index]
}

func (p *configPicker) refilter() {
	panic("unported: mj-chat/src/chat/config_picker.rs refilter")
}

func (p *configPicker) rows() []Line {
	panic("unported: mj-chat/src/chat/config_picker.rs rows")
}

// displayTitle is Rust's `ConfigPicker::title()`, renamed because Go cannot
// give a method the name of the title field.
func (p *configPicker) displayTitle() string {
	if p.filter.IsEmpty() {
		return p.title
	}
	return p.title + ": " + p.filter.Value()
}

func configOptionKey(option *schema.SessionConfigOption) string {
	id := string(option.ID)
	if option.Category != nil {
		switch *option.Category {
		case schema.SessionConfigOptionCategoryModel:
			return "model"
		case schema.SessionConfigOptionCategoryThoughtLevel:
			return "effort"
		}
	}
	if id == "model" {
		return "model"
	}
	if slices.Contains(core.EffortOptionIDs[:], id) {
		return "effort"
	}
	return id
}

// SetPolicyConfigKeys supplies the policy-owned setting keys for this chat's
// session.
func (c *ChatState) SetPolicyConfigKeys(keys []string) {
	panic("unported: mj-chat/src/chat/config_picker.rs SetPolicyConfigKeys")
}

func (c *ChatState) configKeyUserSettable(key string) bool {
	panic("unported: mj-chat/src/chat/config_picker.rs configKeyUserSettable")
}

func (c *ChatState) policyConfigNotice() string {
	return "The target's permission policy sets this setting; use /plan to switch to Plan mode"
}

func (c *ChatState) advertisedConfigValues(key string) []core.SessionConfigChoice {
	switch key {
	case "model":
		return c.modelValues
	case "effort":
		return c.effortValues
	default:
		return nil
	}
}

func (c *ChatState) configValues(key string) []core.SessionConfigChoice {
	if !c.configKeyUserSettable(key) {
		return nil
	}
	return core.SessionConfigChoices(c.acpSurface.ConfigOptions(), key)
}

func (c *ChatState) currentConfigValue(key string) *string {
	panic("unported: mj-chat/src/chat/config_picker.rs currentConfigValue")
}

func (c *ChatState) openConfigMenu() bool {
	panic("unported: mj-chat/src/chat/config_picker.rs openConfigMenu")
}

func (c *ChatState) openConfigPicker(key string) bool {
	return c.openConfigPickerWithPresentation(key, false)
}

func (c *ChatState) openConfigValueDialog(key string) bool {
	return c.openConfigPickerWithPresentation(key, true)
}

func (c *ChatState) openConfigPickerWithPresentation(key string, dialog bool) bool {
	choices := c.configValues(key)
	if len(choices) == 0 {
		return false
	}
	current := c.currentConfigValue(key)
	picker := configPickerNewValues(key, choices, current, dialog)
	c.configPicker = &picker
	return true
}

func (c *ChatState) configPickerActive() bool {
	return c.configPicker != nil
}

// promptConfigChipAt is the config key whose prompt chip covers the cell, or
// false.
func (c *ChatState) promptConfigChipAt(column int, row int) (string, bool) {
	panic("unported: mj-chat/src/chat/config_picker.rs promptConfigChipAt")
}

func (c *ChatState) openPromptConfigPicker(key string) {
	panic("unported: mj-chat/src/chat/config_picker.rs openPromptConfigPicker")
}

func (c *ChatState) configPickerPointerCaptured() bool {
	panic("unported: mj-chat/src/chat/config_picker.rs configPickerPointerCaptured")
}

func (c *ChatState) cancelConfigPickerPointer() {
	panic("unported: mj-chat/src/chat/config_picker.rs cancelConfigPickerPointer")
}

func (c *ChatState) resetConfigPickerGeometry() {
	panic("unported: mj-chat/src/chat/config_picker.rs resetConfigPickerGeometry")
}

// handleConfigPickerMouse routes a pointer event to the open picker,
// reporting whether the picker consumed it and the action it produced.
func (c *ChatState) handleConfigPickerMouse(mouse tea.MouseMsg) (bool, ChatAction) {
	panic("unported: mj-chat/src/chat/config_picker.rs handleConfigPickerMouse")
}

func (c *ChatState) handleConfigPickerEvent(key tea.KeyPressMsg) ChatAction {
	panic("unported: mj-chat/src/chat/config_picker.rs handleConfigPickerEvent")
}

func (c *ChatState) applyConfigPickerSelection() ChatAction {
	panic("unported: mj-chat/src/chat/config_picker.rs applyConfigPickerSelection")
}

// renderConfigPicker draws the open picker anchored to its prompt chip, or
// as a dialog, and reports the rectangle it covers.
func renderConfigPicker(frame *Frame, area uv.Rectangle, chat *ChatState) (uv.Rectangle, bool) {
	panic("unported: mj-chat/src/chat/config_picker.rs renderConfigPicker")
}

func renderConfigDialog(frame *Frame, area uv.Rectangle, picker *configPicker, rows []Line) uv.Rectangle {
	panic("unported: mj-chat/src/chat/config_picker.rs renderConfigDialog")
}
