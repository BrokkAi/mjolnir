// Port of mj-chat/src/chat/config_picker.rs (test-only items).

package chat

func (p *configPicker) selectedValue() (string, bool) {
	value, ok := p.selection().(configPickerItemValue)
	if !ok {
		return "", false
	}
	return value.choice.Value, true
}
