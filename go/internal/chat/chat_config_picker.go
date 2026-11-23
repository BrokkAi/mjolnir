// Port of mj-chat/src/chat/config_picker.rs.

// Compact, anchored session-setting and value dropdowns with type-to-filter.

package chat

import (
	"fmt"
	"slices"
	"strings"

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
	var items []configPickerItem
	for index := range options {
		option := &options[index]
		key := configOptionKey(option)
		if slices.ContainsFunc(items, func(item configPickerItem) bool {
			existing, ok := item.(configPickerItemOption)
			return ok && existing.key == key
		}) {
			continue
		}
		if len(core.SessionConfigChoices(options, key)) == 0 {
			continue
		}
		current := core.ConfigCurrentValue(options, key)
		label := strings.TrimSpace(option.Name)
		if label == "" {
			label = string(option.ID)
		}
		items = append(items, configPickerItemOption{key: key, label: label, current: current})
	}
	if len(items) == 0 {
		return nil
	}
	picker := newConfigPicker("Session settings", nil, nil, dialog, items)
	return &picker
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
	selected := 0
	if current != nil {
		for index, item := range items {
			if value, ok := item.(configPickerItemValue); ok && value.choice.Value == *current {
				selected = index
				break
			}
		}
	}
	filtered := make([]int, len(items))
	for index := range filtered {
		filtered[index] = index
	}
	form := NewForm[configControl]()
	form.Declare(configControlFilter, ControlKindTextField{})
	form.Declare(configControlValues, ControlKindChoiceList{Len: len(items), Selected: selected})
	form.SetListActivation(configControlValues, ListActivationSingleClick)
	form.EndFrame(configControlValues)
	return configPicker{
		title:    title,
		key:      key,
		current:  current,
		dialog:   dialog,
		items:    items,
		filter:   NewTextInput(),
		form:     form,
		filtered: filtered,
		area:     uv.Rectangle{},
	}
}

func (p *configPicker) selected() int {
	selected, ok := p.form.Selected(configControlValues)
	if !ok {
		return 0
	}
	return selected
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
	kept := -1
	if selected := p.selected(); selected >= 0 && selected < len(p.filtered) {
		kept = p.filtered[selected]
	}
	if p.filter.IsEmpty() {
		p.filtered = make([]int, len(p.items))
		for index := range p.filtered {
			p.filtered[index] = index
		}
	} else {
		p.filtered = matchingIndices(p.items, p.filter.Value(), func(item *configPickerItem) (string, *string) {
			switch item := (*item).(type) {
			case configPickerItemOption:
				return item.key, &item.label
			case configPickerItemValue:
				return item.choice.Value, &item.choice.Name
			}
			return "", nil
		})
	}
	selected := 0
	if kept >= 0 {
		if index := slices.Index(p.filtered, kept); index >= 0 {
			selected = index
		}
	}
	// Filtering invalidates old row hitboxes even if the match count is
	// unchanged.
	p.form.ResetGeometry()
	p.form.Declare(configControlValues, ControlKindChoiceList{Len: len(p.filtered), Selected: selected})
}

func (p *configPicker) rows() []Line {
	rows := make([]Line, 0, len(p.filtered))
	for _, index := range p.filtered {
		switch item := p.items[index].(type) {
		case configPickerItemOption:
			current := ""
			if item.current != nil {
				current = ": " + *item.current
			}
			rows = append(rows, RawLine("  "+item.label+current))
		case configPickerItemValue:
			marker := " "
			if p.current != nil && *p.current == item.choice.Value {
				marker = ThemeGlyphs().Check
			}
			rows = append(rows, RawLine(marker+" "+configChoiceName(&item.choice)))
		}
	}
	return rows
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
	// Refreshes call this for every open chat; rebuilding the popups would
	// move an open completion's highlight back to the first entry.
	if slices.Equal(c.policyConfigKeys, keys) {
		return
	}
	c.policyConfigKeys = slices.Clone(keys)
	if c.configPicker != nil && c.configPicker.key != nil && !c.configKeyUserSettable(*c.configPicker.key) {
		c.configPicker = nil
	}
	c.updateAutocomplete()
}

func (c *ChatState) configKeyUserSettable(key string) bool {
	// `/config mode` reaches whichever selector `mode` resolves to.
	resolved := key
	if option := core.FindSessionConfigOption(c.acpSurface.ConfigOptions(), key); option != nil {
		resolved = string(option.ID)
	}
	return !slices.ContainsFunc(c.policyConfigKeys, func(policyKey string) bool {
		return policyKey == resolved || policyKey == key
	})
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
	switch key {
	case "model":
		if model, ok := c.currentModel(); ok {
			return &model
		}
		return nil
	case "effort":
		if effort, ok := c.currentEffort(); ok {
			return &effort
		}
		return nil
	}
	return core.ConfigCurrentValue(c.acpSurface.ConfigOptions(), key)
}

func (c *ChatState) openConfigMenu() bool {
	var options []schema.SessionConfigOption
	for _, option := range c.acpSurface.ConfigOptions() {
		if c.configKeyUserSettable(configOptionKey(&option)) {
			options = append(options, option)
		}
	}
	picker := configPickerNewOptions(options, true)
	if picker == nil {
		return false
	}
	c.configPicker = picker
	return true
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
	position := uv.Pos(column, row)
	for _, chip := range c.configChipAreas {
		if position.In(chip.area) {
			return chip.key, true
		}
	}
	return "", false
}

func (c *ChatState) openPromptConfigPicker(key string) {
	if c.workerClosing() {
		c.SetNotice("The worker is closing; this configuration change was not sent")
		return
	}
	if !c.openConfigPicker(key) {
		c.SetNotice(fmt.Sprintf("The agent does not advertise %s values; usage: /%s <value>", key, key))
	}
}

func (c *ChatState) configPickerPointerCaptured() bool {
	return c.configPicker != nil && c.configPicker.form.CapturesPointer()
}

func (c *ChatState) cancelConfigPickerPointer() {
	if c.configPicker != nil {
		c.configPicker.form.CancelPointer()
	}
}

func (c *ChatState) resetConfigPickerGeometry() {
	if c.configPicker != nil {
		c.configPicker.form.ResetGeometry()
		c.configPicker.area = uv.Rectangle{}
	}
}

// handleConfigPickerMouse routes a pointer event to the open picker,
// reporting whether the picker consumed it and the action it produced.
func (c *ChatState) handleConfigPickerMouse(mouse tea.MouseMsg) (bool, ChatAction) {
	picker := c.configPicker
	if picker == nil {
		return false, ChatActionNone{}
	}
	pointer := mouse.Mouse()
	if _, press := mouse.(tea.MouseClickMsg); press && pointer.Button == tea.MouseLeft &&
		!uv.Pos(pointer.X, pointer.Y).In(picker.area) {
		c.configPicker = nil
		return true, ChatActionNone{}
	}
	result := picker.form.Handle(mouse)
	if activate, ok := result.Action.(InteractionActivate[configControl]); ok && activate.ID == configControlValues {
		return true, c.applyConfigPickerSelection()
	}
	return true, ChatActionNone{}
}

func (c *ChatState) handleConfigPickerEvent(key tea.KeyPressMsg) ChatAction {
	picker := c.configPicker
	if picker == nil {
		return ChatActionNone{}
	}
	// Editing remains available after arrow navigation or a scroll gesture.
	code := KeyPressCode(key)
	editing := IsCharKeyCode(code) || code == tea.KeyBackspace || code == tea.KeyDelete ||
		code == tea.KeyLeft || code == tea.KeyRight
	if editing {
		picker.form.Focus(configControlFilter)
	} else {
		picker.form.Focus(configControlValues)
	}
	switch action := picker.form.Handle(key).Action.(type) {
	case InteractionEdit[configControl]:
		if action.ID == configControlFilter {
			TextFieldApply(&picker.filter, action.Edit)
			picker.refilter()
		}
	case InteractionActivate[configControl]:
		if action.ID == configControlValues {
			return c.applyConfigPickerSelection()
		}
	case InteractionCancel[configControl]:
		c.configPicker = nil
	}
	if c.configPicker != nil {
		c.configPicker.form.Focus(configControlValues)
	}
	return ChatActionNone{}
}

func (c *ChatState) applyConfigPickerSelection() ChatAction {
	picker := c.configPicker
	if picker == nil {
		return ChatActionNone{}
	}
	selection := picker.selection()
	if selection == nil {
		return ChatActionNone{}
	}
	dialog := picker.dialog
	var key, value string
	switch item := selection.(type) {
	case configPickerItemOption:
		if !c.openConfigPickerWithPresentation(item.key, dialog) {
			c.SetNotice(fmt.Sprintf("The agent no longer advertises values for %s; this configuration change was not sent", item.key))
		}
		return ChatActionNone{}
	case configPickerItemValue:
		key, value = item.key, item.choice.Value
	}
	c.configPicker = nil
	if c.workerClosing() {
		c.SetNotice("The worker is closing; this configuration change was not sent")
		return ChatActionNone{}
	}
	if !slices.ContainsFunc(c.configValues(key), func(choice core.SessionConfigChoice) bool { return choice.Value == value }) {
		c.SetNotice(fmt.Sprintf("The agent no longer advertises %s value %s; this configuration change was not sent", key, value))
		return ChatActionNone{}
	}
	return ChatActionSetConfig{Key: key, Value: value}
}

// renderConfigPicker draws the open picker anchored to its prompt chip, or
// as a dialog, and reports the rectangle it covers.
func renderConfigPicker(frame *Frame, area uv.Rectangle, chat *ChatState) (uv.Rectangle, bool) {
	picker := chat.configPicker
	if picker == nil {
		return uv.Rectangle{}, false
	}
	anchor := uv.Rect(area.Min.X, max(area.Max.Y-1, 0), 1, 1)
	anchored := false
	for _, chip := range chat.configChipAreas {
		if picker.key != nil && *picker.key == chip.key {
			anchor, anchored = chip.area, true
			break
		}
	}
	if !anchored && chat.voiceButtonArea != nil {
		anchor = *chat.voiceButtonArea
	}
	rows := picker.rows()
	if picker.dialog {
		return renderConfigDialog(frame, area, picker, rows), true
	}
	title := " " + picker.displayTitle() + " "
	width := len("No matches")
	if len(rows) > 0 {
		width = 0
		for _, row := range rows {
			width = max(width, row.Width())
		}
	}
	width = max(width, displayWidth(title)) + 2
	selected := picker.selected()
	picker.area = uv.Rectangle{}
	outer, inner, ok := AutocompletePopupRender(frame, area, anchor, width, max(len(rows), 1),
		ComboBoxDefaultMaxVisibleRows, title, PopupSideAbove)
	if !ok {
		picker.form.ResetGeometry()
		return uv.Rectangle{}, false
	}
	picker.form.BeginFrame()
	picker.area = outer
	picker.form.Declare(configControlFilter, ControlKindTextField{})
	ChoiceListRender(frame, inner, rows, selected, &picker.form, configControlValues)
	if len(rows) == 0 {
		frame.RenderWidget(NewParagraph(RawText("No matches")).WithStyle(Muted()), inner)
	}
	picker.form.EndFrame(configControlValues)
	return outer, true
}

func renderConfigDialog(frame *Frame, area uv.Rectangle, picker *configPicker, rows []Line) uv.Rectangle {
	width := min(min(max(max(area.Dx()-4, 0), 32), 76), max(area.Dx(), 1))
	desiredHeight := len(rows) + 4
	height := min(desiredHeight, max(area.Dy(), 1), 16)
	popup := centeredModalRectFixed(frame, width, height, area)
	inner := RectInner(popup, Margin{Horizontal: 1, Vertical: 1})
	chunks := VerticalLayout(ConstraintMin(1), ConstraintLength(1)).Split(inner)
	title := LineFrom(StyledSpan(" "+picker.displayTitle()+" ", Title(true)))
	frame.RenderWidget(Panel(true).
		WithTitle(title).
		WithTitleBottom(LineFrom(StyledSpan(" Esc close ", Muted())).RightAligned()), popup)

	picker.form.BeginFrame()
	picker.form.Declare(configControlFilter, ControlKindTextField{})
	ChoiceListRender(frame, chunks[0], rows, picker.selected(), &picker.form, configControlValues)
	if len(rows) == 0 {
		frame.RenderWidget(NewParagraph(RawText("No matches")).WithStyle(Muted()), chunks[0])
	}
	frame.RenderWidget(NewParagraph(RawText(" type to filter · ↑/↓ select · Enter choose ")).WithStyle(Muted()), chunks[1])
	picker.area = popup
	picker.form.EndFrame(configControlValues)
	return popup
}
