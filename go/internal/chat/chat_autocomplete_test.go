// Port of mj-chat/src/chat/autocomplete.rs (test-only items).

package chat

func (c *ChatState) listsCommand(name string) bool {
	for _, command := range c.commandChoices {
		if command.name == name {
			return true
		}
	}
	return false
}
